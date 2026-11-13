//! Extension-owned objects that pg_dump gets wrong for a per-database backup (0.6.2).
//!
//! - **pgmq**: every queue (`pgmq.q_<name>`, `pgmq.a_<name>`) is a member of the pgmq extension, and pg_dump never
//!   dumps extension members. A plain dump has no queues and no messages, and its GRANTs on them fail on restore.
//!   pgbx saves them next to the dump as `<ts>.ext.sql.zst`: a psql script that recreates each queue with
//!   pgmq's own create function (table + indexes), COPYs the rows back, moves the msg_id sequence past them and
//!   replays the grants. Restore runs it with psql after pg_restore.
//! - **pg_cron** can only live in the database named by `cron.database_name`. Restoring a dump that has it into any
//!   other database fails, and restoring it under the same name starts every job at once. So pg_cron's entries
//!   are left out of a restore into another database, and a restore into the cron database pauses every job
//!   (`active = false`); the jobs are listed in the result either way.

use crate::extras::{download, upload};
use crate::worker::{client_tool, connect, pe, Ctx, JobCfg};
use crate::{crypt, globals::jstr};
use postgres::{Client, GenericClient};
use s3::Bucket;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// `<ts>.dump` -> `<ts>.ext.sql.zst`
pub fn ext_key(dump_key: &str) -> String {
    format!("{}.ext.sql.zst", dump_key.strip_suffix(".dump").unwrap_or(dump_key))
}

/// Quote an identifier for SQL.
fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Quote a literal for SQL.
fn ql(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

struct Queue {
    name: String,
    unlogged: bool,
    partitioned: bool,
}

fn queues(cl: &mut impl GenericClient) -> Result<Vec<Queue>, String> {
    let has = cl
        .query_opt("SELECT 1 FROM pg_extension WHERE extname = 'pgmq'", &[])
        .map_err(pe)?
        .is_some();
    if !has {
        return Ok(vec![]);
    }
    // older pgmq has no is_unlogged / is_partitioned columns: read them only when present
    let cols: Vec<String> = cl
        .query("SELECT column_name::text FROM information_schema.columns WHERE table_schema='pgmq' AND table_name='meta'", &[])
        .map_err(pe)?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let flag = |c: &str| if cols.iter().any(|x| x == c) { c.to_string() } else { "false".into() };
    let sql = format!(
        "SELECT queue_name::text, {}, {} FROM pgmq.meta ORDER BY queue_name",
        flag("is_unlogged"),
        flag("is_partitioned")
    );
    Ok(cl
        .query(&sql, &[])
        .map_err(pe)?
        .iter()
        .map(|r| Queue { name: r.get(0), unlogged: r.get(1), partitioned: r.get(2) })
        .collect())
}

/// Columns of a table, in order (COPY needs them listed: a later pgmq may add columns).
fn columns(cl: &mut impl GenericClient, table: &str) -> Result<Vec<String>, String> {
    Ok(cl
        .query(
            "SELECT attname::text FROM pg_attribute WHERE attrelid = to_regclass($1) AND attnum > 0 AND NOT attisdropped ORDER BY attnum",
            &[&table],
        )
        .map_err(pe)?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// GRANT statements on a table, each wrapped so a role missing on the target is a NOTICE, not a failure.
fn grants(cl: &mut impl GenericClient, schema: &str, table: &str) -> Result<Vec<String>, String> {
    Ok(cl
        .query(
            "SELECT grantee::text, string_agg(privilege_type, ', ' ORDER BY privilege_type)
               FROM information_schema.role_table_grants
              WHERE table_schema = $1 AND table_name = $2 AND grantee <> 'postgres'
                AND grantee NOT IN (SELECT rolname FROM pg_roles r JOIN pg_class c ON c.relowner = r.oid
                                     WHERE c.oid = to_regclass(quote_ident($1) || '.' || quote_ident($2)))
              GROUP BY grantee ORDER BY grantee",
            &[&schema, &table],
        )
        .map_err(pe)?
        .iter()
        .map(|r| {
            let (who, privs): (String, String) = (r.get(0), r.get(1));
            let to = if who == "PUBLIC" { "PUBLIC".to_string() } else { qi(&who) };
            let stmt = format!("GRANT {privs} ON {}.{} TO {to}", qi(schema), qi(table));
            format!(
                "DO $g$ BEGIN EXECUTE {}; EXCEPTION WHEN undefined_object THEN RAISE NOTICE 'pgbx: role {} missing, grant skipped'; END $g$;\n",
                ql(&stmt),
                who.replace('\'', "''")
            )
        })
        .collect())
}

/// Write the psql script for every pgmq queue of `db` into `w`. Returns the queue names.
fn write_queues(c: &Ctx, db: &str, w: &mut impl Write) -> Result<Vec<String>, String> {
    let mut cl = connect(c, db)?;
    // one snapshot for every queue, like pg_dump
    let mut tx = cl
        .build_transaction()
        .isolation_level(postgres::IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .map_err(pe)?;
    let qs = queues(&mut tx)?;
    let io = |e: std::io::Error| format!("write: {e}");
    writeln!(w, "-- pgbx: pgmq queues of {db} (pg_dump skips extension members)\n\\set ON_ERROR_STOP on").map_err(io)?;
    let mut names = vec![];
    for q in &qs {
        // the create functions are idempotent (CREATE ... IF NOT EXISTS, meta ON CONFLICT DO NOTHING)
        let create = if q.partitioned {
            // needs pg_partman and the original intervals; pgmq keeps them in its meta table, not here
            writeln!(w, "\\echo 'pgbx: queue {} is partitioned: create it with pgmq.create_partitioned before restoring its rows'", q.name.replace('\'', "")).map_err(io)?;
            continue;
        } else if q.unlogged {
            "pgmq.create_unlogged"
        } else {
            "pgmq.create"
        };
        writeln!(w, "SELECT {create}({});", ql(&q.name)).map_err(io)?;
        for prefix in ["q_", "a_"] {
            let table = format!("{prefix}{}", q.name);
            let full = format!("pgmq.{}", qi(&table));
            let cols = columns(&mut tx, &full)?;
            if cols.is_empty() {
                continue;
            }
            let collist = cols.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ");
            writeln!(w, "TRUNCATE {full};\nCOPY {full} ({collist}) FROM stdin;").map_err(io)?;
            let mut r = tx
                .copy_out(&format!("COPY {full} ({collist}) TO STDOUT"))
                .map_err(|e| format!("copy {full}: {}", pe(e)))?;
            std::io::copy(&mut r, w).map_err(io)?;
            drop(r);
            writeln!(w, "\\.").map_err(io)?;
            // msg_id comes from an identity / serial sequence: move it past the restored rows
            writeln!(
                w,
                "SELECT setval(s, GREATEST(m, 1), m IS NOT NULL) FROM (SELECT pg_get_serial_sequence({}, 'msg_id') AS s, (SELECT max(msg_id) FROM {full}) AS m) x WHERE s IS NOT NULL;",
                ql(&full)
            )
            .map_err(io)?;
            for g in grants(&mut tx, "pgmq", &table)? {
                w.write_all(g.as_bytes()).map_err(io)?;
            }
        }
        names.push(q.name.clone());
    }
    tx.commit().map_err(pe)?;
    Ok(names)
}

/// After the dump: save the pgmq queues of `db` next to it. A JSON fragment for the backup's history params;
/// nothing is written when the database has no queues. A failure fails the backup (the queues would be lost).
pub(crate) fn backup_ext(c: &Ctx, cfg: &JobCfg, b: &Bucket, dump_key: &str, db: &str, k: Option<&crypt::Key>) -> Result<String, String> {
    let has_pgmq = {
        let mut cl = connect(c, db)?;
        !queues(&mut cl)?.is_empty()
    };
    let cron = cron_jobs(c, db).unwrap_or_default();
    let cron_part = if cron.is_empty() { String::new() } else { format!(",\"cron_jobs\":{}", cron.len()) };
    if !has_pgmq {
        return Ok(cron_part);
    }
    let key = ext_key(dump_key);
    let (mut rd, mut wr) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
    let (c2, db2) = (Ctx { socket: c.socket.clone(), port: c.port, bindir: c.bindir.clone(), server: c.server.clone() }, db.to_string());
    let writer = std::thread::spawn(move || {
        let r = write_queues(&c2, &db2, &mut wr);
        drop(wr); // EOF for the upload
        r
    });
    let up = {
        let mut z = zstd::stream::read::Encoder::new(&mut rd, 3).map_err(|e| format!("zstd: {e}"))?;
        upload(b, &key, &mut z, k, cfg.upload_kbps, &mut |_| {})
    };
    drop(rd);
    let names = writer.join().map_err(|_| "pgmq writer panicked".to_string())?;
    match (up, names) {
        (Ok(_), Ok(n)) => Ok(format!(
            ",\"ext\":{},\"pgmq_queues\":[{}]{cron_part}",
            jstr(&key),
            n.iter().map(|q| jstr(q)).collect::<Vec<_>>().join(",")
        )),
        (up, names) => {
            let _ = crate::s3auth::fresh(b).map(|b| b.delete_object(&key));
            Err(format!("pgmq queues not saved: {}", names.err().or(up.err()).unwrap_or_default()))
        }
    }
}

/// After pg_restore: recreate the queues from `<ts>.ext.sql.zst` with psql. Ok(None) when the backup has none
/// (every backup before 0.6.2, and any database without pgmq).
pub(crate) fn restore_ext(c: &Ctx, cfg: &JobCfg, b: &Bucket, dump_key: &str, into: &str) -> Result<Option<String>, String> {
    let key = ext_key(dump_key);
    match crate::s3auth::fresh(b).and_then(|fb| fb.head_object(&key).map_err(|e| e.to_string())) {
        Ok((_, 200)) => {}
        Ok(_) => return Ok(None),
        Err(e) if e.contains("404") => return Ok(None),
        Err(e) => return Err(format!("{key}: {e}")),
    }
    let mut child = Command::new(client_tool(c, "psql").0)
        .env("PGAPPNAME", "pgbx_restore")
        .args(["-X", "-q", "-h", &c.socket, "-p", &c.port.to_string(), "-U", "postgres", "-d", into, "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn psql: {e}"))?;
    let stdin = child.stdin.take().unwrap();
    let mut z = zstd::stream::write::Decoder::new(stdin).map_err(|e| format!("zstd: {e}"))?;
    let dl = download(cfg, b, &key, &mut z, &mut |_| {});
    let fl = z.flush();
    drop(z); // EOF for psql
    let out = child.wait_with_output().map_err(pe)?;
    if !out.status.success() {
        return Err(format!("pgmq queues: psql: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    dl?;
    fl.map_err(|e| format!("zstd: {e}"))?;
    Ok(Some(key))
}

// ---------------------------------------------------------------- pg_cron

/// Jobs of pg_cron when `db` is the cron database (name, schedule, command, database), else empty.
fn cron_jobs(c: &Ctx, db: &str) -> Result<Vec<String>, String> {
    let mut cl = connect(c, db)?;
    let has = cl.query_opt("SELECT 1 FROM pg_extension WHERE extname = 'pg_cron'", &[]).map_err(pe)?.is_some();
    if !has {
        return Ok(vec![]);
    }
    Ok(cl
        .query("SELECT coalesce(jobname, jobid::text) FROM cron.job ORDER BY jobid", &[])
        .map_err(pe)?
        .iter()
        .map(|r| r.get(0))
        .collect())
}

/// One `pg_restore -l` line is pg_cron's: the extension itself, its comment, or anything in schema `cron`.
/// Line shape: `<id>; <catalog oid> <oid> <DESC WORDS> <schema|-> <name...> <owner>`.
pub fn is_cron_entry(line: &str) -> bool {
    let l = line.trim();
    if l.is_empty() || l.starts_with(';') {
        return false;
    }
    let Some((_, rest)) = l.split_once("; ") else { return false };
    let mut it = rest.splitn(3, ' ');
    let (_, _, tail) = (it.next(), it.next(), it.next().unwrap_or(""));
    // the description is the leading UPPERCASE words; the next word is the schema ("-" for none)
    let words: Vec<&str> = tail.split(' ').collect();
    let n = words.iter().take_while(|w| !w.is_empty() && w.chars().all(|ch| ch.is_ascii_uppercase())).count();
    let schema = words.get(n).copied().unwrap_or("");
    let after = words.get(n + 1..).map(|w| w.join(" ")).unwrap_or_default();
    schema == "cron"
        || (schema == "-" && (after.starts_with("pg_cron ") || after == "pg_cron" || after.starts_with("EXTENSION pg_cron") || after.starts_with("SCHEMA cron")))
}

/// One `pg_restore -l` line is an ACL (GRANT) on a pgmq queue table, which only exists after restore_ext.
pub fn is_pgmq_queue_acl(line: &str) -> bool {
    let l = line.trim();
    l.contains(" ACL pgmq TABLE q_") || l.contains(" ACL pgmq TABLE a_")
}

/// The archive's table of contents (`pg_restore -l`), read from the head of the dump; the download is cut as soon
/// as pg_restore has the list.
pub(crate) fn toc(c: &Ctx, cfg: &JobCfg, b: &Bucket, key: &str) -> Result<String, String> {
    let mut child = Command::new(client_tool(c, "pg_restore").0)
        .arg("-l")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn pg_restore -l: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let _ = download(cfg, b, key, &mut stdin, &mut |_| {}); // "stopped reading" once the list is out: expected
    drop(stdin);
    let list = reader.join().unwrap_or_default();
    let out = child.wait_with_output().map_err(pe)?;
    if !out.status.success() || list.is_empty() {
        return Err(format!("pg_restore -l: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(list)
}

/// The cron database of this server (`cron.database_name`; pg_cron's default is `postgres`).
pub(crate) fn cron_database(admin: &mut Client) -> String {
    admin
        .query_one("SELECT coalesce(nullif(current_setting('cron.database_name', true), ''), 'postgres')", &[])
        .map(|r| r.get(0))
        .unwrap_or_else(|_| "postgres".into())
}

/// The list to restore with `pg_restore -L`, and what was left out (for the result). `None` = nothing to filter.
pub fn filter_toc(list: &str, keep_cron: bool) -> Option<(String, usize, usize)> {
    let (mut cron, mut acl) = (0, 0);
    let kept: Vec<&str> = list
        .lines()
        .filter(|l| {
            if !keep_cron && is_cron_entry(l) {
                cron += 1;
                false
            } else if is_pgmq_queue_acl(l) {
                acl += 1;
                false
            } else {
                true
            }
        })
        .collect();
    (cron + acl > 0).then(|| (kept.join("\n") + "\n", cron, acl))
}

/// After a restore into the cron database: pause every job so nothing runs twice. Returns how many were paused.
pub(crate) fn pause_cron(c: &Ctx, into: &str) -> Result<i64, String> {
    let mut cl = connect(c, into)?;
    let has = cl
        .query_opt("SELECT 1 FROM information_schema.columns WHERE table_schema='cron' AND table_name='job' AND column_name='active'", &[])
        .map_err(pe)?
        .is_some();
    if !has {
        return Ok(0);
    }
    Ok(cl.execute("UPDATE cron.job SET active = false WHERE active", &[]).map_err(pe)? as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOC: &str = "\
;
; Archive created at 2026-11-13 11:23:14 UTC
;
3; 3079 17001 EXTENSION - pg_cron
4512; 0 0 COMMENT - EXTENSION pg_cron
12; 2615 2200 SCHEMA - public pg_database_owner
5; 3079 17100 EXTENSION - pgmq
4513; 0 0 COMMENT - EXTENSION pgmq
230; 1259 17300 TABLE public users app
4400; 0 17300 TABLE DATA public users app
4401; 0 17010 TABLE DATA cron job postgres
4600; 0 0 ACL - SCHEMA cron postgres
4601; 0 0 ACL cron FUNCTION schedule(text, text) postgres
4602; 0 0 ACL pgmq TABLE q_jobs postgres
4603; 0 0 ACL pgmq TABLE a_jobs postgres
4604; 0 0 ACL pgmq TABLE meta postgres
4605; 0 0 SEQUENCE SET cron jobid_seq postgres
4606; 0 0 ACL public TABLE users app
";

    #[test]
    fn cron_entries() {
        let cron: Vec<&str> = TOC.lines().filter(|l| is_cron_entry(l)).collect();
        assert_eq!(cron.len(), 6, "{cron:#?}");
        assert!(cron.iter().all(|l| l.contains("cron")));
        assert!(!is_cron_entry("230; 1259 17300 TABLE public users app"));
        assert!(!is_cron_entry("12; 2615 2200 SCHEMA - public pg_database_owner"));
        assert!(!is_cron_entry("230; 1259 17300 TABLE public cron_settings app"));
    }

    #[test]
    fn filter_drops_cron_and_queue_acls_only() {
        let (kept, cron, acl) = filter_toc(TOC, false).unwrap();
        assert_eq!((cron, acl), (6, 2));
        assert!(kept.contains("EXTENSION - pgmq"));
        assert!(kept.contains("ACL pgmq TABLE meta"));
        assert!(kept.contains("TABLE DATA public users"));
        assert!(!kept.contains("pg_cron"));
        // restoring into the cron database: cron stays, queue ACLs still go
        let (kept, cron, acl) = filter_toc(TOC, true).unwrap();
        assert_eq!((cron, acl), (0, 2));
        assert!(kept.contains("EXTENSION - pg_cron"));
        assert!(filter_toc("230; 1259 17300 TABLE public users app\n", false).is_none());
    }

    #[test]
    fn ext_key_next_to_dump() {
        assert_eq!(ext_key("srv/shop/2026-01-01T02-00-00Z.dump"), "srv/shop/2026-01-01T02-00-00Z.ext.sql.zst");
    }
}

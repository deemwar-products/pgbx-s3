//! The background worker. Postgres starts it (shared_preload_libraries), restarts it if it dies, stops it on shutdown.
//! It talks to each database as a normal client over the local socket, so one worker can serve every database.
//!
//! One server-wide job queue (ADR 0001 §0): every poll the worker collects the queued jobs of every database, picks
//! by priority, then age, then round-robin over databases, and starts each picked job on its own thread (which runs
//! pg_dump / pg_restore as a child process). The main loop never blocks on a job: it supervises them every second,
//! keeps polling, and answers SIGTERM. Each running job holds one advisory-lock slot in the admin database, so no
//! more than pgbx.max_concurrent_jobs run at once, even with a second worker.
//!
//! Job threads never call into Postgres (no elog, no GUC reads): settings are snapshotted on the main thread
//! (`JobCfg`), log lines are queued and written by the main thread, and they talk to databases as ordinary clients.

use crate::*;
use chrono::{DateTime, Utc};
use pgrx::bgworkers::{BackgroundWorker, SignalWakeFlags};
use postgres::{Client, NoTls};
use s3::{Bucket, Region, creds::Credentials};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

pub(crate) const VERIFY_PREFIX: &str = "pgbx_verify_";

pub(crate) fn setting(g: &GucSetting<Option<CString>>) -> Option<String> {
    g.get().map(|c| c.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
}

/// Postgres client errors print as just "db error"; surface the server's actual message.
pub(crate) fn pe<E: std::error::Error + 'static>(e: E) -> String {
    let any: &(dyn std::error::Error + 'static) = &e;
    match any.downcast_ref::<postgres::Error>().and_then(|p| p.as_db_error()) {
        Some(d) => format!("{}{}", d.message(), d.detail().map(|x| format!(" ({x})")).unwrap_or_default()),
        None => e.to_string(),
    }
}

static STOPPING: AtomicBool = AtomicBool::new(false);
static MAIN_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();
static LOG_QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// What the main thread knows about one running job, shared with that job's thread.
#[derive(Default)]
pub(crate) struct JobCtl {
    stop: AtomicBool,      // cancel or shutdown: every long step bails out
    cancelled: AtomicBool, // ... because pgbx.cancel() asked
    pid: AtomicI32,        // the pg_dump / pg_restore child, 0 when none
}

thread_local! {
    static JOB: RefCell<Option<Arc<JobCtl>>> = const { RefCell::new(None) };
}

fn on_main_thread() -> bool {
    MAIN_THREAD.get().is_none_or(|t| *t == std::thread::current().id())
}

fn this_job() -> Option<Arc<JobCtl>> {
    JOB.with(|j| j.borrow().clone())
}

/// True once Postgres asked the worker to stop, or (on a job thread) once that job was cancelled. Sticky (pgrx's
/// flag resets on read), so every long step — child processes, upload parts, retry waits — can bail out within a
/// second and never block a shutdown.
pub(crate) fn shutting_down() -> bool {
    if STOPPING.load(Ordering::Relaxed) {
        return true;
    }
    if this_job().is_some_and(|j| j.stop.load(Ordering::Relaxed)) {
        return true;
    }
    if on_main_thread() && unsafe { !pg_sys::MyBgworkerEntry.is_null() } && BackgroundWorker::sigterm_received() {
        STOPPING.store(true, Ordering::Relaxed);
        return true;
    }
    false
}

/// Why shutting_down() is true, for error messages.
pub(crate) fn stop_reason() -> &'static str {
    if this_job().is_some_and(|j| j.cancelled.load(Ordering::Relaxed)) { "cancelled" } else { "Postgres is shutting down" }
}

/// Remember the job's child process, so the main thread can stop it on cancel or shutdown.
pub(crate) fn set_child(pid: u32) {
    if let Some(j) = this_job() {
        j.pid.store(pid as i32, Ordering::Relaxed);
    }
}

/// One line to the server log at LOG level. Not pgrx::log!: on PG13 its pre-check compares LOG numerically with
/// log_min_messages (default WARNING) and drops every line; errstart() itself knows LOG goes to the server log.
fn pg_log(msg: String) {
    pgrx::pg_sys::panic::ErrorReport::new(pgrx::PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION, msg, "pgbx")
        .report(pgrx::PgLogLevel::LOG);
}

/// The Postgres log, from any thread: job threads queue their lines, the main thread writes them.
pub(crate) fn log(msg: &str) {
    if on_main_thread() {
        pg_log(format!("pgbx: {msg}"));
    } else {
        LOG_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push(msg.to_string());
    }
}

fn drain_logs() {
    let lines = std::mem::take(&mut *LOG_QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    for l in lines {
        pg_log(format!("pgbx: {l}"));
    }
}

#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pgbx_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    let _ = MAIN_THREAD.set(std::thread::current().id());
    crate::crypto_provider();
    s3auth::on_new_credentials(log); // "s3 credentials from instance role via IMDSv2 (role r), temporary, valid until ..."
    log("worker started");
    let mut s = Sched::default();
    let mut next_tick = Instant::now();
    loop {
        if BackgroundWorker::sighup_received() {
            unsafe { pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP) };
            log("settings reloaded");
            s.reprobe = true;
        }
        drain_logs();
        // a job that ended frees its slot: look for the next one at once
        if reap(&mut s) || Instant::now() >= next_tick {
            if let Err(e) = tick(&mut s) {
                log(&format!("tick failed: {e}"));
            }
            next_tick = Instant::now() + Duration::from_secs(POLL_SECONDS.get().max(1) as u64);
        }
        drain_logs();
        if shutting_down() {
            break;
        }
        // supervise running jobs every second; otherwise sleep until the next poll
        let left = next_tick.saturating_duration_since(Instant::now());
        let wait = if s.running.is_empty() { left } else { left.min(Duration::from_secs(1)) };
        if !BackgroundWorker::wait_latch(Some(wait.max(Duration::from_millis(50)))) || shutting_down() {
            break;
        }
    }
    stop_all(&mut s);
    crate::pitr::stop();
    log("worker stopping");
}

/// Shutdown: stop every job (their threads abort uploads and record the failure), wait up to 5 s for them. A thread
/// stuck in an S3 request that does not answer (it ends at the request timeout) is not waited for: the job is marked
/// interrupted at the next start, and Postgres stops promptly.
fn stop_all(s: &mut Sched) {
    STOPPING.store(true, Ordering::Relaxed);
    for r in &s.running {
        r.ctl.stop.store(true, Ordering::Relaxed);
        kill_child(&r.ctl);
    }
    let t0 = Instant::now();
    while !s.running.is_empty() && t0.elapsed() < Duration::from_secs(5) {
        reap(s);
        drain_logs();
        std::thread::sleep(Duration::from_millis(100));
    }
    // a child its thread has not reaped yet (the thread is stuck in an S3 request) must not outlive us unreaped:
    // reparented to the postmaster (PID 1 in a container), a child that died by a signal looks like a crashed backend
    for r in &s.running {
        let pid = r.ctl.pid.load(Ordering::Relaxed);
        // WNOHANG first: 0 = still our running child (kill it, then reap), pid = reaped now, -1 = not ours any more
        if pid > 0 && unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == 0 {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
        }
    }
    drain_logs();
}

fn kill_child(ctl: &JobCtl) {
    let pid = ctl.pid.load(Ordering::Relaxed);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[derive(Clone)]
pub(crate) struct Ctx {
    pub socket: String,
    pub port: i32,
    pub bindir: PathBuf,
    pub server: String,
}

fn ctx() -> Ctx {
    let exec = unsafe { std::ffi::CStr::from_ptr(std::ptr::addr_of!(pg_sys::my_exec_path).cast::<std::ffi::c_char>()) }
        .to_string_lossy()
        .into_owned();
    let bindir = PathBuf::from(exec).parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "postgres".into());
    Ctx {
        socket: setting(&SOCKET_DIR).unwrap_or("/var/run/postgresql".into()),
        port: unsafe { pg_sys::PostPortNumber },
        bindir,
        server: setting(&SERVER_NAME).unwrap_or_else(|| host.trim().to_string()),
    }
}

pub(crate) fn connect(c: &Ctx, db: &str) -> Result<Client, String> {
    Client::configure()
        .host_path(&c.socket)
        .port(c.port as u16)
        .user("postgres")
        .dbname(db)
        .application_name("pgbx")
        .connect(NoTls)
        .map_err(|e| format!("connect {db}: {}", pe(e)))
}

/// (access_key_id, secret_access_key) from a keys file (pgbx.credentials_file)
pub(crate) fn credentials(file: &str) -> Result<(String, String), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("read {file}: {e}"))?;
    let get = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim_start_matches('=').trim().to_string()));
    Ok((get("access_key_id").ok_or("access_key_id missing")?, get("secret_access_key").ok_or("secret_access_key missing")?))
}

/// The S3 settings, read where GUCs may be read (the worker's main thread, a backend). Turning them into a bucket
/// (`S3Spec::bucket`) reads the keys file or asks the AWS default chain, so that happens on the job's thread:
/// the main loop never waits for a metadata service.
#[derive(Clone)]
pub(crate) struct S3Spec {
    bucket: Option<String>,
    endpoint: Option<String>,
    region: String,
    credentials: String, // pgbx.credentials_file: a path, or '' / 'aws-default' (s3auth.rs)
}

pub(crate) fn s3_spec() -> S3Spec {
    S3Spec {
        bucket: setting(&S3_BUCKET),
        endpoint: setting(&S3_ENDPOINT),
        region: setting(&S3_REGION).unwrap_or("us-east-1".into()),
        credentials: setting(&CREDENTIALS_FILE).unwrap_or_default(),
    }
}

impl S3Spec {
    /// Any thread: no GUC reads.
    pub(crate) fn bucket(&self) -> Result<Box<Bucket>, String> {
        crate::crypto_provider();
        let name = self.bucket.clone().ok_or("pgbx.s3_bucket is not set")?;
        let endpoint = self.endpoint.clone().ok_or("pgbx.s3_endpoint is not set")?;
        let creds = if s3auth::uses_chain(&self.credentials) {
            s3auth::from_chain()?.s3()
        } else {
            let (ak, sk) = credentials(&self.credentials)?;
            Credentials::new(Some(&ak), Some(&sk), None, None, None).map_err(pe)?
        };
        let b = Bucket::new(&name, Region::Custom { region: self.region.clone(), endpoint }, creds).map_err(pe)?;
        Ok(b.with_path_style())
    }
}

pub(crate) fn bucket() -> Result<Box<Bucket>, String> {
    s3_spec().bucket()
}

/// Every setting a job needs, read on the main thread when the job starts (GUCs must not be read elsewhere).
#[derive(Clone)]
pub(crate) struct JobCfg {
    s3: S3Spec,
    compression: Option<String>,
    compression_busy: Option<String>,
    lock_ms: i32,
    lock_ms_forced: i32,
    nice: i32,
    ionice: Option<String>,
    restore_sync: bool,
    pub(crate) upload_kbps: i32,
    pub(crate) download_kbps: i32,
    pub(crate) alert_command: Option<String>,
    defer_backoff: Option<String>,
    max_defer: i32,
    max_defer_first: i32,
    pub(crate) admin_db: String,
    // 0.6 extras (extras.rs)
    pub(crate) encryption_key_file: Option<String>,
    pub(crate) role_passwords: bool,
    pub(crate) notify: Option<String>,
    pub(crate) notify_secrets_file: Option<String>,
    // point-in-time restore: (<work_dir>/pgbx-wal.conf, the pgbx CLI) for base backup jobs
    pub(crate) pitr: Option<(PathBuf, PathBuf)>,
}

impl JobCfg {
    pub(crate) fn now() -> JobCfg {
        JobCfg {
            s3: s3_spec(),
            compression: setting(&DUMP_COMPRESSION),
            compression_busy: setting(&DUMP_COMPRESSION_BUSY),
            lock_ms: DUMP_LOCK_TIMEOUT.get(),
            lock_ms_forced: DUMP_LOCK_TIMEOUT_FORCED.get(),
            nice: JOB_NICE.get(),
            ionice: setting(&JOB_IONICE),
            restore_sync: RESTORE_SYNCHRONOUS_COMMIT.get(),
            upload_kbps: UPLOAD_KBPS.get(),
            download_kbps: DOWNLOAD_KBPS.get(),
            alert_command: setting(&ALERT_COMMAND),
            defer_backoff: setting(&DEFER_BACKOFF),
            max_defer: MAX_DEFER.get(),
            max_defer_first: MAX_DEFER_FIRST.get(),
            admin_db: setting(&ADMIN_DB).unwrap_or("postgres".into()),
            encryption_key_file: setting(&ENCRYPTION_KEY_FILE),
            role_passwords: BACKUP_ROLE_PASSWORDS.get(),
            notify: setting(&NOTIFY),
            notify_secrets_file: setting(&NOTIFY_SECRETS_FILE),
            pitr: crate::pitr::job_conf(),
        }
    }

    /// On the job's thread: reads the keys file / asks the AWS default chain (cached), never a GUC.
    fn bucket(&self) -> Result<Box<Bucket>, String> {
        self.s3.bucket()
    }
}

/// A job on its thread, as the main loop sees it.
struct Running {
    db: String,
    id: i64,
    kind: String,
    trigger: String,
    requested: DateTime<Utc>,
    started: DateTime<Utc>,
    slot: i32, // advisory-lock slot 1..max_concurrent_jobs; 0 = the restore lane
    owns_db: Option<String>, // the NEW database a restore / restore test writes into: not served while it runs
    ctl: Arc<JobCtl>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// The worker's state across polls.
#[derive(Default)]
struct Sched {
    running: Vec<Running>,
    last_start: HashMap<String, Instant>, // round-robin: the database that waited longest goes first on ties
    speeds: HashMap<String, Vec<f64>>,     // recent job speeds server-wide per kind (dump bytes/s), for estimates
    cap_written: HashMap<String, String>,  // what server_capacity holds in each database
    last_probe: Option<Instant>,
    reprobe: bool,
    act: HashMap<String, Act>,             // activity sampling per database
    srv_hours: std::collections::BTreeMap<i64, (Stat, f64)>, // finished hours, all databases summed
    srv_flushed: i64,
    srv_copied: std::collections::HashSet<String>, // databases holding the latest server histogram
    slots: std::collections::BTreeMap<String, Vec<i32>>, // each database's backup start hours (UTC hour of week)
    load: LoadNow,                          // last load sample
    load_prev: Option<(f64, f64)>,          // (unix secs, total xacts) of the sample before, for tps
}

/// A queued job of some database, as found this poll.
struct Cand {
    db: String,
    id: i64,
    kind: String,
    trigger: String,
    params: String,
    requested: DateTime<Utc>,
    path: String,
    schedule: String,
    max_backups: i32,
    max_days: i32,
    gfs: Option<String>,  // GFS retention spec ('7d,4w,12m'), NULL = none
    gate: Option<String>, // this database's load_gate (NULL = the server's)
}

/// Pick order (ADR 0001 §0): restore (a human waits) > manual backup > scheduled / first backup (and PITR base
/// backups, which keep the restore window) > verify > prune.
pub(crate) fn priority(kind: &str, trigger: &str, params: &str) -> u8 {
    match kind {
        "restore" => 0,
        "backup" if trigger == "manual" || parse_flat_json(params).get("manual").is_some_and(|v| v == "true") => 1,
        "backup" | "base_backup" => 2,
        "verify" => 3,
        "prune" => 4,
        _ => 5,
    }
}

/// Collect finished job threads; true when any ended.
fn reap(s: &mut Sched) -> bool {
    let mut any = false;
    let mut i = 0;
    while i < s.running.len() {
        if s.running[i].handle.as_ref().is_none_or(|h| h.is_finished()) {
            let mut r = s.running.remove(i);
            if let Some(h) = r.handle.take()
                && h.join().is_err()
            {
                log(&format!("{}: {} #{}: job thread panicked", r.db, r.kind, r.id));
            }
            any = true;
        } else {
            i += 1;
        }
    }
    any
}

/// One pass over every database: queue due jobs, collect what is queued server-wide, start what fits.
fn tick(s: &mut Sched) -> Result<(), String> {
    let c = ctx();
    ensure_template1(&c);
    let admin_db = setting(&ADMIN_DB).unwrap_or("postgres".into());
    let mut admin = connect(&c, &admin_db)?;
    sample_load(&mut admin, s);
    admin.batch_execute("CREATE EXTENSION IF NOT EXISTS pgbx").map_err(pe)?;
    let owned: Vec<String> = s.running.iter().filter_map(|r| r.owns_db.clone()).collect();
    drop_stale_verify_dbs(&mut admin, &owned);
    {
        use std::sync::atomic::AtomicBool;
        static CLEANED: AtomicBool = AtomicBool::new(false);
        // once per worker start, before any job runs: uploads a crash cut off are never a backup. On a thread of its
        // own: with S3 unreachable these calls wait for timeouts, and the poll loop (new databases, queued jobs,
        // shutdown) must never wait for S3.
        if s.running.is_empty() && !CLEANED.swap(true, Ordering::Relaxed) {
            // listed uploads: only those older than 10 minutes (S3's clock may lag ours; the list file covers the rest)
            let (prefix, list, cutoff) = (format!("{}/", c.server), transfer::take_remembered(), Utc::now() - chrono::Duration::minutes(10));
            let spec = s3_spec();
            let _ = std::thread::Builder::new().name("pgbx cleanup".into()).spawn(move || match spec.bucket() {
                Ok(b) => {
                    transfer::abort_remembered(&b, &list);
                    transfer::abort_orphans(&b, &prefix, cutoff);
                }
                Err(_) => transfer::remember_again(&list), // no S3 settings / credentials yet: at the next start
            });
        }
    }
    let dbs: Vec<String> = admin
        .query(
            "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate AND datname NOT LIKE $1 ORDER BY datname",
            &[&format!("{VERIFY_PREFIX}%")],
        )
        .map_err(pe)?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let (mut cands, mut deferred) = (Vec::new(), Vec::new());
    let mut conns: HashMap<String, Client> = HashMap::new();
    s.speeds.clear();
    s.slots.clear();
    for db in &dbs {
        if owned.contains(db) {
            continue; // a restore is still writing it: it is not a live database yet
        }
        match scan_db(&c, &mut admin, db, &mut *s) {
            Ok((cl, mut found, mut later)) => {
                cands.append(&mut found);
                deferred.append(&mut later);
                conns.insert(db.clone(), cl);
            }
            Err(e) => {
                log(&format!("{db}: {e}"));
                let _ = admin.execute(
                    "INSERT INTO pgbx.server_overview (database, state, last_error, seen_at) VALUES ($1, 'worker error', $2, now())
                     ON CONFLICT (database) DO UPDATE SET state = 'worker error', last_error = EXCLUDED.last_error, seen_at = now()",
                    &[db, &e],
                );
            }
        }
    }
    let _ = admin.execute("DELETE FROM pgbx.server_overview WHERE NOT (database::text = ANY($1))", &[&dbs]);
    // point-in-time restore (optional): conf, gaps, archiving watch, base backups queued as jobs (admin database)
    crate::pitr::tick(&c, &mut admin);
    let waiting = start_jobs(&c, &admin_db, s, cands, &mut conns);
    flush_server_activity(&mut admin, &admin_db, s, &mut conns);
    publish_queue(&mut admin, s, &waiting, &deferred, &mut conns);
    maybe_probe(s, &c, &mut admin);
    Ok(())
}

/// "0.5.0" < "0.10.0": compare dotted numeric versions; anything unparsable compares as equal (never update).
pub(crate) fn version_older(installed: &str, available: &str) -> bool {
    let p = |v: &str| v.split('.').map(|x| x.parse::<u64>().ok()).collect::<Option<Vec<_>>>();
    match (p(installed), p(available)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

/// After a binary upgrade the library is new but each database still has the old SQL objects:
/// run ALTER EXTENSION ... UPDATE when the installed version is older than the library's default one.
fn update_extension(cl: &mut Client, db: &str) -> Result<(), String> {
    let row = cl
        .query_opt(
            "SELECT e.extversion, a.default_version FROM pg_extension e JOIN pg_available_extensions a ON a.name = e.extname
             WHERE e.extname = 'pgbx'",
            &[],
        )
        .map_err(pe)?;
    let Some(row) = row else { return Ok(()) };
    let (have, want): (String, Option<String>) = (row.get(0), row.get(1));
    if let Some(want) = want.filter(|w| version_older(&have, w)) {
        cl.batch_execute("ALTER EXTENSION pgbx UPDATE").map_err(|e| format!("{db}: ALTER EXTENSION UPDATE: {}", pe(e)))?;
        log(&format!("{db}: updated extension {have} -> {want}"));
    }
    Ok(())
}

/// Put pgbx in template1 so every new database is born with it. Postgres refuses
/// CREATE DATABASE while anyone is connected to the template, so connect rarely (startup, then every
/// 10 minutes) and close at once — never hold a template1 session.
fn ensure_template1(c: &Ctx) {
    use std::sync::atomic::AtomicU64;
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = Utc::now().timestamp() as u64;
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 600 {
        return;
    }
    let r = connect(c, "template1").and_then(|mut t1| {
        t1.batch_execute("CREATE EXTENSION IF NOT EXISTS pgbx").map_err(|e| format!("template1: {}", pe(e)))?;
        update_extension(&mut t1, "template1") // so new databases are born at the current version
    }); // connection dropped here
    match r {
        Ok(()) => LAST.store(now, Ordering::Relaxed),
        Err(e) => log(&e),
    }
}

/// Scratch databases from a restore test that was cut off by a crash (never one a running test still uses).
fn drop_stale_verify_dbs(admin: &mut Client, owned: &[String]) {
    if let Ok(rows) = admin.query("SELECT datname FROM pg_database WHERE datname LIKE $1", &[&format!("{VERIFY_PREFIX}%")]) {
        for r in rows {
            let name: String = r.get(0);
            if !owned.contains(&name) {
                let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"));
            }
        }
    }
}

/// Is a job of this kind due? `last` = when the previous automatic one was requested.
pub(crate) fn is_due(cron: &str, last: Option<DateTime<Utc>>) -> Result<bool, String> {
    match last {
        None => Ok(true),
        Some(t) => Ok(schedule::next_after(cron, t)? <= Utc::now()),
    }
}

/// Whether the next scheduled backup is due, and how many slots a long dump made it skip.
#[derive(Debug, PartialEq)]
pub(crate) enum Due {
    No,
    Yes { skipped: u32, guard: bool },
}

/// pgbx.overrun_policy (ADR 0001 §0.5). `last_auto`: when the last schedule/first backup was requested;
/// `last_run`: (started, finished) of the newest finished backup of any trigger; `last_good`: when the newest
/// successful backup finished.
/// skip: slots that passed while a dump was running are dropped; the next run is the next slot after that dump
/// finished — unless waiting for it would leave more than `max_gap` schedule intervals since the last good backup
/// finished, then it runs right away (`guard`). catch_up: run once right away.
pub(crate) fn due_backup(
    cron: &str, now: DateTime<Utc>, last_auto: Option<DateTime<Utc>>, last_run: Option<(DateTime<Utc>, DateTime<Utc>)>,
    last_good: Option<DateTime<Utc>>, policy: Overrun, max_gap: f64,
) -> Result<Due, String> {
    let Some(r) = last_auto else { return Ok(Due::Yes { skipped: 0, guard: false }) };
    let slot = schedule::next_after(cron, r)?;
    if slot > now {
        return Ok(Due::No);
    }
    let Some((started, finished)) = last_run.filter(|(s, f)| policy == Overrun::Skip && *s <= slot && *f >= slot) else {
        return Ok(Due::Yes { skipped: 0, guard: false });
    };
    let _ = started;
    let (mut skipped, mut next) = (0u32, slot);
    while next <= finished && skipped < 100_000 {
        skipped += 1;
        next = schedule::next_after(cron, next)?;
    }
    if next <= now {
        return Ok(Due::Yes { skipped, guard: false });
    }
    let interval = (schedule::next_after(cron, next)? - next).num_seconds() as f64;
    let gap_ok = last_good.is_some_and(|g| (next - g).num_seconds() as f64 <= max_gap * interval);
    Ok(if gap_ok { Due::No } else { Due::Yes { skipped, guard: true } })
}

pub(crate) fn last_auto(cl: &mut Client, kind: &str) -> Result<Option<DateTime<Utc>>, String> {
    Ok(cl
        .query_one(
            "SELECT max(requested_at) FROM pgbx.history WHERE kind=$1 AND trigger IN ('schedule','first')",
            &[&kind],
        )
        .map_err(pe)?
        .get::<_, Option<SystemTime>>(0)
        .map(DateTime::<Utc>::from))
}

pub(crate) fn pending(cl: &mut Client, kind: &str) -> Result<i64, String> {
    Ok(cl
        .query_one("SELECT count(*) FROM pgbx.history WHERE kind=$1 AND state IN ('queued','running')", &[&kind])
        .map_err(pe)?
        .get(0))
}

/// Queue a scheduled backup when it is due under pgbx.overrun_policy.
fn queue_scheduled_backup(cl: &mut Client, db: &str, schedule: &str) -> Result<(), String> {
    if pending(cl, "backup")? > 0 {
        return Ok(());
    }
    let last = last_auto(cl, "backup")?;
    let ts = |v: Option<SystemTime>| v.map(DateTime::<Utc>::from);
    let run = cl
        .query_opt(
            "SELECT started, finished FROM pgbx.history WHERE kind='backup' AND started IS NOT NULL AND finished IS NOT NULL
             ORDER BY finished DESC LIMIT 1",
            &[],
        )
        .map_err(pe)?
        .and_then(|r| Some((ts(r.get(0))?, ts(r.get(1))?)));
    let good = ts(cl
        .query_one("SELECT max(finished) FROM pgbx.history WHERE kind='backup' AND state IN ('done','expired')", &[])
        .map_err(pe)?
        .get(0));
    let due = due_backup(schedule, Utc::now(), last, run, good, OVERRUN_POLICY.get(), OVERRUN_MAX_GAP.get())?;
    if let Due::Yes { skipped, guard } = due {
        let trigger = if last.is_none() { "first" } else { "schedule" };
        let params = if skipped > 0 {
            format!("{{\"skipped_slots\":{skipped}{}}}", if guard { ",\"overrun_guard\":true" } else { "" })
        } else {
            "{}".into()
        };
        cl.execute("INSERT INTO pgbx.history (kind, trigger, params) VALUES ('backup', $1, $2::text::jsonb)", &[&trigger, &params])
            .map_err(pe)?;
        if skipped > 0 {
            log(&format!(
                "{db}: {skipped} schedule slot(s) passed while a dump was running (pgbx.overrun_policy = skip){}",
                if guard { "; running now, pgbx.overrun_max_gap reached" } else { "" }
            ));
        }
    }
    Ok(())
}

/// Per database, every poll: keep the extension current, recover jobs a restart cut off, pass on cancel requests,
/// queue due jobs, publish the overview row; returns the connection, this database's queued jobs that may start now,
/// and the deferred ones (with the reason).
type Scanned = (Client, Vec<Cand>, Vec<(Cand, String)>);

fn scan_db(c: &Ctx, admin: &mut Client, db: &str, s: &mut Sched) -> Result<Scanned, String> {
    let mut cl = connect(c, db)?;
    cl.batch_execute("CREATE EXTENSION IF NOT EXISTS pgbx;").map_err(pe)?;
    update_extension(&mut cl, db)?;
    // 'running' rows no job thread of this worker owns were cut off by a restart or copied in by a restore
    let mine: Vec<i64> = s.running.iter().filter(|r| r.db == db).map(|r| r.id).collect();
    cl.execute("INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING", &[]).map_err(pe)?;
    // ('wal_gap' / 'wal_archive' rows are open PITR incidents, not jobs: 'running' means still open)
    cl.execute(
        "UPDATE pgbx.history SET state='failed', finished=now(), error='interrupted (worker restart or copied by restore)'
          WHERE state='running' AND NOT (id = ANY($1)) AND kind NOT IN ('wal_gap', 'wal_archive')",
        &[&mine],
    )
    .map_err(pe)?;
    let row = cl
        .query_one(
            "SELECT coalesce(path, current_database()), schedule, max_backups, max_days, enabled, verify_schedule, load_gate, gfs
               FROM pgbx.config",
            &[],
        )
        .map_err(pe)?;
    let (path, schedule, max_backups, max_days, enabled, verify_cron): (String, String, i32, i32, bool, Option<String>) =
        (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4), row.get(5));
    let gate: Option<String> = row.get(6);
    let gfs: Option<String> = row.get(7);
    let max_days = max_days.min(MAX_DAYS_LIMIT.get()); // server-wide ceiling wins

    // pgbx.cancel() of a running job: stop its thread and child; the thread records 'cancelled'
    for r in cl.query("SELECT id FROM pgbx.history WHERE state='running' AND params ? 'cancel_requested'", &[]).map_err(pe)? {
        let id: i64 = r.get(0);
        if let Some(job) = s.running.iter().find(|x| x.db == db && x.id == id)
            && !job.ctl.cancelled.swap(true, Ordering::Relaxed)
        {
            job.ctl.stop.store(true, Ordering::Relaxed);
            kill_child(&job.ctl);
            log(&format!("{db}: {} #{id} cancel requested: stopping it", job.kind));
        }
    }

    if enabled {
        queue_scheduled_backup(&mut cl, db, &schedule)?;
        // restore tests follow their own schedule, and only once there is a backup to test
        if let Some(v) = verify_cron.as_deref() {
            let has_backup: bool = cl
                .query_one("SELECT EXISTS (SELECT 1 FROM pgbx.history WHERE kind='backup' AND state='done')", &[])
                .map_err(pe)?
                .get(0);
            // first restore test waits for the schedule (no 'first' trigger): measure from the first backup
            let last_v = last_auto(&mut cl, "verify")?.or(cl
                .query_one("SELECT min(finished) FROM pgbx.history WHERE kind='backup' AND state='done'", &[])
                .map_err(pe)?
                .get::<_, Option<SystemTime>>(0)
                .map(DateTime::<Utc>::from));
            if has_backup && pending(&mut cl, "verify")? == 0 && is_due(v, last_v)? {
                cl.execute("INSERT INTO pgbx.history (kind, trigger) VALUES ('verify', 'schedule')", &[]).map_err(pe)?;
            }
        }
    }

    // queued jobs; a deferred one waits for its deferred_until
    let (mut found, mut deferred) = (Vec::new(), Vec::new());
    for j in cl
        .query(
            "SELECT id, kind, params::text, trigger, requested_at,
                    CASE WHEN (params->>'deferred_until')::timestamptz > now()
                         THEN format('deferred (%s) until %s UTC, runs anyway from %s UTC', coalesce(params->>'defer_reason', '?'),
                                     to_char((params->>'deferred_until')::timestamptz AT TIME ZONE 'UTC', 'HH24:MI:SS'),
                                     coalesce(to_char((params->>'deadline')::timestamptz AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI'), '?')) END
               FROM pgbx.history WHERE state='queued' AND (kind <> 'base_backup' OR current_database() = $1) ORDER BY id",
            &[&setting(&ADMIN_DB).unwrap_or("postgres".into())], // base backups are whole-server jobs of the admin database
        )
        .map_err(pe)?
    {
        let cand = Cand {
            db: db.to_string(),
            id: j.get(0),
            kind: j.get(1),
            params: j.get(2),
            trigger: j.get(3),
            requested: DateTime::<Utc>::from(j.get::<_, SystemTime>(4)),
            path: path.clone(),
            schedule: schedule.clone(),
            max_backups,
            max_days,
            gfs: gfs.clone(),
            gate: gate.clone(),
        };
        match j.get::<_, Option<String>>(5) {
            Some(why) => deferred.push((cand, why)),
            None => found.push(cand),
        }
    }
    if ACTIVITY_SAMPLING.get()
        && let Err(e) = sample_activity(&mut cl, db, s)
    {
        log(&format!("{db}: activity sampling: {e}"));
    }
    if enabled {
        s.slots.insert(db.to_string(), backup_slots(&schedule, Utc::now()));
    }
    // recent job speeds, for the server-wide fallback of the time estimates
    for r in cl
        .query(
            "SELECT kind, bytes::float8 / extract(epoch FROM finished - started)::float8 FROM (
                 SELECT kind, bytes, started, finished, row_number() OVER (PARTITION BY kind ORDER BY id DESC) AS n
                   FROM pgbx.history WHERE kind IN ('backup', 'restore', 'verify') AND state IN ('done', 'expired')
                    AND bytes >= 1048576 AND finished > started) x
              WHERE n <= $1",
            &[&(ETA_SAMPLES.get() as i64)],
        )
        .map_err(pe)?
    {
        s.speeds.entry(r.get::<_, String>(0)).or_default().push(r.get(1));
    }
    prune_audit(&mut cl, db);
    publish_overview(admin, &mut cl, db, &schedule)?;
    Ok((cl, found, deferred))
}

/// Start queued jobs, best first, while job slots are free. A job that cannot start now (its database is
/// already being dumped) does not hold up the jobs behind it. Returns the jobs left waiting, in pick order,
/// each with the reason.
fn start_jobs(c: &Ctx, admin_db: &str, s: &mut Sched, mut cands: Vec<Cand>, conns: &mut HashMap<String, Client>) -> Vec<(Cand, String)> {
    let max = MAX_CONCURRENT_JOBS.get().clamp(1, 8);
    let lane = RESTORE_LANE.get();
    let never = Instant::now() - Duration::from_secs(365 * 86_400);
    cands.sort_by_key(|j| (priority(&j.kind, &j.trigger, &j.params), j.requested, *s.last_start.get(&j.db).unwrap_or(&never)));
    let mut waiting = Vec::new();
    for j in cands {
        let used: Vec<i32> = s.running.iter().map(|r| r.slot).collect();
        let general: Vec<i32> = (1..=max).filter(|n| !used.contains(n)).collect();
        let lane_free = lane && !used.contains(&0);
        let mut slots = general.clone();
        if j.kind == "restore" && lane_free {
            slots.push(0); // the restore lane: a restore never waits behind a long dump
        }
        // never two dumps of one database at once, never two base backups
        if (j.kind == "backup" || j.kind == "base_backup") && s.running.iter().any(|r| r.db == j.db && r.kind == j.kind) {
            let why = format!("waits for the {} that is running", if j.kind == "backup" { "backup of this database" } else { "base backup" });
            waiting.push((j, why));
            continue;
        }
        if slots.is_empty() {
            let busy: Vec<String> = s.running.iter().map(|r| format!("{} {} #{}", r.db, r.kind, r.id)).collect();
            let why = format!(
                "waits for a job slot: {} of {max} in use ({}){}",
                used.iter().filter(|n| **n > 0).count(),
                busy.join(", "),
                if j.kind == "restore" && lane { "; the restore lane is busy too" } else { "" }
            );
            waiting.push((j, why));
            continue;
        }
        let cfg = JobCfg::now();
        let p = parse_flat_json(&j.params);
        let deadline = defer_deadline(j.requested, j.trigger == "first", &j.schedule, &cfg);
        // the load gate (ADR 0001 §1): shadow records, on defers with backoff until the deadline
        let mode = gate_of(j.gate.as_deref());
        let manual = j.trigger == "manual" || p.get("manual").is_some_and(|v| v == "true");
        let busy = !s.load.reasons.is_empty();
        let reasons = s.load.reasons.join(", ");
        let action = gate_action(mode, GATE_MANUAL_JOBS.get(), &j.kind, manual, busy, Utc::now() >= deadline);
        if action == GateAction::Defer {
            let n = |k: &str| p.get(k).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
            let wait = backoff_minutes(cfg.defer_backoff.as_deref(), n("deferrals") as usize);
            let until = (Utc::now() + chrono::Duration::minutes(wait as i64)).min(deadline);
            if let Some(cl) = conns.get_mut(&j.db) {
                let r = cl.execute(
                    "UPDATE pgbx.history SET params = params || jsonb_build_object(
                        'deferrals', $2::int, 'busy_deferrals', $3::int, 'deferred_until', $4::timestamptz,
                        'defer_reason', 'busy: ' || $5::text, 'deadline', $6::timestamptz) WHERE id=$1 AND state='queued'",
                    &[&j.id, &(n("deferrals") + 1), &(n("busy_deferrals") + 1), &SystemTime::from(until), &reasons, &SystemTime::from(deadline)],
                );
                if r.is_ok() {
                    log(&format!(
                        "{}: {} #{} deferred, server busy ({reasons}); retry at {}, runs anyway from {}",
                        j.db, j.kind, j.id, until.format("%H:%M:%S"), deadline.format("%Y-%m-%d %H:%M:%S UTC")
                    ));
                }
            }
            let why = format!("deferred (busy: {reasons}) until {} UTC", until.format("%H:%M:%S"));
            waiting.push((j, why));
            continue;
        }
        let Some((slot, lock)) = take_slot(c, admin_db, &slots) else {
            waiting.push((j, "waits for a job slot: another pgbx worker holds them (pgbx.max_concurrent_jobs)".into()));
            continue;
        };
        let Some(cl) = conns.get_mut(&j.db) else { continue };
        let forced = j.kind == "backup" && p.contains_key("deferrals") && Utc::now() >= deadline;
        let shadow: Option<String> = (action == GateAction::Shadow).then(|| reasons.clone());
        if shadow.is_some() {
            log(&format!("{}: {} #{} would wait for a quieter moment (pgbx.load_gate = shadow: {reasons}); starts now", j.db, j.kind, j.id));
        }
        let load_at_start: Option<String> = (mode != Gate::Off).then(|| {
            if busy { format!("busy: {reasons}") } else { format!("quiet ({} active, {} tps)", s.load.active, s.load.tps.map(|x| format!("{x:.0}")).unwrap_or("?".into())) }
        });
        // a cancel() between the scan and now wins: only a row still queued starts
        // what it is expected to take is recorded now: progress is measured against it, accuracy judged by it
        let n = cl.execute(
            "UPDATE pgbx.history h SET state='running', started=now(),
                    params = (h.params - 'queue_position' - 'wait_reason' - 'eta_start')
                             || CASE WHEN $2 THEN '{\"forced\":true}'::jsonb ELSE '{}'::jsonb END
                             || jsonb_build_object('est_bytes', e.est_bytes, 'eta_sec', round(e.est_secs::numeric), 'eta_basis', e.basis)
                             || CASE WHEN h.kind = 'backup' THEN jsonb_build_object('db_size', pg_database_size(current_database()))
                                     ELSE '{}'::jsonb END
                             || CASE WHEN $4::text IS NULL THEN '{}'::jsonb
                                     ELSE jsonb_build_object('would_defer', true, 'would_defer_reason', $4::text) END
                             || CASE WHEN $5::text IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('load_at_start', $5::text) END
               FROM pgbx._estimate($3) e
              WHERE h.id=$1 AND h.state='queued'",
            &[&j.id, &forced, &j.kind, &shadow, &load_at_start],
        );
        if !matches!(n, Ok(1)) {
            continue;
        }
        let owns_db = match j.kind.as_str() {
            "restore" => p.get("into_db").cloned(),
            "verify" => Some(format!("{VERIFY_PREFIX}{}_{}", Utc::now().format("%Y%m%d%H%M%S"), j.id)),
            _ => None,
        };
        let ctl = Arc::new(JobCtl::default());
        let job = Job {
            c: c.clone(),
            cfg,
            db: j.db.clone(),
            id: j.id,
            kind: j.kind.clone(),
            trigger: j.trigger.clone(),
            params: j.params.clone(),
            requested: j.requested,
            path: j.path.clone(),
            schedule: j.schedule.clone(),
            max_backups: j.max_backups,
            max_days: j.max_days,
            gfs: j.gfs.clone(),
            forced,
            scratch: owns_db.clone().filter(|_| j.kind == "verify"),
        };
        let thread_ctl = ctl.clone();
        let spawned = std::thread::Builder::new().name(format!("pgbx job {}", j.id)).spawn(move || {
            JOB.with(|x| *x.borrow_mut() = Some(thread_ctl));
            run_job(job, lock);
        });
        match spawned {
            Ok(h) => {
                let lane_note = if slot == 0 { "restore lane" } else { "slot" };
                log(&format!("{}: {} #{} started ({lane_note} {slot})", j.db, j.kind, j.id));
                s.last_start.insert(j.db.clone(), Instant::now());
                s.running.push(Running {
                    db: j.db,
                    id: j.id,
                    kind: j.kind,
                    trigger: j.trigger,
                    requested: j.requested,
                    started: Utc::now(),
                    slot,
                    owns_db,
                    ctl,
                    handle: Some(h),
                });
            }
            Err(e) => {
                let _ = cl.execute(
                    "UPDATE pgbx.history SET state='failed', finished=now(), error=$2 WHERE id=$1",
                    &[&j.id, &format!("could not start a job thread: {e}")],
                );
            }
        }
    }
    waiting
}

/// Mirror the server-wide queue into the admin database (pgbx.server_queue, `pgbx jobs`) with time estimates, tell
/// each waiting job why it waits and when it should start (history.params, shown by status() / job_eta()), and copy
/// the server's capacity row into every database. Per-database rows are only written when they changed.
fn publish_queue(admin: &mut Client, s: &mut Sched, waiting: &[(Cand, String)], deferred: &[(Cand, String)], conns: &mut HashMap<String, Client>) {
    struct Row<'a> {
        db: &'a str,
        id: i64,
        kind: &'a str,
        trigger: &'a str,
        state: &'a str,
        position: Option<i32>,
        slot: Option<i32>,
        requested: SystemTime,
        started: Option<SystemTime>,
        detail: String,
        eta_start: Option<SystemTime>,
        eta_finish: Option<SystemTime>,
        est_bytes: Option<i64>,
        done_bytes: Option<i64>,
        progress: Option<String>,
    }
    let now = Utc::now();
    let secs_to = |t: Option<SystemTime>| t.map(|t| (DateTime::<Utc>::from(t) - now).num_milliseconds() as f64 / 1000.0);
    let mut sim = QueueSim::new(MAX_CONCURRENT_JOBS.get().clamp(1, 8), RESTORE_LANE.get());
    let mut rows: Vec<Row> = Vec::new();
    for r in &s.running {
        let cancelling = r.ctl.cancelled.load(Ordering::Relaxed);
        let eta = conns.get_mut(&r.db).and_then(|cl| {
            cl.query_opt("SELECT eta_start, eta_finish, est_bytes, done_bytes, progress FROM pgbx.job_eta($1)", &[&r.id]).ok().flatten()
        });
        type EtaRow = (Option<SystemTime>, Option<SystemTime>, Option<i64>, Option<i64>, Option<String>);
        let (es, ef, eb, db_, pr): EtaRow = match &eta {
            Some(x) => (x.get(0), x.get(1), x.get(2), x.get(3), x.get(4)),
            None => (None, None, None, None, None),
        };
        sim.busy(r.slot, secs_to(ef).unwrap_or(0.0));
        rows.push(Row {
            db: &r.db,
            id: r.id,
            kind: &r.kind,
            trigger: &r.trigger,
            state: if cancelling { "cancelling" } else { "running" },
            position: None,
            slot: Some(r.slot),
            requested: r.requested.into(),
            started: Some(r.started.into()),
            detail: if cancelling {
                "cancel requested: stopping it".into()
            } else if r.slot == 0 {
                "running in the restore lane".into()
            } else {
                format!("running in job slot {}", r.slot)
            },
            eta_start: es,
            eta_finish: ef,
            est_bytes: eb,
            done_bytes: db_,
            progress: pr,
        });
    }
    for (i, (j, why)) in waiting.iter().enumerate() {
        let est = conns.get_mut(&j.db).and_then(|cl| cl.query_opt("SELECT est_bytes, est_secs FROM pgbx._estimate($1)", &[&j.kind]).ok().flatten());
        let (eb, secs): (Option<i64>, f64) = est.map(|x| (x.get(0), x.get::<_, f64>(1))).unwrap_or((None, 0.0));
        let start = sim.place(j.kind == "restore", secs);
        let at = now + chrono::Duration::milliseconds((start * 1000.0) as i64);
        let fin = at + chrono::Duration::milliseconds((secs * 1000.0) as i64);
        let when = if start < 60.0 { "now".to_string() } else { at.format("%H:%M UTC").to_string() };
        rows.push(Row {
            db: &j.db, id: j.id, kind: &j.kind, trigger: &j.trigger, state: "queued", position: Some(i as i32 + 1), slot: None,
            requested: j.requested.into(), started: None, detail: why.clone(), eta_start: Some(at.into()), eta_finish: Some(fin.into()),
            est_bytes: eb, done_bytes: None,
            progress: Some(format!("queued, #{} in line: starts ~{when}, takes ~{}", i + 1, dur(secs))),
        });
    }
    for (j, why) in deferred {
        rows.push(Row {
            db: &j.db, id: j.id, kind: &j.kind, trigger: &j.trigger, state: "deferred", position: None, slot: None,
            requested: j.requested.into(), started: None, detail: why.clone(), eta_start: None, eta_finish: None, est_bytes: None,
            done_bytes: None, progress: None,
        });
    }
    let r = (|| -> Result<(), postgres::Error> {
        let mut tx = admin.transaction()?;
        tx.execute("DELETE FROM pgbx.server_queue", &[])?;
        for q in &rows {
            tx.execute(
                "INSERT INTO pgbx.server_queue (database, job_id, kind, trigger, state, position, slot, requested_at, started_at, detail,
                                                eta_start, eta_finish, est_bytes, done_bytes, progress)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
                &[&q.db, &q.id, &q.kind, &q.trigger, &q.state, &q.position, &q.slot, &q.requested, &q.started, &q.detail,
                  &q.eta_start, &q.eta_finish, &q.est_bytes, &q.done_bytes, &q.progress],
            )?;
        }
        tx.commit()
    })();
    if let Err(e) = r {
        log(&format!("server_queue: {}", pe(e)));
    }
    for q in rows.iter().filter(|q| q.state == "queued" || q.state == "deferred") {
        if let Some(cl) = conns.get_mut(q.db) {
            // eta_start only moves the stored value when it shifts by a minute or more (no churn every poll)
            let _ = cl.execute(
                "UPDATE pgbx.history SET params = params || jsonb_build_object('queue_position', $2::int, 'wait_reason', $3::text,
                                                                               'eta_start', $4::timestamptz)
                  WHERE id=$1 AND state='queued'
                    AND (params->'queue_position' IS DISTINCT FROM to_jsonb($2::int) OR params->>'wait_reason' IS DISTINCT FROM $3::text
                         OR abs(extract(epoch FROM coalesce((params->>'eta_start')::timestamptz, '-infinity') - coalesce($4::timestamptz, '-infinity'))) >= 60
                         OR ((params->>'eta_start') IS NULL) <> ($4::timestamptz IS NULL))",
                &[&q.id, &q.position, &q.detail, &q.eta_start],
            );
        }
    }
    publish_capacity(admin, s, conns, (s.running.len() + waiting.len()) as i32, sim.wait_for_new());
}

/// Where queued jobs would start, given when each job slot frees up (seconds from now).
pub(crate) struct QueueSim {
    free: Vec<(i32, f64)>, // (slot, free in secs); slot 0 = the restore lane
}

impl QueueSim {
    pub(crate) fn new(max: i32, lane: bool) -> Self {
        let mut free: Vec<(i32, f64)> = (1..=max).map(|n| (n, 0.0)).collect();
        if lane {
            free.push((0, 0.0));
        }
        QueueSim { free }
    }
    /// A running job holds `slot` for `secs` more.
    pub(crate) fn busy(&mut self, slot: i32, secs: f64) {
        if let Some(f) = self.free.iter_mut().find(|f| f.0 == slot) {
            f.1 = f.1.max(secs.max(0.0));
        }
    }
    /// The next waiting job (restores may use the lane) takes the earliest slot: returns its start in secs.
    pub(crate) fn place(&mut self, restore: bool, secs: f64) -> f64 {
        let Some(f) = self.free.iter_mut().filter(|f| f.0 > 0 || restore).min_by(|a, b| a.1.total_cmp(&b.1)) else { return 0.0 };
        let start = f.1;
        f.1 += secs.max(0.0);
        start
    }
    /// How long a backup queued now would wait.
    pub(crate) fn wait_for_new(&self) -> f64 {
        self.free.iter().filter(|f| f.0 > 0).map(|f| f.1).fold(f64::INFINITY, f64::min).max(0.0)
    }
}

/// Cumulative pg_stat_database counters of one database.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) struct Stat {
    pub xacts: f64,
    pub writes: f64,
    pub reads: f64,
}

/// One database's activity in the current UTC hour, built from per-poll deltas (ADR 0001 §2).
#[derive(Default)]
pub(crate) struct Act {
    last: Option<(f64, Stat, Option<SystemTime>)>, // previous sample: unix secs, counters, stats_reset
    hour: i64,                                     // unix hour being summed
    sum: Stat,
    active_max: f64,
    covered: f64, // seconds of the hour that deltas cover
}

impl Act {
    /// Add a sample taken at `t` (unix seconds). When the hour rolled over and at least 10 minutes of it were seen,
    /// returns that hour's per-hour rates (scaled up to a full hour) and the most active sessions seen. A stats reset
    /// or a counter going backwards drops that delta.
    pub(crate) fn add(&mut self, t: f64, now: Stat, reset: Option<SystemTime>, active: f64) -> Option<(i64, Stat, f64)> {
        let hour = (t / 3600.0).floor() as i64;
        let mut done = None;
        if hour != self.hour {
            if self.covered >= 600.0 {
                let k = 3600.0 / self.covered;
                done = Some((self.hour, Stat { xacts: self.sum.xacts * k, writes: self.sum.writes * k, reads: self.sum.reads * k }, self.active_max));
            }
            (self.hour, self.sum, self.active_max, self.covered) = (hour, Stat::default(), 0.0, 0.0);
        }
        if let Some((pt, prev, preset)) = self.last {
            let dt = t - pt;
            let sane = preset == reset && now.xacts >= prev.xacts && now.writes >= prev.writes && now.reads >= prev.reads;
            if sane && dt > 0.0 && dt < 3600.0 {
                self.sum.xacts += now.xacts - prev.xacts;
                self.sum.writes += now.writes - prev.writes;
                self.sum.reads += now.reads - prev.reads;
                self.covered += dt;
            }
        }
        self.active_max = self.active_max.max(active);
        self.last = Some((t, now, reset));
        done
    }
}

/// Sample this database's activity (pgbx.activity_sampling) and fold a finished hour into its histogram.
fn sample_activity(cl: &mut Client, db: &str, s: &mut Sched) -> Result<(), String> {
    let r = cl
        .query_one(
            "SELECT (d.xact_commit + d.xact_rollback)::float8, (d.tup_inserted + d.tup_updated + d.tup_deleted)::float8,
                    d.blks_read::float8, d.stats_reset,
                    (SELECT count(*) FROM pg_stat_activity a WHERE a.datname = current_database() AND a.state <> 'idle'
                        AND a.backend_type = 'client backend' AND a.pid <> pg_backend_pid()
                        AND coalesce(a.application_name, '') NOT LIKE 'pgbx%')::float8,
                    extract(epoch FROM clock_timestamp())::float8
               FROM pg_stat_database d WHERE d.datname = current_database()",
            &[],
        )
        .map_err(pe)?;
    let now = Stat { xacts: r.get(0), writes: r.get(1), reads: r.get(2) };
    let finished = s.act.entry(db.to_string()).or_default().add(r.get(5), now, r.get(3), r.get(4));
    if let Some((hour, rate, active)) = finished {
        cl.execute(
            "SELECT pgbx._activity_add('db', to_timestamp($1::float8), $2, $3, $4, $5, $6)",
            &[&((hour * 3600) as f64), &rate.xacts, &rate.writes, &rate.reads, &active, &ACTIVITY_DECAY.get()],
        )
        .map_err(pe)?;
        if hour > s.srv_flushed {
            let e = s.srv_hours.entry(hour).or_default();
            e.0.xacts += rate.xacts;
            e.0.writes += rate.writes;
            e.0.reads += rate.reads;
            e.1 += active;
        }
    }
    Ok(())
}

/// Finished hours of every database together: fold them into the admin database's 'server' histogram and copy that
/// into every database (suggest_window() scores the whole server: a dump competes with every database's traffic).
fn flush_server_activity(admin: &mut Client, admin_db: &str, s: &mut Sched, conns: &mut HashMap<String, Client>) {
    let current = (Utc::now().timestamp() as f64 / 3600.0).floor() as i64;
    let done: Vec<i64> = s.srv_hours.keys().copied().filter(|h| *h < current).collect();
    for h in &done {
        let (rate, active) = s.srv_hours.remove(h).unwrap_or_default();
        let r = admin.execute(
            "SELECT pgbx._activity_add('server', to_timestamp($1::float8), $2, $3, $4, $5, $6)",
            &[&((h * 3600) as f64), &rate.xacts, &rate.writes, &rate.reads, &active, &ACTIVITY_DECAY.get()],
        );
        if let Err(e) = r {
            log(&format!("server activity: {}", pe(e)));
        }
        s.srv_flushed = s.srv_flushed.max(*h);
    }
    if !done.is_empty() {
        s.srv_copied.clear();
    }
    let rows = match admin.query(
        "SELECT dow, hour, samples, xacts, writes, reads, active_max FROM pgbx.activity_hourly WHERE scope = 'server'",
        &[],
    ) {
        Ok(r) if !r.is_empty() => r,
        _ => return,
    };
    let col_i16 = |i: usize| rows.iter().map(|r| r.get::<_, i16>(i)).collect::<Vec<i16>>();
    let col_f64 = |i: usize| rows.iter().map(|r| r.get::<_, f64>(i)).collect::<Vec<f64>>();
    let (dow, hour, xacts, writes, reads, act) = (col_i16(0), col_i16(1), col_f64(3), col_f64(4), col_f64(5), col_f64(6));
    let samples: Vec<i32> = rows.iter().map(|r| r.get(2)).collect();
    for (db, cl) in conns.iter_mut() {
        if db == admin_db || s.srv_copied.contains(db) {
            continue;
        }
        let r = (|| -> Result<(), postgres::Error> {
            let mut tx = cl.transaction()?;
            tx.execute("DELETE FROM pgbx.activity_hourly WHERE scope = 'server'", &[])?;
            tx.execute(
                "INSERT INTO pgbx.activity_hourly (scope, dow, hour, samples, xacts, writes, reads, active_max, updated_at)
                 SELECT 'server', * , now() FROM unnest($1::smallint[], $2::smallint[], $3::int[], $4::float8[], $5::float8[],
                                                        $6::float8[], $7::float8[])",
                &[&dow, &hour, &samples, &xacts, &writes, &reads, &act],
            )?;
            tx.commit()
        })();
        match r {
            Ok(()) => {
                s.srv_copied.insert(db.clone());
            }
            Err(e) => log(&format!("{db}: server activity copy: {}", pe(e))),
        }
    }
}

/// Hours of the week (UTC, 0 = Sunday 00:00) a schedule starts backups in, over the next 7 days.
pub(crate) fn backup_slots(cron: &str, from: DateTime<Utc>) -> Vec<i32> {
    use chrono::{Datelike, Timelike};
    let mut out = Vec::new();
    let mut t = from;
    for _ in 0..200 {
        let Ok(n) = schedule::next_after(cron, t) else { break };
        if n > from + chrono::Duration::days(7) {
            break;
        }
        let idx = n.weekday().num_days_from_sunday() as i32 * 24 + n.hour() as i32;
        if !out.contains(&idx) {
            out.push(idx);
        }
        t = n;
    }
    out.sort();
    out
}

/// The server's load, sampled once per poll from the admin connection (ADR 0001 §1); pgbx's own sessions never count.
#[derive(Clone, Debug, Default)]
pub(crate) struct LoadNow {
    at: Option<DateTime<Utc>>,
    active: i32,
    tps: Option<f64>,
    reasons: Vec<String>,
}

/// pgbx.busy_* thresholds; 0 = that signal is ignored.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Busy {
    pub active: i32,
    pub tps: i32,
    pub long_xact: i32,
    pub lag: i32,
    pub loadavg: f64,
}

/// Why the server counts as busy right now (empty = quiet).
pub(crate) fn busy_reasons(active: i32, tps: Option<f64>, long_writers: i32, lag: Option<f64>, load_core: Option<f64>, t: Busy) -> Vec<String> {
    let mut r = Vec::new();
    if t.active > 0 && active > t.active {
        r.push(format!("{active} active sessions > {}", t.active));
    }
    if let Some(x) = tps.filter(|x| t.tps > 0 && *x > t.tps as f64) {
        r.push(format!("{x:.0} tps > {}", t.tps));
    }
    if t.long_xact > 0 && long_writers > 0 {
        r.push(format!("{long_writers} writing transaction(s) open > {}s", t.long_xact));
    }
    if let Some(x) = lag.filter(|x| t.lag > 0 && *x > t.lag as f64) {
        r.push(format!("replica {x:.0}s behind > {}s", t.lag));
    }
    if let Some(x) = load_core.filter(|x| t.loadavg > 0.0 && *x > t.loadavg) {
        r.push(format!("load {x:.2} per core > {}", t.loadavg));
    }
    r
}

/// What the gate does with a job about to start.
#[derive(Debug, PartialEq)]
pub(crate) enum GateAction {
    Run,
    Shadow, // run, and record that it would have waited
    Defer,  // leave it queued with a backoff
}

/// Scheduled backups and restore tests are gated; human jobs only with pgbx.gate_manual_jobs = defer; restores and
/// prunes never. Past the deadline (max_defer) a job always runs.
pub(crate) fn gate_action(mode: Gate, manual_policy: GateManual, kind: &str, manual: bool, busy: bool, past_deadline: bool) -> GateAction {
    let gated = matches!(kind, "backup" | "verify") && (!manual || manual_policy == GateManual::Defer);
    if !busy || !gated || mode == Gate::Off {
        return GateAction::Run;
    }
    match mode {
        Gate::Shadow => GateAction::Shadow,
        Gate::On if past_deadline => GateAction::Run,
        _ => GateAction::Defer,
    }
}

fn gate_of(s: Option<&str>) -> Gate {
    match s {
        Some("off") => Gate::Off,
        Some("on") => Gate::On,
        Some("shadow") => Gate::Shadow,
        _ => LOAD_GATE.get(),
    }
}

/// Sample the load (once per poll, before anything else so the window between polls is the app's, not ours).
fn sample_load(admin: &mut Client, s: &mut Sched) {
    let r = admin.query_one(
        "SELECT (SELECT count(*) FROM pg_stat_activity a WHERE a.state <> 'idle' AND a.backend_type = 'client backend'
                    AND a.pid <> pg_backend_pid() AND coalesce(a.application_name, '') NOT LIKE 'pgbx%')::int,
                (SELECT sum(xact_commit + xact_rollback) FROM pg_stat_database)::float8,
                (SELECT count(*) FROM pg_stat_activity a WHERE a.state = 'active' AND a.backend_type = 'client backend'
                    AND coalesce(a.application_name, '') NOT LIKE 'pgbx%' AND $1::float8 > 0
                    AND now() - a.xact_start > make_interval(secs => $1::float8)
                    AND EXISTS (SELECT 1 FROM pg_locks l WHERE l.pid = a.pid AND l.granted
                                AND l.mode IN ('RowExclusiveLock', 'ShareUpdateExclusiveLock', 'ShareLock', 'ShareRowExclusiveLock',
                                               'ExclusiveLock', 'AccessExclusiveLock')))::int,
                (SELECT extract(epoch FROM max(replay_lag))::float8 FROM pg_stat_replication),
                extract(epoch FROM clock_timestamp())::float8",
        &[&(BUSY_LONG_XACT.get() as f64)],
    );
    let r = match r {
        Ok(r) => r,
        Err(e) => return log(&format!("load sample: {}", pe(e))),
    };
    let (active, xacts, long, lag, t): (i32, f64, i32, Option<f64>, f64) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
    let tps = s.load_prev.filter(|(pt, px)| t > *pt && xacts >= *px).map(|(pt, px)| (xacts - px) / (t - pt));
    s.load_prev = Some((t, xacts));
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    let load_core = std::fs::read_to_string("/proc/loadavg").ok().and_then(|x| x.split_whitespace().next()?.parse::<f64>().ok()).map(|l| l / cores);
    let th = Busy {
        active: BUSY_ACTIVE_BACKENDS.get(),
        tps: BUSY_TPS.get(),
        long_xact: BUSY_LONG_XACT.get(),
        lag: BUSY_REPLICA_LAG.get(),
        loadavg: BUSY_LOADAVG.get(),
    };
    let reasons = busy_reasons(active, tps, long, lag, load_core, th);
    let was = s.load.reasons.is_empty();
    if was != reasons.is_empty() {
        log(&if reasons.is_empty() { "load: quiet again".to_string() } else { format!("load: busy ({})", reasons.join(", ")) });
    }
    s.load = LoadNow { at: Some(Utc::now()), active, tps, reasons };
}

/// Same as pgbx._dur() in SQL.
pub(crate) fn dur(secs: f64) -> String {
    if secs < 90.0 {
        format!("{} s", secs.round())
    } else if secs < 5400.0 {
        format!("{} min", (secs / 60.0).round())
    } else if secs < 172_800.0 {
        format!("{:.1} h", secs / 3600.0)
    } else {
        format!("{:.1} d", secs / 86_400.0)
    }
}

/// pgbx.server_capacity in every database (only where it changed): cpu probe, disk, network, load, server-wide
/// job speeds and the queue summary that the NOTICE on job create uses.
fn publish_capacity(admin: &mut Client, s: &mut Sched, conns: &mut HashMap<String, Client>, queue_jobs: i32, wait_secs: f64) {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let lf = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
        .map(|l| load_factor(l, cores))
        .unwrap_or(1.0);
    let disk: Option<f64> = admin
        .query_one(
            "SELECT sum(blks_read) * current_setting('block_size')::float8 / nullif(sum(blk_read_time) / 1000, 0) FROM pg_stat_database",
            &[],
        )
        .ok()
        .and_then(|r| r.get(0));
    let (up, down) = transfer::network_rates();
    let probe = PROBE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let speeds: Vec<Option<f64>> = ["backup", "restore", "verify"].iter().map(|k| transfer::median(s.speeds.get(*k).map(|v| v.as_slice()).unwrap_or(&[]))).collect();
    let r2 = |x: Option<f64>| x.map(|v| (v / 1e4).round() * 1e4); // 10 kB/s steps: no rewrite for noise
    let wait_min = if wait_secs.is_finite() { (wait_secs / 60.0).round() * 60.0 } else { 0.0 };
    let vals = (
        cores as i32,
        r2(probe.as_ref().map(|p| p.0)),
        probe.as_ref().map(|p| p.1.clone()),
        r2(disk),
        r2(up),
        r2(down),
        (lf * 10.0).round() / 10.0,
        r2(speeds[0]),
        r2(speeds[1]),
        r2(speeds[2]),
        queue_jobs,
        wait_min,
        probe.as_ref().map(|p| SystemTime::from(p.2)),
        format!(
            "{{{}}}",
            s.slots.iter().map(|(d, v)| format!("{}:[{}]", jstr(d), v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))).collect::<Vec<_>>().join(",")
        ),
    );
    let key = format!("{:?} {:?}", (vals.0, vals.1, &vals.2, vals.3, vals.4, vals.5, vals.6), (vals.7, vals.8, vals.9, vals.10, vals.11, vals.12, &vals.13));
    let load = s.load.clone();
    let load_at = load.at.map(SystemTime::from);
    let (busy, reasons) = (load.at.map(|_| !load.reasons.is_empty()), (!load.reasons.is_empty()).then(|| load.reasons.join(", ")));
    let tps = load.tps.map(|x| x.round());
    let admin_db = setting(&ADMIN_DB).unwrap_or("postgres".into());
    let minute = Utc::now().timestamp() / 60;
    for (db, cl) in conns.iter_mut() {
        // the admin database (pgbx load, doctor) gets every sample; the others when busy flips or once a minute
        let key = if *db == admin_db { format!("{key} {load_at:?}") } else { format!("{key} {busy:?} {minute}") };
        if s.cap_written.get(db) == Some(&key) {
            continue;
        }
        let r = cl.execute(
            "INSERT INTO pgbx.server_capacity AS c (id, cores, cpu_bps, cpu_codec, disk_bps, upload_bps, download_bps, load_factor,
                     backup_bps, restore_bps, verify_bps, queue_jobs, wait_secs, measured_at, updated_at, backup_slots)
             VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, now(), $14::text::jsonb)
             ON CONFLICT (id) DO UPDATE SET cores=$1, cpu_bps=$2, cpu_codec=$3, disk_bps=$4, upload_bps=$5, download_bps=$6,
                 load_factor=$7, backup_bps=$8, restore_bps=$9, verify_bps=$10, queue_jobs=$11, wait_secs=$12, measured_at=$13, updated_at=now(),
                 backup_slots=$14::text::jsonb",
            &[&vals.0, &vals.1, &vals.2, &vals.3, &vals.4, &vals.5, &vals.6, &vals.7, &vals.8, &vals.9, &vals.10, &vals.11, &vals.12, &vals.13],
        );
        match r {
            Ok(_) => {
                let _ = cl.execute(
                    "UPDATE pgbx.server_capacity SET load_at=$1, load_busy=$2, load_reasons=$3, load_active=$4, load_tps=$5",
                    &[&load_at, &busy, &reasons, &load.active, &tps],
                );
                s.cap_written.insert(db.clone(), key.clone());
            }
            Err(e) => log(&format!("{db}: server_capacity: {}", pe(e))),
        }
    }
}

/// /proc/loadavg per core -> how much slower a niced dump runs: x1 below 0.7 per core, x3 from 2.0, linear between.
pub(crate) fn load_factor(load1: f64, cores: usize) -> f64 {
    let l = load1 / cores.max(1) as f64;
    (1.0 + (l - 0.7) * 2.0 / 1.3).clamp(1.0, 3.0)
}

/// Last cpu probe: (bytes/s of one core, codec, when).
static PROBE: Mutex<Option<(f64, String, DateTime<Utc>)>> = Mutex::new(None);
static PROBING: AtomicBool = AtomicBool::new(false);

/// pgbx.dump_compression -> (deflate level to time, how much faster the real codec is than that deflate level).
/// None: no compression, so cpu never limits. zstd 1-3 runs ~2.5x deflate level 1; lz4 ~4x.
pub(crate) fn probe_level(codec: &str) -> Option<(u8, f64)> {
    let c = codec.trim().to_ascii_lowercase();
    if matches!(c.as_str(), "none" | "0" | "gzip:0") {
        return None;
    }
    if c.starts_with("zstd") {
        return Some((1, 2.5));
    }
    if c.starts_with("lz4") {
        return Some((1, 4.0));
    }
    let n = c.trim_start_matches("gzip").trim_start_matches(':').parse::<u8>().unwrap_or(6);
    Some((n.clamp(1, 9), 1.0))
}

/// Once a day (and after a reload), with no job running: time one niced core compressing up to 64 MiB of the
/// largest table's pages (pgbx.eta_calibrate). Runs on its own thread; about a second of one core.
fn maybe_probe(s: &mut Sched, c: &Ctx, admin: &mut Client) {
    if !ETA_CALIBRATE.get() || !s.running.is_empty() || PROBING.load(Ordering::Relaxed) {
        return;
    }
    if !s.reprobe && s.last_probe.is_some_and(|t| t.elapsed() < Duration::from_secs(86_400)) {
        return;
    }
    s.reprobe = false;
    s.last_probe = Some(Instant::now());
    let codec = compression(setting(&DUMP_COMPRESSION).as_deref(), client_tool(c, "pg_dump").1);
    let Some((level, factor)) = probe_level(&codec) else {
        *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return;
    };
    let file: Option<String> = (|| {
        let big: String = admin
            .query_opt("SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY pg_database_size(oid) DESC LIMIT 1", &[])
            .ok()??
            .get(0);
        let mut cl = connect(c, &big).ok()?;
        cl.query_opt(
            "SELECT current_setting('data_directory') || '/' || pg_relation_filepath(c.oid) FROM pg_class c
              WHERE c.relkind IN ('r', 'm', 't') AND c.relpersistence = 'p' ORDER BY pg_relation_size(c.oid) DESC LIMIT 1",
            &[],
        )
        .ok()??
        .get(0)
    })();
    let nice = JOB_NICE.get().clamp(0, 19);
    PROBING.store(true, Ordering::Relaxed);
    let spawned = std::thread::Builder::new().name("pgbx probe".into()).spawn(move || {
        #[cfg(target_os = "linux")]
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            if nice > libc::getpriority(libc::PRIO_PROCESS, tid) {
                libc::setpriority(libc::PRIO_PROCESS, tid, nice);
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = nice;
        let mut data = Vec::new();
        if let Some(f) = &file {
            use std::io::Read;
            if let Ok(fh) = std::fs::File::open(f) {
                let _ = fh.take(64 << 20).read_to_end(&mut data);
            }
        }
        if data.len() < (1 << 20) {
            // an empty server: hex text, about as compressible as typical rows
            let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
            data = (0..(16 << 20))
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    b"0123456789abcdef"[(x & 15) as usize]
                })
                .collect();
        }
        let t0 = Instant::now();
        let mut done = 0usize;
        for chunk in data.chunks(1 << 20) {
            let _ = miniz_oxide::deflate::compress_to_vec(chunk, level);
            done += chunk.len();
            if t0.elapsed() > Duration::from_millis(1500) {
                break;
            }
        }
        let rate = done as f64 / t0.elapsed().as_secs_f64().max(1e-3) * factor;
        log(&format!("capacity: one core compresses {:.0} MB/s ({codec}, measured as deflate level {level} x{factor})", rate / 1e6));
        *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = Some((rate, codec, Utc::now()));
        PROBING.store(false, Ordering::Relaxed);
    });
    if spawned.is_err() {
        PROBING.store(false, Ordering::Relaxed);
    }
}

/// One free job slot, held as a session advisory lock in the admin database for as long as the returned
/// connection lives (the job thread keeps it). None when every slot is taken (by this or another worker).
fn take_slot(c: &Ctx, admin_db: &str, slots: &[i32]) -> Option<(i32, Client)> {
    let mut lc = connect(c, admin_db).map_err(|e| log(&format!("job slot: {e}"))).ok()?;
    for &n in slots {
        let got: bool = lc.query_one("SELECT pg_try_advisory_lock(hashtext('pgbx_job'), $1)", &[&n]).ok()?.get(0);
        if got {
            return Some((n, lc));
        }
    }
    None
}

/// Everything a job thread needs; nothing in here touches Postgres internals.
struct Job {
    c: Ctx,
    cfg: JobCfg,
    db: String,
    id: i64,
    kind: String,
    trigger: String,
    params: String,
    requested: DateTime<Utc>,
    path: String,
    schedule: String,
    max_backups: i32,
    max_days: i32,
    gfs: Option<String>,
    forced: bool,
    scratch: Option<String>,
}

/// A job thread: run it, record the result in its database. `_slot` holds the advisory lock until the end.
fn run_job(j: Job, _slot: Client) {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(&j)))
        .unwrap_or_else(|p| Err(format!("job panicked: {}", p.downcast_ref::<String>().cloned().or(p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())));
    let mut last = String::new();
    for attempt in 0..3 {
        match connect(&j.c, &j.db).and_then(|mut cl| record(&j, &mut cl, &res)) {
            Ok(()) => return,
            Err(e) => last = e,
        }
        if attempt < 2 && !STOPPING.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    log(&format!("{}: {} #{}: could not record the result ({last}); it is marked failed at the next start", j.db, j.kind, j.id));
}

/// Live progress: the bytes moved so far go into the job's history row (history.bytes) as they happen.
fn progress_writer(c: &Ctx, db: &str, id: i64) -> impl FnMut(u64) + Send + use<> {
    let (c, db) = (c.clone(), db.to_string());
    let mut cl: Option<Client> = None;
    move |n: u64| {
        if cl.is_none() {
            cl = connect(&c, &db).ok();
        }
        if let Some(x) = cl.as_mut() {
            let _ = x.execute("UPDATE pgbx.history SET bytes=$2 WHERE id=$1 AND state='running'", &[&id, &(n as i64)]);
        }
    }
}

fn execute(j: &Job) -> Result<Done, String> {
    let mut progress = progress_writer(&j.c, &j.db, j.id);
    match j.kind.as_str() {
        "backup" => backup(&j.c, &j.cfg, &j.db, &j.path, j.forced, &mut progress),
        "restore" => {
            let mut admin = connect(&j.c, &j.cfg.admin_db)?;
            restore(&j.c, &j.cfg, &mut admin, &j.path, &j.params, &mut progress)
        }
        "verify" => {
            let mut admin = connect(&j.c, &j.cfg.admin_db)?;
            let mut src = connect(&j.c, &j.db)?;
            let scratch = j.scratch.clone().unwrap_or_else(|| format!("{VERIFY_PREFIX}{}", j.id));
            verify(&j.c, &j.cfg, &mut admin, &mut src, &j.path, &scratch, &mut progress)
        }
        "prune" => Ok(Done::default()),
        "base_backup" => crate::pitr::run_base_backup(&j.cfg),
        other => Err(format!("unknown job kind {other}")),
    }
}

/// Write a finished job's outcome into its history row (and expire backups after a backup / prune).
fn record(j: &Job, cl: &mut Client, res: &Result<Done, String>) -> Result<(), String> {
    let (db, id, kind) = (j.db.as_str(), j.id, j.kind.as_str());
    let cancelled = this_job().is_some_and(|x| x.cancelled.load(Ordering::Relaxed));
    match res {
        // pgbx.cancel() of a running job: its upload was aborted / its partial database dropped; no alert
        Err(e) if cancelled => {
            cl.execute(
                "UPDATE pgbx.history SET state='cancelled', finished=now(), bytes=NULL,
                        error='cancelled by ' || coalesce(params->>'cancelled_by', '?') || ' while running' WHERE id=$1",
                &[&id],
            )
            .map_err(pe)?;
            log(&format!("{db}: {kind} #{id} cancelled ({e})"));
        }
        Ok(done) => {
            // record the new backup first so prune sees it as a live row, then expire, then mark the job done
            if kind == "backup" {
                cl.execute("UPDATE pgbx.history SET s3_key=$2, bytes=$3 WHERE id=$1", &[&id, &done.key, &done.bytes])
                    .map_err(pe)?;
            }
            if kind == "backup" || kind == "prune" {
                match prune(&j.c, &j.cfg, &j.path, j.max_backups, j.max_days, j.gfs.as_deref()) {
                    Ok(gone) if !gone.is_empty() => {
                        cl.execute("UPDATE pgbx.history SET state='expired' WHERE kind='backup' AND s3_key = ANY($1)", &[&gone])
                            .map_err(pe)?;
                        log(&format!("{db}: expired {} old backup(s)", gone.len()));
                    }
                    Ok(_) => {}
                    Err(e) => log(&format!("{db}: prune failed: {e}")),
                }
            }
            // a base backup's result is the CLI's JSON: a few of its fields go into params, its kept list into pitr_state
            let extra = if kind == "base_backup" {
                crate::pitr::record_base_backup(cl, id, &done.extra)?;
                "{}"
            } else {
                done.extra.as_str()
            };
            cl.execute(
                "UPDATE pgbx.history SET state='done', finished=now(), s3_key=$2, bytes=$3,
                        params = params || $4::text::jsonb WHERE id=$1",
                &[&id, &done.key, &done.bytes, &extra],
            )
            .map_err(pe)?;
            log(&format!("{db}: {kind} #{id} done ({}, {} bytes)", done.key.as_deref().unwrap_or("-"), done.bytes));
            crate::extras::job_finished(&j.cfg, &j.c.server, db, kind, id, None);
        }
        // a lock held by DDL/a migration: never queue behind it; try again later, until the deadline
        Err(e) if kind == "backup" && !j.forced && is_lock_timeout(e) => {
            let p = parse_flat_json(&j.params);
            let n = |k: &str| p.get(k).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
            let (deferrals, locks) = (n("deferrals"), n("lock_timeouts"));
            let deadline = defer_deadline(j.requested, j.trigger == "first", &j.schedule, &j.cfg);
            let wait = backoff_minutes(j.cfg.defer_backoff.as_deref(), deferrals as usize);
            let until = (Utc::now() + chrono::Duration::minutes(wait as i64)).min(deadline);
            cl.execute(
                "UPDATE pgbx.history SET state='queued', started=NULL, bytes=NULL, params = params || jsonb_build_object(
                    'deferrals', $2::int, 'lock_timeouts', $3::int, 'deferred_until', $4::timestamptz,
                    'defer_reason', 'lock_timeout', 'deadline', $5::timestamptz) WHERE id=$1",
                &[&id, &(deferrals + 1), &(locks + 1), &SystemTime::from(until), &SystemTime::from(deadline)],
            )
            .map_err(pe)?;
            log(&format!(
                "{db}: backup #{id} could not get its table locks within pgbx.dump_lock_timeout (DDL holds one); retry at {}, runs anyway from {}",
                until.format("%H:%M:%S"), deadline.format("%Y-%m-%d %H:%M:%S UTC")
            ));
        }
        Err(e) => {
            cl.execute("UPDATE pgbx.history SET state='failed', finished=now(), error=$2, bytes=NULL WHERE id=$1", &[&id, e])
                .map_err(pe)?;
            log(&format!("{db}: {kind} #{id} failed: {e}"));
            alert(j.cfg.alert_command.as_deref(), &j.c.server, db, kind, id, e);
            crate::extras::job_finished(&j.cfg, &j.c.server, db, kind, id, Some(e));
        }
    }
    Ok(())
}

/// Audit retention (pgbx.audit_days): at most once an hour per database.
fn prune_audit(cl: &mut Client, db: &str) {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    {
        let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.get_or_insert_with(HashMap::new);
        if m.get(db).is_some_and(|t| t.elapsed() < Duration::from_secs(3600)) {
            return;
        }
        m.insert(db.to_string(), Instant::now());
    }
    match crate::audit::prune(cl, crate::AUDIT_DAYS.get()) {
        Ok(0) => {}
        Ok(n) => log(&format!("{db}: pruned {n} history row(s) older than pgbx.audit_days")),
        Err(e) => log(&format!("{db}: audit prune failed: {e}")),
    }
}

/// Copy this database's status() into the admin database's server_overview (plus what doctor() needs to judge the
/// schedule: its interval and how long the last 3 backups took).
fn publish_overview(admin: &mut Client, cl: &mut Client, db: &str, cron: &str) -> Result<(), String> {
    let r = cl
        .query_one(
            "SELECT state, schedule, last_backup_at, last_backup_size, next_backup_at, backups_kept, last_verify_result, last_error
             FROM pgbx.status()",
            &[],
        )
        .map_err(pe)?;
    let (state, schedule, last_at, size, next_at, kept, verify, err): (
        Option<String>, Option<String>, Option<std::time::SystemTime>, Option<String>, Option<std::time::SystemTime>,
        Option<i64>, Option<String>, Option<String>,
    ) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5), r.get(6), r.get(7));
    let interval = schedule_interval(cron, Utc::now()).map(|s| s as f64);
    let dump_secs: Vec<f64> = cl
        .query(
            "SELECT extract(epoch FROM finished - started)::float8 FROM pgbx.history
              WHERE kind='backup' AND state IN ('done','expired') AND started IS NOT NULL ORDER BY id DESC LIMIT 3",
            &[],
        )
        .map_err(pe)?
        .iter()
        .map(|r| r.get(0))
        .collect();
    // how far off the time estimates were for recent jobs that ran long enough to judge (>= 30 s)
    let eta_error: Option<f64> = cl
        .query_one(
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY abs(x.secs - x.eta) / x.secs) FROM (
                 SELECT extract(epoch FROM finished - started)::float8 AS secs, (params->>'eta_sec')::float8 AS eta FROM pgbx.history
                  WHERE state IN ('done', 'expired') AND params ? 'eta_sec' AND finished - started >= interval '30 seconds'
                  ORDER BY id DESC LIMIT 5) x",
            &[],
        )
        .map_err(pe)?
        .get(0);
    let g = cl
        .query_one(
            "SELECT coalesce((SELECT load_gate FROM pgbx.config), current_setting('pgbx.load_gate', true)),
                    count(*) FILTER (WHERE params ? 'would_defer')::int, count(*) FILTER (WHERE params ? 'busy_deferrals')::int,
                    count(*) FILTER (WHERE kind = 'backup' AND params->>'forced' = 'true')::int
               FROM pgbx.history WHERE requested_at > now() - interval '7 days' AND kind IN ('backup', 'verify')",
            &[],
        )
        .map_err(pe)?;
    let (gate, would, deferred, forced): (Option<String>, i32, i32, i32) = (g.get(0), g.get(1), g.get(2), g.get(3));
    let w = cl.query_one("SELECT cron, score, current_score, confidence FROM pgbx.suggest_window()", &[]).map_err(pe)?;
    let (wcron, wscore, wcur, wconf): (Option<String>, Option<f64>, Option<f64>, Option<String>) = (w.get(0), w.get(1), w.get(2), w.get(3));
    // Prometheus metrics (pgbx metrics, GET /metrics on pgbx ui)
    let m = cl
        .query_one(
            "SELECT (SELECT bytes FROM pgbx.history WHERE kind='backup' AND state='done' ORDER BY id DESC LIMIT 1),
                    (SELECT count(*) FROM pgbx.history WHERE state='failed' AND kind IN ('backup', 'restore', 'verify')),
                    (SELECT count(*) FROM pgbx.history WHERE state='queued' AND kind IN ('backup', 'restore', 'verify', 'prune')),
                    (SELECT state = 'done' FROM pgbx.history WHERE kind='verify' AND state IN ('done','failed') ORDER BY id DESC LIMIT 1),
                    (SELECT coalesce((params->>'encrypted')::bool, false) FROM pgbx.history
                      WHERE kind='backup' AND state='done' ORDER BY id DESC LIMIT 1)",
            &[],
        )
        .map_err(pe)?;
    let (mbytes, mfail, mqueued, mverify, menc): (Option<i64>, i64, i64, Option<bool>, Option<bool>) =
        (m.get(0), m.get(1), m.get(2), m.get(3), m.get(4));
    admin
        .execute(
            "INSERT INTO pgbx.server_overview AS o
                (database, state, schedule, last_backup_at, last_backup_size, next_backup_at, backups_kept, last_verify, last_error,
                 seen_at, interval_secs, dump_secs, eta_error, window_cron, window_score, current_score, window_confidence,
                 load_gate, would_defer_7d, deferred_7d, forced_7d,
                 last_backup_bytes, failures_total, queued_jobs, last_verify_ok, last_backup_encrypted)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, now(), $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25)
             ON CONFLICT (database) DO UPDATE SET state=$2, schedule=$3, last_backup_at=$4, last_backup_size=$5,
                next_backup_at=$6, backups_kept=$7, last_verify=$8, last_error=$9, seen_at=now(), interval_secs=$10, dump_secs=$11,
                eta_error=$12, window_cron=$13, window_score=$14, current_score=$15, window_confidence=$16,
                load_gate=$17, would_defer_7d=$18, deferred_7d=$19, forced_7d=$20,
                last_backup_bytes=$21, failures_total=$22, queued_jobs=$23, last_verify_ok=$24, last_backup_encrypted=$25",
            &[&db, &state, &schedule, &last_at, &size, &next_at, &kept, &verify, &err, &interval, &dump_secs, &eta_error,
              &wcron, &wscore, &wcur, &wconf, &gate, &would, &deferred, &forced, &mbytes, &mfail, &mqueued, &mverify, &menc],
        )
        .map_err(pe)?;
    Ok(())
}

/// What a finished job reports back into its history row.
pub(crate) struct Done {
    pub(crate) key: Option<String>,
    pub(crate) bytes: i64,
    pub(crate) extra: String, // JSON object merged into params; always a valid object ("{}" when there is nothing to add)
}

impl Default for Done {
    fn default() -> Self {
        Done { key: None, bytes: 0, extra: "{}".into() }
    }
}

/// Run pgbx.alert_command for a failed job: JSON on stdin, PGBX_* env vars, 30 s limit.
pub(crate) fn alert(cmd: Option<&str>, server: &str, db: &str, kind: &str, id: i64, error: &str) {
    let Some(cmd) = cmd else { return };
    let json = format!(
        "{{\"server\":{},\"database\":{},\"kind\":{},\"job_id\":{id},\"error\":{},\"at\":\"{}\"}}\n",
        jstr(server), jstr(db), jstr(kind), jstr(error), Utc::now().to_rfc3339()
    );
    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("PGBX_SERVER", server)
        .env("PGBX_DATABASE", db)
        .env("PGBX_KIND", kind)
        .env("PGBX_JOB_ID", id.to_string())
        .env("PGBX_ERROR", error)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return log("alert_command could not start") };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(json.as_bytes());
    }
    for _ in 0..300 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    log("alert_command timed out after 30s");
}

fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn prefix(c: &Ctx, path: &str) -> String {
    format!("{}/{}/", c.server, path)
}

fn user_tables(cl: &mut Client) -> Result<i64, String> {
    Ok(cl
        .query_one(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
             WHERE c.relkind IN ('r','p') AND n.nspname NOT IN ('pg_catalog','information_schema','pgbx')
               AND n.nspname NOT LIKE 'pg_toast%'
               AND NOT c.relispartition
               AND (NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e')
                    OR (n.nspname = 'pgmq' AND (c.relname LIKE 'q\\_%' OR c.relname LIKE 'a\\_%')))",
            &[],
        )
        .map_err(pe)?
        .get(0))
}

/// pg_dump (custom format) streamed straight into a multipart S3 upload — no temp file, no full copy in memory.
/// Key: s3://bucket/<server>/<path>/<UTC timestamp>.dump. Records how many user tables it saw (for verify).
fn backup(c: &Ctx, cfg: &JobCfg, db: &str, path: &str, forced: bool, progress: &mut dyn FnMut(u64)) -> Result<Done, String> {
    let mut src = connect(c, db)?;
    let tables = user_tables(&mut src)?;
    // data scope: definitions of every table are dumped; rows are skipped for these (resolved now, so new tables count)
    let rowless: Vec<String> = src.query("SELECT table_name FROM pgbx.rowless_tables()", &[]).map_err(pe)?
        .iter().map(|r| r.get(0)).collect();
    let roles = crate::extras::referenced_roles(&mut src);
    drop(src);
    let enc = crate::extras::encryption_key(cfg)?; // a bad key file fails the backup before anything is uploaded
    let b = cfg.bucket()?;
    let key = format!("{}{}.dump", prefix(c, path), Utc::now().format("%Y-%m-%dT%H-%M-%SZ"));
    let (pg_dump, major) = client_tool(c, "pg_dump");
    let normal = compression(cfg.compression.as_deref(), major);
    let compress = if forced { compression_busy(cfg.compression_busy.as_deref(), &normal, major) } else { normal };
    // pg_dump SETs lock_timeout=0 itself, so its own --lock-wait-timeout is the knob; C messages so a timeout is recognised
    let lock_ms = if forced { cfg.lock_ms_forced } else { cfg.lock_ms };
    let mut child = job_command(&pg_dump, "pgbx_dump", cfg)
        .env("PGOPTIONS", "-c lc_messages=C")
        .args(["-Fc", "--compress", &compress, "-h", &c.socket, "-p", &c.port.to_string(), "-U", "postgres", "-d", db])
        .args((lock_ms > 0).then(|| format!("--lock-wait-timeout={lock_ms}ms")))
        .args(rowless.iter().map(|t| format!("--exclude-table-data={t}")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn pg_dump: {e}"))?;
    set_child(child.id());
    let mut out = child.stdout.take().unwrap();
    let up = crate::extras::upload(&b, &key, &mut out, enc.as_ref(), cfg.upload_kbps, progress);
    drop(out); // if the upload gave up, this stops pg_dump (broken pipe)
    if up.is_err() {
        let _ = child.kill();
    }
    let res = child.wait_with_output().map_err(pe);
    set_child(0);
    let res = res?;
    if !res.status.success() || (up.is_ok() && shutting_down()) {
        let _ = s3auth::fresh(&b).map(|b| b.delete_object(&key)); // never leave a truncated (or cancelled) dump that looks like a backup
        let why = up.err().unwrap_or_else(|| {
            if shutting_down() { format!("stopped, {}", stop_reason()) } else { String::from_utf8_lossy(&res.stderr).trim().to_string() }
        });
        return Err(format!("backup failed: {why}"));
    }
    let bytes = up?;
    let skipped = rowless.iter().map(|t| jstr(t)).collect::<Vec<_>>().join(",");
    // the roles this database needs, next to the dump (<ts>.globals.sql.zst); never fails the backup
    let globals = crate::extras::backup_globals(c, cfg, &b, &key, &roles, enc.as_ref());
    // pgmq queues (extension members, never in a pg_dump) next to the dump; losing them fails the backup
    let ext = match crate::extdata::backup_ext(c, cfg, &b, &key, db, enc.as_ref()) {
        Ok(x) => x,
        Err(e) => {
            let _ = s3auth::fresh(&b).map(|b| { let _ = b.delete_object(&key); b.delete_object(crate::globals::globals_key(&key)) });
            return Err(format!("backup failed: {e}"));
        }
    };
    Ok(Done {
        key: Some(key),
        bytes: bytes as i64,
        extra: format!(
            "{{\"tables\":{tables},\"compression\":\"{compress}\",\"rows_skipped\":[{skipped}],\"encrypted\":{}{globals}{ext}}}",
            enc.is_some()
        ),
    })
}

/// Major version from `pg_dump --version` output ("pg_dump (PostgreSQL) 16.4 (Debian ...)" -> 16).
pub(crate) fn parse_major(out: &str) -> Option<u32> {
    let v = out.split(')').nth(1)?.split_whitespace().next()?;
    v.split(|ch: char| !ch.is_ascii_digit()).next()?.parse().ok()
}

/// pgbx.dump_compression: 'auto' (or unset) = zstd:3 when pg_dump is 16+, else gzip level 6
/// (pg_dump before 16 takes only a 0-9 level).
pub(crate) fn compression(setting: Option<&str>, pg_dump_major: Option<u32>) -> String {
    match setting.map(str::trim) {
        Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("auto") => s.to_string(),
        _ if pg_dump_major.unwrap_or(0) >= 16 => "zstd:3".into(),
        _ => "6".into(),
    }
}

/// pgbx.dump_compression_busy: 'auto' (or unset) = zstd:1 when pg_dump is 16+, else gzip level 1. Single-threaded
/// either way (no zstd workers=): one core at most. 'auto' never costs more than `normal` (none / 0 / lz4 stay).
pub(crate) fn compression_busy(setting: Option<&str>, normal: &str, pg_dump_major: Option<u32>) -> String {
    let n = normal.trim().to_ascii_lowercase();
    match setting.map(str::trim) {
        Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("auto") => s.to_string(),
        _ if matches!(n.as_str(), "none" | "0" | "gzip:0") || n.starts_with("lz4") => normal.to_string(),
        _ if pg_dump_major.unwrap_or(0) >= 16 => "zstd:1".into(),
        _ => "1".into(),
    }
}

/// Did pg_dump give up waiting for a table lock (--lock-wait-timeout)? It runs the LOCK TABLEs under a
/// statement_timeout; with NOWAIT the server says "could not obtain lock".
pub(crate) fn is_lock_timeout(err: &str) -> bool {
    err.contains("LOCK TABLE")
        && (err.contains("statement timeout") || err.contains("lock timeout") || err.contains("could not obtain lock"))
}

/// pgbx.defer_backoff ("1,2,4,8,15", minutes, each 1-60): wait before retry number `n` (0-based); the last value
/// repeats. A list that does not parse falls back to the default.
pub(crate) fn backoff_minutes(setting: Option<&str>, n: usize) -> u32 {
    const DEFAULT: [u32; 5] = [1, 2, 4, 8, 15];
    let parsed: Option<Vec<u32>> = setting.map(|s| {
        s.split(',').map(|x| x.trim().parse::<u32>().ok().filter(|m| (1..=60).contains(m))).collect::<Option<Vec<_>>>()
    })
    .flatten()
    .filter(|v| !v.is_empty());
    let list = parsed.as_deref().unwrap_or(&DEFAULT);
    list[n.min(list.len() - 1)]
}

/// When a deferred job runs anyway: queued + pgbx.max_defer (first backup: pgbx.max_defer_first), never later than
/// one schedule interval, so a deferred backup and the next scheduled one never pile up.
pub(crate) fn defer_deadline(requested: DateTime<Utc>, first: bool, cron: &str, cfg: &JobCfg) -> DateTime<Utc> {
    let max = if first { cfg.max_defer_first } else { cfg.max_defer };
    requested + chrono::Duration::seconds(defer_secs(max as i64, schedule_interval(cron, requested)))
}

fn defer_secs(max_defer: i64, interval: Option<i64>) -> i64 {
    interval.map_or(max_defer, |i| max_defer.min(i)).max(0)
}

/// Seconds between the two schedule slots after `at` (None when the schedule cannot be read).
pub(crate) fn schedule_interval(cron: &str, at: DateTime<Utc>) -> Option<i64> {
    let a = schedule::next_after(cron, at).ok()?;
    let b = schedule::next_after(cron, a).ok()?;
    Some((b - a).num_seconds())
}

/// pgbx.job_ionice -> the ioprio_set value: None = leave alone ('none'), else class << 13 | level
/// ('idle' = class 3; 'best-effort-N' = class 2, level N 0-7).
pub(crate) fn parse_ionice(s: &str) -> Result<Option<i32>, String> {
    const BE: i32 = 2 << 13;
    const IDLE: i32 = 3 << 13;
    match s.trim().to_ascii_lowercase().as_str() {
        "none" => Ok(None),
        "idle" => Ok(Some(IDLE)),
        v => v
            .strip_prefix("best-effort-")
            .and_then(|n| n.parse::<i32>().ok())
            .filter(|n| (0..=7).contains(n))
            .map(|n| Some(BE | n))
            .ok_or(format!("pgbx.job_ionice '{s}' not understood (none, idle, best-effort-0..7); using best-effort-7")),
    }
}

/// pg_dump / pg_restore as a polite child: lower CPU priority (pgbx.job_nice) and, on Linux, IO priority
/// (pgbx.job_ionice), set between fork and exec so they hold from its first read. PGAPPNAME tags its connection
/// so load checks skip it and doctor() can see how long it runs. Never raises priority.
fn job_command(prog: &std::path::Path, app: &str, cfg: &JobCfg) -> Command {
    let mut cmd = Command::new(prog);
    cmd.env("PGAPPNAME", app);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let nice = cfg.nice.clamp(0, 19);
        let ioprio = parse_ionice(cfg.ionice.as_deref().unwrap_or("best-effort-7")).unwrap_or_else(|e| {
            log(&e);
            Some((2 << 13) | 7)
        });
        let current = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        // SAFETY: only async-signal-safe syscalls between fork and exec; failures are ignored (best effort)
        unsafe {
            cmd.pre_exec(move || {
                if nice > current {
                    libc::setpriority(libc::PRIO_PROCESS, 0, nice);
                }
                #[cfg(target_os = "linux")]
                if let Some(p) = ioprio {
                    libc::syscall(libc::SYS_ioprio_set, 1 /* IOPRIO_WHO_PROCESS */, 0, p);
                }
                #[cfg(not(target_os = "linux"))]
                let _ = ioprio;
                Ok(())
            });
        }
    }
    cmd
}

/// The newest installed copy of a client tool (pg_dump / pg_restore): newer clients dump and restore older
/// servers, so prefer e.g. /usr/lib/postgresql/18/bin over the server's own bindir. Returns (path, major).
pub(crate) fn client_tool(c: &Ctx, name: &str) -> (PathBuf, Option<u32>) {
    let mut cands = vec![c.bindir.join(name)];
    for root in ["/usr/lib/postgresql", "/usr"] {
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let f = e.file_name().to_string_lossy().into_owned();
                if root == "/usr/lib/postgresql" || f.starts_with("pgsql-") {
                    cands.push(e.path().join("bin").join(name));
                }
            }
        }
    }
    let mut best: (PathBuf, Option<u32>) = (c.bindir.join(name), None);
    for p in cands.into_iter().filter(|p| p.is_file()) {
        let major = Command::new(&p).arg("--version").output().ok().and_then(|o| parse_major(&String::from_utf8_lossy(&o.stdout)));
        if major.is_some() && major > best.1 {
            best = (p, major);
        }
    }
    best
}

fn list_dumps(c: &Ctx, b: &Bucket, path: &str) -> Result<Vec<String>, String> {
    let mut keys: Vec<String> = s3auth::fresh(b)?
        .list(prefix(c, path), None)
        .map_err(|e| format!("list: {e}"))?
        .into_iter()
        .flat_map(|page| page.contents.into_iter().map(|o| o.key))
        .filter(|k| k.ends_with(".dump"))
        .collect();
    keys.sort(); // timestamp names sort chronologically
    Ok(keys)
}

fn key_time(key: &str) -> Option<DateTime<Utc>> {
    let stem = key.rsplit('/').next()?.strip_suffix(".dump")?;
    chrono::NaiveDateTime::parse_from_str(stem, "%Y-%m-%dT%H-%M-%SZ").ok().map(|n| n.and_utc())
}

/// Delete what the retention rules do not keep (max_backups, max_days and the optional GFS spec; see retention.rs),
/// never the newest backup, together with each dump's roles file. Returns the deleted dump keys.
fn prune(c: &Ctx, cfg: &JobCfg, path: &str, max_backups: i32, max_days: i32, gfs: Option<&str>) -> Result<Vec<String>, String> {
    let b = cfg.bucket()?;
    let keys = list_dumps(c, &b, path)?; // oldest first
    let g = gfs.map(crate::retention::Gfs::parse).transpose()?.flatten();
    let times: Vec<_> = keys.iter().map(|k| key_time(k)).collect();
    let mut gone = Vec::new();
    for i in crate::retention::to_delete(&times, max_backups, max_days, g.as_ref(), Utc::now()) {
        let k = &keys[i];
        let b = s3auth::fresh(&b)?;
        b.delete_object(k).map_err(|e| format!("delete {k}: {e}"))?;
        let _ = b.delete_object(crate::globals::globals_key(k)); // absent for dumps taken before 0.6
        let _ = b.delete_object(crate::extdata::ext_key(k)); // only for databases with pgmq queues (0.6.2+)
        gone.push(k.clone());
    }
    Ok(gone)
}

fn pick_backup(c: &Ctx, b: &Bucket, path: &str, at: DateTime<Utc>) -> Result<String, String> {
    list_dumps(c, b, path)?
        .into_iter()
        .filter(|k| key_time(k).is_some_and(|t| t <= at))
        .next_back()
        .ok_or(format!("no backup of '{path}' at or before {at}"))
}

/// CREATE DATABASE <into> TEMPLATE template0, then stream the S3 object straight into pg_restore's stdin.
/// `keep_owner`: restore object owners (with_roles: the roles were created first); otherwise `--no-owner`.
#[allow(clippy::too_many_arguments)]
fn restore_key_into(
    c: &Ctx, cfg: &JobCfg, admin: &mut Client, b: &Bucket, key: &str, into: &str, app: &str, keep_owner: bool,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(i64, String), String> {
    // pg_cron can only exist in cron.database_name: left out of any other target, paused in that one.
    // GRANTs on pgmq queues are left out: the queues only exist once restore_ext (below) has run.
    let keep_cron = crate::extdata::cron_database(admin) == into;
    let list = crate::extdata::toc(c, cfg, b, key)?;
    let filtered = crate::extdata::filter_toc(&list, keep_cron);
    let list_file = match &filtered {
        Some((l, _, _)) => {
            let f = std::env::temp_dir().join(format!("pgbx-{into}-{}.list", std::process::id()));
            std::fs::write(&f, l).map_err(|e| format!("write {}: {e}", f.display()))?;
            Some(f)
        }
        None => None,
    };
    // template0: the dump brings its own CREATE EXTENSION pgbx, so start from a database without it
    admin
        .batch_execute(&format!("CREATE DATABASE \"{into}\" TEMPLATE template0"))
        .map_err(|e| format!("create {into}: {}", pe(e)))?;
    // the target is a NEW database: losing the tail of it in a crash just means restoring again
    let sync = if cfg.restore_sync { "on" } else { "off" };
    let mut child = job_command(&client_tool(c, "pg_restore").0, app, cfg)
        .env("PGOPTIONS", format!("-c synchronous_commit={sync}"))
        .args(["-h", &c.socket, "-p", &c.port.to_string(), "-U", "postgres", "-d", into])
        .args((!keep_owner).then_some("--no-owner"))
        .args(list_file.iter().flat_map(|f| ["-L".to_string(), f.display().to_string()]))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn pg_restore: {e}"))?;
    set_child(child.id());
    let mut stdin = child.stdin.take().unwrap();
    let dl = crate::extras::download(cfg, b, key, &mut stdin, progress); // decrypts an encrypted dump in the stream
    drop(stdin); // EOF for pg_restore
    let bad_cipher = dl.as_ref().is_err_and(|e| e.starts_with("decrypt") || e.starts_with("pgbx.encryption_key_file"));
    if (dl.is_err() && shutting_down()) || bad_cipher {
        let _ = child.kill();
    }
    let out = child.wait_with_output().map_err(pe);
    set_child(0);
    if let Some(f) = &list_file {
        let _ = std::fs::remove_file(f);
    }
    let out = out?;
    if shutting_down() {
        if stop_reason() == "cancelled" {
            // cancelled: the half-restored NEW database (created above) is of no use to anyone
            let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{into}\" WITH (FORCE)"));
        }
        return Err(format!("restore stopped, {}", stop_reason()));
    }
    if let Some(e) = dl.as_ref().err().filter(|_| bad_cipher) {
        // the real cause, not pg_restore's complaint about a cut-off input; the half-restored NEW database goes
        let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{into}\" WITH (FORCE)"));
        return Err(e.clone());
    }
    if !out.status.success() {
        return Err(format!("pg_restore failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let n = dl?;
    let mut notes = String::new();
    if let Some(q) = crate::extdata::restore_ext(c, cfg, b, key, into)? {
        notes.push_str(&format!(",\"pgmq_restored_from\":{}", jstr(&q)));
    }
    if let Some((_, cron, _)) = filtered.filter(|f| f.1 > 0) {
        notes.push_str(&format!(",\"cron_left_out\":\"{cron} pg_cron entries: pg_cron lives only in the database named by cron.database_name\""));
    }
    if keep_cron {
        let paused = crate::extdata::pause_cron(c, into)?;
        if paused > 0 {
            notes.push_str(&format!(",\"cron_jobs_paused\":{paused}"));
        }
    }
    Ok((n as i64, notes))
}

/// Newest dump at or before `at` -> a NEW database. The live one is never touched.
fn restore(c: &Ctx, cfg: &JobCfg, admin: &mut Client, path: &str, params: &str, progress: &mut (dyn FnMut(u64) + Send)) -> Result<Done, String> {
    let p = parse_flat_json(params);
    let into = p.get("into_db").ok_or("restore needs into_db")?.clone();
    if !into.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return Err(format!("into_db '{into}' must be [A-Za-z0-9_]"));
    }
    let at = p
        .get("at")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)))
        .unwrap_or_else(Utc::now);
    let b = cfg.bucket()?;
    let key = pick_backup(c, &b, path, at)?;
    // with_roles: create the roles the dump needs first (existing roles are never changed), then keep owners
    let with_roles = p.get("with_roles").is_some_and(|v| v == "true");
    let roles = if with_roles {
        let scope = p.get("roles").map(String::as_str).unwrap_or("referenced");
        Some(crate::extras::restore_globals(cfg, admin, &b, &key, scope)?)
    } else {
        None
    };
    let (bytes, notes) = restore_key_into(c, cfg, admin, &b, &key, &into, "pgbx_restore", with_roles, progress)?;
    // the copy must not back up into the original's folder: give it its own path (= its own name)
    let mut copy = connect(c, &into)?;
    copy.batch_execute("UPDATE pgbx.config SET path = NULL").map_err(pe)?;
    let extra = format!("{{{}}}", (roles.map(|r| format!(",\"roles\":{r}")).unwrap_or_default() + &notes).trim_start_matches(','));
    Ok(Done { key: Some(key), bytes, extra })
}

/// Restore test: newest backup -> scratch database -> same number of user tables as at backup time -> drop.
fn verify(
    c: &Ctx, cfg: &JobCfg, admin: &mut Client, source: &mut Client, path: &str, scratch: &str, progress: &mut (dyn FnMut(u64) + Send),
) -> Result<Done, String> {
    let b = cfg.bucket()?;
    let key = pick_backup(c, &b, path, Utc::now())?;
    let result = (|| {
        let (bytes, _) = restore_key_into(c, cfg, admin, &b, &key, scratch, "pgbx_verify", false, progress)?;
        let mut sc = connect(c, scratch)?;
        let got = user_tables(&mut sc)?;
        let ok = sc.query_one("SELECT 1", &[]).is_ok();
        drop(sc);
        // compare with what the backup job counted when it took this dump (recorded in the source database)
        let want: Option<i64> = source
            .query_opt("SELECT (params->>'tables')::bigint FROM pgbx.history WHERE s3_key=$1 AND kind='backup'", &[&key])
            .map_err(pe)?
            .and_then(|r| r.get(0));
        match want {
            Some(w) if w != got => Err(format!("restored {got} tables, backup had {w}")),
            _ if !ok => Err("restored database does not answer queries".into()),
            _ => Ok(Done {
                key: Some(key.clone()),
                bytes,
                extra: format!("{{\"checked\":\"restored {got} tables from {}\"}}", key.rsplit('/').next().unwrap_or("")),
            }),
        }
    })();
    let _ = admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{scratch}\" WITH (FORCE)"));
    result
}

/// Tiny parser for the flat {"k": "v"} objects jsonb renders (values are strings, numbers, null or timestamps).
pub(crate) fn parse_flat_json(s: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for part in s.trim().trim_start_matches('{').trim_end_matches('}').split(',') {
        if let Some((k, v)) = part.split_once(':') {
            let (k, v) = (k.trim().trim_matches('"'), v.trim());
            if v != "null" && !k.is_empty() {
                m.insert(k.to_string(), v.trim_matches('"').to_string());
            }
        }
    }
    m
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn versions() {
        assert!(version_older("0.4.0", "0.5.0"));
        assert!(version_older("0.9.0", "0.10.0"));
        assert!(!version_older("0.5.0", "0.5.0"));
        assert!(!version_older("0.6.0", "0.5.0"));
        assert!(!version_older("dev", "0.5.0"));
    }

    #[test]
    fn compression_by_pg_dump_version() {
        assert_eq!(parse_major("pg_dump (PostgreSQL) 16.4 (Debian 16.4-1.pgdg120+1)\n"), Some(16));
        assert_eq!(parse_major("pg_restore (PostgreSQL) 13.15\n"), Some(13));
        assert_eq!(parse_major("pg_dump (PostgreSQL) 18beta1"), Some(18));
        assert_eq!(compression(Some("auto"), Some(16)), "zstd:3");
        assert_eq!(compression(None, Some(13)), "6");
        assert_eq!(compression(Some("auto"), None), "6");
        assert_eq!(compression(Some("lz4"), Some(13)), "lz4");
    }

    #[test]
    fn prune_job_extra_is_valid_json() {
        assert_eq!(Done::default().extra, "{}");
    }

    #[test]
    fn ionice_values() {
        assert_eq!(parse_ionice("best-effort-7"), Ok(Some((2 << 13) | 7)));
        assert_eq!(parse_ionice(" Best-Effort-0 "), Ok(Some(2 << 13)));
        assert_eq!(parse_ionice("idle"), Ok(Some(3 << 13)));
        assert_eq!(parse_ionice("none"), Ok(None));
        for bad in ["best-effort-8", "best-effort--1", "best-effort", "realtime-0", "", "7"] {
            assert!(parse_ionice(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn busy_compression_is_cheap() {
        assert_eq!(compression_busy(Some("auto"), "zstd:3", Some(16)), "zstd:1");
        assert_eq!(compression_busy(None, "6", Some(15)), "1");
        assert_eq!(compression_busy(Some("lz4"), "zstd:3", Some(16)), "lz4");
        // auto never makes a forced run more expensive than the normal setting
        for cheap in ["none", "0", "lz4", "lz4:1"] {
            assert_eq!(compression_busy(Some("auto"), cheap, Some(16)), cheap);
        }
    }

    #[test]
    fn lock_timeout_detected() {
        // what pg_dump --lock-wait-timeout prints when a table is held ACCESS EXCLUSIVE (lc_messages=C)
        let e = "backup failed: pg_dump: error: query failed: ERROR:  canceling statement due to statement timeout\n\
                 pg_dump: detail: Query was: LOCK TABLE public.t IN ACCESS SHARE MODE";
        assert!(is_lock_timeout(e));
        assert!(is_lock_timeout("ERROR:  could not obtain lock on relation \"t\"\nQuery was: LOCK TABLE public.t IN ACCESS SHARE MODE NOWAIT"));
        assert!(!is_lock_timeout("backup failed: pg_dump: error: connection to server failed"));
        assert!(!is_lock_timeout("ERROR:  canceling statement due to statement timeout\nQuery was: SELECT 1"));
    }

    #[test]
    fn backoff_schedule() {
        let b = |s: Option<&str>| (0..7).map(|n| backoff_minutes(s, n)).collect::<Vec<_>>();
        assert_eq!(b(None), [1, 2, 4, 8, 15, 15, 15]);
        assert_eq!(b(Some("1,2,4,8,15")), [1, 2, 4, 8, 15, 15, 15]);
        assert_eq!(b(Some(" 5 , 10 ")), [5, 10, 10, 10, 10, 10, 10]);
        assert_eq!(b(Some("0,5")), [1, 2, 4, 8, 15, 15, 15]); // each 1-60, else the default
        assert_eq!(b(Some("61")), [1, 2, 4, 8, 15, 15, 15]);
        assert_eq!(b(Some("")), [1, 2, 4, 8, 15, 15, 15]);
    }

    #[test]
    fn deadline_capped_by_schedule_interval() {
        let at = DateTime::parse_from_rfc3339("2026-10-02T02:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(schedule_interval("0 2 * * *", at), Some(86_400));
        assert_eq!(schedule_interval("0 * * * *", at), Some(3_600));
        assert_eq!(schedule_interval("not cron", at), None);
        assert_eq!(defer_secs(4 * 3600, Some(86_400)), 4 * 3600); // daily: 4h
        assert_eq!(defer_secs(4 * 3600, Some(3_600)), 3_600); // hourly: never past the next slot
        assert_eq!(defer_secs(900, None), 900);
        assert_eq!(defer_secs(0, Some(3_600)), 0);
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn overrun_skip_and_guard() {
        let (skip, catch) = (Overrun::Skip, Overrun::CatchUp);
        let hourly = "0 * * * *";
        // never backed up: due at once
        assert_eq!(due_backup(hourly, t("2026-10-02T02:30:00Z"), None, None, None, skip, 1.5), Ok(Due::Yes { skipped: 0, guard: false }));
        // normal: the 01:00 dump took 10 min; at 01:30 the 02:00 slot has not come yet, at 02:00 it is due
        let run = Some((t("2026-10-02T01:00:00Z"), t("2026-10-02T01:10:00Z")));
        let auto = Some(t("2026-10-02T01:00:00Z"));
        assert_eq!(due_backup(hourly, t("2026-10-02T01:30:00Z"), auto, run, run.map(|r| r.1), skip, 1.5), Ok(Due::No));
        assert_eq!(due_backup(hourly, t("2026-10-02T02:00:05Z"), auto, run, run.map(|r| r.1), skip, 1.5), Ok(Due::Yes { skipped: 0, guard: false }));
        // hourly + 70-min dump (01:00 -> 02:10): the 02:00 slot is skipped, next run 03:00, not back to back
        let run = Some((t("2026-10-02T01:00:00Z"), t("2026-10-02T02:10:00Z")));
        let good = run.map(|r| r.1);
        assert_eq!(due_backup(hourly, t("2026-10-02T02:10:05Z"), auto, run, good, skip, 1.5), Ok(Due::No));
        assert_eq!(due_backup(hourly, t("2026-10-02T03:00:01Z"), auto, run, good, skip, 1.5), Ok(Due::Yes { skipped: 1, guard: false }));
        // catch_up: right away (today's behaviour)
        assert_eq!(due_backup(hourly, t("2026-10-02T02:10:05Z"), auto, run, good, catch, 1.5), Ok(Due::Yes { skipped: 0, guard: false }));
        // the overrunning dump FAILED and the last good backup is old: waiting for 03:00 would leave > 1.5 h -> now
        let old_good = Some(t("2026-10-01T23:10:00Z"));
        assert_eq!(due_backup(hourly, t("2026-10-02T02:10:05Z"), auto, run, old_good, skip, 1.5), Ok(Due::Yes { skipped: 1, guard: true }));
        assert_eq!(due_backup(hourly, t("2026-10-02T02:10:05Z"), auto, run, None, skip, 1.5), Ok(Due::Yes { skipped: 1, guard: true }));
        // daily 02:00; a manual dump ran 01:30-02:20 over the slot: tomorrow 02:00 is 23.7 h away < 36 h -> wait
        let daily = "0 2 * * *";
        let (auto, run) = (Some(t("2026-10-01T02:00:00Z")), Some((t("2026-10-02T01:30:00Z"), t("2026-10-02T02:20:00Z"))));
        assert_eq!(due_backup(daily, t("2026-10-02T02:20:05Z"), auto, run, run.map(|r| r.1), skip, 1.5), Ok(Due::No));
        // ... but if that dump failed and yesterday's 02:05 is the last good one: 48 h > 36 h -> run now
        assert_eq!(due_backup(daily, t("2026-10-02T02:20:05Z"), auto, run, Some(t("2026-10-01T02:05:00Z")), skip, 1.5),
                   Ok(Due::Yes { skipped: 1, guard: true }));
        // several slots inside one dump are counted
        let run = Some((t("2026-10-02T01:00:00Z"), t("2026-10-02T04:30:00Z")));
        assert_eq!(due_backup(hourly, t("2026-10-02T05:00:01Z"), Some(t("2026-10-02T01:00:00Z")), run, run.map(|r| r.1), skip, 1.5),
                   Ok(Due::Yes { skipped: 3, guard: false }));
    }

    #[test]
    fn queue_simulation() {
        // one slot busy for 100 s, restore lane on: a restore starts now in the lane, a backup after the slot frees
        let mut q = QueueSim::new(1, true);
        q.busy(1, 100.0);
        assert_eq!(q.place(true, 30.0), 0.0);
        assert_eq!(q.place(false, 50.0), 100.0);
        assert_eq!(q.place(true, 10.0), 30.0); // the lane frees first
        assert_eq!(q.wait_for_new(), 150.0);
        let mut q = QueueSim::new(2, false);
        q.busy(1, 20.0);
        assert_eq!(q.place(true, 5.0), 0.0); // slot 2 is free
        assert_eq!(q.wait_for_new(), 5.0);
    }

    #[test]
    fn eta_helpers() {
        assert_eq!(dur(45.0), "45 s");
        assert_eq!(dur(1080.0), "18 min");
        assert_eq!(dur(9000.0), "2.5 h");
        assert_eq!(load_factor(0.5, 4), 1.0);
        assert_eq!(load_factor(8.0, 4), 3.0);
        assert!((load_factor(1.35 * 4.0, 4) - 2.0).abs() < 1e-9);
        assert_eq!(probe_level("zstd:3"), Some((1, 2.5)));
        assert_eq!(probe_level("6"), Some((6, 1.0)));
        assert_eq!(probe_level("gzip:9"), Some((9, 1.0)));
        assert_eq!(probe_level("lz4"), Some((1, 4.0)));
        assert_eq!(probe_level("none"), None);
    }

    #[test]
    fn activity_hours() {
        let st = |x: f64| Stat { xacts: x, writes: x / 10.0, reads: 0.0 };
        let mut a = Act::default();
        let h0 = 1_000_000.0 * 3600.0; // an hour boundary
        assert_eq!(a.add(h0 + 10.0, st(100.0), None, 1.0), None);
        // 30 min of samples at 2 xacts/s in that hour
        for i in 1..=30 {
            assert_eq!(a.add(h0 + 10.0 + 60.0 * i as f64, st(100.0 + 120.0 * i as f64), None, 3.0), None);
        }
        // a stats reset (counters back to ~0, stats_reset changed) is dropped, not counted as negative
        let reset = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(5));
        assert_eq!(a.add(h0 + 1900.0, st(5.0), reset, 0.0), None);
        let (hour, rate, act) = a.add(h0 + 3600.0 + 5.0, st(5.0 + 120.0), reset, 0.0).unwrap();
        assert_eq!(hour, 1_000_000);
        assert!((rate.xacts - 7200.0).abs() < 1.0, "{rate:?}"); // 2/s scaled to a full hour
        assert!((rate.writes - 720.0).abs() < 1.0);
        assert_eq!(act, 3.0);
        // less than 10 minutes seen in an hour: nothing to fold
        let mut b = Act::default();
        b.add(h0 + 3000.0, st(0.0), None, 0.0);
        b.add(h0 + 3300.0, st(10.0), None, 0.0);
        assert_eq!(b.add(h0 + 3700.0, st(20.0), None, 0.0), None);
    }

    #[test]
    fn schedule_slots_of_the_week() {
        let from = t("2026-10-01T12:00:00Z"); // a Thursday
        assert_eq!(backup_slots("0 2 * * *", from), vec![2, 26, 50, 74, 98, 122, 146]);
        assert_eq!(backup_slots("30 4 * * 0", from), vec![4]);
        assert_eq!(backup_slots("0 * * * *", from).len(), 168);
    }

    #[test]
    fn gate_decisions() {
        let th = Busy { active: 4, tps: 200, long_xact: 30, lag: 30, loadavg: 0.8 };
        assert!(busy_reasons(4, Some(200.0), 0, Some(30.0), Some(0.8), th).is_empty(), "at the threshold is not busy");
        let r = busy_reasons(12, Some(900.0), 1, Some(45.0), Some(1.5), th);
        assert_eq!(r.len(), 5, "{r:?}");
        assert_eq!(r[0], "12 active sessions > 4");
        assert_eq!(r[1], "900 tps > 200");
        let off = Busy { active: 0, tps: 0, long_xact: 0, lag: 0, loadavg: 0.0 };
        assert!(busy_reasons(999, Some(1e6), 9, Some(1e4), Some(9.0), off).is_empty(), "0 = ignore");
        assert!(busy_reasons(1, None, 0, None, None, th).is_empty(), "no tps yet, no replica, no /proc");
        use GateAction::*;
        let (sh, on, of) = (Gate::Shadow, Gate::On, Gate::Off);
        let (warn, defer) = (GateManual::Warn, GateManual::Defer);
        assert_eq!(gate_action(sh, warn, "backup", false, true, false), Shadow, "shadow never delays");
        assert_eq!(gate_action(on, warn, "backup", false, true, false), Defer);
        assert_eq!(gate_action(on, warn, "backup", false, true, true), Run, "deadline reached: runs (forced)");
        assert_eq!(gate_action(on, warn, "backup", false, false, false), Run, "quiet");
        assert_eq!(gate_action(of, warn, "backup", false, true, false), Run);
        assert_eq!(gate_action(on, warn, "backup", true, true, false), Run, "manual + warn: starts now");
        assert_eq!(gate_action(on, defer, "backup", true, true, false), Defer, "manual + defer");
        assert_eq!(gate_action(on, defer, "restore", true, true, false), Run, "restores are never gated");
        assert_eq!(gate_action(on, warn, "verify", false, true, false), Defer);
        assert_eq!(gate_action(on, warn, "prune", false, true, false), Run);
    }

    #[test]
    fn pick_priority() {
        assert!(priority("restore", "manual", "{}") < priority("backup", "manual", "{}"));
        assert!(priority("backup", "manual", "{}") < priority("backup", "schedule", "{}"));
        assert_eq!(priority("backup", "schedule", "{\"manual\": true, \"coalesced\": 1}"), priority("backup", "manual", "{}"));
        assert_eq!(priority("backup", "first", "{}"), priority("backup", "schedule", "{}"));
        assert!(priority("backup", "first", "{}") < priority("verify", "manual", "{}"));
        assert!(priority("verify", "schedule", "{}") < priority("prune", "manual", "{}"));
    }

    #[test]
    fn update_script_matches_install() {
        // each function the update script replaces must read exactly as the install script creates it
        let lib = include_str!("lib.rs");
        let upd = include_str!("../sql/pgbx--0.5.0--0.6.0.sql");
        let fns: Vec<&str> = upd.split("CREATE OR REPLACE FUNCTION ").skip(1).collect();
        assert!(!fns.is_empty());
        for f in fns {
            let body = &f[..f.find("END $$;").expect("plpgsql body")];
            assert!(lib.contains(&format!("CREATE FUNCTION {body}")), "update script differs from lib.rs: {}", &body[..40]);
        }
    }
}

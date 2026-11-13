//! pgbx as a plain Postgres client: profiles, adapters, `pgbx query` and agent memory all work against a
//! server WITHOUT the pgbx extension. This module tells "the extension is simply not there" (client-only use,
//! fine: backups are off) apart from "pgbx is on the server but broken here" (a real failure), and turns the
//! raw `schema "pgbx" does not exist` errors of backup commands into one friendly sentence.

use crate::one;
use postgres::Client;
use serde_json::{json, Value};

pub const INSTALL: &str = "curl -fsSL https://pgbx.deemwar.com/install.sh | sh";
pub const OFF: &str = "pgbx extension not installed on this server: backups are off; queries, profiles and adapters work";
pub fn turn_on() -> String {
    format!("optional, to turn on backups: on the database server run `{}` and then `sudo pgbx setup server`", INSTALL)
}

#[derive(Debug, PartialEq)]
pub enum Ext {
    /// created in this database: the backup commands work
    Here,
    /// not on the server at all (no package, not preloaded): client-only use
    Absent,
    /// on the server (package or shared_preload_libraries) but not created in this database
    NotInDb,
}

pub fn classify(created: bool, available: bool, preloaded: bool) -> Ext {
    match (created, available || preloaded) {
        (true, _) => Ext::Here,
        (false, false) => Ext::Absent,
        (false, true) => Ext::NotInDb,
    }
}

/// Where the extension stands for the database `c` is connected to.
pub fn ext(c: &mut Client) -> Result<Ext, String> {
    let v = one(c, "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pgbx') AS created,
                           EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pgbx') AS available,
                           -- pg_settings hides superuser-only settings from other roles instead of raising, unlike current_setting()
                           coalesce((SELECT setting FROM pg_settings WHERE name = 'shared_preload_libraries'), '') ~ '(^|[ ,])pgbx($|[ ,])' AS preloaded", &[])?;
    Ok(classify(v["created"] == true, v["available"] == true, v["preloaded"] == true))
}

/// The `pgbx status` reply when the extension is not in `db`; None when it is (the caller reads pgbx.status()).
pub fn status_without(e: &Ext, db: &str) -> Option<Value> {
    match e {
        Ext::Here => None,
        Ext::Absent => Some(json!({"ok": true, "postgres": "up", "database": db, "backups": "off", "extension": null,
            "status": null, "info": OFF, "next_steps": [turn_on()]})),
        Ext::NotInDb => Some(json!({"ok": false, "postgres": "up", "database": db, "extension": null, "status": null,
            "error": format!("pgbx is on this server but not in database {db}: the worker adds it to every database on its next poll \
                (pgbx.poll_seconds); if it stays missing run `pgbx doctor`")})),
    }
}

/// Raw "pgbx objects do not exist" errors from backup commands, said plainly. Other errors pass through.
pub fn friendly(cmd: &str, e: String) -> String {
    let missing = e.contains("schema \"pgbx\" does not exist") || e.contains("relation \"pgbx.") || e.contains("function pgbx.");
    if missing && e.contains("does not exist") {
        format!("{OFF}. `pgbx {cmd}` needs the extension ({e})")
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_extension_states() {
        assert_eq!(classify(true, true, true), Ext::Here);
        assert_eq!(classify(true, false, false), Ext::Here);
        assert_eq!(classify(false, false, false), Ext::Absent);
        assert_eq!(classify(false, true, false), Ext::NotInDb);
        assert_eq!(classify(false, false, true), Ext::NotInDb);
    }

    #[test]
    fn status_reply_per_state() {
        assert!(status_without(&Ext::Here, "d").is_none());
        let v = status_without(&Ext::Absent, "shop").unwrap();
        assert_eq!((v["ok"].clone(), v["backups"].as_str()), (json!(true), Some("off")));
        assert!(v["info"].as_str().unwrap().contains("queries, profiles and adapters work"));
        let v = status_without(&Ext::NotInDb, "shop").unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("not in database shop"));
    }

    #[test]
    fn friendly_errors() {
        let e = friendly("list", "schema \"pgbx\" does not exist".into());
        assert!(e.starts_with(OFF) && e.contains("pgbx list"));
        assert!(friendly("list", "relation \"pgbx.backups\" does not exist".into()).starts_with(OFF));
        assert!(friendly("now", "function pgbx.backup_now() does not exist".into()).starts_with(OFF));
        assert_eq!(friendly("list", "cannot connect".into()), "cannot connect");
        assert_eq!(friendly("list", "relation \"public.t\" does not exist".into()), "relation \"public.t\" does not exist");
    }
}

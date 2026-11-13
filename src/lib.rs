//! pgbx — zero-touch Postgres backups to S3, controlled from SQL.
//!
//! Loaded via `shared_preload_libraries`, it registers server-wide settings (`pgbx.*`) and one
//! background worker. The worker installs the extension into every database (and `template1`, so new
//! databases are born with it), backs each one up on its schedule, and runs queued manual backups/restores.
//! Per-database policy lives in `pgbx.config`; every run is recorded in `pgbx.history`.

use pgrx::bgworkers::BackgroundWorkerBuilder;
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::prelude::*;
use std::ffi::CString;
use std::time::Duration;

mod audit;
pub mod crypt;
mod extdata;
mod extras;
pub mod globals;
mod notify;
mod pitr;
mod retention;
pub mod s3auth;
mod schedule;
mod transfer;
mod worker;

/// 'every 1 hour' / 'daily at 02:30' / cron -> cron. Errors (with the accepted forms) if it can't be read.
#[pg_extern(immutable, strict)]
fn to_cron(schedule: &str) -> String {
    schedule::to_cron(schedule).unwrap_or_else(|e| pgrx::error!("pgbx: {e}"))
}

/// Internal: presign a GET for one key. Execute is revoked from PUBLIC; use pgbx.download_url().
#[pg_extern(strict, name = "_presign")]
fn presign(key: &str, expires_seconds: i32) -> String {
    let secs = expires_seconds.clamp(60, 604_800) as u32; // S3 allows at most 7 days
    let b = worker::bucket().unwrap_or_else(|e| pgrx::error!("pgbx: {e}"));
    b.presign_get(key, secs, None).unwrap_or_else(|e| pgrx::error!("pgbx: presign failed: {e}"))
}

/// Internal (doctor): which source of the AWS default chain answers right now, for pgbx.credentials_file = '' or
/// 'aws-default' (s3auth.rs). Never a key or token: the source, the role name and until when, or what failed.
#[pg_extern(name = "_s3_credentials")]
fn s3_credentials() -> TableIterator<'static, (name!(ok, bool), name!(detail, String))> {
    TableIterator::once(match s3auth::from_chain() {
        Ok(c) => (true, format!("source: {} ({})", c.source.name(), c.describe())),
        Err(e) => (false, e),
    })
}

/// Next run (epoch seconds) of a cron schedule strictly after `after_epoch`; used by status().
#[pg_extern(immutable, strict)]
fn next_run_epoch(cron: &str, after_epoch: f64) -> f64 {
    schedule::next_after_epoch(cron, after_epoch).unwrap_or_else(|e| pgrx::error!("pgbx: {e}"))
}

::pgrx::pg_module_magic!(name, version);

// ---- server-wide settings (postgresql.conf; change + `SELECT pg_reload_conf()`) ----
pub static S3_ENDPOINT: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static S3_BUCKET: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static S3_REGION: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"us-east-1"));
pub static SERVER_NAME: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static CREDENTIALS_FILE: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c"/etc/pgbx/s3.credentials"));
pub static SOCKET_DIR: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"/var/run/postgresql"));
pub static POLL_SECONDS: GucSetting<i32> = GucSetting::<i32>::new(5);
pub static MAX_DAYS_LIMIT: GucSetting<i32> = GucSetting::<i32>::new(90);
// alerts: run on every failed job (JSON on stdin, PGBX_* env vars)
pub static ALERT_COMMAND: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static DUMP_COMPRESSION: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"auto"));
pub static AUDIT_DAYS: GucSetting<i32> = GucSetting::<i32>::new(30);
pub static ADMIN_DB: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"postgres"));
// bandwidth caps per job (ADR 0001 §3)
pub static UPLOAD_KBPS: GucSetting<i32> = GucSetting::<i32>::new(0);
pub static DOWNLOAD_KBPS: GucSetting<i32> = GucSetting::<i32>::new(0);
// resource caps on pg_dump / pg_restore (ADR 0001 §3)
pub static JOB_NICE: GucSetting<i32> = GucSetting::<i32>::new(10);
pub static JOB_IONICE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"best-effort-7"));
pub static DUMP_COMPRESSION_BUSY: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"auto"));
pub static DUMP_LOCK_TIMEOUT: GucSetting<i32> = GucSetting::<i32>::new(5_000); // ms
pub static DUMP_LOCK_TIMEOUT_FORCED: GucSetting<i32> = GucSetting::<i32>::new(60_000); // ms
pub static RESTORE_SYNCHRONOUS_COMMIT: GucSetting<bool> = GucSetting::<bool>::new(false);
pub static DOCTOR_LONG_JOB: GucSetting<i32> = GucSetting::<i32>::new(3600); // s
// deferring a backup (lock timeout now, the load gate later): backoff and the deadline it never passes
pub static DEFER_BACKOFF: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"1,2,4,8,15"));
pub static MAX_DEFER: GucSetting<i32> = GucSetting::<i32>::new(4 * 3600); // s
pub static MAX_DEFER_FIRST: GucSetting<i32> = GucSetting::<i32>::new(15 * 60); // s
pub static COALESCE_MANUAL: GucSetting<bool> = GucSetting::<bool>::new(true);
// one server-wide job queue (ADR 0001 §0)
pub static MAX_CONCURRENT_JOBS: GucSetting<i32> = GucSetting::<i32>::new(1);
pub static RESTORE_LANE: GucSetting<bool> = GucSetting::<bool>::new(true);
// time estimates (ADR 0001 §4)
pub static ETA_DEFAULT_MBPS: GucSetting<i32> = GucSetting::<i32>::new(20);
pub static ETA_CALIBRATE: GucSetting<bool> = GucSetting::<bool>::new(true);
pub static ETA_SAMPLES: GucSetting<i32> = GucSetting::<i32>::new(5);
// quiet-window suggestion (ADR 0001 §2)
pub static ACTIVITY_SAMPLING: GucSetting<bool> = GucSetting::<bool>::new(true);
pub static ACTIVITY_DECAY: GucSetting<f64> = GucSetting::<f64>::new(0.9);
pub static SUGGEST_MIN_DAYS: GucSetting<i32> = GucSetting::<i32>::new(7);
pub static DOCTOR_BUSY_RATIO: GucSetting<f64> = GucSetting::<f64>::new(3.0);
// load gate (ADR 0001 §1): shadow by default (owner decision 2026-10-02)
pub static LOAD_GATE: GucSetting<Gate> = GucSetting::<Gate>::new(Gate::Shadow);
pub static BUSY_ACTIVE_BACKENDS: GucSetting<i32> = GucSetting::<i32>::new(4);
pub static BUSY_TPS: GucSetting<i32> = GucSetting::<i32>::new(200);
pub static BUSY_LONG_XACT: GucSetting<i32> = GucSetting::<i32>::new(30); // s
pub static BUSY_REPLICA_LAG: GucSetting<i32> = GucSetting::<i32>::new(30); // s
pub static BUSY_LOADAVG: GucSetting<f64> = GucSetting::<f64>::new(0.8);
pub static GATE_MANUAL_JOBS: GucSetting<GateManual> = GucSetting::<GateManual>::new(GateManual::Warn);

/// pgbx.load_gate: off = never look; shadow = record would_defer, never delay; on = defer while busy, up to max_defer.
#[derive(pgrx::guc::PostgresGucEnum, Clone, Copy, PartialEq, Debug)]
pub enum Gate {
    #[name = c"off"]
    Off,
    #[name = c"shadow"]
    Shadow,
    #[name = c"on"]
    On,
}

/// pgbx.gate_manual_jobs: what the gate does to backup_now() / verify_now() / restore().
#[derive(pgrx::guc::PostgresGucEnum, Clone, Copy, PartialEq, Debug)]
pub enum GateManual {
    #[name = c"warn"]
    Warn,
    #[name = c"defer"]
    Defer,
    #[name = c"off"]
    Off,
}
// 0.6 extras: client-side encryption, roles with every backup, notifications
pub static ENCRYPTION_KEY_FILE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static BACKUP_ROLE_PASSWORDS: GucSetting<bool> = GucSetting::<bool>::new(false);
pub static NOTIFY: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static NOTIFY_SECRETS_FILE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
// point-in-time restore (optional, off by default): WAL archiving with `pgbx wal-push` + scheduled base backups
pub static PITR: GucSetting<bool> = GucSetting::<bool>::new(false);
pub static PITR_SCHEDULE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"daily at 01:00"));
pub static PITR_RETENTION: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"7 days"));
pub static WAL_QUEUE_MAX: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"4GB"));
pub static WAL_GAP_MARGIN: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"60s"));
pub static WAL_ALERT_AFTER: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"15 min"));
pub static WAL_ALERT_SIZE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"2GB"));
pub static WORK_DIR: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static CLI_PATH: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static OVERRUN_POLICY: GucSetting<Overrun> = GucSetting::<Overrun>::new(Overrun::Skip);
pub static OVERRUN_MAX_GAP: GucSetting<f64> = GucSetting::<f64>::new(1.5);

/// pgbx.overrun_policy: what happens to schedule slots that passed while a dump of that database ran.
#[derive(pgrx::guc::PostgresGucEnum, Clone, Copy, PartialEq, Debug)]
pub enum Overrun {
    #[name = c"skip"]
    Skip,
    #[name = c"catch_up"]
    CatchUp,
}

/// Install rustls' process-wide crypto provider once. The build has both aws-lc-rs (rust-s3) and ring (ureq, lettre),
/// and without an explicit choice rustls panics on the first HTTPS connection: every backup to a real (https) S3
/// endpoint failed in 0.6.0 ("Could not automatically determine the process-level CryptoProvider").
pub(crate) fn crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    crypto_provider(); // in the postmaster: every backend and the worker inherit it
    GucRegistry::define_string_guc(c"pgbx.s3_endpoint", c"S3 endpoint URL", c"e.g. https://hel1.your-objectstorage.com", &S3_ENDPOINT, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.s3_bucket", c"S3 bucket for all backups of this server", c"", &S3_BUCKET, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.s3_region", c"S3 region", c"", &S3_REGION, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.server_name", c"Top-level folder for this server in the bucket", c"defaults to the hostname", &SERVER_NAME, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.credentials_file", c"File with access_key_id= and secret_access_key= lines", c"keep it readable by the postgres OS user only", &CREDENTIALS_FILE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.socket_dir", c"Unix socket directory the worker connects through", c"", &SOCKET_DIR, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.max_days_limit", c"Ceiling on any database's max_days", c"stops a database from keeping backups longer than the server allows", &MAX_DAYS_LIMIT, 1, 36500, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.alert_command", c"Shell command run for every failed job", c"gets JSON on stdin and PGBX_DATABASE/PGBX_KIND/PGBX_JOB_ID/PGBX_ERROR/PGBX_SERVER env vars", &ALERT_COMMAND, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.dump_compression", c"pg_dump --compress for per-database backups", c"auto (default: zstd:3 with pg_dump 16+, gzip level 6 before), or e.g. zstd:3, lz4, gzip:6, 6, none", &DUMP_COMPRESSION, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.admin_db", c"Database that holds the server-wide overview and doctor()", c"", &ADMIN_DB, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.audit_days", c"Days of history (audit trail) kept per database", c"older rows are pruned, except kept backups, open incidents/gaps and the newest row of each kind", &AUDIT_DAYS, 1, 36500, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.poll_seconds", c"How often the worker looks for new databases and due/queued jobs", c"", &POLL_SECONDS, 1, 3600, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.upload_kbps", c"Upload bandwidth cap per job in KiB/s", c"0 = unlimited", &UPLOAD_KBPS, 0, 10_000_000, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.download_kbps", c"Download (restore) bandwidth cap per job in KiB/s", c"0 = unlimited", &DOWNLOAD_KBPS, 0, 10_000_000, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.job_nice", c"CPU niceness of pg_dump / pg_restore", c"0-19; never raises priority above the worker's own", &JOB_NICE, 0, 19, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.job_ionice", c"IO priority of pg_dump / pg_restore (Linux)", c"none, idle, or best-effort-0 .. best-effort-7; anything else means best-effort-7", &JOB_IONICE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.dump_compression_busy", c"pg_dump --compress for a backup forced to run while the server is busy", c"auto (default: zstd:1 with pg_dump 16+, gzip level 1 before; never more than pgbx.dump_compression), or a pg_dump --compress value", &DUMP_COMPRESSION_BUSY, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.dump_lock_timeout", c"How long pg_dump waits for its table locks before the backup is retried later", c"0 = wait forever; never queue behind DDL", &DUMP_LOCK_TIMEOUT, 0, 600_000, GucContext::Sighup, GucFlags::UNIT_MS);
    GucRegistry::define_int_guc(c"pgbx.dump_lock_timeout_forced", c"pg_dump lock wait once a deferred backup reached its deadline", c"if it still times out, the backup fails and alerts", &DUMP_LOCK_TIMEOUT_FORCED, 0, 600_000, GucContext::Sighup, GucFlags::UNIT_MS);
    GucRegistry::define_bool_guc(c"pgbx.restore_synchronous_commit", c"synchronous_commit for pg_restore", c"off by default: the target is a new database, a crash just means restoring again", &RESTORE_SYNCHRONOUS_COMMIT, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.doctor_long_job", c"doctor() warns about a backup/restore process running longer than this", c"0 = never", &DOCTOR_LONG_JOB, 0, 86_400, GucContext::Sighup, GucFlags::UNIT_S);
    GucRegistry::define_string_guc(c"pgbx.defer_backoff", c"Minutes between retries of a deferred backup", c"comma list, each 1-60; the last value repeats", &DEFER_BACKOFF, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.max_defer", c"A deferred backup runs anyway this long after it was queued", c"capped at the schedule interval; backups are never skipped", &MAX_DEFER, 0, 86_400, GucContext::Sighup, GucFlags::UNIT_S);
    GucRegistry::define_int_guc(c"pgbx.max_defer_first", c"max_defer for a new database's first backup", c"", &MAX_DEFER_FIRST, 0, 86_400, GucContext::Sighup, GucFlags::UNIT_S);
    GucRegistry::define_bool_guc(c"pgbx.coalesce_manual", c"backup_now() / verify_now() return the job already queued instead of adding another", c"", &COALESCE_MANUAL, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.max_concurrent_jobs", c"Jobs (backups, restore tests, restores, prunes) running at once on this server", c"1-8; each running job holds an advisory lock slot in the admin database, so nothing can exceed it", &MAX_CONCURRENT_JOBS, 1, 8, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_bool_guc(c"pgbx.restore_lane", c"One extra job slot for restores only", c"a restore never waits behind a long dump; turn off on very small servers", &RESTORE_LANE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.eta_default_mbps", c"Speed (MB/s) assumed for time estimates before anything was measured", c"deliberately slow, so first estimates err long", &ETA_DEFAULT_MBPS, 1, 10_000, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_bool_guc(c"pgbx.eta_calibrate", c"Measure one core's compression speed once a day (about 1 s of one niced core)", c"", &ETA_CALIBRATE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.eta_samples", c"Recent jobs per database that time estimates are based on", c"1-50", &ETA_SAMPLES, 1, 50, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_bool_guc(c"pgbx.activity_sampling", c"Learn each database's activity per hour of the week (one pg_stat_database read per poll)", c"feeds pgbx.suggest_window()", &ACTIVITY_SAMPLING, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_float_guc(c"pgbx.activity_decay", c"Weight of the past in each hour's activity average", c"0.5-0.99; 0.9 adapts in about two weeks", &ACTIVITY_DECAY, 0.5, 0.99, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.suggest_min_days", c"Days of activity samples before suggest_window() has high confidence", c"1-90", &SUGGEST_MIN_DAYS, 1, 90, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_float_guc(c"pgbx.doctor_busy_ratio", c"doctor() warns when the schedule's hour is this many times busier than the suggested window", c"1-100", &DOCTOR_BUSY_RATIO, 1.0, 100.0, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_enum_guc(c"pgbx.load_gate", c"Look at server load before starting a scheduled backup or restore test", c"off; shadow (default): record would_defer, never delay; on: defer while busy, never past pgbx.max_defer (per database: configure(load_gate => ...))", &LOAD_GATE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.busy_active_backends", c"Busy when more client sessions than this are not idle", c"0 = ignore; pgbx's own sessions never count", &BUSY_ACTIVE_BACKENDS, 0, 10_000, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.busy_tps", c"Busy above this many transactions per second (all databases, between two polls)", c"0 = ignore; pgbx's own polling adds a few per database", &BUSY_TPS, 0, 10_000_000, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.busy_long_xact", c"Busy while a writing transaction has been open longer than this", c"0 = ignore; never stack a dump on a migration", &BUSY_LONG_XACT, 0, 3600, GucContext::Sighup, GucFlags::UNIT_S);
    GucRegistry::define_int_guc(c"pgbx.busy_replica_lag", c"Busy while a standby replays more than this behind", c"0 = ignore", &BUSY_REPLICA_LAG, 0, 3600, GucContext::Sighup, GucFlags::UNIT_S);
    GucRegistry::define_float_guc(c"pgbx.busy_loadavg", c"Busy above this 1-minute load average per core (Linux)", c"0 = ignore; ignored where /proc/loadavg is missing", &BUSY_LOADAVG, 0.0, 10.0, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_enum_guc(c"pgbx.gate_manual_jobs", c"What the load gate does to backup_now() / verify_now() / restore()", c"warn (default): a NOTICE, it starts anyway; defer: like scheduled jobs; off: nothing", &GATE_MANUAL_JOBS, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_enum_guc(c"pgbx.overrun_policy", c"Schedule slots that passed while a dump of the database was running", c"skip: the next run is the next slot after the dump finished (see pgbx.overrun_max_gap); catch_up: run once right away", &OVERRUN_POLICY, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.encryption_key_file", c"Encrypt every dump and roles file with this key (AES-256-GCM)", c"file with 32 random bytes as base64 or hex, chmod 600, owned by postgres; empty (default) = no encryption", &ENCRYPTION_KEY_FILE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_bool_guc(c"pgbx.backup_role_passwords", c"Keep role password hashes in the roles file stored with each backup", c"off (default): pg_dumpall --no-role-passwords; restored roles have no password", &BACKUP_ROLE_PASSWORDS, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.notify", c"Notification channels by name: slack:NAME, telegram:NAME, webhook:NAME, email:NAME", c"names only; URLs and tokens go in pgbx.notify_secrets_file", &NOTIFY, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.notify_secrets_file", c"File with the URLs/tokens of the pgbx.notify channels", c"chmod 600, owned by postgres; e.g. slack.ops.url = https://hooks.slack.com/...", &NOTIFY_SECRETS_FILE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_bool_guc(c"pgbx.pitr", c"Point-in-time restore: archive WAL with pgbx wal-push and take scheduled base backups", c"needs archive_mode=on and archive_command='<pgbx> wal-push %p' (pgbx setup pitr)", &PITR, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.pitr_schedule", c"When base backups for point-in-time restore run", c"same forms as set_schedule(); default 'daily at 01:00'", &PITR_SCHEDULE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.pitr_retention", c"How far back point-in-time restore must reach", c"e.g. '7 days'; older base backups and WAL are deleted, the newest base backup is always kept", &PITR_RETENTION, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.wal_queue_max", c"WAL waiting to be archived above which pgbx wal-push drops WAL (recorded as a gap) instead of filling the disk", c"e.g. '4GB'; 'off' = never drop", &WAL_QUEUE_MAX, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.wal_gap_margin", c"Safety margin before a recorded WAL gap in which restores are refused", c"default '60s'", &WAL_GAP_MARGIN, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.wal_alert_after", c"Alert when the oldest WAL waiting to be archived is older than this", c"default '15 min'", &WAL_ALERT_AFTER, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.wal_alert_size", c"Alert when this much WAL waits to be archived", c"default '2GB'; 'off' = never", &WAL_ALERT_SIZE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.work_dir", c"Directory for point-in-time restore state (conf, spool, drop log)", c"default <data_directory>/../pgbx; must be OUTSIDE the data directory", &WORK_DIR, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.cli_path", c"Path of the pgbx command line program the worker runs for base backups", c"default /usr/local/bin/pgbx or /usr/bin/pgbx", &CLI_PATH, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_float_guc(c"pgbx.overrun_max_gap", c"With overrun_policy=skip, never wait for a slot more than this many schedule intervals after the last good backup finished", c"1.0-10; past it the backup runs right away", &OVERRUN_MAX_GAP, 1.0, 10.0, GucContext::Sighup, GucFlags::default());

    // Only register the worker when loaded at server start (shared_preload_libraries),
    // not when a backend loads the library for CREATE EXTENSION.
    if unsafe { pg_sys::process_shared_preload_libraries_in_progress } {
        BackgroundWorkerBuilder::new("pgbx scheduler")
            .set_function("pgbx_worker_main")
            .set_library("pgbx")
            .set_restart_time(Some(Duration::from_secs(10)))
            .load();
    }
}

extension_sql!(
    r#"
-- Access roles (server-wide; created once, reused by every database). Nobody else gets anything.
--   pgbx_viewer: status(), backups      pgbx_admin: manage this database's backups (+ viewer)
DO $roles$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pgbx_viewer') THEN
        CREATE ROLE pgbx_viewer NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pgbx_admin') THEN
        CREATE ROLE pgbx_admin NOLOGIN IN ROLE pgbx_viewer;
    END IF;
END $roles$;

-- Server-wide summary, one row per database, written by the worker; read via overview() in the admin database.
CREATE TABLE pgbx.server_overview (
    database         name PRIMARY KEY,
    state            text,
    schedule         text,
    last_backup_at   timestamptz,
    last_backup_size text,
    next_backup_at   timestamptz,
    backups_kept     bigint,
    last_verify      text,
    last_error       text,
    seen_at          timestamptz NOT NULL DEFAULT now(),
    interval_secs    float8,                                   -- seconds between this schedule's slots
    dump_secs        float8[],                                 -- run time of the last 3 backups, newest first
    eta_error        float8,                                   -- median |actual - estimate| / actual of recent jobs
    window_cron      text,                                     -- suggest_window(): the quietest slot
    window_score     float8,
    current_score    float8,                                   -- the current schedule's slot, scored the same way
    window_confidence text,
    load_gate        text,                                     -- this database's effective pgbx.load_gate
    would_defer_7d   int,                                      -- jobs the gate would have deferred (shadow), 7 days
    deferred_7d      int,                                      -- jobs it deferred (on)
    forced_7d        int,                                      -- backups that ran at their max_defer deadline
    -- Prometheus metrics (pgbx metrics / GET /metrics on pgbx ui)
    last_backup_bytes     bigint,
    failures_total        bigint,                               -- failed backups / restores / restore tests in the history
    queued_jobs           bigint,
    last_verify_ok        bool,
    last_backup_encrypted bool
);

-- The server-wide job queue as the worker sees it (every database's running, queued and deferred jobs), rewritten
-- every poll in the admin database; `pgbx jobs` reads it. Readable like server_overview (pgbx_viewer).
CREATE TABLE pgbx.server_queue (
    database     name NOT NULL,
    job_id       bigint NOT NULL,
    kind         text NOT NULL,
    trigger      text NOT NULL,
    state        text NOT NULL,                                -- running | cancelling | queued | deferred
    position     int,                                          -- 1 = starts next; NULL while running or deferred
    slot         int,                                          -- job slot while running; 0 = the restore lane
    requested_at timestamptz,
    started_at   timestamptz,
    detail       text,                                         -- why it waits, or where it runs
    seen_at      timestamptz NOT NULL DEFAULT now(),
    eta_start    timestamptz,                                  -- estimated start (queued) / start (running)
    eta_finish   timestamptz,
    est_bytes    bigint,
    done_bytes   bigint,                                       -- live progress of a running job
    progress     text,                                         -- '41 % · ~9 min left' / 'queued, #2 in line: starts ~14:05, takes ~18 min'
    PRIMARY KEY (database, job_id)
);

-- What this server can do (ADR 0001 §4), measured by the worker and copied into every database so time estimates
-- work anywhere. Rates are bytes per second; NULL = not measured (yet).
CREATE TABLE pgbx.server_capacity (
    id           int PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    cores        int,                                          -- shown only: a dump uses one core
    cpu_bps      float8,                                       -- one core compressing real table pages (pgbx.job_nice)
    cpu_codec    text,                                         -- the pgbx.dump_compression that was measured
    disk_bps     float8,                                       -- reads, from blk_read_time (needs track_io_timing)
    upload_bps   float8,                                       -- median of the last 20 upload parts
    download_bps float8,                                       -- median of recent downloads
    load_factor  float8,                                       -- 1 idle .. 3 saturated: estimates stretch by it
    backup_bps   float8,                                       -- server-wide median speed of recent jobs (dump bytes/s)
    restore_bps  float8,
    verify_bps   float8,
    queue_jobs   int,                                          -- jobs running or waiting, server-wide
    wait_secs    float8,                                       -- estimated wait for a job queued now
    measured_at  timestamptz,                                  -- last cpu probe
    updated_at   timestamptz NOT NULL DEFAULT now(),
    backup_slots jsonb,                                        -- {database: [hour of week (UTC) its backups start in]}
    load_at      timestamptz,                                  -- last load sample (ADR 0001 §1)
    load_busy    bool,
    load_reasons text,                                         -- '12 active sessions > 4, 900 tps > 200'
    load_active  int,
    load_tps     float8
);

-- Activity per hour of the week, learned from pg_stat_database deltas every poll (ADR 0001 §2). Hours are UTC, like
-- pgbx schedules. scope 'db' = this database; 'server' = every database together (kept in the admin database and
-- copied into each one hourly). A bucket is a decayed average (x = d*x + (1-d)*hour, pgbx.activity_decay): it adapts
-- and never grows (336 rows at most). Not part of a dump: a restored copy learns its own.
CREATE TABLE pgbx.activity_hourly (
    scope      text NOT NULL CHECK (scope IN ('db', 'server')),
    dow        smallint NOT NULL CHECK (dow BETWEEN 0 AND 6),      -- 0 = Sunday
    hour       smallint NOT NULL CHECK (hour BETWEEN 0 AND 23),
    samples    int NOT NULL DEFAULT 0,                             -- hours folded in
    xacts      float8 NOT NULL DEFAULT 0,                          -- transactions per hour
    writes     float8 NOT NULL DEFAULT 0,                          -- rows inserted + updated + deleted per hour
    reads      float8 NOT NULL DEFAULT 0,                          -- blocks read per hour
    active_max float8 NOT NULL DEFAULT 0,                          -- most non-idle sessions seen in the hour
    updated_at timestamptz,
    PRIMARY KEY (scope, dow, hour)
);

-- One row per database. path NULL = use the database name (so the row copied from template1 stays correct).
CREATE TABLE pgbx.config (
    id             int PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    path           text,
    schedule       text NOT NULL DEFAULT '0 2 * * *',          -- cron (normalized)
    schedule_label text NOT NULL DEFAULT 'daily at 02:00',     -- as the user wrote it
    max_backups    int  NOT NULL DEFAULT 14 CHECK (max_backups >= 1),   -- keep at most this many backups
    max_days       int  NOT NULL DEFAULT 90 CHECK (max_days >= 1),      -- and none older than this (newest always kept)
    verify_schedule text DEFAULT '0 4 * * 0',                  -- restore test; NULL = never
    verify_label   text NOT NULL DEFAULT 'weekly on sunday at 04:00',
    enabled        bool NOT NULL DEFAULT true,                 -- false = paused
    paused_reason  text,
    paused_at      timestamptz,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    -- data scope: every table's DEFINITION is always backed up; these only decide whose ROWS are kept
    include_data   text[],                                     -- NULL/empty = rows of all tables
    exclude_data   text[],                                     -- rows of these are skipped
    load_gate      text CHECK (load_gate IN ('off', 'shadow', 'on')), -- NULL = the server's pgbx.load_gate
    gfs            text                                        -- e.g. '7d,4w,12m': also keep the newest per day/week/month
);
-- No default row here: a restored dump brings its own row, and a fresh database gets one from the
-- worker or configure() (INSERT ... ON CONFLICT DO NOTHING), so the two never collide.

-- Every backup / restore, queued manual or automatic. The worker claims rows in state 'queued'.
CREATE TABLE pgbx.history (
    id           bigserial PRIMARY KEY,
    kind         text NOT NULL CHECK (kind IN ('backup', 'restore', 'config', 'pause', 'resume', 'prune', 'verify',
                                               'base_backup', 'wal_gap', 'wal_archive')),
    trigger      text NOT NULL DEFAULT 'manual' CHECK (trigger IN ('manual', 'schedule', 'first', 'migration', 'wal_gap')),
    params       jsonb NOT NULL DEFAULT '{}',
    state        text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running', 'done', 'failed', 'expired', 'cancelled')),
    requested_at timestamptz NOT NULL DEFAULT now(),
    started      timestamptz,
    finished     timestamptz,
    s3_key       text,
    bytes        bigint,
    error        text,
    who          text DEFAULT session_user                     -- the role that asked (worker rows: the worker's role)
);
CREATE INDEX ON pgbx.history (state) WHERE state IN ('queued', 'running');

-- keep policy + history in logical dumps of this database
SELECT pg_catalog.pg_extension_config_dump('pgbx.config', '');
SELECT pg_catalog.pg_extension_config_dump('pgbx.history', '');

-- internal: record a policy change in history
CREATE FUNCTION pgbx._log(kind text, params jsonb) RETURNS void LANGUAGE sql AS $$
    INSERT INTO pgbx.history (kind, trigger, params, state, started, finished)
    VALUES (kind, 'migration', params, 'done', now(), now())
$$;

-- Change this database's policy. NULL arguments leave a setting unchanged. Meant for migrations.
-- schedule accepts the same forms as set_schedule().
-- load_gate: 'off' | 'shadow' | 'on' for this database ('default' = follow the server's pgbx.load_gate).
CREATE FUNCTION pgbx.configure(
    schedule text DEFAULT NULL, max_backups int DEFAULT NULL, max_days int DEFAULT NULL,
    enabled bool DEFAULT NULL, path text DEFAULT NULL, load_gate text DEFAULT NULL
) RETURNS pgbx.config LANGUAGE plpgsql AS $$
DECLARE r pgbx.config; c text := CASE WHEN schedule IS NULL THEN NULL ELSE pgbx.to_cron(schedule) END;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config x SET
        schedule       = coalesce(c, x.schedule),
        schedule_label = coalesce(configure.schedule, x.schedule_label),
        max_backups    = coalesce(configure.max_backups, x.max_backups),
        max_days       = coalesce(pgbx._check_days(configure.max_days), x.max_days),
        enabled        = coalesce(configure.enabled, x.enabled),
        paused_at      = CASE WHEN configure.enabled IS NULL THEN x.paused_at WHEN configure.enabled THEN NULL ELSE now() END,
        paused_reason  = CASE WHEN configure.enabled IS NULL THEN x.paused_reason WHEN configure.enabled THEN NULL ELSE 'configure(enabled => false)' END,
        path           = coalesce(configure.path, x.path),
        load_gate      = CASE WHEN configure.load_gate IS NULL THEN x.load_gate WHEN configure.load_gate = 'default' THEN NULL
                              ELSE configure.load_gate END,
        updated_at     = now()
    RETURNING * INTO r;
    PERFORM pgbx._log('config', to_jsonb(r));
    IF configure.max_backups IS NOT NULL OR configure.max_days IS NOT NULL THEN
        INSERT INTO pgbx.history (kind, trigger) VALUES ('prune', 'migration');
    END IF;
    RETURN r;
END $$;

-- internal: enforce the server-wide ceiling pgbx.max_days_limit
CREATE FUNCTION pgbx._check_days(d int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE lim int := coalesce(nullif(current_setting('pgbx.max_days_limit', true), '')::int, 90);
BEGIN
    IF d IS NOT NULL AND d > lim THEN
        RAISE EXCEPTION 'pgbx: max_days % exceeds this server''s limit of % days (pgbx.max_days_limit)', d, lim;
    END IF;
    RETURN d;
END $$;

-- Expiry: keep at most max_backups backups AND none older than max_days (whichever deletes first;
-- the newest backup is always kept). gfs ('7d,4w,12m'; 'off' clears it) ALSO keeps the newest backup of each of
-- the last N days / ISO weeks / months / years ('y'); a GFS keeper is never deleted by the other two rules, and the
-- span must fit pgbx.max_days_limit. Takes effect on the worker's next pass.
CREATE FUNCTION pgbx.set_retention(max_backups int DEFAULT NULL, max_days int DEFAULT NULL, gfs text DEFAULT NULL) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE r pgbx.config; span int; lim int := coalesce(nullif(current_setting('pgbx.max_days_limit', true), '')::int, 90);
BEGIN
    IF set_retention.gfs IS NOT NULL THEN
        span := pgbx._gfs_span(set_retention.gfs);
        IF span > lim THEN
            RAISE EXCEPTION 'pgbx: gfs ''%'' reaches back % days, beyond this server''s limit of % days (raise pgbx.max_days_limit)',
                set_retention.gfs, span, lim;
        END IF;
    END IF;
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config x SET
        max_backups = coalesce(set_retention.max_backups, x.max_backups),
        max_days    = coalesce(pgbx._check_days(set_retention.max_days), x.max_days),
        gfs         = CASE WHEN set_retention.gfs IS NULL THEN x.gfs WHEN span = 0 THEN NULL ELSE lower(trim(set_retention.gfs)) END,
        updated_at  = now()
    RETURNING * INTO r;
    PERFORM pgbx._log('config', jsonb_build_object('max_backups', r.max_backups, 'max_days', r.max_days, 'gfs', r.gfs));
    INSERT INTO pgbx.history (kind, trigger) VALUES ('prune', 'manual');
    RETURN format('keeping at most %s backups and nothing older than %s days%s (newest always kept); pruning now',
                  r.max_backups, r.max_days,
                  CASE WHEN r.gfs IS NULL THEN '' ELSE format(', plus the newest backup per period of gfs %s', r.gfs) END);
END $$;

-- 'every 1 hour', 'every 15 minutes', 'daily at 02:30', 'weekly on sunday at 03:00', 'hourly', or cron.
-- Validated immediately; returns a sentence saying when the next backup will run.
CREATE FUNCTION pgbx.set_schedule(schedule text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE c text := pgbx.to_cron(schedule);
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET schedule = c, schedule_label = set_schedule.schedule, updated_at = now();
    PERFORM pgbx._log('config', jsonb_build_object('schedule', set_schedule.schedule, 'cron', c));
    RETURN format('schedule set to "%s" (cron %s); next backup at %s', schedule, c,
                  to_timestamp(pgbx.next_run_epoch(c, extract(epoch FROM now()))));
END $$;

-- Stop automatic backups (manual backup_now()/restore() keep working). The reason shows in status().
CREATE FUNCTION pgbx.pause(reason text DEFAULT NULL) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET enabled = false, paused_at = now(), paused_reason = reason, updated_at = now();
    PERFORM pgbx._log('pause', jsonb_build_object('reason', reason));
    RETURN 'automatic backups paused' || coalesce(' (' || reason || ')', '') || '; resume with SELECT pgbx.resume()';
END $$;

CREATE FUNCTION pgbx.resume() RETURNS text LANGUAGE plpgsql AS $$
DECLARE c text;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET enabled = true, paused_at = NULL, paused_reason = NULL, updated_at = now() RETURNING schedule INTO c;
    PERFORM pgbx._log('resume', '{}');
    RETURN format('automatic backups resumed; next backup at %s', to_timestamp(pgbx.next_run_epoch(c, extract(epoch FROM now()))));
END $$;

-- Every completed backup of this database, newest first.
CREATE VIEW pgbx.backups AS
    SELECT id, finished AS taken_at, now() - finished AS age, trigger, pg_size_pretty(bytes) AS size, bytes, s3_key
    FROM pgbx.history WHERE kind = 'backup' AND state = 'done' ORDER BY id DESC;

-- One row answering "are we backed up?": state, schedule, last + next backup, last error, where it goes.
CREATE FUNCTION pgbx.status() RETURNS TABLE (
    database name, state text, schedule text, cron text, next_backup_at timestamptz,
    last_backup_at timestamptz, last_backup_age interval, last_backup_size text, last_backup_key text,
    backups_kept bigint, retention text, data_scope text, verify_schedule text, last_verified_at timestamptz, last_verify_result text,
    last_error text, last_error_at timestamptz, paused_reason text, paused_at timestamptz,
    queued_jobs bigint, location text, running_job bigint, next_job bigint, queue_position int, waiting_reason text,
    job_progress text, job_eta text, suggested_schedule text, load_gate text, last_load text, would_defer_7d bigint
) LANGUAGE plpgsql STABLE AS $$
DECLARE cfg pgbx.config; lb pgbx.history; le pgbx.history; lv pgbx.history; last_auto timestamptz;
BEGIN
    SELECT * INTO cfg FROM pgbx.config;
    IF NOT FOUND THEN  -- worker hasn't visited this database yet
        cfg := ROW(1, NULL, '0 2 * * *', 'daily at 02:00', 14, 90, '0 4 * * 0', 'weekly on sunday at 04:00',
                   true, NULL, NULL, now(), NULL, NULL, NULL, NULL)::pgbx.config;
    END IF;
    SELECT * INTO lb FROM pgbx.history WHERE kind='backup' AND history.state='done' ORDER BY id DESC LIMIT 1;
    -- the admin database's server-wide PITR rows (base_backup, wal_gap, wal_archive) are not this database's jobs
    SELECT * INTO le FROM pgbx.history WHERE history.state='failed'
       AND history.kind NOT IN ('base_backup', 'wal_gap', 'wal_archive') ORDER BY id DESC LIMIT 1;
    SELECT * INTO lv FROM pgbx.history WHERE kind='verify' AND history.state IN ('done','failed') ORDER BY id DESC LIMIT 1;
    SELECT max(requested_at) INTO last_auto FROM pgbx.history WHERE kind='backup' AND trigger IN ('schedule','first');
    RETURN QUERY SELECT
        current_database()::name,
        CASE WHEN NOT cfg.enabled THEN 'paused'
             WHEN EXISTS (SELECT 1 FROM pgbx.history h WHERE h.state='running'
                          AND h.kind NOT IN ('base_backup', 'wal_gap', 'wal_archive')) THEN 'running'
             WHEN lb.id IS NULL THEN 'waiting for first backup'
             WHEN le.id > lb.id THEN 'failing'
             ELSE 'active' END,
        cfg.schedule_label, cfg.schedule,
        CASE WHEN NOT cfg.enabled THEN NULL
             WHEN last_auto IS NULL THEN now()
             ELSE to_timestamp(pgbx.next_run_epoch(cfg.schedule, extract(epoch FROM last_auto))) END,
        lb.finished, now() - lb.finished, pg_size_pretty(lb.bytes), lb.s3_key,
        (SELECT count(*) FROM pgbx.history h WHERE h.kind='backup' AND h.state='done'),
        format('max %s backups, max %s days', cfg.max_backups, cfg.max_days) || coalesce(', gfs ' || cfg.gfs, ''),
        CASE WHEN coalesce(cardinality(cfg.include_data), 0) = 0 AND coalesce(cardinality(cfg.exclude_data), 0) = 0
             THEN 'all tables, all rows'
             ELSE 'all tables; rows of ' ||
                  CASE WHEN coalesce(cardinality(cfg.include_data), 0) > 0 THEN 'only ' || array_to_string(cfg.include_data, ', ')
                       ELSE 'all tables' END ||
                  CASE WHEN coalesce(cardinality(cfg.exclude_data), 0) > 0 THEN ' except ' || array_to_string(cfg.exclude_data, ', ')
                       ELSE '' END ||
                  format(' (%s tables backed up without rows)', (SELECT count(*) FROM pgbx.rowless_tables()))
        END,
        coalesce(cfg.verify_label, 'never'), lv.finished,
        CASE WHEN lv.id IS NULL THEN NULL WHEN lv.state = 'done' THEN 'ok: ' || coalesce(lv.params->>'checked', '') ELSE 'FAILED: ' || lv.error END,
        le.error, le.finished, cfg.paused_reason, cfg.paused_at,
        (SELECT count(*) FROM pgbx.history h WHERE h.state='queued'),
        format('s3://%s/%s/%s/', current_setting('pgbx.s3_bucket', true),
               coalesce(nullif(current_setting('pgbx.server_name', true), ''), '<hostname>'),
               coalesce(cfg.path, current_database())),
        (SELECT max(h.id) FROM pgbx.history h WHERE h.state='running' AND h.kind NOT IN ('wal_gap', 'wal_archive')),
        nq.id, (nq.params->>'queue_position')::int,
        coalesce(nq.params->>'wait_reason', CASE WHEN nq.id IS NOT NULL THEN 'queued; the worker picks it up within pgbx.poll_seconds' END),
        (SELECT j.progress FROM pgbx.history h, pgbx.job_eta(h.id) j WHERE h.state='running' AND h.kind NOT IN ('wal_gap', 'wal_archive')
          ORDER BY h.id DESC LIMIT 1),
        CASE WHEN nq.id IS NOT NULL THEN (SELECT j.progress FROM pgbx.job_eta(nq.id) j)
             WHEN cfg.enabled THEN format('next backup %s, takes ~%s',
                 CASE WHEN last_auto IS NULL THEN 'within a minute (first backup)'
                      ELSE 'at ' || to_char(to_timestamp(pgbx.next_run_epoch(cfg.schedule, extract(epoch FROM last_auto))), 'YYYY-MM-DD HH24:MI') END,
                 (SELECT pgbx._dur(e.est_secs) FROM pgbx._estimate('backup') e)) END,
        (SELECT CASE WHEN w.cron IS NULL THEN w.start_at
                     ELSE format('%s (%s, %sx average activity vs %sx now; %s confidence) — never applied by itself: %s',
                                 w.cron, w.start_at, w.score, w.current_score, w.confidence, w.apply_sql) END
           FROM pgbx.suggest_window() w),
        coalesce(cfg.load_gate, current_setting('pgbx.load_gate', true), 'shadow'),
        (SELECT format('%s at %s', CASE WHEN c.load_busy THEN 'busy: ' || c.load_reasons ELSE 'quiet' END,
                       to_char(c.load_at AT TIME ZONE 'UTC', 'HH24:MI:SS "UTC"'))
           FROM pgbx.server_capacity c WHERE c.load_at IS NOT NULL),
        (SELECT count(*) FROM pgbx.history h WHERE h.params ? 'would_defer' AND h.requested_at > now() - interval '7 days')
    FROM (SELECT NULL::bigint AS id, NULL::jsonb AS params
          UNION ALL (SELECT h.id, h.params FROM pgbx.history h WHERE h.state='queued'
                     ORDER BY (h.params->>'queue_position')::int NULLS LAST, h.id LIMIT 1)
          ORDER BY id NULLS LAST LIMIT 1) nq;
END $$;

-- internal: queue a manual job, or (pgbx.coalesce_manual, default on) return the one of that kind already queued
-- in this database; either way a NOTICE says when it starts and how long it takes
CREATE FUNCTION pgbx._queue_manual(k text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE j bigint;
BEGIN
    IF coalesce(current_setting('pgbx.coalesce_manual', true), 'on') <> 'off' THEN
        PERFORM pg_advisory_xact_lock(hashtext('pgbx_coalesce'), hashtext(k));
        SELECT id INTO j FROM pgbx.history WHERE kind = k AND state = 'queued' ORDER BY id LIMIT 1;
        IF j IS NOT NULL THEN
            UPDATE pgbx.history SET params = params || jsonb_build_object('manual', true,
                       'coalesced', coalesce((params->>'coalesced')::int, 0) + 1)
             WHERE id = j;
            RAISE NOTICE 'pgbx: % job % is already queued; returning it instead of adding another', k, j;
            PERFORM pgbx._notice_eta(j);
            RETURN j;
        END IF;
    END IF;
    INSERT INTO pgbx.history (kind, trigger) VALUES (k, 'manual') RETURNING id INTO j;
    PERFORM pgbx._notice_eta(j);
    RETURN j;
END $$;
-- internal: a duration for people ('45 s', '18 min', '2.5 h')
CREATE FUNCTION pgbx._dur(secs float8) RETURNS text LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    RETURN CASE WHEN secs IS NULL THEN '?' WHEN secs < 90 THEN round(secs) || ' s' WHEN secs < 5400 THEN round(secs / 60) || ' min'
                WHEN secs < 172800 THEN round((secs / 3600)::numeric, 1) || ' h' ELSE round((secs / 86400)::numeric, 1) || ' d' END;
END $$;

-- internal: how big and how long a job of kind k in this database will be (ADR 0001 §4). Speed: this database's
-- last pgbx.eta_samples jobs of that kind (>= 3: high/medium confidence), else the server's recent jobs (medium), else
-- the slowest measured pipe (cpu / disk / network, low), else pgbx.eta_default_mbps (low). A bandwidth cap and a busy
-- server (load_factor) always apply.
CREATE FUNCTION pgbx._estimate(k text, OUT est_bytes bigint, OUT est_secs float8, OUT speed_bps float8, OUT confidence text,
                               OUT basis text)
LANGUAGE plpgsql STABLE AS $$
DECLARE cap pgbx.server_capacity; n int := greatest(1, least(50, coalesce(nullif(current_setting('pgbx.eta_samples', true), '')::int, 5)));
        ratio float8; speeds float8[]; s float8; why text; capkb int; srv float8;
BEGIN
    SELECT * INTO cap FROM pgbx.server_capacity;
    -- a backup is the database size times this database's compression ratio (0.3 until known); a restore, its dump
    SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x.r) INTO ratio
      FROM (SELECT h.bytes::float8 / (h.params->>'db_size')::float8 AS r FROM pgbx.history h
             WHERE h.kind = 'backup' AND h.state IN ('done', 'expired') AND h.bytes > 0 AND (h.params->>'db_size')::bigint > 0
             ORDER BY h.id DESC LIMIT n) x;
    IF k = 'backup' THEN
        est_bytes := (pg_database_size(current_database()) * coalesce(ratio, 0.3))::bigint;
    ELSIF k IN ('restore', 'verify') THEN
        SELECT h.bytes INTO est_bytes FROM pgbx.history h WHERE h.kind = 'backup' AND h.state = 'done' AND h.bytes > 0
         ORDER BY h.id DESC LIMIT 1;
    END IF;
    est_bytes := coalesce(est_bytes, 0);
    SELECT array_agg(x.b / x.secs) INTO speeds
      FROM (SELECT h.bytes::float8 AS b, extract(epoch FROM h.finished - h.started)::float8 AS secs FROM pgbx.history h
             WHERE h.kind = k AND h.state IN ('done', 'expired') AND h.bytes >= 1048576 AND h.finished > h.started
             ORDER BY h.id DESC LIMIT n) x;   -- a job under 1 MiB is mostly fixed overhead, not speed
    srv := CASE k WHEN 'backup' THEN cap.backup_bps WHEN 'restore' THEN cap.restore_bps WHEN 'verify' THEN cap.verify_bps END;
    IF coalesce(cardinality(speeds), 0) >= least(3, n) THEN
        SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v) INTO s FROM unnest(speeds) v;
        confidence := CASE WHEN (SELECT max(v) / nullif(min(v), 0) FROM unnest(speeds) v) <= 2 THEN 'high' ELSE 'medium' END;
        basis := format('from the last %s %s jobs of this database', cardinality(speeds), k);
    ELSIF srv > 0 THEN
        s := srv; confidence := 'medium'; basis := format('from recent %s jobs on this server', k);
    ELSE
        -- the slowest of the pipes a dump goes through; raw read / compression rates shrink by the ratio
        SELECT p.pipe, p.rate INTO why, s FROM (VALUES
            ('cpu', CASE WHEN k = 'backup' THEN cap.cpu_bps * coalesce(ratio, 0.3) END),
            ('disk', CASE WHEN k = 'backup' THEN cap.disk_bps * coalesce(ratio, 0.3) END),
            ('upload', CASE WHEN k = 'backup' THEN cap.upload_bps END),
            ('download', CASE WHEN k IN ('restore', 'verify') THEN cap.download_bps END)) p(pipe, rate)
         WHERE p.rate > 0 ORDER BY p.rate LIMIT 1;
        IF s IS NULL THEN
            s := 1e6 * greatest(1, coalesce(nullif(current_setting('pgbx.eta_default_mbps', true), '')::int, 20));
            basis := 'nothing measured yet: pgbx.eta_default_mbps';
        ELSE
            basis := format('limited by %s (%s/s)', why, pg_size_pretty(s::bigint));
        END IF;
        confidence := 'low';
    END IF;
    capkb := coalesce(nullif(current_setting(CASE WHEN k = 'backup' THEN 'pgbx.upload_kbps' ELSE 'pgbx.download_kbps' END, true), '')::int, 0);
    IF capkb > 0 AND capkb * 1024.0 < s THEN
        s := capkb * 1024.0;
        basis := format('limited by %s (%s/s)', CASE WHEN k = 'backup' THEN 'pgbx.upload_kbps' ELSE 'pgbx.download_kbps' END,
                        pg_size_pretty(s::bigint));
    END IF;
    IF coalesce(cap.load_factor, 1) > 1.05 THEN
        s := s / cap.load_factor;
        basis := basis || format(', slower: server busy (x%s)', round(cap.load_factor::numeric, 1));
    END IF;
    speed_bps := s;
    est_secs := greatest(1, est_bytes / s);
END $$;

-- When will job job_id start and finish, and how far is it? A running job's estimate comes from its real throughput
-- (the worker records bytes after every 16 MiB); a queued one from its place in the server-wide queue.
CREATE FUNCTION pgbx.job_eta(job_id bigint) RETURNS TABLE (queue_position int, eta_start timestamptz, eta_finish timestamptz,
    est_bytes bigint, done_bytes bigint, confidence text, progress text)
LANGUAGE plpgsql STABLE AS $$
DECLARE h pgbx.history; e record; left_s float8; el float8;
BEGIN
    SELECT * INTO h FROM pgbx.history x WHERE x.id = job_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'pgbx: no job % in database %', job_id, current_database();
    ELSIF h.state NOT IN ('queued', 'running') THEN
        RETURN QUERY SELECT NULL::int, h.started, h.finished, h.bytes, h.bytes, NULL::text, h.state;
        RETURN;
    END IF;
    SELECT * INTO e FROM pgbx._estimate(h.kind);
    IF h.state = 'running' THEN
        est_bytes := coalesce((h.params->>'est_bytes')::bigint, e.est_bytes);
        done_bytes := coalesce(h.bytes, 0);
        el := extract(epoch FROM now() - h.started);
        IF done_bytes > 0 AND el > 5 THEN
            left_s := greatest(est_bytes - done_bytes, 0) / (done_bytes / el); -- the job's own throughput so far
            confidence := 'measured';
        ELSE
            left_s := greatest(coalesce((h.params->>'eta_sec')::float8, e.est_secs) - el, 0);
            confidence := e.confidence;
        END IF;
        queue_position := 0; eta_start := h.started; eta_finish := now() + make_interval(secs => left_s);
        progress := format('%s %% · ~%s left', least(99, floor(100.0 * done_bytes / greatest(est_bytes, 1)))::int, pgbx._dur(left_s));
    ELSE
        queue_position := (h.params->>'queue_position')::int;
        eta_start := greatest(now(), coalesce((h.params->>'eta_start')::timestamptz, now()));
        est_bytes := e.est_bytes; done_bytes := 0; confidence := e.confidence;
        eta_finish := eta_start + make_interval(secs => e.est_secs);
        progress := format('queued%s: starts ~%s, takes ~%s', coalesce(', #' || queue_position || ' in line', ''),
                           CASE WHEN eta_start < now() + interval '1 minute' THEN 'now' ELSE to_char(eta_start, 'HH24:MI') END,
                           pgbx._dur(e.est_secs));
    END IF;
    RETURN NEXT;
END $$;

-- internal (worker): fold one hour of activity into its bucket
CREATE FUNCTION pgbx._activity_add(sc text, hour_start timestamptz, x float8, w float8, r float8, act float8, decay float8)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO pgbx.activity_hourly AS a (scope, dow, hour, samples, xacts, writes, reads, active_max, updated_at)
    VALUES (sc, extract(dow FROM hour_start AT TIME ZONE 'UTC'), extract(hour FROM hour_start AT TIME ZONE 'UTC'), 1, x, w, r, act, now())
    ON CONFLICT (scope, dow, hour) DO UPDATE SET
        samples    = a.samples + 1,
        xacts      = _activity_add.decay * a.xacts + (1 - _activity_add.decay) * excluded.xacts,
        writes     = _activity_add.decay * a.writes + (1 - _activity_add.decay) * excluded.writes,
        reads      = _activity_add.decay * a.reads + (1 - _activity_add.decay) * excluded.reads,
        active_max = _activity_add.decay * a.active_max + (1 - _activity_add.decay) * excluded.active_max,
        updated_at = now();
END $$;

-- The quietest time to back up this database, learned from activity (server-wide when known, else this database's).
-- Never applied by itself: apply_sql is the configure() call to run, or `pgbx schedule suggest --apply`.
-- score / current_score: activity of the window relative to an average hour (1.0); the window is at least `hours` long and
-- long enough for the estimated dump; hours another database's backup starts in are avoided. Weekly when weekdays
-- differ by more than 2x, else daily. confidence: 'none' (no samples), 'low' (< pgbx.suggest_min_days days), 'high'.
CREATE FUNCTION pgbx.suggest_window(hours int DEFAULT 1) RETURNS TABLE (start_at text, cron text, score float8, confidence text,
    current_schedule text, current_score float8, window_hours int, est_duration text, days_sampled float8, apply_sql text)
LANGUAGE plpgsql STABLE AS $$
DECLARE
    sc text; mx float8; mw float8; ma float8; tot bigint; k int; r record; s float8[]; p float8[] := array_fill(0::float8, ARRAY[24]);
    dayt float8[] := array_fill(0::float8, ARRAY[7]); blocked bool[] := array_fill(false, ARRAY[168]); weekly bool;
    best float8; bestpen bool; pen bool; bi int; v float8; i int; j int; cfg pgbx.config; slots jsonb; e record; nxt timestamptz; cur int;
    mind int := greatest(1, coalesce(nullif(current_setting('pgbx.suggest_min_days', true), '')::int, 7));
    days text[] := ARRAY['sunday', 'monday', 'tuesday', 'wednesday', 'thursday', 'friday', 'saturday'];
BEGIN
    SELECT * INTO cfg FROM pgbx.config;
    current_schedule := coalesce(cfg.schedule_label, 'daily at 02:00');
    sc := CASE WHEN EXISTS (SELECT 1 FROM pgbx.activity_hourly a WHERE a.scope = 'server' AND a.samples > 0) THEN 'server' ELSE 'db' END;
    SELECT avg(a.xacts), avg(a.writes), avg(a.active_max), coalesce(sum(a.samples), 0) INTO mx, mw, ma, tot
      FROM pgbx.activity_hourly a WHERE a.scope = sc;
    days_sampled := round((tot / 24.0)::numeric, 1);
    SELECT * INTO e FROM pgbx._estimate('backup');
    window_hours := least(12, greatest(1, coalesce(hours, 1), ceil(e.est_secs / 3600.0)::int));
    est_duration := pgbx._dur(e.est_secs);
    IF tot = 0 THEN
        confidence := 'none'; start_at := 'no activity samples yet (the worker learns them hour by hour)';
        RETURN NEXT;
        RETURN;
    END IF;
    -- each signal relative to its weekly mean, summed; an hour not sampled yet counts as average
    k := (mx > 0)::int + (mw > 0)::int + (ma > 0)::int;
    s := array_fill(k::float8, ARRAY[168]);
    FOR r IN SELECT * FROM pgbx.activity_hourly a WHERE a.scope = sc AND a.samples > 0 LOOP
        s[r.dow * 24 + r.hour + 1] := coalesce(r.xacts / nullif(mx, 0), 0) + coalesce(r.writes / nullif(mw, 0), 0)
                                      + coalesce(r.active_max / nullif(ma, 0), 0);
    END LOOP;
    FOR i IN 0..167 LOOP
        p[i % 24 + 1] := p[i % 24 + 1] + s[i + 1] / 7;
        dayt[i / 24 + 1] := dayt[i / 24 + 1] + s[i + 1];
    END LOOP;
    weekly := (SELECT min(d) FROM unnest(dayt) d) > 0 AND (SELECT max(d) / min(d) FROM unnest(dayt) d) > 2;
    -- hours other databases' backups start in (published by the worker)
    SELECT c.backup_slots INTO slots FROM pgbx.server_capacity c;
    FOR r IN SELECT x.key, x.value FROM jsonb_each(coalesce(slots, '{}')) x WHERE x.key <> current_database() LOOP
        FOR i IN SELECT jsonb_array_elements_text(r.value)::int LOOP
            blocked[i + 1] := true;
        END LOOP;
    END LOOP;
    best := NULL;
    FOR i IN 0..(CASE WHEN weekly THEN 167 ELSE 23 END) LOOP
        v := 0; pen := false;
        FOR j IN 0..window_hours - 1 LOOP
            v := v + CASE WHEN weekly THEN s[(i + j) % 168 + 1] ELSE p[(i + j) % 24 + 1] END;
            pen := pen OR (weekly AND blocked[(i + j) % 168 + 1])
                   OR (NOT weekly AND (SELECT bool_or(blocked[d * 24 + (i + j) % 24 + 1]) FROM generate_series(0, 6) d));
        END LOOP;
        -- a window another database's backup starts in only wins when every window is taken
        IF best IS NULL OR (bestpen AND NOT pen) OR (pen = bestpen AND v < best) THEN
            best := v; bestpen := pen; bi := i;
        END IF;
    END LOOP;
    score := round((best / (window_hours * greatest(k, 1)))::numeric, 3);
    IF weekly THEN
        cron := format('0 %s * * %s', bi % 24, bi / 24);
        start_at := format('%s %s:00 UTC', days[bi / 24 + 1], lpad((bi % 24)::text, 2, '0'));
    ELSE
        cron := format('0 %s * * *', bi);
        start_at := format('daily %s:00 UTC', lpad(bi::text, 2, '0'));
    END IF;
    -- the current schedule's next slot, scored the same way
    BEGIN
        nxt := to_timestamp(pgbx.next_run_epoch(coalesce(cfg.schedule, '0 2 * * *'), extract(epoch FROM now())));
        cur := extract(dow FROM nxt AT TIME ZONE 'UTC')::int * 24 + extract(hour FROM nxt AT TIME ZONE 'UTC')::int;
        v := 0;
        FOR j IN 0..window_hours - 1 LOOP
            v := v + CASE WHEN weekly THEN s[(cur + j) % 168 + 1] ELSE p[(cur % 24 + j) % 24 + 1] END;
        END LOOP;
        current_score := round((v / (window_hours * greatest(k, 1)))::numeric, 3);
    EXCEPTION WHEN others THEN
        current_score := NULL;
    END;
    confidence := CASE WHEN tot / 24.0 >= mind THEN 'high' ELSE 'low' END;
    apply_sql := format('SELECT pgbx.configure(schedule => %L);', cron);
    RETURN NEXT;
END $$;

-- internal: tell whoever queued job j when it starts and how long it takes, and what limits it
CREATE FUNCTION pgbx._notice_eta(j bigint) RETURNS void LANGUAGE plpgsql AS $$
DECLARE h pgbx.history; e record; cap pgbx.server_capacity; ahead int; starts timestamptz; act int; gate text; manual text;
BEGIN
    SELECT * INTO h FROM pgbx.history x WHERE x.id = j;
    SELECT * INTO e FROM pgbx._estimate(h.kind);
    SELECT * INTO cap FROM pgbx.server_capacity;
    ahead := coalesce(cap.queue_jobs, 0);
    starts := now() + make_interval(secs => coalesce(cap.wait_secs, 0));
    RAISE NOTICE '%', format('pgbx: %s job %s queued%s, starts ~%s, takes ~%s (%s, %s confidence, %s)', h.kind, j,
        CASE WHEN ahead > 0 THEN format(' (%s job(s) running or waiting on this server)', ahead) ELSE '' END,
        CASE WHEN starts < now() + interval '1 minute' THEN 'now' ELSE to_char(starts, 'HH24:MI') END,
        pgbx._dur(e.est_secs), pg_size_pretty(e.est_bytes), e.confidence, e.basis);
    -- the load gate on human jobs (pgbx.gate_manual_jobs): say it competes with the app, right now
    gate := coalesce((SELECT c.load_gate FROM pgbx.config c), current_setting('pgbx.load_gate', true), 'shadow');
    manual := coalesce(current_setting('pgbx.gate_manual_jobs', true), 'warn');
    SELECT count(*) INTO act FROM pg_stat_activity a WHERE a.state <> 'idle' AND a.backend_type = 'client backend'
       AND a.pid <> pg_backend_pid() AND coalesce(a.application_name, '') NOT LIKE 'pgbx%';
    IF gate <> 'off' AND manual <> 'off' AND h.kind IN ('backup', 'verify', 'restore')
       AND (coalesce(cap.load_busy, false)
            OR act > nullif(coalesce(nullif(current_setting('pgbx.busy_active_backends', true), '')::int, 4), 0)) THEN
        RAISE NOTICE '%', format('pgbx: the server is busy (%s active session(s) now%s): this %s will compete with the app; %s',
            act, coalesce(', last sample: ' || cap.load_reasons, ''), h.kind,
            CASE WHEN manual = 'defer' AND gate = 'on' AND h.kind <> 'restore'
                 THEN 'it waits for a quieter moment, at most pgbx.max_defer'
                 ELSE 'it starts anyway (pgbx.gate_manual_jobs = warn)' END);
    END IF;
END $$;


-- Cancel a job of this database: a queued one never starts; a running one is stopped by the worker within a few
-- seconds (its child is killed, a half-done upload is aborted so nothing is left in S3, a half-restored database is
-- dropped). Either way it ends as 'cancelled'.
CREATE FUNCTION pgbx.cancel(job_id bigint) RETURNS text LANGUAGE plpgsql AS $$
DECLARE h pgbx.history;
BEGIN
    SELECT * INTO h FROM pgbx.history WHERE id = job_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'pgbx: no job % in database %', job_id, current_database();
    ELSIF h.state = 'queued' THEN
        UPDATE pgbx.history SET state = 'cancelled', finished = now(), error = 'cancelled by ' || session_user WHERE id = job_id;
        RETURN format('%s job %s cancelled before it started', h.kind, job_id);
    ELSIF h.state = 'running' THEN
        UPDATE pgbx.history SET params = params || jsonb_build_object('cancel_requested', now(), 'cancelled_by', session_user)
         WHERE id = job_id;
        RETURN format('%s job %s is running: the worker stops it within a few seconds and it ends as cancelled '
                      '(nothing is left in S3)', h.kind, job_id);
    END IF;
    RAISE EXCEPTION 'pgbx: job % is % already (only a queued or running job can be cancelled)', job_id, h.state;
END $$;

-- Restore test: restore the newest backup into a scratch database, check it, drop it. Like backup_now(), returns
-- the restore test already queued (pgbx.coalesce_manual) instead of adding another.
CREATE FUNCTION pgbx.verify_now() RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
    RETURN pgbx._queue_manual('verify');
END $$;

-- 'weekly on sunday at 04:00', 'daily at 05:00', ... or 'never'.
CREATE FUNCTION pgbx.set_verify_schedule(schedule text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE s text := set_verify_schedule.schedule;
        c text := CASE WHEN lower(trim(set_verify_schedule.schedule)) IN ('never', 'off') THEN NULL
                       ELSE pgbx.to_cron(set_verify_schedule.schedule) END;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET verify_schedule = c, verify_label = CASE WHEN c IS NULL THEN 'never' ELSE s END, updated_at = now();
    PERFORM pgbx._log('config', jsonb_build_object('verify_schedule', s, 'cron', c));
    RETURN CASE WHEN c IS NULL THEN 'restore tests disabled'
           ELSE format('restore test "%s" (cron %s); next at %s', s, c,
                       to_timestamp(pgbx.next_run_epoch(c, extract(epoch FROM now())))) END;
END $$;

-- internal: server-wide functions (overview, doctor) run in the admin database (pgbx.admin_db, default 'postgres')
CREATE FUNCTION pgbx._require_admin_db() RETURNS void LANGUAGE plpgsql AS $$
DECLARE a text := coalesce(nullif(current_setting('pgbx.admin_db', true), ''), 'postgres');
BEGIN
    IF current_database() <> a THEN
        RAISE EXCEPTION 'pgbx: server-wide functions run in database "%" (pgbx.admin_db); connect there', a;
    END IF;
END $$;

-- A time-limited link to download one backup of THIS database (newest if backup_id is NULL) — no bucket keys needed:
--   curl -o shop.dump "$url"   or   curl -s "$url" | pg_restore -d some_db
-- Not open to PUBLIC: GRANT EXECUTE ON FUNCTION pgbx.download_url(bigint, interval) TO <role>;
CREATE FUNCTION pgbx.download_url(backup_id bigint DEFAULT NULL, expires interval DEFAULT '1 hour')
RETURNS text LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pgbx AS $$
DECLARE k text; secs int := extract(epoch FROM expires)::int;
BEGIN
    IF secs < 60 OR secs > 604800 THEN
        RAISE EXCEPTION 'pgbx: expires must be between 1 minute and 7 days';
    END IF;
    SELECT s3_key INTO k FROM pgbx.history
     WHERE kind = 'backup' AND state = 'done' AND s3_key IS NOT NULL AND (backup_id IS NULL OR id = backup_id)
     ORDER BY id DESC LIMIT 1;
    IF k IS NULL THEN
        RAISE EXCEPTION 'pgbx: no such backup in this database%', coalesce(' (id ' || backup_id || ')', '');
    END IF;
    PERFORM pgbx._log('config', jsonb_build_object('download_url', k, 'by', session_user, 'expires', expires));
    RETURN pgbx._presign(k, secs);
END $$;
REVOKE ALL ON FUNCTION pgbx.download_url(bigint, interval) FROM PUBLIC;

-- Every database on this server in one view ("is the whole server backed up?"). Admin database only.
CREATE FUNCTION pgbx.overview() RETURNS TABLE (
    database name, state text, schedule text, last_backup_at timestamptz, last_backup_age interval,
    last_backup_size text, next_backup_at timestamptz, backups_kept bigint, last_verify text, last_error text, seen_at timestamptz
) LANGUAGE plpgsql STABLE AS $$
BEGIN
    PERFORM pgbx._require_admin_db();
    RETURN QUERY SELECT o.database, o.state, o.schedule, o.last_backup_at, now() - o.last_backup_at, o.last_backup_size,
                        o.next_backup_at, o.backups_kept, o.last_verify, o.last_error, o.seen_at
                 FROM pgbx.server_overview o ORDER BY o.database;
END $$;

-- internal: how old a backup on this schedule may get before it counts as overdue: 2x the longest gap
-- between the next few runs + 1 hour. NULL when the schedule cannot be read.
CREATE FUNCTION pgbx._overdue_after(schedule text) RETURNS interval LANGUAGE plpgsql STABLE AS $$
DECLARE c text; t float8; prev float8; gap float8 := 0;
BEGIN
    c := pgbx.to_cron(schedule);
    prev := pgbx.next_run_epoch(c, extract(epoch FROM now())::float8);
    FOR i IN 1..3 LOOP
        t := pgbx.next_run_epoch(c, prev);
        gap := greatest(gap, t - prev);
        prev := t;
    END LOOP;
    RETURN make_interval(secs => 2 * gap) + interval '1 hour';
EXCEPTION WHEN others THEN
    RETURN NULL;
END $$;

-- internal: strip anything that could carry a secret from text shown by doctor()
CREATE FUNCTION pgbx._scrub(t text) RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT regexp_replace(regexp_replace(t,
        '(x-amz-[a-z0-9-]*|[a-z0-9_-]*(key|secret)[a-z0-9_-]*)\s*[=:]\s*\S+', '\1=<redacted>', 'gi'),
        '[A-Za-z0-9/+]{32,}', '<redacted>', 'g')
$$;

-- internal: the checks behind doctor(); nothing here quotes the credentials file
CREATE FUNCTION pgbx._doctor() RETURNS TABLE (name text, ok bool, detail text, fix text)
LANGUAGE plpgsql STABLE AS $$
DECLARE
    o record; v text; lim interval; n int; bad text; worst_ratio float8 := -1; su bool; p record;
BEGIN
    PERFORM pgbx._require_admin_db();
    su := coalesce((SELECT r.rolsuper FROM pg_roles r WHERE r.rolname = session_user), false);

    v := current_setting('shared_preload_libraries', true);
    name := 'extension loaded'; ok := v ~ '(^|[ ,])pgbx($|[ ,])';
    detail := CASE WHEN ok THEN 'pgbx is in shared_preload_libraries' ELSE 'pgbx is not in shared_preload_libraries, so no background worker runs' END;
    fix := CASE WHEN ok THEN NULL ELSE 'add pgbx to shared_preload_libraries in postgresql.conf and restart Postgres' END;
    RETURN NEXT;

    name := 's3 settings';
    v := concat_ws(', ', CASE WHEN coalesce(current_setting('pgbx.s3_endpoint', true), '') = '' THEN 'pgbx.s3_endpoint' END,
                         CASE WHEN coalesce(current_setting('pgbx.s3_bucket', true), '') = '' THEN 'pgbx.s3_bucket' END);
    ok := v = '';
    detail := CASE WHEN ok THEN format('bucket %s at %s', current_setting('pgbx.s3_bucket', true), current_setting('pgbx.s3_endpoint', true)) ELSE 'not set: ' || v END;
    fix := CASE WHEN ok THEN NULL ELSE 'set ' || v || ' in postgresql.conf, then SELECT pg_reload_conf()' END;
    RETURN NEXT;

    -- a keys file: judged from what the worker reported (last_error per database); the file itself is never read here.
    -- '' / 'aws-default': asked now (pgbx._s3_credentials: which source answers, never a key or token)
    name := 's3 credentials';
    IF coalesce(current_setting('pgbx.credentials_file', true), '') IN ('', 'aws-default') THEN
        SELECT c.ok, c.detail INTO ok, detail FROM pgbx._s3_credentials() c;
        fix := CASE WHEN ok THEN NULL ELSE 'on EC2: attach an instance profile whose role allows s3:PutObject, s3:GetObject, s3:ListBucket, '
            || 's3:DeleteObject, s3:AbortMultipartUpload and s3:ListMultipartUploadParts on the bucket (IMDSv2; hop limit 2 when '
            || 'Postgres runs in a container); or set pgbx.credentials_file to a keys file (access_key_id and secret_access_key lines)' END;
    ELSE
        ok := NOT EXISTS (SELECT 1 FROM pgbx.server_overview s
                          WHERE s.last_error ~* '(credentials_file|access_key_id missing|secret_access_key missing|read [^ ]*credentials)');
        detail := 'source: file; '
                  || CASE WHEN su THEN 'pgbx.credentials_file = ' || current_setting('pgbx.credentials_file', true) ELSE 'pgbx.credentials_file is set' END
                  || CASE WHEN ok THEN '; the worker reported no problem reading it' ELSE '; the worker cannot read it or it lacks access_key_id= / secret_access_key= lines' END;
        fix := CASE WHEN ok THEN NULL ELSE 'make the file readable by the postgres OS user (chmod 600, chown postgres) with access_key_id= and secret_access_key= lines' END;
    END IF;
    RETURN NEXT;

    name := 'database backups';
    SELECT count(*) INTO n FROM pgbx.server_overview;
    bad := NULL; ok := true; detail := NULL;
    FOR o IN SELECT s.database, s.schedule, s.last_backup_at FROM pgbx.server_overview s
             WHERE coalesce(s.state, '') NOT ILIKE 'paused%' ORDER BY s.database LOOP
        lim := pgbx._overdue_after(o.schedule);
        IF o.last_backup_at IS NULL THEN
            ok := false; bad := o.database; detail := format('database %s has no backup yet', o.database); worst_ratio := 1e9;
        ELSIF lim IS NOT NULL AND extract(epoch FROM now() - o.last_backup_at) / extract(epoch FROM lim) > greatest(worst_ratio, 1) THEN
            ok := false; bad := o.database; worst_ratio := extract(epoch FROM now() - o.last_backup_at) / extract(epoch FROM lim);
            detail := format('database %s: newest backup is %s old, overdue after %s (schedule: %s)',
                             o.database, date_trunc('second', now() - o.last_backup_at), lim, o.schedule);
        END IF;
    END LOOP;
    IF n = 0 THEN
        ok := false; detail := 'no database has reported in yet'; fix := 'wait for the scheduler worker (see the "workers" check)';
    ELSIF ok THEN
        detail := format('all %s databases have a backup within their schedule', n); fix := NULL;
    ELSE
        fix := format('connect to %s, check SELECT * FROM pgbx.status(), then SELECT pgbx.backup_now()', bad);
    END IF;
    RETURN NEXT;

    name := 'restore tests';
    SELECT string_agg(s.database || ': ' || s.last_verify, '; ' ORDER BY s.database) INTO v
      FROM pgbx.server_overview s WHERE s.last_verify LIKE 'FAILED%';
    ok := v IS NULL;
    detail := CASE WHEN ok THEN format('no failed restore test (%s of %s databases tested so far)',
                                       (SELECT count(*) FROM pgbx.server_overview s WHERE s.last_verify IS NOT NULL),
                                       (SELECT count(*) FROM pgbx.server_overview))
                   ELSE 'last restore test failed for ' || v END;
    fix := CASE WHEN ok THEN NULL ELSE 'connect to that database, check SELECT * FROM pgbx.status(), then SELECT pgbx.verify_now()' END;
    RETURN NEXT;

    -- the scheduler talks to Postgres through ordinary connections (application_name 'pgbx'), so it is
    -- judged by its heartbeat: it stamps server_overview.seen_at every poll
    name := 'workers';
    ok := EXISTS (SELECT 1 FROM pg_stat_activity s WHERE s.backend_type = 'pgbx scheduler')
          OR coalesce((SELECT max(s.seen_at) FROM pgbx.server_overview s) > now() - interval '10 minutes', false);
    detail := CASE WHEN ok THEN 'the scheduler worker is alive (recent heartbeat)' ELSE 'no recent heartbeat from: pgbx scheduler' END;
    fix := CASE WHEN ok THEN NULL ELSE 'make sure pgbx is in shared_preload_libraries and restart Postgres; a crashed worker restarts within 10 s — check the Postgres log' END;
    RETURN NEXT;

    -- point-in-time restore is optional (pgbx.pitr); per-database dumps never need WAL archiving
    IF coalesce(current_setting('pgbx.pitr', true), 'off') NOT IN ('on', 'true', '1', 'yes') THEN
        name := 'archive_mode';
        v := coalesce(current_setting('archive_command', true), '');
        ok := true;
        detail := 'archive_mode = ' || current_setting('archive_mode') || '; point-in-time restore is off (pgbx.pitr)' ||
                  CASE WHEN current_setting('archive_mode') <> 'off' AND v IN ('', '(disabled)')
                       THEN ' and archive_mode=on with no archive_command is not needed for pgbx'
                       ELSE '' END;
        fix := CASE WHEN current_setting('archive_mode') <> 'off' AND v IN ('', '(disabled)')
                    THEN 'optional: if nothing else uses WAL archiving, set archive_mode = off at the next planned restart' END;
        RETURN NEXT;
    ELSE
        SELECT * INTO p FROM pgbx.pitr_status();
        name := 'pitr archiving';
        v := coalesce(current_setting('archive_command', true), '');
        ok := current_setting('archive_mode') <> 'off' AND v ~ ' wal-push %p' AND NOT p.state LIKE 'archiving failing%';
        detail := CASE
            WHEN current_setting('archive_mode') = 'off' THEN 'pgbx.pitr is on but archive_mode is off: no WAL is archived'
            WHEN v !~ ' wal-push %p' THEN 'archive_command is not pgbx wal-push: ' || CASE WHEN su THEN v ELSE '(set)' END
            WHEN p.state LIKE 'archiving failing%' THEN coalesce(p.last_error, 'WAL archiving is failing')
            ELSE format('WAL archived until %s; %s file(s) waiting (%s)', p.wal_archived_until, p.backlog_segments, pg_size_pretty(p.backlog_bytes)) END;
        fix := CASE
            WHEN current_setting('archive_mode') = 'off' THEN 'run pgbx setup pitr --yes, then restart Postgres once'
            WHEN v !~ ' wal-push %p' THEN 'run pgbx setup pitr --yes (it refuses to replace an archive_command another tool set)'
            WHEN NOT ok THEN 'check S3 reachability and credentials; the Postgres log shows the pgbx wal-push error' END;
        RETURN NEXT;

        name := 'pitr base backups';
        lim := pgbx._overdue_after(coalesce(nullif(current_setting('pgbx.pitr_schedule', true), ''), 'daily at 01:00'));
        ok := p.last_base_backup_at IS NOT NULL AND (lim IS NULL OR now() - p.last_base_backup_at <= lim);
        detail := CASE WHEN p.last_base_backup_at IS NULL THEN 'no base backup yet, so no point-in-time restore is possible yet'
                       ELSE format('newest base backup %s (%s ago); restorable from %s; %s kept', p.last_base_backup,
                                   date_trunc('second', now() - p.last_base_backup_at), p.restorable_from, p.base_backups) END;
        fix := CASE WHEN ok THEN NULL ELSE 'SELECT pgbx.pitr_backup_now(); then SELECT * FROM pgbx.pitr_status() (see last_error)' END;
        RETURN NEXT;

        name := 'pitr gaps';
        ok := p.open_gaps = 0;
        detail := CASE WHEN ok THEN coalesce('no open WAL gap; earlier: ' || p.gaps, 'no WAL gap recorded')
                       ELSE 'WAL was dropped (pgbx.wal_queue_max): no restore to ' || p.gaps END;
        fix := CASE WHEN ok THEN NULL ELSE 'fix archiving (see pitr archiving); a base backup is queued automatically once WAL reaches S3 again and closes the gap' END;
        RETURN NEXT;
    END IF;

    name := 'replication_slots';
    SELECT string_agg(format('%s (%s, %s, holds %s)', r.slot_name, r.slot_type, coalesce(r.wal_status, '?'),
                             coalesce(pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), r.restart_lsn)), '?')), ', '),
           count(*)
      INTO v, n FROM pg_replication_slots r WHERE NOT r.active AND r.restart_lsn IS NOT NULL;
    ok := n = 0;
    detail := CASE WHEN ok THEN 'no inactive replication slot is holding WAL' ELSE 'inactive slot(s) pinning WAL: ' || v END
        || format('; max_slot_wal_keep_size = %s, max_wal_size = %s, wal_keep_size = %s',
                  current_setting('max_slot_wal_keep_size'), current_setting('max_wal_size'), current_setting('wal_keep_size'));
    fix := CASE WHEN ok THEN
                CASE WHEN current_setting('max_slot_wal_keep_size') = '-1' AND EXISTS (SELECT 1 FROM pg_replication_slots)
                     THEN 'consider max_slot_wal_keep_size (e.g. ''10GB'') so a dead consumer cannot fill the disk' END
           ELSE '(destructive, needs human approval: a dropped slot cannot be recreated at the same position and its consumer must be rebuilt) drop the slot only if its consumer is gone for good: SELECT pg_drop_replication_slot(''<name>''); '
                || 'and set max_slot_wal_keep_size (e.g. ''10GB'') so a dead consumer cannot fill the disk' END;
    RETURN NEXT;

    -- a running dump holds ACCESS SHARE on every table it reads until it ends: DDL on them waits for it
    name := 'long_running_job';
    lim := make_interval(secs => coalesce((SELECT s.setting::int FROM pg_settings s WHERE s.name = 'pgbx.doctor_long_job'), 3600));
    SELECT string_agg(format('%s in %s for %s (pid %s)', a.application_name, a.datname,
                             date_trunc('second', now() - a.backend_start), a.pid), ', ' ORDER BY a.backend_start)
      INTO v FROM pg_stat_activity a
     WHERE a.application_name IN ('pgbx_dump', 'pgbx_restore', 'pgbx_verify')
       AND lim > interval '0' AND now() - a.backend_start > lim;
    ok := v IS NULL;
    detail := CASE WHEN lim = interval '0' THEN 'check off (pgbx.doctor_long_job = 0)'
                   WHEN ok THEN format('no backup or restore process running longer than %s', lim)
                   ELSE format('running longer than %s: %s', lim, v) END;
    fix := CASE WHEN ok THEN NULL
                ELSE 'a running dump blocks DDL (ALTER TABLE, migrations) on the tables it reads; move the schedule to a quiet hour, '
                     || 'or skip the rows of big tables with pgbx.set_data_scope()' END;
    RETURN NEXT;

    -- each of the last 3 dumps ran longer than the schedule interval: slots get skipped (or run back to back)
    name := 'dump_longer_than_interval';
    v := NULL; bad := NULL; fix := NULL;
    FOR o IN SELECT s.database, s.interval_secs, s.dump_secs, (SELECT min(d) FROM unnest(s.dump_secs) d) AS shortest,
                    (SELECT max(d) FROM unnest(s.dump_secs) d) AS longest
               FROM pgbx.server_overview s
              WHERE coalesce(s.state, '') NOT ILIKE 'paused%' AND s.interval_secs > 0 AND cardinality(s.dump_secs) >= 3
              ORDER BY s.database LOOP
        CONTINUE WHEN o.shortest <= o.interval_secs;
        v := concat_ws('; ', v, format('%s: last 3 backups took %s; the schedule runs every %s', o.database,
                 (SELECT string_agg(make_interval(secs => round(d))::text, ', ') FROM unnest(o.dump_secs) d),
                 make_interval(secs => round(o.interval_secs))));
        IF bad IS NULL THEN
            bad := o.database;
            SELECT l.label INTO fix FROM (VALUES ('every 15 minutes', 900), ('every 30 minutes', 1800), ('every 1 hour', 3600),
                    ('every 2 hours', 7200), ('every 3 hours', 10800), ('every 4 hours', 14400), ('every 6 hours', 21600),
                    ('every 12 hours', 43200), ('daily', 86400), ('weekly', 604800)) l(label, secs)
             WHERE l.secs >= 2 * o.longest ORDER BY l.secs LIMIT 1;
        END IF;
    END LOOP;
    ok := v IS NULL;
    detail := CASE WHEN ok THEN 'recent backups of every database finish within its schedule interval' ELSE v END;
    fix := CASE WHEN ok THEN NULL
                ELSE format('connect to %s and give it a longer schedule: SELECT pgbx.configure(schedule => %L);',
                            bad, coalesce(fix, 'weekly')) END;
    RETURN NEXT;

    -- informational: what the time estimates are based on (ADR 0001 §4)
    name := 'capacity';
    ok := true; fix := NULL;
    SELECT format('one core compresses %s/s (%s)%s; disk reads %s; upload %s, download %s; load x%s; %s core(s)',
                  coalesce(pg_size_pretty(c.cpu_bps::bigint), '?'), coalesce(c.cpu_codec, 'not measured'),
                  coalesce(', measured ' || date_trunc('minute', c.measured_at)::text, ''),
                  coalesce(pg_size_pretty(c.disk_bps::bigint) || '/s', 'unknown (track_io_timing off)'),
                  coalesce(pg_size_pretty(c.upload_bps::bigint) || '/s', 'not seen yet'),
                  coalesce(pg_size_pretty(c.download_bps::bigint) || '/s', 'not seen yet'),
                  round(coalesce(c.load_factor, 1)::numeric, 1), coalesce(c.cores::text, '?'))
      INTO detail FROM pgbx.server_capacity c;
    detail := coalesce(detail, 'not measured yet (the worker measures it once a day, pgbx.eta_calibrate)');
    RETURN NEXT;

    -- how good the time estimates were: median |actual - estimate| / actual over each database's recent jobs
    name := 'eta_accuracy';
    SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY s.eta_error), count(*) INTO worst_ratio, n
      FROM pgbx.server_overview s WHERE s.eta_error IS NOT NULL;
    ok := n = 0 OR worst_ratio <= 0.5;
    detail := CASE WHEN n = 0 THEN 'no finished job with an estimate yet'
                   ELSE format('estimates were off by %s %% (median over %s database(s))', round((100 * worst_ratio)::numeric), n) END;
    fix := CASE WHEN ok THEN NULL
                ELSE 'estimates settle after 3 runs of a job; if they stay off, check pgbx.upload_kbps / load, or lower pgbx.eta_samples so they follow recent growth' END;
    RETURN NEXT;

    -- the schedule sits in a busy hour while a much quieter one is known (ADR 0001 §2)
    name := 'schedule_in_quiet_window';
    v := NULL; bad := NULL; fix := NULL;
    FOR o IN SELECT s.database, s.window_cron, s.window_score, s.current_score, s.schedule FROM pgbx.server_overview s
              WHERE coalesce(s.state, '') NOT ILIKE 'paused%' AND s.window_confidence = 'high' AND s.current_score > 1
                AND s.current_score > coalesce(nullif(current_setting('pgbx.doctor_busy_ratio', true), '')::float8, 3) * s.window_score
              ORDER BY s.current_score / greatest(s.window_score, 0.001) DESC LOOP
        v := concat_ws('; ', v, format('%s: "%s" runs at %sx average activity; %s would be %sx', o.database, o.schedule,
                                       o.current_score, o.window_cron, o.window_score));
        IF bad IS NULL THEN
            bad := o.database;
            fix := format('connect to %s and run SELECT pgbx.configure(schedule => %L); (or pgbx schedule suggest --db %s --apply)',
                          o.database, o.window_cron, o.database);
        END IF;
    END LOOP;
    ok := v IS NULL;
    detail := CASE WHEN ok THEN 'no schedule sits in a busy hour while a much quieter one is known (pgbx.suggest_window())' ELSE v END;
    RETURN NEXT;

    -- the load gate (ADR 0001 §1): informational; it never fails a check by itself
    name := 'load_gate';
    ok := true; fix := NULL;
    SELECT format('pgbx.load_gate = %s%s; last sample %s: %s; last 7 days: %s job(s) would have waited (shadow), %s deferred, %s forced',
                  current_setting('pgbx.load_gate', true),
                  coalesce('; on in ' || (SELECT string_agg(s.database, ', ' ORDER BY s.database) FROM pgbx.server_overview s WHERE s.load_gate = 'on'), ''),
                  coalesce(to_char(c.load_at AT TIME ZONE 'UTC', 'HH24:MI:SS "UTC"'), 'none yet'),
                  CASE WHEN c.load_busy THEN 'busy (' || c.load_reasons || ')' WHEN c.load_busy IS NULL THEN '?' ELSE 'quiet' END,
                  (SELECT coalesce(sum(s.would_defer_7d), 0) FROM pgbx.server_overview s),
                  (SELECT coalesce(sum(s.deferred_7d), 0) FROM pgbx.server_overview s),
                  (SELECT coalesce(sum(s.forced_7d), 0) FROM pgbx.server_overview s))
      INTO detail FROM (SELECT 1) one LEFT JOIN pgbx.server_capacity c ON true;
    RETURN NEXT;

    -- backups that kept hitting their max_defer deadline: the schedule sits in a busy window
    name := 'forced_backups_7d';
    SELECT string_agg(format('%s: %s forced', s.database, s.forced_7d), ', ' ORDER BY s.forced_7d DESC), min(s.database)
      INTO v, bad FROM pgbx.server_overview s WHERE s.forced_7d > 2;
    ok := v IS NULL;
    detail := CASE WHEN ok THEN 'no database had more than 2 backups forced past a busy server in 7 days' ELSE v END;
    fix := CASE WHEN ok THEN NULL ELSE format('move the schedule to a quieter hour: pgbx schedule suggest --db %s', bad) END;
    RETURN NEXT;
END $$;

-- Health checks for the CLI: one row per check, plain-language detail and the fix. Admin database only.
-- Viewers may call it; every text is scrubbed of anything that looks like a key or signature.
CREATE FUNCTION pgbx.doctor() RETURNS TABLE (name text, ok bool, detail text, fix text)
LANGUAGE sql STABLE AS $$
    SELECT d.name, d.ok, pgbx._scrub(d.detail), pgbx._scrub(d.fix) FROM pgbx._doctor() d
$$;

-- internal: pg_dump-style pattern ('schema.table', * and ?; bare name = public) -> LIKE pattern
CREATE FUNCTION pgbx._like(p text) RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT replace(replace(
             replace(replace(replace(CASE WHEN position('.' IN p) = 0 THEN 'public.' || p ELSE p END,
                                     '\', '\\'), '_', '\_'), '%', '\%'),
           '*', '%'), '?', '_')
$$;

-- Tables whose ROWS the next backup skips (definitions are always kept). pgbx's own tables never appear.
CREATE FUNCTION pgbx.rowless_tables() RETURNS TABLE (table_name text) LANGUAGE sql STABLE AS $$
    SELECT format('%I.%I', n.nspname, c.relname)
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    CROSS JOIN (SELECT include_data, exclude_data FROM pgbx.config) cfg
    WHERE c.relkind IN ('r', 'p') AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pgbx')
      AND n.nspname NOT LIKE 'pg_toast%'
      AND (   (coalesce(cardinality(cfg.include_data), 0) > 0
               AND NOT EXISTS (SELECT 1 FROM unnest(cfg.include_data) p WHERE n.nspname || '.' || c.relname LIKE pgbx._like(p)))
           OR EXISTS (SELECT 1 FROM unnest(coalesce(cfg.exclude_data, '{}')) p WHERE n.nspname || '.' || c.relname LIKE pgbx._like(p)))
    ORDER BY 1
$$;

-- Which tables keep their ROWS in backups. Table definitions are ALWAYS backed up, so a restore never misses a table.
--   set_data_scope(exclude => ARRAY['public.sessions', 'audit_log_*'])   -- skip rows of these
--   set_data_scope(include => ARRAY['billing.*', 'users'])               -- keep rows of only these
--   set_data_scope()                                                     -- everything (default)
CREATE FUNCTION pgbx.set_data_scope(include text[] DEFAULT NULL, exclude text[] DEFAULT NULL) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE n bigint;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET include_data = nullif(set_data_scope.include, '{}'),
                                  exclude_data = nullif(set_data_scope.exclude, '{}'), updated_at = now();
    PERFORM pgbx._log('config', jsonb_build_object('include_data', set_data_scope.include, 'exclude_data', set_data_scope.exclude));
    SELECT count(*) INTO n FROM pgbx.rowless_tables();
    RETURN CASE WHEN set_data_scope.include IS NULL AND set_data_scope.exclude IS NULL
                THEN 'backups keep every table and every row'
                ELSE format('backups keep every table definition; rows skipped for %s table(s) right now — see pgbx.rowless_tables()', n) END;
END $$;

-- Point-in-time restore (optional, pgbx.pitr = on): state written by the worker in the admin database.
CREATE TABLE pgbx.pitr_state (
    id           int PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    system_id    text,
    work_dir     text,
    base_backups jsonb NOT NULL DEFAULT '[]',      -- backup.json of every kept base backup, oldest first
    updated_at   timestamptz
);

-- One row answering "can I restore this server to any moment, and from when?". Admin database only.
CREATE FUNCTION pgbx.pitr_status() RETURNS TABLE (
    enabled bool, state text, archive_mode text, schedule text, retention text,
    last_base_backup_at timestamptz, last_base_backup text, base_backups int,
    restorable_from timestamptz, wal_archived_until timestamptz,
    open_gaps bigint, gaps text, backlog_segments bigint, backlog_bytes bigint,
    last_error text, location text
) LANGUAGE plpgsql STABLE AS $$
DECLARE
    st pgbx.pitr_state; lb pgbx.history; le pgbx.history; inc pgbx.history; og pgbx.history;
    en bool; n bigint; seg bigint; rf timestamptz;
BEGIN
    PERFORM pgbx._require_admin_db();
    en := coalesce(current_setting('pgbx.pitr', true), 'off') IN ('on', 'true', '1', 'yes');
    SELECT * INTO st FROM pgbx.pitr_state s WHERE s.id = 1;
    SELECT * INTO lb FROM pgbx.history h WHERE h.kind = 'base_backup' AND h.state = 'done' ORDER BY h.id DESC LIMIT 1;
    SELECT * INTO le FROM pgbx.history h WHERE h.kind = 'base_backup' AND h.state = 'failed' ORDER BY h.id DESC LIMIT 1;
    SELECT * INTO inc FROM pgbx.history h WHERE h.kind = 'wal_archive' AND h.state = 'running' ORDER BY h.id DESC LIMIT 1;
    SELECT * INTO og FROM pgbx.history h WHERE h.kind = 'wal_gap' AND h.state = 'running' ORDER BY h.id DESC LIMIT 1;
    seg := pg_size_bytes(current_setting('wal_segment_size'));
    SELECT count(*) INTO n FROM pg_ls_archive_statusdir() s WHERE s.name LIKE '%.ready';
    SELECT min((b->>'stop_time')::timestamptz) INTO rf FROM jsonb_array_elements(coalesce(st.base_backups, '[]')) b;
    rf := coalesce(rf, (lb.params->>'stop_time')::timestamptz);
    RETURN QUERY SELECT
        en,
        CASE WHEN NOT en THEN 'off'
             WHEN current_setting('archive_mode') = 'off' THEN 'restart needed (archive_mode is off)'
             WHEN og.id IS NOT NULL THEN 'gap: WAL was dropped; restores refused from ' ||
                  ((og.params->>'safe_until')::timestamptz - make_interval(secs => coalesce((og.params->>'margin_s')::int, 60)))::text
             WHEN inc.id IS NOT NULL THEN 'archiving failing'
             WHEN EXISTS (SELECT 1 FROM pgbx.history h WHERE h.kind = 'base_backup' AND h.state = 'running') AND lb.id IS NULL
                  THEN 'first base backup running'
             WHEN lb.id IS NULL THEN 'waiting for first base backup'
             WHEN le.id > lb.id THEN 'base backups failing'
             ELSE 'active' END,
        current_setting('archive_mode'),
        coalesce(nullif(current_setting('pgbx.pitr_schedule', true), ''), 'daily at 01:00'),
        coalesce(nullif(current_setting('pgbx.pitr_retention', true), ''), '7 days'),
        lb.finished, lb.params->>'label',
        greatest(jsonb_array_length(coalesce(st.base_backups, '[]')), CASE WHEN lb.id IS NULL THEN 0 ELSE 1 END),
        rf,
        CASE WHEN og.id IS NOT NULL
             THEN (og.params->>'safe_until')::timestamptz - make_interval(secs => coalesce((og.params->>'margin_s')::int, 60))
             ELSE (SELECT a.last_archived_time FROM pg_stat_archiver a) END,
        (SELECT count(*) FROM pgbx.history h WHERE h.kind = 'wal_gap' AND h.state = 'running'),
        (SELECT string_agg(format('#%s %s .. %s', h.id,
                    date_trunc('second', (h.params->>'safe_until')::timestamptz - make_interval(secs => coalesce((h.params->>'margin_s')::int, 60))),
                    coalesce(date_trunc('second', (h.params->>'healed_at')::timestamptz)::text, 'open')), '; ' ORDER BY h.id)
           FROM pgbx.history h WHERE h.kind = 'wal_gap'
            AND (h.state = 'running' OR rf IS NULL OR (h.params->>'healed_at')::timestamptz > rf)),
        n, n * seg,
        coalesce(inc.error, CASE WHEN le.id > coalesce(lb.id, 0) THEN le.error END),
        format('s3://%s/%s/%s/', current_setting('pgbx.s3_bucket', true),
               coalesce(nullif(current_setting('pgbx.server_name', true), ''), '<hostname>'), coalesce(st.system_id, '<system id>'));
END $$;

-- Queue a base backup now (superuser; admin database). Returns the history id.
CREATE FUNCTION pgbx.pitr_backup_now() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE jid bigint;
BEGIN
    PERFORM pgbx._require_admin_db();
    IF NOT coalesce((SELECT r.rolsuper FROM pg_roles r WHERE r.rolname = session_user), false) THEN
        RAISE EXCEPTION 'pgbx: pitr_backup_now() needs a superuser';
    END IF;
    IF coalesce(current_setting('pgbx.pitr', true), 'off') NOT IN ('on', 'true', '1', 'yes') THEN
        RAISE EXCEPTION 'pgbx: point-in-time restore is off on this server (pgbx.pitr); enable it with: pgbx setup pitr --yes';
    END IF;
    INSERT INTO pgbx.history (kind, trigger) VALUES ('base_backup', 'manual') RETURNING id INTO jid;
    RETURN jid;
END $$;


-- Queue a backup now. Returns the history id; watch it in pgbx.history. While a backup of this database is still
-- queued it returns that one instead (pgbx.coalesce_manual), so calling it five times costs one dump.
CREATE FUNCTION pgbx.backup_now() RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
    RETURN pgbx._queue_manual('backup');
END $$;

-- Queue a restore of this database's newest backup taken at or before `at` into a NEW database `into_db`.
-- The live database is never touched; swap names yourself once the restore is verified.
-- with_roles: first create the roles in the backup's roles file that do not exist on this server (existing roles are
-- never changed), then restore keeping object owners. roles: 'referenced' (default; only the roles this database
-- uses) or 'all'. A NOTICE says when it starts and how long it takes.
CREATE FUNCTION pgbx.restore(into_db text, at timestamptz DEFAULT now(), with_roles bool DEFAULT false,
                             roles text DEFAULT 'referenced') RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE j bigint;
BEGIN
    IF restore.roles IS NULL OR restore.roles NOT IN ('referenced', 'all') THEN
        RAISE EXCEPTION 'pgbx: roles must be referenced or all';
    END IF;
    INSERT INTO pgbx.history (kind, trigger, params)
    VALUES ('restore', 'manual', jsonb_build_object('into_db', restore.into_db, 'at', restore.at,
                                                     'with_roles', coalesce(restore.with_roles, false), 'roles', restore.roles))
    RETURNING id INTO j;
    PERFORM pgbx._notice_eta(j);
    RETURN j;
END $$;
"#,
    name = "schema",
    bootstrap
);

extension_sql!(
    r#"
-- ---- privileges: nothing for PUBLIC; viewer reads; admin manages through SECURITY DEFINER functions ----
REVOKE ALL ON SCHEMA pgbx FROM PUBLIC;
REVOKE ALL ON ALL TABLES IN SCHEMA pgbx FROM PUBLIC;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA pgbx FROM PUBLIC;
REVOKE ALL ON ALL FUNCTIONS IN SCHEMA pgbx FROM PUBLIC;
GRANT USAGE ON SCHEMA pgbx TO pgbx_viewer;

-- viewer: read-only
GRANT SELECT ON pgbx.config, pgbx.history, pgbx.backups, pgbx.server_overview, pgbx.server_queue, pgbx.server_capacity,
      pgbx.activity_hourly, pgbx.pitr_state TO pgbx_viewer;
GRANT EXECUTE ON FUNCTION pgbx.status(), pgbx.overview(), pgbx.to_cron(text),
      pgbx.next_run_epoch(text, double precision), pgbx._require_admin_db(),
      pgbx.rowless_tables(), pgbx._like(text), pgbx._dur(double precision), pgbx._estimate(text) TO pgbx_viewer;
-- job_eta() reads pg_database_size and the history: runs as the extension owner, viewers may call it
ALTER FUNCTION pgbx.job_eta(bigint) SECURITY DEFINER SET search_path = pg_catalog, pgbx;
GRANT EXECUTE ON FUNCTION pgbx.job_eta(bigint) TO pgbx_viewer;
ALTER FUNCTION pgbx.suggest_window(int) SECURITY DEFINER SET search_path = pg_catalog, pgbx;
GRANT EXECUTE ON FUNCTION pgbx.suggest_window(int) TO pgbx_viewer;
-- doctor() reads server-wide settings and slots: runs as the extension owner, viewers may call it
ALTER FUNCTION pgbx.doctor() SECURITY DEFINER SET search_path = pg_catalog, pgbx;
GRANT EXECUTE ON FUNCTION pgbx.doctor() TO pgbx_viewer;
-- pitr_status() reads the archiver and archive_status: runs as the extension owner, viewers may call it
ALTER FUNCTION pgbx.pitr_status() SECURITY DEFINER SET search_path = pg_catalog, pgbx;
GRANT EXECUTE ON FUNCTION pgbx.pitr_status() TO pgbx_viewer;

-- admin: management functions run as the extension owner (so admins never need write access to the tables)
DO $lock$
DECLARE f text;
BEGIN
    FOREACH f IN ARRAY ARRAY[
        'pgbx.configure(text, int, int, bool, text, text)', 'pgbx.set_schedule(text)',
        'pgbx.set_retention(int, int, text)', 'pgbx.pause(text)', 'pgbx.resume()',
        'pgbx.backup_now()', 'pgbx.restore(text, timestamptz, bool, text)', 'pgbx.verify_now()',
        'pgbx.set_verify_schedule(text)', 'pgbx.download_url(bigint, interval)',
        'pgbx.set_data_scope(text[], text[])', 'pgbx.cancel(bigint)']
    LOOP
        EXECUTE format('ALTER FUNCTION %s SECURITY DEFINER SET search_path = pg_catalog, pgbx', f);
        EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO pgbx_admin', f);
    END LOOP;
END $lock$;
-- internals (_presign, _log, _check_days, _queue_manual, _notice_eta, _activity_add, _gfs_span, _s3_credentials) and pitr_backup_now():
-- superuser only — no grants.
-- (_estimate only reads: status() calls it as the viewer.)
"#,
    name = "lockdown",
    finalize
);

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}

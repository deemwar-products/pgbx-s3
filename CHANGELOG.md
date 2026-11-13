# Changelog

## 0.6.2

- **Fix: pgmq queues were missing from backups.** pgmq makes every queue table (`pgmq.q_*`, `pgmq.a_*`) a member of
  its extension, and pg_dump never dumps extension members: a backup had no queues and no messages, and restoring it
  failed on the GRANTs for them. pgbx now saves the queues next to each dump (`<ts>.ext.sql.zst`, encrypted when
  `pgbx.encryption_key_file` is set); a restore recreates each queue with pgmq's own create function (table and
  indexes), puts its messages and archive back, moves the msg_id sequence past them and replays the grants.
  Partitioned queues (pg_partman) are listed but not recreated. Retention deletes the file with its dump.
- **Fix: restoring a database that uses pg_cron into a new database failed.** pg_cron can only exist in the database
  named by `cron.database_name`. A restore into any other database now leaves pg_cron out (the result says so,
  `cron_left_out`); a restore into the cron database itself keeps the jobs but pauses them (`active = false`,
  `cron_jobs_paused`), so nothing runs twice. Backups record how many cron jobs they hold (`cron_jobs`).
- **Fix: the restore test counted tables a restore never brings back** (extension members), so it failed on databases
  with pgmq. It now counts what a restore recreates. Backups taken before 0.6.2 recorded the old count: take a new
  backup before judging a restore test.
- **Fix: `pgbx setup server` ignored `shared_preload_libraries` set with ALTER SYSTEM** (`postgresql.auto.conf`, which
  overrides conf.d): the server would start without pgbx, or setup's advice led to a one-literal value Postgres cannot
  start with. Setup now merges pgbx into that file too, as a correct list.
- New test `tests/ext_e2e.sh`: real pgmq and pg_cron through backup, restore test and restore. Run by `tests/run_all.sh`.
- No schema change (`pgbx--0.6.1--0.6.2.sql` is empty).

## 0.6.1

- **Fix: backups to HTTPS S3 endpoints failed in 0.6.0** (AWS, R2, Hetzner, Backblaze...): every backup job panicked with
  "Could not automatically determine the process-level CryptoProvider". The 0.6 dependencies enable two rustls crypto
  providers (aws-lc-rs and ring) and none was chosen; pgbx now installs aws-lc-rs explicitly in the extension and the
  CLI. Plain-http endpoints were not affected, which is why the test suites (local S3 over http) missed it.
- New test `tests/https_s3_e2e.sh`: real https calls to AWS S3 with dummy keys, from the extension's backup job and the
  CLI; it reproduces the 0.6.0 panic and passes on 0.6.1. Run by `tests/run_all.sh`.
- No schema change (`pgbx--0.6.0--0.6.1.sql` is empty); the worker updates every database's extension version itself.

## 0.6.0

- Windows: one x64 build (`pgbx-windows-amd64.zip`). Windows on ARM runs it through its built-in x64 emulation, and `install.ps1` installs it there; the separate arm64 Windows build is dropped (its cross-build of the TLS/crypto C libraries fails).

- **Breaking: pgbx's built-in SSH is gone** (ADR 0003). `--ssh`, `--ssh-port`, `--ssh-jump`, `--tunnel-idle` and
  `pgbx tunnel` are removed (they now fail with a pointer to the ssh adapter); doctor / logs / diagnose /
  setup no longer run over SSH. Use the ssh example adapter:
  `pgbx profile add prod --adapter ssh target=ops@db1 user=app 'password=$PGPASSWORD'`.
- **Breaking: profiles live in `config.yaml`.** A `profiles.json` is migrated automatically on first use (SSH
  profiles become `adapter: ssh` profiles, direct ones `url:` profiles); pgbx says so once and keeps the old file
  as `profiles.json.migrated`.
- **Connection adapters** (ADR 0003): a profile is a connection string or an adapter, any program that hands
  pgbx a connection string over a two-message stdin protocol (own process group / Job Object, 30 s ready timeout,
  stop + 5 s grace, Ctrl-C stops it). One-off commands start and stop it; `pgbx serve` keeps one per connection.
  Examples for ssh, aws (SSM), gcp (Cloud SQL Auth Proxy) and azure in `adapters/`, shipped in the release
  archives and copied to `<config dir>/adapters` by the installers.
- **`config.yaml`** (0600) with `default`, `secrets`, `adapters`, `profiles`; `pgbx profile edit`;
  `--adapter` / `--adapter-command` / `key=value` settings on `profile add`; `--url` / `PGBX_URL` for one run.
- **`$VAR` / `${VAR}` references** anywhere in a profile or url, expanded at run time (`$$` = `$`) from the
  environment, then the `secrets:` source (a .env file or a handler command run as `CMD NAME`). pgbx stores no
  secrets: literal passwords are refused, and every output masks passwords and expanded secrets.
  `pg_restore` gets the password through its environment.
- **TLS to Postgres** for every CLI connection, with libpq's `sslmode`: `disable`, `allow` / `prefer` (the
  default: TLS when the server offers it), `require`, `verify-ca`, `verify-full`, and `sslrootcert` (a PEM file,
  else `~/.postgresql/root.crt`, else the built-in Mozilla roots plus the OS store). From the connection string,
  then the profile's `sslmode:` / `sslrootcert:` keys, then `PGSSLMODE` / `PGSSLROOTCERT`. Servers that force
  TLS (Azure flexible server, RDS with `rds.force_ssl`) and the aws / azure adapters' `sslmode=require` URLs
  (`iam_auth`, `entra_auth`) now connect; `pg_restore` gets the same settings. rustls, so still no OpenSSL and one
  static binary. A connection that fails says why (`invalid peer certificate: UnknownIssuer`, ...). Behaviour
  change: a server with `ssl=on` is now reached over TLS by default, as `psql` does.
- **Resource caps** (ADR 0001 §3): pg_dump / pg_restore run at `pgbx.job_nice` (10) and, on Linux, IO priority
  `pgbx.job_ionice` (best-effort-7), set before exec; their connections are named `pgbx_dump` / `pgbx_restore` /
  `pgbx_verify`.
- **Never queued behind DDL:** pg_dump waits at most `pgbx.dump_lock_timeout` (5s) for its table locks; on a timeout
  the backup stays queued and is retried with `pgbx.defer_backoff`, until `pgbx.max_defer` (4h, first backup 15min,
  never past one schedule interval). Then it runs `forced` with `pgbx.dump_lock_timeout_forced` (60s) and
  `pgbx.dump_compression_busy`, and fails + alerts if it still cannot lock.
- pg_restore runs with `synchronous_commit=off` (`pgbx.restore_synchronous_commit`).
- Bandwidth caps `pgbx.upload_kbps` / `pgbx.download_kbps` (KiB/s, 0 = unlimited).
- doctor(): `long_running_job` (`pgbx.doctor_long_job`, 1h).
- `backup_now()` / `verify_now()` return the job already queued instead of adding another (`pgbx.coalesce_manual`,
  on); new `pgbx.cancel(job_id)` cancels a queued job, or a **running** one: its child is killed, the upload aborted
  (nothing left in S3), a half-restored database dropped, no alert; state `cancelled`.
- **One server-wide job queue** (ADR 0001 §0): the worker supervises jobs as child processes instead of blocking on
  them, picks restore > manual backup > scheduled backup > restore test > prune, then oldest, then round-robin over
  databases; `pgbx.max_concurrent_jobs` (1) enforced by advisory-lock slots in the admin database; `pgbx.restore_lane`
  (on) gives restores their own slot. `pgbx.overrun_policy` (`skip`, or `catch_up`) with `pgbx.overrun_max_gap`
  (1.5 intervals): a dump longer than its interval no longer runs back to back (`params.skipped_slots`).
  doctor(): `dump_longer_than_interval`. Restart recovery and orphan-upload abort keep working (crash e2e).
- `pgbx.server_queue` (admin database) and status() `running_job`, `next_job`, `queue_position`, `waiting_reason`
  say what runs and why a job waits; CLI `pgbx jobs [cancel ID --yes]`; a queue card in `pgbx ui`; `pgbx ui` and
  `--wait` know the `cancelled` state.
- **Time estimates** (§4): a NOTICE on `backup_now()` / `verify_now()` / `restore()` with start, duration, size,
  confidence and the bottleneck; `pgbx.job_eta(job_id)`; live progress (`history.bytes` after every 16 MiB) in
  status() `job_progress` / `job_eta`, `pgbx jobs` and the UI; a daily cpu probe and network / disk rates in
  `pgbx.server_capacity`; doctor(): `capacity`, `eta_accuracy`. Settings `pgbx.eta_samples`, `pgbx.eta_default_mbps`,
  `pgbx.eta_calibrate`.
- **Quiet-window suggestion** (§2): activity per hour of the week (UTC) learned from `pg_stat_database` deltas into
  `pgbx.activity_hourly` (`pgbx.activity_sampling`, `pgbx.activity_decay`); `pgbx.suggest_window()`, status()
  `suggested_schedule`, CLI `pgbx schedule suggest [--apply]`, a suggestion card with copy-to-apply buttons in the
  UI, doctor(): `schedule_in_quiet_window` (`pgbx.doctor_busy_ratio`, `pgbx.suggest_min_days`). Never applied by
  itself.
- **Load gate** (§1), default **shadow**: one load sample per poll (active sessions, tps, long writers, replica lag,
  load average; `pgbx.busy_*`), `params.would_defer` on jobs it would have held back, never a delay. Per database
  `configure(load_gate => 'on')` (new `load_gate` argument) defers scheduled backups / restore tests with
  `pgbx.defer_backoff` up to `pgbx.max_defer`, then runs them `forced`. Human jobs get a NOTICE that they compete with
  the app (`pgbx.gate_manual_jobs`). CLI `pgbx load [--gate ...]`, a load card in the UI, doctor(): `load_gate`,
  `forced_backups_7d`.
- `bench/load_overhead.sh` + `bench/RESULTS.md`: pgbench TPS and latency with no backup, an uncapped and a capped one.
- Fix: multipart uploads cut off by a crash are aborted at the next worker start even where the S3 listing of
  open uploads cannot be parsed (RustFS): the worker keeps its own list (`pgbx_open_uploads` in the data directory).
- Fix: a shutdown during a restore download no longer hangs (the writer returned `Interrupted`, which `write_all` retries).
- **Client-side encryption** (off by default): `pgbx.encryption_key_file` (32 random bytes, base64 or hex, chmod 600)
  encrypts every dump and its roles file with AES-256-GCM in 4 MiB authenticated frames, streamed (no temp files,
  constant memory). Restores, restore tests and `pgbx db-restore --from-s3 --key-file` decrypt in the stream;
  tampering, truncation, reordered frames and a wrong key fail loudly; older unencrypted dumps still restore.
  `pgbx decrypt` for download links (they serve the ciphertext). `history.params.encrypted`.
- **Roles with every backup**: `<ts>.globals.sql.zst` next to each dump (`pg_dumpall --globals-only`, no passwords
  unless `pgbx.backup_role_passwords = on`) plus the roles the database references. `restore(..., with_roles => true,
  roles => 'referenced'|'all')`, `pgbx db-restore [--from-s3] --with-roles [--roles R]`: missing roles are created,
  existing ones never changed, owners kept; running it twice changes nothing. Pruning deletes each dump's roles file.
- **Notifications**: `pgbx.notify = 'slack:NAME, telegram:NAME, webhook:NAME, email:NAME'`, URLs and tokens only in
  `pgbx.notify_secrets_file` (chmod 600); one message per incident plus one "OK again", sent from the job's thread.
  A URL written into `pgbx.notify` is refused and the error never repeats it. `pgbx.alert_command` unchanged.
- **Prometheus**: `GET /metrics` on `pgbx ui`, `pgbx metrics`; `server_overview` gained `last_backup_bytes`,
  `failures_total`, `queued_jobs`, `last_verify_ok`, `last_backup_encrypted`.
- **GFS retention**: `set_retention(gfs => '7d,4w,12m')` (`'off'` clears it; a span beyond `pgbx.max_days_limit` is
  refused), `pgbx retention --gfs` (changing or clearing an existing spec needs `--yes`); `status()` shows it.
- **Point-in-time restore** (optional, whole server, no pgBackRest): `pgbx setup pitr [--yes]` (alias
  `setup --pitr`; ALTER SYSTEM archive_mode / archive_command / `pgbx.pitr`, refuses a foreign archive_command, one
  restart). `pgbx wal-push` (archive_command: zstd + sha256, never overwrites a different checksum, async spool with
  parallel look-ahead, drops WAL past `pgbx.wal_queue_max` instead of filling the disk and records the gap) and
  `pgbx wal-get` (restore_command: verifies every file, parallel prefetch). Base backups (`pg_basebackup` → zstd →
  parallel multipart) are jobs of the server-wide queue (kind `base_backup` in the admin database: `pgbx jobs`,
  cancel, prompt shutdown), on `pgbx.pitr_schedule`, kept for `pgbx.pitr_retention` together with their WAL; a gap
  is healed by a base backup queued automatically. `pgbx pitr status|list|backup-now|restore --time TS|latest
  --target DIR` (never starts Postgres; a copy gets archive_mode=off and its own server_name; in place only with
  `--yes-replace-whole-server` on a stopped server, moved aside). WAL archiving incidents and gaps alert through
  `pgbx.alert_command` and `pgbx.notify`, from a thread. SQL `pitr_status()`, `pitr_backup_now()`,
  `pgbx.pitr_state`; history kinds `base_backup`, `wal_gap`, `wal_archive`, trigger `wal_gap`; doctor() rows
  `pitr archiving`, `pitr base backups`, `pitr gaps`; a PITR card in `pgbx ui`. Settings `pgbx.pitr`,
  `pgbx.pitr_schedule`, `pgbx.pitr_retention`, `pgbx.wal_queue_max`, `pgbx.wal_gap_margin`, `pgbx.wal_alert_after`,
  `pgbx.wal_alert_size`, `pgbx.work_dir`, `pgbx.cli_path`. Designs ported from pgBackRest (MIT, see `NOTICE`).
- **S3 credentials without a keys file** (EC2 instance role, for the AWS Marketplace AMI):
  `pgbx.credentials_file = 'aws-default'` (or empty) uses the AWS default chain instead of static keys: env
  `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` (CLI only), web identity (EKS IRSA), container
  credentials (ECS, EKS Pod Identity), then the **EC2 instance role through IMDSv2** (IMDSv1 is never used;
  `AWS_EC2_METADATA_SERVICE_ENDPOINT` is honoured). Temporary credentials are cached and renewed 5 minutes before
  they expire or on a 403 `ExpiredToken`, between multipart parts and download resumes, so long transfers run across
  a refresh. Everywhere pgbx talks to S3: the worker and its jobs (which still read no settings: the job's own thread
  resolves the credentials, so the poll loop never waits for a metadata service), `wal-push` / `wal-get` / `pitr`,
  `backups --from-s3` and `db-restore --from-s3` (`--credentials-file` may now be left out, meaning `aws-default`).
  `pgbx setup server --credentials aws-default` writes the setting and no keys file. doctor(): the
  `credentials file` row is now `s3 credentials` and names the source in use (`file`, `env`, `web-identity`, `ecs`,
  `instance-role`, with the role and expiry), or why none works and the IAM policy to attach; `s3 settings` no longer
  requires `pgbx.credentials_file`. No key, secret or token is ever logged or stored. A non-empty file path behaves
  exactly as before.
- SQL signature changes: `set_retention(int, int, text)`, `restore(text, timestamptz, bool, text)`; old calls keep
  working through the defaults.
- Fix: the startup cleanup of orphaned multipart uploads runs on a thread of its own (an unreachable S3 no longer
  holds up the worker's first poll) and only aborts the worker's own objects (`.dump`, `.globals.sql.zst`) older than
  10 minutes, so it can never cut off a base backup another `pgbx` process is uploading.
- Fix: a shutdown waits at most 5 s for running jobs (was 20 s), so a job stuck in an S3 request that does not
  answer no longer delays Postgres; the job is marked interrupted at the next start. A job child its thread has not
  reaped yet is killed and reaped before the worker exits.
- Fix: no child process of pgbx is ever left to the postmaster. Where Postgres runs as PID 1 (containers), an orphan
  that died by a signal made the postmaster restart the whole server (seen when a base backup was cancelled:
  pg_basebackup died of SIGPIPE). `pgbx pitr backup` reaps pg_basebackup on SIGTERM; the detached wal-push / wal-get
  background processes only ever exit 0 or 1.
- Schema update script `pgbx--0.5.0--0.6.0.sql` (the worker applies it by itself) covers all of the above; an updated
  database equals a fresh 0.6.0 install.

## 0.5.0

First public release.

- Per-database backups: every database (and `template1`) gets the extension; `pg_dump`/`pg_restore` streamed to
  and from S3. No `archive_mode` or extra package needed.
- `pgbx backups --from-s3` and `pgbx db-restore --from-s3 [--backup KEY | --time TS]` — restore onto a new
  server without the extension (its objects are skipped), streamed with resumable download.
- PostgreSQL 13–18. `pgbx.dump_compression` defaults to `auto` (zstd:3 with pg_dump 16+, gzip level 6 before);
  the newest installed pg_dump/pg_restore is used.
- The worker runs `ALTER EXTENSION pgbx UPDATE` itself when a database or `template1` is older than the library.
- doctor(): extension loaded, s3 settings, credentials file, database backups, restore tests, workers,
  archive_mode (info), replication_slots.
- Docker images take a `PG_MAJOR` build arg.

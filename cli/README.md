# pgbx — command line

Standalone binary for when you cannot (or would rather not) use SQL — above all a disaster where Postgres is down
or the server is gone. With Postgres up it calls the extension's SQL; `pgbx diagnose` explains why Postgres is down;
`pgbx backups --from-s3` / `pgbx db-restore --from-s3` read per-database dumps straight from S3 onto any server,
with or without the extension.

Build: `cargo build --release --manifest-path cli/Cargo.toml` (separate crate; not part of the extension).
Tests: `cd cli && cargo test`. The runtime Docker image ships it as `/usr/local/bin/pgbx`.

```
pgbx status  [--db X]                         pgbx now [--db X] [--wait]
pgbx list    [--db X]                         pgbx verify [--db X] [--wait]
pgbx doctor   pgbx diagnose [--log F]         pgbx db-restore --db X --into NEWDB [--time TS] [--wait]
pgbx logs    [--lines N]
pgbx backups    --from-s3 --db X <s3 flags>                                  (list dumps in S3, newest first)
pgbx db-restore --from-s3 --db X --into NEWDB [--backup KEY | --time TS] <s3 flags>   (no extension needed)
  s3 flags: --s3-endpoint URL --s3-bucket B [--s3-region R] --server-name S --credentials-file F
pgbx schedule [TEXT]   pgbx retention [--max-backups N] [--max-days N]   pgbx pause --reason T --yes   pgbx resume
pgbx scope [--include P1,P2] [--exclude P1,P2] [--reset]   pgbx verify-schedule TEXT|never   pgbx overview
pgbx link [--backup-id N] [--expires '1 hour']   (prints only the URL; never paste it into chat)
pgbx skill install [--no-codex] | uninstall | where      pgbx --version
pgbx ui [--listen 127.0.0.1:8432] [--strict]    (read-only audit web UI + Prometheus GET /metrics)
pgbx metrics                                   pgbx decrypt --key-file F [--in FILE] [--out FILE]
pgbx db-restore ... [--with-roles [--roles referenced|all]] [--key-file F]   pgbx retention ... [--gfs 7d,4w,12m|off]
pgbx setup pitr [--yes]                        optional whole-server point-in-time restore (one restart)
pgbx pitr status | list | backup-now [--wait]
pgbx pitr restore --time TS|latest --target DIR (--conf F | <s3 flags> [--system-id N]) [--yes-replace-whole-server]
pgbx wal-push %p [--conf F]   pgbx wal-get %f %p --conf F      (archive_command / restore_command)
```
`--time` must carry a UTC offset (`+00`, `Z`). Policy commands show the current value when given no arguments.
Every command takes `--json` (one JSON object on stdout, always with `ok`, `command`, `safety`) and exits non-zero on
failure. Connection: `--host --port --user` (defaults `/var/run/postgresql`, 5432, `postgres`; PGHOST/PGPORT/PGUSER/
PGPASSWORD honoured), `--admin-db`, `--timeout SECS` for `--wait`.

## Safety levels (enforced in code, safe for agents)

| level | commands | rule |
|---|---|---|
| read-only | `status`, `list`, `backups --from-s3`, `doctor`, `logs`, `overview`, `ui`, `metrics`, `decrypt`, `pitr status`, `pitr list`, policy commands with no arguments | never mutate anything |
| safe | `now`, `verify`, `db-restore`, `resume`, `link`, `schedule TEXT`, `skill`, `db-restore --from-s3`, `pitr backup-now`, `pitr restore` (empty directory) | queue jobs / write only somewhere new; `db-restore` refuses an existing database or the source |
| guarded | `pause`, lowering `retention` or changing GFS, narrowing `scope`, `verify-schedule never`, `setup pitr` | refused without `--yes` (with an explanation of what would be lost) |
| destructive | `pitr restore --yes-replace-whole-server` | only onto a STOPPED server's data directory, which is moved aside (never deleted) |
No command overwrites a live database: every restore goes into a NEW database (or, for point-in-time restore, an empty
directory).

## Diagnose (Postgres down, disk full, pg_wal growing)

`pgbx diagnose [--log FILE] [--pgdata DIR] --json` -> `{postgres, probable_cause, evidence[{source,line}], steps[{tier,why,command,needs_human_approval}], facts}`.
Sources (each optional): postmaster.pid + liveness, `pg_ctl status`, the newest file in `log_directory`, `journalctl -u
postgresql*`, `--log FILE` (e.g. `docker logs`), the kernel log (OOM kills), `df`/`du` of the data dir, pg_wal
(replication slots holding WAL, max_wal_size), pgsql_tmp, the log dir, data dir owner/mode. Causes: oom_kill, disk_full,
wal_backlog, stale_pid, crash, corruption, permissions, config_error, too_many_connections, unknown. pgbx never runs a
step; destructive ones are marked for human approval, pg_wal/base/global/pg_xact are never offered for deletion.
`pgbx doctor` runs it automatically when Postgres is unreachable (`diagnosis` in the JSON).

## Disaster recovery on a fresh machine

Every per-database backup is a `pg_dump -Fc` file at `s3://<bucket>/<server_name>/<db>/<UTC timestamp>.dump`.
On a new server (pgbx not needed; `pg_restore` on PATH):

```
pgbx backups --from-s3 --db shop --s3-endpoint https://hel1.your-objectstorage.com --s3-bucket my-backups \
  --s3-region hel1 --server-name db1 --credentials-file ./s3.credentials
pgbx db-restore --from-s3 --db shop --into shop_restored --time '2026-09-30 18:00:00+00' \
  --s3-endpoint https://hel1.your-objectstorage.com --s3-bucket my-backups --s3-region hel1 \
  --server-name db1 --credentials-file ./s3.credentials --host /var/run/postgresql --user postgres
```
It picks the newest dump at or before `--time` (default newest; `--backup KEY` picks one exactly), creates `--into`
(refusing if it exists), and streams the dump into `pg_restore --no-owner`, resuming the download with HTTP Range
after a network drop. The credentials file has `access_key_id=` / `secret_access_key=` lines; keys are never printed.
Without the pgbx library on the target, the pgbx schema/extension is left out so only your data comes back.

## Agent skill

The agent skill (`skills/pgbx-skill`) is embedded in the binary at build time (`build.rs`).
`pgbx skill install` unpacks it to `~/.local/share/pgbx/skill/<version>/` and links
`~/.claude/skills/pgbx-skill` (`$CLAUDE_SKILLS_DIR`) and, when `$AGENTS_SKILLS_DIR` is set or `codex` is on PATH,
`~/.agents/skills/pgbx-skill`. The installer (`curl -fsSL https://pgbx.deemwar.com/install.sh | sh`) installs the binary and the skill. Server images ship
only the binary.

## Audit UI (`pgbx ui`)

```
pgbx ui [--listen 127.0.0.1:8432] [--strict] [--host --port --user --admin-db]   # then open http://127.0.0.1:8432/
```
A read-only web UI served by the pgbx binary itself (std::net HTTP/1.1; page embedded at build time; no CDN, no
external fetches, works offline; dark/light, mobile). Screens: **Overview** (every database: state, schedule, last/next
backup, size, last restore test, last error), **Timeline** (every `history` row of the last 30 days across all
databases — backups, restores, verify, config/pause/resume/scope changes, download links issued — with who/when/what, filter by database/kind/state, newest first), **Health** (`doctor()` rows; fixes are shown, never run). Click a database for its status, kept backups and history.
JSON: `GET /api/overview`, `/api/db/<name>`, `/api/timeline?days=30`, `/api/health`.

Read-only by construction: only `GET` is served (anything else gets 405), there are no action buttons, and every
database connection runs `SET default_transaction_read_only = on` before its first query. At start it checks the role:
a superuser or a role that can execute `pause()`, `backup_now()`, `restore()`, `download_url()` gets a
strong warning (`--strict` refuses to start). Use a dedicated viewer login:
```sql
CREATE ROLE pgbx_ui LOGIN PASSWORD '...' IN ROLE pgbx_viewer;   -- then: PGPASSWORD=... pgbx ui --user pgbx_ui --strict
```
Default bind is 127.0.0.1 and requests must carry `Host: localhost/127.0.0.1/[::1]` (DNS-rebinding guard). Binding a
non-loopback address prints a warning: exposing it is your responsibility; it is read-only but shows every database's
backup history — prefer an SSH tunnel (`ssh -L 8432:127.0.0.1:8432 db1`) or an authenticating reverse proxy.

History is kept `pgbx.audit_days` days (default 30, min 1; postgresql.conf). The per-database worker prunes
older rows hourly, except backups still kept in S3 (state `done`), queued/running jobs, and the
newest row of each kind. Each row records `who` (the role that asked; worker rows show the worker's role).

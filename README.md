# pgbx

Zero-touch Postgres backups to S3. **Create a database and it is backed up.** Everything else is optional SQL.

Three parts: the **pgbx extension** runs inside Postgres and does the work (schedules, `pg_dump` to S3,
restore tests, restores); the **pgbx CLI** (`pgbx`) is the client that talks to one or more Postgres servers
(and restores straight from S3 when Postgres is down); the **agent skill** drives the CLI with `--json`.
See [How it works](https://pgbx.deemwar.com/concepts/how-it-works/).

Full documentation: [`site/`](site/README.md) (Astro Starlight, live at https://pgbx.deemwar.com).

## Quick start (Linux, PostgreSQL 13–18)
```sh
curl -fsSL https://pgbx.deemwar.com/install.sh | sh
sudo pgbx setup server                        # S3 settings; prints the ONE restart command
sudo systemctl restart postgresql@16-main     # (the command setup printed)
# on your laptop:
pgbx setup client prod --adapter ssh target=ops@db.example.com user=postgres 'password=$PGPASSWORD'
pgbx doctor
```
Windows (CLI + agent skill): `powershell -c "irm https://pgbx.deemwar.com/install.ps1 | iex"`.
Flags, manual install and uninstall: [Install](https://pgbx.deemwar.com/getting-started/install/).

## Everyday SQL (run inside the database)
```sql
SELECT * FROM pgbx.status();        -- state, schedule, last + next backup, retention, last error, S3 location
SELECT * FROM pgbx.backups;         -- every backup kept: when, age, size, trigger, s3_key

SELECT pgbx.set_schedule('every 1 hour');            -- or 'every 15 minutes', 'daily at 02:30',
SELECT pgbx.set_schedule('weekly on sunday at 03:00'); --    'hourly', 'daily', 'weekly', or cron '0 */6 * * *'
SELECT pgbx.set_retention(max_backups => 14, max_days => 90);  -- whichever deletes first; newest always kept

SELECT pgbx.pause('migrating, back at 6pm');         -- stops automatic backups; manual still work
SELECT pgbx.resume();

SELECT pgbx.backup_now();                            -- returns a job id
SELECT pgbx.restore(into_db => 'myapp_restored', at => '2026-10-01 09:00');  -- live DB untouched
```
Restore tests, download links, every database at once:
```sql
SELECT pgbx.verify_now();                            -- restore newest backup into a scratch DB, check, drop
SELECT pgbx.set_verify_schedule('weekly on sunday at 04:00');   -- default; 'never' to disable
SELECT pgbx.download_url(expires => '1 hour');       -- presigned link: curl -s "$url" | pg_restore -d x

-- in the admin database (postgres):
SELECT * FROM pgbx.overview();                       -- every database on the server, one row each
SELECT * FROM pgbx.doctor();                          -- health checks: name, ok, detail, fix
```
Server gone? Restore any database from S3 onto a new server — no extension needed there:
```sh
pgbx db-restore --from-s3 --db shop --into shop_restored [--time '2026-09-30 18:00+00'] \
  --s3-endpoint https://hel1.your-objectstorage.com --s3-bucket my-backups --s3-region hel1 \
  --server-name db-prod-1 --credentials-file ./s3.credentials
```

## Who can do what
| role | can |
|---|---|
| nobody (PUBLIC) | nothing |
| `pgbx_viewer` | `status()`, `backups`, `history`, `overview()`, `doctor()` |
| `pgbx_admin` | + schedule, retention, pause/resume, backup_now, restore, verify, download_url, configure |
| superuser | + server-wide settings |

`GRANT pgbx_admin TO my_migrator;` lets migrations call `configure()`.

In a migration, one call does it all:
```sql
SELECT pgbx.configure(schedule => 'every 6 hours', max_backups => 28, max_days => 30);
```
Defaults for a new database: daily at 02:00, keep 14 backups, max 90 days. Bad schedules and retention above
the server limit are rejected immediately.

## Audit UI
`pgbx ui` serves a read-only web UI on 127.0.0.1:8432: every database, a 30-day timeline of every action (who/when/what),
and health checks. Run it as a `pgbx_viewer` login
(see [cli/README.md](cli/README.md#audit-ui-pgbx-ui)).

## For AI agents
One agent skill (Claude Code / Codex): [`skills/pgbx-skill`](skills/pgbx-skill/SKILL.md) —
status, backup now / before deploy, restore a database (also from S3 onto a new server), verify, schedule, retention.
Restores only ever go into a new database; risky changes are gated in code. Install: `sh skills/pgbx-skill/install.sh`;
self-check: `sh skills/pgbx-skill/tests/all.sh`.

## Requirements

PostgreSQL 13–18 for the extension. `pgbx db-restore --from-s3` needs only `pg_restore` on the restoring host and
any server the dump can be restored into.

## Server setup (once, postgresql.conf)
```ini
shared_preload_libraries     = 'pgbx'          # needs one restart
pgbx.s3_endpoint      = 'https://hel1.your-objectstorage.com'
pgbx.s3_bucket        = 'my-backups'
pgbx.s3_region        = 'hel1'
pgbx.server_name      = 'db-prod-1'            # folder for this server in the bucket
pgbx.credentials_file = '/etc/pgbx/s3.credentials'   # access_key_id=... / secret_access_key=...
pgbx.max_days_limit   = 90                     # no database may keep backups longer
pgbx.alert_command    = 'curl -s -X POST -d @- https://hooks.example/alert'   # JSON on stdin per failure
pgbx.dump_compression = 'auto'
pgbx.audit_days       = 30                       # history (audit trail) kept; kept backups never pruned
```
No `archive_mode` and no WAL archiving are needed for the default per-database backups. Optional since 0.6.0:
client-side encryption (`pgbx.encryption_key_file`), the roles kept next to every dump (`restore(..., with_roles => true)`),
Slack / Telegram / webhook / email notifications (`pgbx.notify`), Prometheus metrics (`pgbx metrics`, `GET /metrics`),
GFS retention (`set_retention(gfs => '7d,4w,12m')`) and whole-server **point-in-time restore** (`pgbx setup pitr --yes`,
one restart; WAL archived by `pgbx wal-push`, base backups as queue jobs, `pgbx pitr restore --time TS --target DIR`;
see [the guide](site/src/content/docs/guides/point-in-time-restore.md)). `doctor()` reports inactive
`replication_slots` pinning WAL.
When Postgres is down, `pgbx diagnose` (and `pgbx doctor`) names the probable cause with tiered steps it never runs.
Backups land at `s3://<bucket>/<server_name>/<database>/<UTC timestamp>.dump`.

## Robustness
- Streaming: `pg_dump` -> 16 MB multipart parts -> S3, no temp file; restore streams S3 -> `pg_restore`.
- Each part retried with backoff (~2 min); a restore download resumes from the byte it reached.
- Half-finished uploads are aborted; orphaned ones (the worker's own, older than 10 minutes) are cleaned at startup, off the poll loop.
- A Postgres shutdown never waits on a backup (stops within ~1 s, at most 5 s; the job is marked interrupted).

## Test
`tests/run_all.sh` — rebuild, fresh containers, unique S3 folder, then
`tests/e2e.sh` (against the real bucket), `tests/cli_e2e.sh` (incl. `db-restore --from-s3` onto a server without
the extension) and
`tests/bench_local.sh` (~1.2 GB against a local S3, S3 outages, shutdown). Counts and throughput: to be re-measured for 0.5.0.
Needs `docker/.env`, `docker/test.credentials`, `docker/local.env`, `docker/local.credentials` (gitignored).

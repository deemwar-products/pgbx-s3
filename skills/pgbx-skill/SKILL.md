---
name: pgbx-skill
description: "Zero-touch PostgreSQL backups to S3 via `pgbx` (pgbx), and a plain Postgres client without them: read queries as JSON through profiles (a connection string, or an adapter for SSH/jump hosts, AWS, GCP, Azure), per-database memory, status, backup now, restore a database (also from S3 onto a new server), verify, schedule, retention. Trigger: query the database, run a select, connect over ssh, is postgres backed up, take a backup, backup before deploy, restore the db, restore on a new server, backup failing, postgres is down, disk full, wal growing, pgbx."
---

<!-- version: 0.6.2 -->

# pgbx-skill

A natural-language front door for pgbx, the PostgreSQL extension that backs
every database up to S3 with no setup per database. `pgbx` is the engine; this
skill owns intent routing and recipe knowledge.

pgbx is also a plain Postgres client: profiles, adapters (SSH, AWS, GCP, Azure), `pgbx query` and the per-database memory work
against ANY Postgres, with or without the extension. Many users never turn backups on.

## Core Rules

- **`pgbx` is the engine.** Every recipe runs a `pgbx …` command first (requires
  pgbx v0.3). SQL against the `pgbx` schema is the documented fallback,
  used only when the CLI is not available. Recipes mark those lines `Fallback (SQL):`.
- **Always `--json`, never parse decorative output.** With SQL, use
  `psql -XAtq` and statements that return one row (`row_to_json(...)`). Tables,
  colours and `psql` borders are for humans, never for parsing.
- **Pick the server first, then pin it.** Run `pgbx profile list --json` (step-00), choose or ask
  for the profile, and pass `--profile <name>` on every `pgbx` call. Never mix servers in one task.
- **No shell needed: use the CLI.** Read questions → `pgbx query "SELECT ..." --json` (STAT-R-7); a server
  behind SSH or a cloud → a profile with an adapter (`--adapter ssh target=user@host ...`; pgbx starts and stops
  it). Prefer these over `ssh`, `psql` or any shell command; never try to get around `pgbx query`'s SELECT-only
  guard.
- **Never ask for, read, echo or store a secret.** Profiles hold `$VAR` references: suggest
  `--url 'postgres://app:$PGPASSWORD@host:5432/db'` (or `'password=$PGPASSWORD'` for an adapter) and tell the
  user to export the variable in their own shell. Never type a password into a command, a profile or chat.
  Add or edit a profile only when the user asks for it.
- **Discover before acting.** Run `pgbx status --json` / `pgbx doctor --json`
  (step-00) before any mutation. A `failing` database fails the next backup too.
  Read-only questions (`pgbx query`, memory, profiles) skip doctor: pick the profile and ask.
- **No extension is fine.** `backups: "off"` (status, setup client) or the `backups (pgbx extension)` info check
  (doctor) means pgbx is a client on that server: answer read questions normally and do not push installing it.
  Only a backup route (backup, restore from pgbx, verify, policy) needs it — then say backups are off on this
  server and give the optional step from `next_steps` once.
- **Never print credentials.** Do not `cat` `~/.config/pgbx/config.yaml` or a secrets `.env`; use
  `pgbx profile list --json` (it masks passwords). S3 keys live in `pgbx.credentials_file`
  (or `sec`). Read setting NAMES only; never `cat` the file or echo a key.
  `download_url()` output is a bearer link: use it, never paste it into chat.
- **Four safety tiers** (full table in `references/workflow.md`):
  read-only → just do it; safe mutation → do it and report the id;
  higher-risk → ask first; destructive → explicit human approval, quoting
  exactly what will be replaced.
- **The guarded gate is enforced in code — never bypass it.**
  `--yes`, the admin-database check and superuser are the human's signature.
  Never add them on your own, never retry with them after the error, never escalate roles.
- **Every database has its own memory: read it first, write it only when asked.**
  `${PGBX_MEMORY_DIR:-~/pgbx}/<connection>/<db>/memories.md` (facts, named questions + SQL) and `tables.md`
  (tables and columns). Read both before any `pgbx query` or action on that database (MEM-R-1); reuse a saved
  question's SQL instead of inventing one. Never create these files on your own: only when the user says
  "remember", "put it here", "save this query" (MEM-W-1/2). Never store rows, credentials or links.
  `PGBX_MEMORY=off` turns it off.
- **Restore one database = into a NEW database, never in place.** The live
  database is untouched; swapping names afterwards is destructive and needs approval.

## Session Context

Held in conversation memory only. The per-database memory files (`references/memory.md`) are the user's,
and are written only when the user asks.

```
pgbx_available:   true | false      # from step-00
backups:          on | off           # off = no extension on this server: client use only, read routes still work
profile:          prod               # from `pgbx profile list --json`; --profile on every call
target_db:        myapp             # database the user means
admin_db:         postgres          # pgbx.admin_db, for overview()/doctor()
last_job_id:      1234              # history id of the job we just queued
memory_dir:       ~/pgbx/prod/myapp # ${PGBX_MEMORY_DIR:-~/pgbx}/<profile>/<db>; read at start (MEM-R-1)
```

## Process

1. Read `references/router.xml` first — it routes the user's phrase to a route
   and lists the steps + family reference for it.
2. Run `references/steps/step-00-preflight.md` (pgbx on PATH, `pgbx profile list --json`; `pgbx doctor --json`
   only for backup routes — a read query needs no doctor and no extension).
3. Read `references/workflow.md` for invariants, the safety tiers and failure handling.
4. Follow the route's step file, if any:
   - Backup before deploy → `references/steps/step-01-backup-before-deploy.md`
   - Restore one database → `references/steps/step-02-restore-one-database.md`
   - Postgres down / disk full / pg_wal growing → `references/steps/step-04-postgres-down.md` (diagnose FIRST)
   - Server lost, restore on a new server → `references/steps/step-03-disaster-restore.md`
5. Load the matching family file and run the recipe verbatim.

## Self-check

    sh tests/all.sh

Runs:
  - tests/validate-recipes.sh   frontmatter, family files, recipe format, router refs
  - tests/install-test.sh        install/uninstall idempotency
  - tests/router-stress.sh       phrase → expected route

## Families at a glance

| Family | Covers | Reference |
|---|---|---|
| Status | is it backed up, list backups, doctor, logs, audit UI / who did what, read queries (STAT-R-7, also with no extension), job queue + ETA + cancel (`pgbx jobs`), server load / gate (`pgbx load`) | `references/status.md` |
| Backup | backup now, wait, backup id | `references/backup.md` |
| Restore | db into a new database, from S3 onto a new server, with roles (`--with-roles`), encrypted dumps (`--key-file`) | `references/restore.md` |
| Verify | restore tests, verify schedule, failures | `references/verify.md` |
| Policy | schedule, quietest time to back up (`pgbx schedule suggest`, never auto-applied), retention (incl. GFS), pause/resume, data scope | `references/policy.md` |
| Access | roles, download links | `references/access.md` |
| Security | encryption on/off, decrypt a downloaded dump | `references/security.md` |
| Alerts | Slack/Telegram/email/webhook notifications, Prometheus metrics | `references/alerts.md` |
| PITR | whole-server point-in-time restore (optional): status, enable (`pgbx setup pitr`), base backups, restore to a moment into a new directory | `references/pitr.md` |
| Memory | per-database memories.md / tables.md: read first, write only when asked | `references/memory.md` |
| Diagnose | postgres down, disk full, WAL growing, OOM, corruption — cause + tiered steps | `references/diagnose.md` |

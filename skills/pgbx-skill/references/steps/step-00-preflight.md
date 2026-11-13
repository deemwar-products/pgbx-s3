# Step 00 — Preflight

Run **once per session**, before any other `pgbx` call.

```bash
command -v pgbx && pgbx --version 2>/dev/null
```

Outcomes:

- **`pgbx` not on PATH** — respond exactly:
  > `pgbx` is not installed. It ships with pgbx v0.3 — from the pgbx repo run
  > `curl -fsSL https://pgbx.deemwar.com/install.sh | sh` (installs the binary and this skill; `pgbx skill install` re-installs just the skill).
  > Until then I can use the SQL API if you give me a `psql` connection.

  If `psql` works, set `pgbx_available=false` and use the `Fallback (SQL):` lines.
  Otherwise **stop**.

- **`pgbx` on PATH** — proceed. If `pgbx skill where --json` shows `same_version: false`, suggest `pgbx skill install`.

## Pick the server (profile)

```bash
pgbx profile list --json
```

- **One profile, or a `default` set** — use it; tell the user which (`"Using profile prod (db.prod:5432)."`).
- **Several profiles, no default, or the user named a server** — match the user's words to a profile name, its
  `settings.url` host or its adapter settings (`target`, `host`, `instance`); if still ambiguous, **ask** which one (list the names). Never guess between servers.
- **No profiles** — use the flags/env the user gave (`PGHOST`, `PGBX_URL` etc.); when the user asks to save
  the server, offer `pgbx setup client <name> --url 'postgres://USER:$PGPASSWORD@HOST:5432/DB' --yes --json`
  (safe, no sudo: saves the profile and tests it — connection, extension version, status — with `next_steps` on
  failure). The password is ALWAYS a `$VAR` reference in single quotes: never ask for the value, never put it in
  a command; tell the user to `export PGPASSWORD=...` in their own shell (pgbx refuses a literal password anyway).
  `ok: true` with `test.backups: "off"` is success: the server has no pgbx extension, so pgbx works as a client
  there (queries, profiles, memory). Its `next_steps` entry starts with "optional"; mention it once, do not insist.
  A server reachable only over SSH: `pgbx setup client <name> --adapter ssh target=user@host [jump=J] [pg_host=H]
  user=U 'password=$PGPASSWORD' --yes --json` — the ssh adapter runs the system ssh for that command and stops
  afterwards; you never run `ssh` yourself. AWS / GCP / Azure: `--adapter aws|gcp|azure` with that adapter's
  settings (docs: guides/adapters). An error naming `$VAR is not set` means the user must export it.

Set `profile` in session context and pass `--profile <name>` on **every** later `pgbx` call
(recipes show commands without it — add it). Check `profile_used` in each reply matches.

## Health (backup routes only)

Skip this section for read-only questions — `pgbx query` (STAT-R-7), memory, profiles. They need
neither doctor nor the pgbx extension.

```bash
pgbx doctor --json
```
Fallback (SQL): `psql -XAtq -d postgres -c "SELECT row_to_json(d) FROM pgbx.doctor() d"` (pgbx v0.3)

Stop on unhealthy and give the plain-language fix, e.g.:

| doctor finding | say |
|---|---|
| extension not preloaded | "Add `pgbx` to `shared_preload_libraries` and restart Postgres." |
| S3 unreachable / auth failed | "Postgres can't write to the bucket. Check `pgbx.s3_endpoint`/`s3_bucket` and the credentials file (I won't read it)." |
| `archive_mode` info row | "pgbx does not need `archive_mode`; leave it unless nothing else uses it." (info only) |
| info `backups (pgbx extension)` (or status `backups: "off"`) | "This server has no pgbx extension, so backups are off; I can still query it." Set `backups=off`; read routes continue, backup routes stop here with the optional `next_steps` line. |
| Postgres not reachable | "Postgres is down — only the CLI can help; see step-04 (or step-03 if the server is lost)." |
| info `host-side checks` (an adapter profile) | "Disk and log checks need the database host: run `pgbx doctor` (or `pgbx diagnose`) there." `diagnose` and `setup server` refuse through an adapter; say the same, never try to ssh in yourself. |
| error naming an adapter (`adapter state: ...`, `gave no result`) | Quote the error (it is already masked) and point at `pgbx profile show <name> --json` and the adapter's own tool (e.g. `ssh <target> true`). Never print the profile's secrets. |

Read-only intents may continue past warnings; mutations may not continue past failures.

Once the profile and the database are known, read that database's memory (`references/memory.md`, MEM-R-1):
`${PGBX_MEMORY_DIR:-~/pgbx}/<profile>/<db>/memories.md` and `tables.md`. Missing files are normal; do not create them.

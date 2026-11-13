---
title: Use pgbx as your Postgres client, no backups needed
description: Profiles, SSH and cloud adapters, read queries as JSON and agent memory against any Postgres, without installing the pgbx extension.
sidebar: { order: 0 }
---

You do not need backups to use pgbx. The CLI is a good everyday Postgres client on its own: it remembers your
servers, reaches them directly or through an adapter (SSH with jump hosts, AWS, GCP, Azure or your own),
answers read queries as JSON, and gives your AI agent a notebook per database. None of that needs the pgbx extension on the server, and nothing needs S3 or sudo.

## Install the CLI only

```sh
curl -fsSL https://pgbx.deemwar.com/install.sh | sh
```

On your laptop that puts `pgbx` on your PATH, copies the example adapters to `~/.config/pgbx/adapters` and
installs the agent skill. Do not run `pgbx setup server`;
leave the database server alone.

## Save a server as a profile

```sh
pgbx setup client prod --url 'postgres://app_ro:$PGPASSWORD@db.example.com:5432/shop'           # reachable directly
pgbx setup client prod --adapter ssh target=ops@db.example.com user=postgres                      # only reachable over SSH
pgbx setup client prod --adapter ssh target=ops@db.internal jump=bastion.example.com user=postgres   # through a jump host
```

`setup client` saves the profile (the first one becomes the default), connects once to test it, and reports:

```
ok: true
test:
  backups: off
  connected: true
  extension: absent
  info: pgbx extension not installed on this server: backups are off; queries, profiles and adapters work
  postgres_version: 16.4
next_steps:
  - optional, to turn on backups: on the database server run `curl ... | sh` and then `sudo pgbx setup server`
```

`backups: off` is not an error, and the command exits 0. The next step is optional: ignore it until you want
backups.

Profiles hold where a server is and `$VAR` references, never a password: pgbx reads `$PGPASSWORD` from your
environment (or your [secrets source](../../reference/config/#secrets)) when a command runs, and a profile
without a password uses `PGPASSWORD` or `~/.pgpass`. Manage them with
`pgbx profile list | show | edit | use | remove` ([CLI reference](../../reference/cli/#profiles)). For one run
without a profile: `--url '...'` or `PGBX_URL`.

## SSH and jump hosts

The ssh adapter runs your own `ssh` program, so your keys, your agent, `~/.ssh/config` hosts and jump hosts all
work and pgbx never sees a key. It needs Node 18+ on your machine. Each command starts it (`ssh -N -L` to a free
local port), uses the connection, and stops it when the command ends; `pgbx serve` keeps it open while it runs.

Postgres on the far side is reached as the SSH host sees it: `pg_host` (default `localhost`) and `pg_port`
(default `5432`), so a database on another machine behind the bastion works too:

```sh
pgbx profile add prod --adapter ssh target=ops@bastion.example.com pg_host=10.0.3.7 user=app 'password=$PGPASSWORD'
```

All the keys, the AWS, GCP and Azure adapters, and how to write your own:
[Connect through SSH, AWS, GCP, Azure or your own adapter](../adapters/). Need a port for a GUI? Run
`ssh -N -L 5433:localhost:5432 ops@db.example.com` yourself; pgbx no longer keeps tunnels open between commands.

## Read queries as JSON

```sh
pgbx query "SELECT datname, pg_size_pretty(pg_database_size(datname)) AS size FROM pg_database"
pgbx query "SELECT state, count(*) FROM pg_stat_activity GROUP BY 1" --db shop --max-rows 200 --timeout 10s
pgbx query "SHOW max_connections" --profile staging
```

Every reply is one JSON object: `columns` with their types, `rows` with typed values, `row_count`,
`truncated`. `pgbx query` runs one SELECT-style statement inside a read-only transaction with a statement
timeout, and refuses writes and functions with side effects. That is a guard for agents, not a security
boundary: for a hard guarantee, connect as a role that can only read.

## Agent memory per database

Your agent keeps what it learns about each database in plain Markdown files on your machine:

```
~/pgbx/<profile>/<db>/memories.md   # facts and named questions with their SQL
~/pgbx/<profile>/<db>/tables.md     # tables, columns and what they mean
```

It reads them before answering and writes them only when you ask ("remember this", "save this query as
orders today"). Move them between machines with `pgbx memories export` and `pgbx memories import`; see
[For AI agents](../../agents/skill/#memory-one-folder-per-database).

## What works without the extension

| command | without the extension |
|---|---|
| `setup client`, `profile`, `query`, `memories`, `skill`, `serve` | works |
| `status` | exit 0: `backups: off` and the sentence above |
| `doctor` | exit 0: an `info` line for the extension; through an adapter it runs the SQL checks and says that disk and log checks run on the database host |
| `now`, `verify`, `list`, `logs`, `overview`, `schedule`, `db-restore`, ... | refuse with "pgbx extension not installed on this server: backups are off ..." |
| `backups --from-s3`, `db-restore --from-s3` | work (they read S3 directly), if you have backups in a bucket |

## Turning backups on later

Nothing to migrate: run the [server install](../../getting-started/install/#on-the-database-server) on the
database server. Your profiles, adapters and memory stay as they are, and `pgbx status` starts showing backups.

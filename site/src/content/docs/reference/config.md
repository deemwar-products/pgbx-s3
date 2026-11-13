---
title: Configuration (config.yaml)
description: pgbx's one config file - adapters, profiles, where $VAR secrets come from, and the adapter protocol.
sidebar: { order: 5 }
---

pgbx keeps everything about your connections in one file, `config.yaml`. It holds **references, never
secrets**: a password is written as `$PGPASSWORD` and read from your environment (or your secrets source) when a
command runs. pgbx contains no connection code of its own: a connection is a plain connection string or an
**adapter**, an external program that hands pgbx a connection string
([ADR 0003](https://github.com/deemwar-products/pgbx/blob/main/docs/adr/0003-connection-adapters.md)).

## Where it lives

| | path |
|---|---|
| Linux, macOS | `~/.config/pgbx/config.yaml` (`$XDG_CONFIG_HOME/pgbx/config.yaml` when set) |
| Windows | `%APPDATA%\pgbx\config.yaml` |
| any | `$PGBX_CONFIG_DIR/config.yaml` when `PGBX_CONFIG_DIR` is set |

pgbx writes it with mode 0600 (its directory 0700). Treat it like `~/.ssh/config`: it names commands pgbx runs.
`pgbx profile add | edit | remove | use` rewrite it for you; you can also edit it by hand (comments are not
kept when pgbx rewrites it). `pgbx memories import` never brings profiles or adapters with it.

## A full example

```yaml
default: prod                       # the profile used when none is named (pgbx profile use NAME)
secrets: env                        # env (default) | a .env file | a handler command, see below
adapters:                           # name -> the command that runs it (no shell; a string or a list)
  ssh: node ~/.config/pgbx/adapters/ssh/ssh-adapter.js
  aws: node ~/.config/pgbx/adapters/aws/aws-adapter.js
  corp: [/usr/local/bin/corp-pg, --region, eu]
profiles:
  dev:                              # a plain connection string
    url: postgres://$PGUSER:$PGPASSWORD@localhost:5432/shop
  prod:                             # an adapter + whatever settings that adapter reads
    adapter: ssh
    target: ops@db1.example.com
    pg_host: localhost
    user: app
    password: $PGPASSWORD
    dbname: shop
    s3-endpoint: https://s3.eu-central-1.amazonaws.com    # pgbx's own keys sit next to them
    s3-bucket: my-backups
    server-name: db1
    credentials-file: ~/.config/pgbx/prod.credentials
  prod-eu:
    adapter: aws
    target: i-0abc123def4567890
    db_host: shop.cluster-xyz.eu-west-1.rds.amazonaws.com
    region: eu-west-1
    aws_profile: acme-prod
    user: app
    password: ${SHOP_DB_PASSWORD}
    ready_timeout: 60s
```

## Adapters

`adapters:` maps a name to a command. A string is split into words like a shell would (quotes work, no
variables, globs or pipes) and run **without** a shell; a list is used as is. A leading `~/` is expanded. The
command runs in the config directory, so a relative path is relative to it.

The installer copies four examples to `<config dir>/adapters/` (`PGBX_ADAPTERS_DIR` overrides): `ssh`, `aws`,
`gcp` and `azure`. They are Node scripts (Node 18+); pgbx itself never needs Node. `pgbx profile add NAME
--adapter ssh ...` registers the example by itself when it is there; `--adapter-command CMD` defines any other
adapter, and the reply says exactly what it will run. See
[Connect through SSH, AWS, GCP, Azure or your own adapter](../../guides/adapters/).

## Profiles

A profile has either `url:` or `adapter:`, never both.

- `url:` a connection string, URL (`postgres://user:pass@host:port/db?...`) or key=value
  (`host=... user=... dbname=...`).
- `adapter:` an adapter name, plus any settings: everything that is not one of pgbx's own keys goes to the
  adapter, with `$VAR`s expanded.

pgbx's own keys:

| key | meaning |
|---|---|
| `admin-db` | the admin database (default: `pgbx.admin_db`, else `postgres`) |
| `s3-endpoint`, `s3-bucket`, `s3-region`, `server-name`, `credentials-file` | for `--from-s3`; S3 keys stay in the credentials file |
| `ready_timeout` | how long the adapter may take to answer (`30s` default; `60s`, `2m`) |
| `sslmode`, `sslrootcert` | TLS to Postgres (see [TLS](#tls)); an adapter gets them too, in its settings |

Without `--db`, a command uses the database named in the connection string, else `postgres`. A connection
string without a user uses `PGUSER`, else `postgres`; without a password, `PGPASSWORD` (or `~/.pgpass` for
`pg_restore`).

## Which connection a command uses

1. `--url URL` (one run, no profile)
2. `--profile NAME`
3. `--host` / `--port` flags (direct, no profile)
4. `PGBX_URL`
5. `PGBX_PROFILE`
6. the default profile
7. `PGHOST` / `PGPORT` / `PGUSER` and the built-in defaults (`/var/run/postgresql`, `5432`, `postgres`)

## TLS

Every CLI connection to Postgres (`query`, `status`, `doctor`, `jobs`, `serve`, `setup client`,
`db-restore --from-s3`, ...) uses TLS the way `psql` does, with libpq's `sslmode`:

| `sslmode` | TLS | certificate check |
|---|---|---|
| `disable` | never | |
| `allow`, `prefer` (the default) | when the server offers it, else plain | none |
| `require` | always | none (encrypted, but not authenticated) |
| `verify-ca` | always | the certificate chains to a trusted root; the host name is not checked |
| `verify-full` | always | trusted root, and the certificate names the host you connect to |

Trusted roots for `verify-ca` / `verify-full`: the PEM file in `sslrootcert`; else `~/.postgresql/root.crt` if
it exists (as libpq); else the Mozilla roots built into pgbx plus your operating system's store
(`sslrootcert=system` asks for these explicitly and, alone, means `verify-full`, as in libpq 16).

Where the settings come from, first match wins, for each of the two:

1. the connection string: `postgres://...?sslmode=verify-full&sslrootcert=/etc/pgbx/ca.pem`, or
   `sslmode=... sslrootcert=...` in key=value form (an adapter's URL counts here);
2. the profile's `sslmode:` / `sslrootcert:` keys (`$VAR`s and `~/` allowed);
3. `PGSSLMODE` / `PGSSLROOTCERT`;
4. the defaults above.

A Unix socket never uses TLS (libpq ignores `sslmode` there too). `pg_restore` (for `db-restore --from-s3`) gets
the same settings as `PGSSLMODE` / `PGSSLROOTCERT`; when pgbx used its default roots that is
`PGSSLROOTCERT=system`, which needs `pg_restore` 16 or newer (older ones: set `sslrootcert` to a file). pgbx
has no client certificates (`sslcert` / `sslkey`). TLS is rustls: no OpenSSL, still one static binary.

```yaml
profiles:
  prod:
    url: postgres://app:$PGPASSWORD@shop.cluster-xyz.eu-west-1.rds.amazonaws.com:5432/shop?sslmode=verify-full
    sslrootcert: ~/.config/pgbx/rds-global-bundle.pem
```

## `$VAR` references

Anywhere in a profile, in `--url` or in `PGBX_URL`:

- `$NAME` and `${NAME}` are replaced when a command runs; `$$` is a literal `$`. A `$` not followed by a name
  stays as it is.
- A missing (or empty) variable is an error that names it: `$PGPASSWORD is not set (looked in the
  environment)`. Its value is never printed.
- Values that land in the `user:password@` part of a URL are percent-encoded for you, so a password with `@`,
  `:` or `/` in it works.
- Type them in **single quotes** so your shell leaves them for pgbx:
  `pgbx profile add dev --url 'postgres://app:$PGPASSWORD@db:5432/shop'`.
- pgbx refuses to store a literal password: in a `url`, or in a setting whose name contains `password`,
  `passwd`, `secret` or `token`.

## Secrets

Each `$VAR` comes from the **process environment first**, so `PGPASSWORD=... pgbx ...` always wins. Then from
the `secrets:` source:

| `secrets:` | |
|---|---|
| `env` | the default: the environment only |
| `.secrets/.env`, `/path/db.env` | a .env file (a path ending in `.env`; relative to the config dir): `KEY=value` lines, `export KEY=value`, `#` comments, optional single or double quotes |
| `node secret-handler.js` | any other value is a command: pgbx runs it once per variable as `<command> NAME`; stdout is the value (one trailing newline stripped). A non-zero exit, empty output or no answer within 10 s is an error that names the variable, never a value |

pgbx ships no secret-manager integrations; a handler is a few lines. For `sec`, Vault, 1Password, AWS Secrets
Manager and so on:

```sh
#!/bin/sh
# secrets: sh ~/.config/pgbx/secret.sh
case "$1" in
  PGPASSWORD) exec aws secretsmanager get-secret-value --secret-id shop/db --query SecretString --output text ;;
  *) echo "unknown secret $1" >&2; exit 1 ;;
esac
```

```js
// secrets: node secret-handler.js
const { execFileSync } = require('node:child_process');
const name = process.argv[2];
const ids = { PGPASSWORD: 'op://prod/shop-db/password' };
if (!ids[name]) { console.error(`unknown secret ${name}`); process.exit(1); }
process.stdout.write(execFileSync('op', ['read', ids[name]]));
```

## Redaction

Values are used in memory only. Every output, `--json` or text, and every `pgbx serve` API answer, masks
passwords in connection strings (`postgres://app:***@db:5432/shop`, `password=***`) and replaces any value that
came from the secrets source, or from a secret-looking environment variable (`PGPASSWORD`, `*_TOKEN`, ...), with
`***`. `pgbx profile show` and `list` mask the same way. Child tools (`pg_restore`) get the password through
their environment, never their arguments.

## The adapter protocol (v1)

1. pgbx starts the adapter's command in its own process group (Unix) or Job Object (Windows), with stdin and
   stdout as pipes and stderr captured.
2. It writes one line: `{"action":"start","name":"prod","config":{...}}`, the profile's adapter settings with
   `$VAR`s expanded.
3. The adapter prints **exactly one line** on stdout within `ready_timeout` (30 s):
   `{"url":"postgres://...","state":"ready","name":"prod"}`. Any other `state` is a failure and pgbx shows it
   with the adapter's stderr (masked). `name` must match the profile. Anything else on stdout first (a log line)
   is a protocol error: logs go to stderr.
4. pgbx uses the URL and does not talk to the adapter again until it is done.
5. Then it writes `{"action":"stop"}` and closes stdin (a closed stdin also means stop, so the adapter exits if
   pgbx dies), waits up to 5 s, and kills what is left of the process group (SIGTERM, then SIGKILL; on Windows
   the Job Object is terminated). Ctrl-C does the same before pgbx exits.

A one-off command starts its adapter on its first connection and stops it when it finishes. `pgbx serve` keeps
one adapter per connection for its whole run.

The URL an adapter returns may carry `sslmode` / `sslrootcert`; they win over the profile's keys (see [TLS](#tls)).

## Migrating from `profiles.json` (pgbx 0.5)

The first time pgbx 0.6 runs and finds `profiles.json` but no `config.yaml`, it moves the profiles over, tells
you once, and keeps the old file as `profiles.json.migrated`:

- a direct profile (`host`, `port`, `user`) becomes `url: postgres://user@host:port/` (a socket directory is
  percent-encoded in the host);
- an SSH profile (`ssh`, `ssh-port`, `ssh-jump`, `host`, `port`, `user`) becomes `adapter: ssh` with `target`,
  `ssh_port`, `jump`, `pg_host`, `pg_port`, `user`, and `adapters: ssh:` points at the example in
  `<config dir>/adapters/ssh/` (copy `adapters/` from the release or the repo there if it is missing);
- `tunnel-idle` is dropped: there are no background tunnels any more.

The S3 keys and `admin-db` are carried over as they were.

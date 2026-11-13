---
title: Upgrading
description: Move to a new pgbx version.
sidebar: { order: 11 }
---

1. Install the new extension files and restart Postgres (the worker is a preloaded library).
2. The worker runs `ALTER EXTENSION pgbx UPDATE` by itself in every database and in `template1` whenever the
   installed version is older than the library's, and logs it. New databases are therefore never born at an old
   version. To do it by hand:

```sql
ALTER EXTENSION pgbx UPDATE;
```

3. Update the `pgbx` CLI by re-running the installer: `curl -fsSL https://pgbx.deemwar.com/install.sh | sh` (it also refreshes the agent skill).

## 0.6.0 → 0.6.1

Upgrade now if you back up to an **https** S3 endpoint (AWS, R2, Hetzner, Backblaze...): in 0.6.0 every backup job
to one failed ("Could not automatically determine the process-level CryptoProvider"). Plain-http endpoints were not
affected. No schema change; install the new files, restart Postgres, and re-run the CLI installer.

## 0.5 → 0.6

- **Built-in SSH is gone.** `--ssh`, `--ssh-port`, `--ssh-jump`, `--tunnel-idle` and `pgbx tunnel` now fail with a
  pointer to the ssh adapter: `pgbx profile add prod --adapter ssh target=ops@db1 user=app 'password=$PGPASSWORD'`.
  See [Connect through an adapter](../adapters/).
- **Profiles move to `config.yaml`.** An existing `profiles.json` is migrated on first use and kept as
  `profiles.json.migrated`. See [Configuration](../../reference/config/).
- **TLS by default.** The CLI now uses TLS whenever the server offers it (`sslmode=prefer`, like `psql`).
- The 0.5.0 → 0.6.0 update script brings every database to exactly a fresh 0.6.0 catalog; old call forms keep working.

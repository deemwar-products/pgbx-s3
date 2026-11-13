---
title: Testing & guarantees
description: What the test suites prove, and the known limits.
---

All suites run against fresh containers. Counts below are from `tests/run_all.sh` on PostgreSQL 16, 13 and 18
(2026-10-02, the same count on each major unless noted).

| suite | checks | proves |
|---|---|---|
| `tests/e2e.sh` | 85 | auto-install in new databases, first backup, schedules, retention, pause/resume, data scope, roles, one-database restore, verify, download links, against a real S3 bucket |
| `tests/cli_e2e.sh` | 68 | every `pgbx` command, `--json` shape, safety gates (`--yes`), `db-restore --from-s3` onto a plain Postgres container without the extension, Postgres-down paths, diagnose |
| `tests/ui_e2e.sh` | 23 | `pgbx ui`: pages, JSON, GET only, viewer role; no whole-server endpoint |
| `tests/client_only_e2e.sh` | 27 (PG16 only) | pgbx as a plain client: stock `postgres:16` with no extension and no S3, directly and through the ssh adapter to a host without pgbx; setup client, query, status, doctor, memories, skill exit 0; backup commands refuse plainly |
| `tests/ssh_e2e.sh` | 53 (PG16 only) | connections (ADR 0003) against a password-protected Postgres and an sshd: a url profile with `$PGPASSWORD` from the environment, a .env file and a secret handler; literal passwords refused; the ssh example adapter started and stopped per command; host-side commands refused; adapter errors shown; `profiles.json` migration; the query guard; the password never in output or on disk |
| `tests/tls_e2e.sh` | 35 (PG16 only) | TLS with libpq `sslmode`: disable / prefer / require / verify-ca / verify-full against a TLS-only Postgres, wrong-CA and wrong-name failures, `sslrootcert`, key=value strings, precedence, query/status/doctor through a `url:` profile, and the aws (IAM) and azure (Entra) example adapters over TLS |
| `tests/serve_e2e.sh` | 73 | `pgbx serve`: token, Host guard, safe actions only with `--allow-safe`, client-only server, one adapter for the whole run and Ctrl-C stops it |
| `tests/upgrade_e2e.sh` | 19 (PG16, with a 0.5.0 image), 6 (PG13 / PG18: no 0.5.0 image of that major, so the update-script section is skipped; `pitr_e2e.sh` checks it there) | worker auto-update (`ALTER EXTENSION pgbx UPDATE`, incl. `template1`); with `PGBX_PREV_IMAGE` (a 0.5.0 image) the 0.5.0 → 0.6.0 update script gives exactly a fresh 0.6.0 install (functions, columns, privileges) and old call forms keep working |
| `tests/queue_e2e.sh` | 63 | the server-wide job queue: slots, pick order, overrun skip, cancel, time estimates, crash recovery |
| `tests/load_e2e.sh` | 32 | quiet-window suggestion and the load gate under pgbench load |
| `tests/extras_e2e.sh` | 59 | 0.6 extras in throwaway containers with their own S3: encrypted dumps (ciphertext in S3, restore tests and restores decrypt, older plain dumps still restore), wrong / missing key, truncated and tampered files fail loudly, `pgbx decrypt`; roles file next to every dump, `restore(with_roles)` keeps owners and never changes existing roles, `--from-s3 --with-roles` on a new server creates only the referenced roles and is idempotent; webhook notifications (one per incident, one recovered, no URL leaked); GFS retention; `pgbx metrics` and `GET /metrics` |
| `tests/pitr_e2e.sh` | 52 | point-in-time restore in throwaway containers: `setup pitr` refuses a foreign `archive_command`; base backup job; restore to a time into a copy (rows before the time present, after it absent; the copy never archives); wal-push idempotency and checksum refusal; S3 down → drops past `pgbx.wal_queue_max` → gap + alert, no false "recovered", a new database still gets the extension at once, restore inside the gap refused, healing base backup closes it; status / doctor / privileges; 0.5.0 → 0.6.0 update == fresh catalog; prompt shutdown during a base backup |
| `tests/s3down_e2e.sh` | 11 | S3 that never answers: the worker's poll loop never waits for it (new databases get the extension within seconds, heartbeat moves, `pgbx jobs` answers), cancel of a stuck backup, prompt shutdown |
| `tests/https_s3_e2e.sh` | 6 | real https to AWS S3 with dummy keys, from the extension's backup job and the CLI: AWS answering `InvalidAccessKeyId` / 403 proves the TLS handshake and the signed request; a panic fails. Reproduces the 0.6.0 HTTPS bug (4 of 6 fail) and passes on 0.6.1. Needs internet, no account |
| `tests/release_https_proof.sh vX` | release check | a **published** release, installed by the public `install.sh` into `postgres:16`, backs up 200,000 rows to a real `https://` S3 bucket (Hetzner) and restores them into a new database; row count and md5 must match. v0.6.1: pass, same md5 on both sides ([log](https://github.com/deemwar-products/pgbx/blob/main/docs/proofs/2026-10-04-v0.6.1-https-s3.log)); v0.6.0: the backup panics (CryptoProvider) |
| AWS Marketplace AMI checks (real EC2) | release check | pgbx 0.6.1 on the AMI, real EC2 (us-east-1, t3.medium, x86_64), IMDSv2 only, an instance role and **no keys**, real Amazon S3 over https: backup + restore of 10,000 rows with matching md5, with the instance cut off from the internet (S3 only through a VPC gateway endpoint, 0 downloads at first boot); and a buyer-style run (TLS connect, wrong password and non-TLS refused, backup objects seen in the bucket from outside). 2026-10-04 |
| `tests/imds_e2e.sh` | 39 (PG16) | S3 credentials from the EC2 instance role (`pgbx.credentials_file = 'aws-default'`): a fake IMDSv2 hands out real MinIO STS session credentials that rotate every 30 s and disables the retired ones; doctor names the source, backup and restore work, a ~90 s multipart upload spans two rotations with no retry and restores to the same data, the CLI (`backups` / `db-restore --from-s3`, `pitr list`, `setup server --credentials aws-default`) works, no key or token in logs, history, doctor or CLI output, and an IMDSv1-only endpoint or no role attached is refused with the reason |
| `tests/diag_smoke.sh`, `tests/ui_smoke.sh` | to be re-measured | throwaway-container smokes: doctor/diagnose with pg_wal piling up; UI + audit retention |
| `tests/bench_local.sh` | 11 | ~1.2 GB against a local S3: backup and restore throughput, S3 outages, fast shutdown |
| `tests/pitr_bench.sh` | numbers, not checks | wal-push latency, archiving throughput of a pgbench backlog, wal-get replay with and without prefetch, ~2 GB base backup + point-in-time restore, each next to pgBackRest in the same container (benchmark only) |

`tests/run_all.sh` rebuilds, starts fresh containers, uses a unique S3 folder and runs the suites.
Set `PG_MAJOR=13`…`18` to run them on another Postgres major (needs a dev image built with that `PG_MAJOR`).
Unit tests: `cd cli && cargo test` (131: argument parsing, safety levels, S3 key selection, diagnose classification,
policy, metrics, PITR target/retention/gap rules, WAL names, config.yaml and its migration, `$VAR` expansion and
secret sources, the adapter protocol with a fake adapter, TLS modes, redaction) and `cli/tests/adapter_cli.rs` runs
the binary (an adapter stopped after a command, seeing EOF when pgbx is killed, stopped on Ctrl-C);
`cargo test --lib` in the dev image (49: encryption format incl. tamper / truncation / reorder / wrong key, roles
plan, GFS, notifications against local fakes, worker); `cd adapters && npm test` tests the example adapters.
Ignored on purpose (they need a real S3 or measure speed): `crypt_throughput`, `s3_roundtrip_large`.

## Robustness built in

- Streaming: `pg_dump` → 16 MB multipart parts → S3, no temp file. Restore streams S3 → `pg_restore`.
- Each part is retried with backoff (~2 min). A restore download resumes from the byte it reached.
- Half-finished uploads are aborted; orphaned ones (the worker's own dumps and roles files, older than 10 minutes)
  are cleaned at startup on a thread of their own, so a base backup another `pgbx` process is uploading is never cut
  off and an unreachable S3 never holds up the worker.
- A Postgres shutdown never waits on a backup (stops within ~1 s, at most 5 s even when an S3 request hangs; the job
  is marked interrupted).

## Known limits

- A restore is only as fine-grained as the database's schedule: you get the newest dump at or before the
  time you ask for. Back up more often (`every 15 minutes`) where that matters.
- Between dumps, point-in-time restore needs the optional whole-server [PITR](../guides/point-in-time-restore/)
  (`pgbx.pitr`, 0.6.0).
- The full suite runs on PostgreSQL 13, 16 and 18 (`PG_MAJOR`); 14, 15 and 17 were not run for this release.

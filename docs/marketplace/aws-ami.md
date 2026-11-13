# AWS Marketplace AMI: spec for the marketplace agent

Free AMI: self-managed PostgreSQL on EC2 with pgbx backups built in. Standing rule for every listing: "pgbx is free
(MIT). The only paid offers are installation, support and training: io@deemwar.com." Status: **spec, not built.** Owner decisions are in docs/adr/0003 (marketplace section).

## 1. Postgres

- The pgbx extension supports **PostgreSQL 13–18** (amd64 and arm64). Ship **PostgreSQL 17** by default (18 is
  newest but younger); one AMI per arch (x86_64 and arm64/Graviton).
- **Ubuntu 24.04 LTS + PGDG apt** (`apt.postgresql.org`), not Amazon Linux 2023. Release artifacts are built on
  Ubuntu 22.04 (glibc 2.35) and load on Debian 12 / Ubuntu 22.04+; AL2023 has no PGDG apt and would need RPM
  packaging we don't have or test.

## 2. Install (in the Packer build)

```sh
apt-get install -y postgresql-17            # from PGDG
curl -fsSL https://pgbx.deemwar.com/install.sh | sh -s -- --version v0.6.0 --pg-version 17 --no-skill
```

- `install.sh` checks every file against the release's SHA256SUMS and installs the pgbx CLI to /usr/local/bin and
  the extension files for every local Postgres. It **never edits config or restarts anything**. Pin `--version`;
  **v0.6.0 must be released first** (today only v0.5.0 is; the owner runs the manual Release workflow).
- No system deps beyond Postgres itself (`pg_dump` / `pg_restore` come with it) and `curl` + `sha256sum` for the
  installer. The CLI is a static binary.
- Config: `shared_preload_libraries = 'pgbx'` (one restart), plus `pgbx.s3_endpoint`, `pgbx.s3_bucket`,
  `pgbx.s3_region`, `pgbx.server_name` and `pgbx.credentials_file = 'aws-default'` (no keys file: the instance
  role, see §3). `sudo pgbx setup server --credentials aws-default --yes` writes all of them into
  `conf.d/pgbx.conf` and prints the restart command. **No `CREATE EXTENSION` is needed:** the pgbx
  worker creates the extension in `template1` and every database by itself (name: `pgbx`, schema `pgbx`).
- Optional point-in-time restore: `sudo pgbx setup pitr --yes` (sets `archive_mode`/`archive_command`; another
  restart). Leave it off by default; document it.

## 3. First boot (per instance)

- Generate the `postgres` password into a root-only file (e.g. `/root/pgbx-ami/postgres-password`, 0600); no
  default password anywhere in the image.
- `listen_addresses = '*'` with `pg_hba.conf` allowing only the VPC CIDR (from IMDSv2), `scram-sha-256`; TLS on
  with a self-signed cert generated at first boot (`ssl = on`). Clients connect with `sslmode=require`; pgbx's CLI
  supports it.
- **pgbx-specific:** `pgbx.server_name` = the instance id (IMDSv2), so each instance gets its own folder in the
  bucket. S3 bucket/region come from CloudFormation parameters (or EC2 user data), written by
  `pgbx setup server --yes`.
- **S3 credentials: the instance role, no keys anywhere.** `pgbx.credentials_file = 'aws-default'` (written by
  `pgbx setup server --credentials aws-default`): the worker, its jobs, `wal-push` / `wal-get` and the CLI take
  temporary credentials from the EC2 instance role through **IMDSv2** (session token first; IMDSv1 is never used),
  cache them and fetch new ones 5 minutes before they expire, so a long upload runs across a refresh. Launch the
  instance with `HttpTokens=required` (IMDSv2 only); `HttpPutResponseHopLimit` 1 is fine for Postgres on the host
  (2 if it ever runs in a container). The CloudFormation template attaches an instance profile whose role has this
  policy, scoped to the customer's bucket only:

  ```json
  {"Version": "2012-10-17", "Statement": [
    {"Effect": "Allow", "Action": ["s3:ListBucket", "s3:ListBucketMultipartUploads"],
     "Resource": "arn:aws:s3:::BUCKET"},
    {"Effect": "Allow",
     "Action": ["s3:PutObject", "s3:GetObject", "s3:DeleteObject", "s3:AbortMultipartUpload", "s3:ListMultipartUploadParts"],
     "Resource": "arn:aws:s3:::BUCKET/*"}]}
  ```

  (`ListBucketMultipartUploads` lets the worker clean up uploads a crash left behind; without it that cleanup is
  skipped and logged, nothing else breaks.) `SELECT detail FROM pgbx.doctor() WHERE name = 's3 credentials'`
  shows `source: instance-role (instance role via IMDSv2 (role NAME), temporary, valid until ...)`; with no role
  attached it says so and the fix names this policy. Download links (`pgbx.download_url`) signed with role
  credentials stop working when that role session ends (at most ~6 hours), whatever interval was asked for.
- Optional: encryption for dumps (`pgbx.encryption_key_file`: 32 random bytes as base64 or hex, generated at first boot, postgres
  only, chmod 600). Tell the customer to back up that key; without it the dumps can't be restored.

## 4. Smoke test (Packer build and launch test)

With S3 configured:

```sql
-- in the postgres database, as postgres
SELECT name, ok, detail FROM pgbx.doctor() WHERE NOT ok;      -- expect 0 rows (advice rows may warn)
CREATE DATABASE ami_smoke;
\c ami_smoke
SELECT * FROM pgbx.status();                                  -- state shows 'waiting for first backup' or 'ok'
SELECT pgbx.backup_now();                                     -- returns a job id; poll:
SELECT state, error FROM pgbx.history WHERE kind = 'backup' ORDER BY id DESC LIMIT 1;   -- 'done' within ~1 min
```

Without S3 (Packer build, no bucket yet): `SELECT extversion FROM pg_extension WHERE extname = 'pgbx'` returns
`0.6.0` in `template1`, and `pgbx --version` prints `pgbx 0.6.0`. `pgbx doctor --json` reports only the
"s3 settings" check as not ok (and "s3 credentials" if the build instance has no instance profile).

## 5. Listing text

- Docs: **https://pgbx.deemwar.com/** (no muthuishere links anywhere).
- Description:
  > PostgreSQL with backups that look after themselves. This AMI runs PostgreSQL 17 with pgbx, an open-source (MIT)
  > extension that backs up every database to your own S3 bucket automatically, including databases you create
  > later, and restores any of them with one command into a new database, even onto a new server. Backups are plain
  > pg_dump files you own: portable, restorable anywhere, kept on your schedule and retention. Optional
  > point-in-time restore and encryption. pgbx is free (MIT). The only paid offers are installation, support and
  > training: io@deemwar.com.

## 6. The AMI must NOT include

- Any S3 keys, passwords, encryption keys or certificates (all generated or provided at first boot).
- Telemetry or phone-home of any kind (pgbx has none; don't add any).
- The agent skill (`--no-skill`), memory files (`~/pgbx/`) or CLI profiles.
- The future "runner" mode (for RDS/Cloud SQL): not built; a separate container listing later.
- The `/metrics` endpoint or `pgbx serve` exposed to the network (both local-only by default; keep it that way).

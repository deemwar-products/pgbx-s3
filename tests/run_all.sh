#!/usr/bin/env bash
# Full verification from scratch: rebuild the image, fresh containers, unique S3 folder per run, then
#   1) tests/e2e.sh        — every per-database feature against the real S3 bucket
#   2) tests/cli_e2e.sh    — the pgbx CLI (policy, refusals, skill, db-restore --from-s3 onto a server without pgbx)
#      tests/ui_e2e.sh     — pgbx ui: read-only audit UI (pages, JSON, GET only, viewer role)
#      tests/serve_e2e.sh  — pgbx serve: the web app (token, Host guard, safe actions only with --allow-safe, client-only)
#      tests/queue_e2e.sh  — the server-wide job queue: slots, pick order, overrun skip, cancel, ETA, crash recovery (restarts db)
#      tests/load_e2e.sh   — quiet-window suggestion and the load gate (pgbench)
#      tests/extras_e2e.sh — 0.6 extras: encryption, roles file + with_roles, notifications, GFS, metrics (own S3)
#      tests/pitr_e2e.sh   — optional point-in-time restore: setup, wal-push/get, restore to a time, gaps, upgrade (own S3)
#      tests/ext_e2e.sh    — pgmq queues and pg_cron jobs through backup, restore test and restore (own S3)
#      tests/s3down_e2e.sh — S3 that never answers: new databases, heartbeat, cancel and shutdown never wait for it
#      tests/https_s3_e2e.sh — real https to AWS S3 with dummy keys: the TLS path itself (0.6.0 panicked there)
#      tests/imds_e2e.sh   — S3 credentials from the EC2 instance role (IMDSv2, fake endpoint, rotating MinIO STS keys)
#   3) tests/bench_local.sh — ~1.2 GB speed + S3-outage chaos against a local S3
#   4) tests/upgrade_e2e.sh — worker auto-update (ALTER EXTENSION UPDATE incl. template1)
# PG_MAJOR=13..18 picks the Postgres major (needs a dev image built with that PG_MAJOR, see docker/Dockerfile).
set -u
cd "$(dirname "$0")/../docker"
export PGBX_SERVER="pgbx-test-$(date -u +%Y%m%d%H%M%S)"
echo "== run $PGBX_SERVER"
docker compose -f compose.test.yml down -v >/dev/null 2>&1
docker compose -f compose.local.yml down -v >/dev/null 2>&1
docker compose -f compose.test.yml build -q || exit 1
docker compose -f compose.test.yml up -d >/dev/null 2>&1
# wait for the REAL server: the image first runs a temporary one for initdb and stops it, so a fixed sleep is not enough
for _ in $(seq 300); do docker compose -f compose.test.yml logs db 2>&1 | grep -q "init process complete" && break; sleep 1; done
ok=0; for _ in $(seq 120); do
  if docker compose -f compose.test.yml exec -T db psql -U postgres -qAt -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 3 ] && break; else ok=0; fi; sleep 1
done
echo "== server ready"
../tests/e2e.sh; e2e=$?
../tests/cli_e2e.sh; cli=$?
../tests/ui_e2e.sh; ui=$?
../tests/serve_e2e.sh; serve=$?
../tests/queue_e2e.sh; queue=$?
../tests/load_e2e.sh; load=$?
IMAGE=pgbx:test ../tests/extras_e2e.sh; extras=$?
PITR_IMAGE=pgbx:test ../tests/pitr_e2e.sh; pitr=$?
IMAGE=pgbx:test ../tests/ext_e2e.sh; ext=$?
IMAGE=pgbx:test ../tests/s3down_e2e.sh; s3down=$?
IMAGE=pgbx:test ../tests/imds_e2e.sh; imds=$?
PGBX_IMAGE=pgbx:test ../tests/https_s3_e2e.sh; https=$?   # real https to AWS S3 (dummy keys; needs internet)
../tests/bench_local.sh; bench=$?
(set -a; [ ! -f ./.env ] || . ./.env; set +a; ../tests/upgrade_e2e.sh); upgrade=$?   # S3 env: docker/.env, else the environment
diag=0
if [ "${PGBX_DIAG_SMOKE:-0}" = 1 ]; then   # optional: pgbx doctor/diagnose with pg_wal piling up, no S3 (throwaway container)
  (cd .. && docker run --rm --name pgbx-diag-smoke -v "$PWD":/src -v pgbx-diag-target:/src/target \
     -v pgbx-diag-clitarget:/src/cli/target -w /src pgbx-dev sh tests/diag_smoke.sh); diag=$?
fi
# disk hygiene: the stacks go away with their volumes, and a LOCAL test bucket is emptied (never a real one)
docker compose -f compose.test.yml down -v >/dev/null 2>&1
docker compose -f compose.local.yml down -v >/dev/null 2>&1
(set -a; [ ! -f ./.env ] || . ./.env; set +a
 case "$S3_ENDPOINT" in
   *orb.local*|*localhost*|*127.0.0.1*|*host.docker.internal*)
     docker run --rm --env-file local.env amazon/aws-cli --endpoint-url "$S3_ENDPOINT" s3 rm --recursive --quiet "s3://$S3_BUCKET/" >/dev/null 2>&1 \
       && echo "== emptied the local test bucket $S3_BUCKET" ;;
 esac)
echo "== e2e exit $e2e, cli_e2e exit $cli, ui_e2e exit $ui, serve_e2e exit $serve, queue_e2e exit $queue, load_e2e exit $load, extras_e2e exit $extras, pitr_e2e exit $pitr, ext_e2e exit $ext, s3down_e2e exit $s3down, imds_e2e exit $imds, https_s3_e2e exit $https, bench exit $bench, upgrade exit $upgrade, diag smoke exit $diag"
[ $e2e -eq 0 ] && [ $cli -eq 0 ] && [ $ui -eq 0 ] && [ $serve -eq 0 ] && [ $queue -eq 0 ] && [ $load -eq 0 ] && [ $extras -eq 0 ] && [ $pitr -eq 0 ] && [ $ext -eq 0 ] && [ $s3down -eq 0 ] && [ $imds -eq 0 ] && [ $https -eq 0 ] && [ $bench -eq 0 ] && [ $upgrade -eq 0 ] && [ $diag -eq 0 ]

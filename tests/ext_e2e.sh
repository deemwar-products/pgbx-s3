#!/usr/bin/env bash
# pgmq queues and pg_cron jobs through backup / restore test / restore (0.6.2), in throwaway containers (bash 3.2-safe).
#   - pgmq queues are extension members, which pg_dump never dumps: pgbx saves them next to the dump
#     (<ts>.ext.sql.zst) and a restore brings back the queue, its indexes, its messages, its archive and its grants;
#     the msg_id sequence continues after the restored messages
#   - pg_cron lives only in cron.database_name: a restore into another database leaves it out and still succeeds
#   - the restore test (verify) passes on such a database (it failed with 0.6.1)
# IMAGE (pgbx:test, built by tests/run_all.sh). PGBX_EXT_KEEP=1 leaves the containers running.
set -u
IMAGE=${IMAGE:-pgbx:test}; PG_MAJOR=${PG_MAJOR:-16}; PGMQ_REF=${PGMQ_REF:-v1.5.1}
NET=pgbx-ext-net; S3=pgbx-ext-s3; PG=pgbx-ext-pg; EXT_IMAGE=pgbx:test-ext
BUCKET=ext; SERVER="ext-$(date -u +%Y%m%d%H%M%S)"
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS $1"; pass=$((pass+1)); else echo "  FAIL $1 (got '$2', want '$3')"; fail=$((fail+1)); fi; }
P() { docker exec -i -u postgres "$PG" psql -v ON_ERROR_STOP=1 -qAt "$@"; }
cleanup() { docker rm -fv "$PG" "$S3" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; [ -n "${WORK:-}" ] && rm -rf "$WORK"; }
trap '[ "${PGBX_EXT_KEEP:-0}" = 1 ] || cleanup' EXIT
cleanup
WORK=$(mktemp -d)
AK="ext$RANDOM$RANDOM"; SK=$(LC_ALL=C tr -dc 'a-z0-9' </dev/urandom | head -c 32)
printf 'access_key_id=%s\nsecret_access_key=%s\n' "$AK" "$SK" > "$WORK/s3.credentials"
AWS() { docker run --rm --network "$NET" -e AWS_ACCESS_KEY_ID="$AK" -e AWS_SECRET_ACCESS_KEY="$SK" \
          -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli --endpoint-url "http://$S3:9000" "$@"; }

echo "== ext_e2e image $IMAGE + pg_cron + pgmq $PGMQ_REF (server folder $SERVER)"
# pg_cron from PGDG; pgmq is SQL only but installs through PGXS, so build it in a throwaway stage
cat > "$WORK/Dockerfile" <<EOF
FROM $IMAGE
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends postgresql-$PG_MAJOR-cron git make \
      postgresql-server-dev-$PG_MAJOR >/dev/null && \
    git clone -q --depth 1 -b $PGMQ_REF https://github.com/pgmq/pgmq /tmp/pgmq && \
    make -s -C /tmp/pgmq/pgmq-extension && make -s -C /tmp/pgmq/pgmq-extension install >/dev/null && rm -rf /tmp/pgmq /var/lib/apt/lists/*
EOF
docker build -q -t "$EXT_IMAGE" "$WORK" >/dev/null || { echo "  FAIL build $EXT_IMAGE"; exit 1; }
docker network create "$NET" >/dev/null
docker run -d --rm --name "$S3" --network "$NET" -e RUSTFS_ACCESS_KEY="$AK" -e RUSTFS_SECRET_KEY="$SK" rustfs/rustfs >/dev/null
for _ in $(seq 30); do AWS s3 mb "s3://$BUCKET" 2>&1 | grep -qE 'make_bucket|BucketAlready' && break; sleep 1; done
docker run -d --name "$PG" --network "$NET" -v "$WORK:/w:ro" -e POSTGRES_PASSWORD=test-only-not-secret "$EXT_IMAGE" \
  sh -c 'mkdir -p /etc/pgbx && cp /w/s3.credentials /etc/pgbx/ && chown postgres /etc/pgbx/s3.credentials &&
         chmod 600 /etc/pgbx/s3.credentials && exec docker-entrypoint.sh "$@"' -- \
  postgres -c shared_preload_libraries=pg_cron,pgbx -c cron.database_name=app \
  -c pgbx.s3_endpoint="http://$S3:9000" -c pgbx.s3_bucket=$BUCKET -c pgbx.s3_region=us-east-1 \
  -c pgbx.server_name="$SERVER" -c pgbx.credentials_file=/etc/pgbx/s3.credentials -c pgbx.poll_seconds=2 >/dev/null
for _ in $(seq 120); do docker logs "$PG" 2>&1 | grep -q "init process complete" && break; sleep 1; done
ok=0; for _ in $(seq 120); do if P -c "SELECT 1" >/dev/null 2>&1; then ok=$((ok+1)); [ $ok -ge 2 ] && break; else ok=0; fi; sleep 1; done

echo "## a database with pgmq queues and pg_cron jobs"
P -c "CREATE ROLE app_reader" -c "CREATE DATABASE app" >/dev/null
for _ in $(seq 30); do [ "$(P -d app -c "SELECT count(*) FROM pg_extension WHERE extname='pgbx'" 2>/dev/null)" = 1 ] && break; sleep 1; done
P -d app >/dev/null <<'SQL'
CREATE EXTENSION pgmq;
CREATE EXTENSION pg_cron;
CREATE TABLE users (id int PRIMARY KEY, name text);
INSERT INTO users VALUES (1, 'a'), (2, 'b');
SELECT pgmq.create('jobs');
SELECT pgmq.create_unlogged('fast');
SELECT pgmq.send('jobs', '{"n":1}'), pgmq.send('jobs', '{"n":2}'), pgmq.send('jobs', '{"n":3}'), pgmq.send('fast', '{"f":1}');
SELECT pgmq.archive('jobs', 1);
GRANT SELECT ON pgmq.q_jobs TO app_reader;
SELECT cron.schedule('nightly', '0 3 * * *', 'SELECT 1');
SQL
check "pg_dump alone would miss the queue tables (they are extension members)" \
  "$(P -d app -c "SELECT count(*) FROM pg_depend d JOIN pg_class c ON c.oid=d.objid WHERE d.deptype='e' AND c.relname IN ('q_jobs','a_jobs')")" 2

echo "## backup"
docker exec -u postgres "$PG" pgbx now --db app --wait >/dev/null 2>&1
check "backup done" "$(P -d app -c "SELECT state FROM pgbx.history WHERE kind='backup' ORDER BY id DESC LIMIT 1")" done
check "backup lists the queues" "$(P -d app -c "SELECT params->'pgmq_queues' FROM pgbx.history WHERE kind='backup' ORDER BY id DESC LIMIT 1")" '["fast", "jobs"]'
check "backup counts the cron jobs" "$(P -d app -c "SELECT params->>'cron_jobs' FROM pgbx.history WHERE kind='backup' ORDER BY id DESC LIMIT 1")" 1
key=$(P -d app -c "SELECT s3_key FROM pgbx.history WHERE kind='backup' ORDER BY id DESC LIMIT 1")
check "queues saved next to the dump" "$(AWS s3 ls "s3://$BUCKET/${key%.dump}.ext.sql.zst" 2>/dev/null | wc -l | tr -d ' ')" 1

echo "## restore test"
docker exec -u postgres "$PG" pgbx verify --db app --wait >/dev/null 2>&1
check "restore test passes (pg_cron left out, queues counted)" \
  "$(P -d app -c "SELECT state||coalesce(' '||error,'') FROM pgbx.history WHERE kind='verify' ORDER BY id DESC LIMIT 1")" done

echo "## restore into a new database"
docker exec -u postgres "$PG" pgbx db-restore --db app --into app_copy --wait >/dev/null 2>&1
check "restore done" "$(P -d app -c "SELECT state||coalesce(' '||error,'') FROM pgbx.history WHERE kind='restore' ORDER BY id DESC LIMIT 1")" done
check "app tables and rows" "$(P -d app_copy -c "SELECT count(*) FROM users")" 2
check "queue messages back (3 sent, 1 archived)" "$(P -d app_copy -c "SELECT count(*) FROM pgmq.q_jobs")" 2
check "archive back" "$(P -d app_copy -c "SELECT count(*) FROM pgmq.a_jobs")" 1
check "unlogged queue back" "$(P -d app_copy -c "SELECT count(*) FROM pgmq.q_fast")" 1
check "queues registered in pgmq.meta" "$(P -d app_copy -c "SELECT string_agg(queue_name, ',' ORDER BY queue_name) FROM pgmq.meta")" fast,jobs
check "queue indexes back" "$(P -d app_copy -c "SELECT count(*) > 0 FROM pg_indexes WHERE schemaname='pgmq' AND tablename='q_jobs'")" t
check "grant on the queue back" "$(P -d app_copy -c "SELECT has_table_privilege('app_reader', 'pgmq.q_jobs', 'SELECT')")" t
check "msg_id continues after the restored messages" "$(P -d app_copy -c "SELECT pgmq.send('jobs', '{\"n\":4}') > 3")" t
check "read works on the restored queue" "$(P -d app_copy -c "SELECT count(*) FROM pgmq.read('jobs', 30, 10)")" 3
check "pg_cron left out of the copy (it only runs in cron.database_name)" "$(P -d app_copy -c "SELECT count(*) FROM pg_extension WHERE extname='pg_cron'")" 0
check "restore result says pg_cron was left out" "$(P -d app -c "SELECT params ? 'cron_left_out' FROM pgbx.history WHERE kind='restore' ORDER BY id DESC LIMIT 1")" t
check "the original still has its cron job" "$(P -d app -c "SELECT count(*) FROM cron.job")" 1

echo "== ext_e2e: $pass passed, $fail failed"
[ $fail -eq 0 ]

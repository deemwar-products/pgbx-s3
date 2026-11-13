FROM postgres:16
ARG PGBX_VERSION
RUN apt-get update -qq && apt-get install -y -qq curl ca-certificates >/dev/null \
 && curl -fsSL https://pgbx.deemwar.com/install.sh | sh -s -- --version "$PGBX_VERSION" --pg-version 16 --no-skill --prefix /usr/local/bin
COPY entry.sh /entry.sh
ENTRYPOINT ["/entry.sh"]

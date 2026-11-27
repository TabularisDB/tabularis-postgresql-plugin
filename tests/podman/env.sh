#!/usr/bin/env bash
# Throwaway servers for the live tests CI cannot run: TLS (verify-full, mutual
# TLS), PgBouncer in transaction and session mode, and a real Unix socket.
#
#   tests/podman/env.sh up       start everything, print the env vars to export
#   tests/podman/env.sh env      print the env vars again
#   tests/podman/env.sh down     remove everything it created
#
# Needs: podman, openssl, psql, and (for the socket) a native PostgreSQL 14+
# (`brew install postgresql@16`), because a container's Unix socket does not
# cross the podman VM on macOS. Everything lives under $RO_ENV_DIR (default
# /private/tmp/ro-live-env) and in containers named ro-*; nothing else is touched.
# The plain server on :54320 (the CI service) must already be running: the
# PgBouncers sit in front of it.
set -euo pipefail

# Resolved: podman mounts the real path, and macOS /tmp is a symlink it will not follow.
DIR="${RO_ENV_DIR:-/private/tmp/ro-live-env}"
PG_IMAGE="${RO_PG_IMAGE:-docker.io/library/postgres:16}"
PGB_IMAGE="${RO_PGB_IMAGE:-docker.io/edoburu/pgbouncer:v1.23.1-p3}"
TLS_PORT=55433 TXN_PORT=64331 SESSION_PORT=64332 SOCK_PORT=55434
SOCK_DIR="$DIR/sock"
NATIVE_BIN="${RO_NATIVE_PG_BIN:-/opt/homebrew/opt/postgresql@16/bin}"

certs() {
  mkdir -p "$DIR/certs" && cd "$DIR/certs"
  [ -f ca.crt ] && return
  openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days 30 -subj "/CN=ro-test-ca" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\n' > san.ext
  openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=localhost" 2>/dev/null
  openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out server.crt -days 30 -extfile san.ext 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -keyout client.key -out client.csr -subj "/CN=mtls_user" 2>/dev/null
  # Must be X.509 v3 (an extensions file forces it); rustls rejects a v1 client certificate.
  printf 'basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=clientAuth\n' > client.ext
  openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out client.crt -days 30 -extfile client.ext 2>/dev/null
  # tokio-postgres-rustls reads PKCS#8, not the traditional RSA form.
  openssl pkcs8 -topk8 -nocrypt -in client.key -out client.pk8.key 2>/dev/null
  chmod 644 ./*.crt ./*.key
}

tls_server() {
  podman rm -f ro-tls >/dev/null 2>&1 || true
  cat > "$DIR/certs/pg_hba.conf" <<'HBA'
local   all  all                    trust
hostssl all  mtls_user  all         cert
hostssl all  all        all         scram-sha-256
host    all  all        all         reject
HBA
  podman run -d --name ro-tls -p "$TLS_PORT:5432" -v "$DIR/certs:/certs:ro" \
    -e POSTGRES_PASSWORD=password -e POSTGRES_DB=testdb "$PG_IMAGE" bash -c '
      mkdir -p /tls && cp /certs/server.crt /certs/server.key /certs/ca.crt /certs/pg_hba.conf /tls/ &&
      chown -R postgres:postgres /tls && chmod 600 /tls/server.key &&
      exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tls/server.crt \
        -c ssl_key_file=/tls/server.key -c ssl_ca_file=/tls/ca.crt -c hba_file=/tls/pg_hba.conf' >/dev/null
  until PGPASSWORD=password psql "host=127.0.0.1 port=$TLS_PORT user=postgres dbname=testdb sslmode=require" -Atc 'select 1' >/dev/null 2>&1; do sleep 1; done
  # Created through the trusted local socket, so no certificate is needed yet.
  podman exec ro-tls psql -U postgres -d testdb -qc \
    "CREATE ROLE mtls_user LOGIN; GRANT ALL ON SCHEMA public TO mtls_user;" >/dev/null
}

pgbouncer() { # name port mode
  podman rm -f "$1" >/dev/null 2>&1 || true
  # One backend, so the next client always lands on the connection the last one used.
  podman run -d --name "$1" -p "$2:5432" -e DB_HOST=host.containers.internal -e DB_PORT=54320 \
    -e DB_USER=postgres -e DB_PASSWORD=password -e POOL_MODE="$3" -e AUTH_TYPE=plain \
    -e ADMIN_USERS=postgres -e MAX_CLIENT_CONN=100 -e DEFAULT_POOL_SIZE=1 \
    -e MAX_PREPARED_STATEMENTS=0 "$PGB_IMAGE" >/dev/null
}

native_socket() {
  "$NATIVE_BIN/pg_ctl" -D "$DIR/native" stop -m immediate >/dev/null 2>&1 || true
  rm -rf "$DIR/native" "$SOCK_DIR" && mkdir -p "$SOCK_DIR"
  "$NATIVE_BIN/initdb" -D "$DIR/native" -U postgres --auth=trust >/dev/null
  "$NATIVE_BIN/pg_ctl" -D "$DIR/native" -w -l "$DIR/native.log" \
    -o "-p $SOCK_PORT -c listen_addresses='' -c unix_socket_directories='$SOCK_DIR'" start >/dev/null
  psql -h "$SOCK_DIR" -p "$SOCK_PORT" -U postgres -qc "CREATE DATABASE testdb" postgres >/dev/null
}

print_env() {
  cat <<ENV
export PG_PLUGIN_TEST_HOST=127.0.0.1 PG_PLUGIN_TEST_PORT=54320
export PG_PLUGIN_TEST_POOLER_PORT=$TXN_PORT
export PG_PLUGIN_TEST_SESSION_POOLER_PORT=$SESSION_PORT
export PG_PLUGIN_TEST_TLS_PORT=$TLS_PORT PG_PLUGIN_TEST_TLS_CERTS=$DIR/certs
export PG_PLUGIN_TEST_UNIX_SOCKET=$SOCK_DIR/.s.PGSQL.$SOCK_PORT
ENV
}

case "${1:-}" in
  up)
    command -v podman openssl psql >/dev/null
    pg_isready -h 127.0.0.1 -p 54320 >/dev/null || { echo "the CI-style server on :54320 is not running" >&2; exit 1; }
    certs; tls_server
    pgbouncer ro-pgb-txn "$TXN_PORT" transaction
    pgbouncer ro-pgb-session "$SESSION_PORT" session
    native_socket
    sleep 2; print_env ;;
  env) print_env ;;
  down)
    podman rm -f ro-tls ro-pgb-txn ro-pgb-session >/dev/null 2>&1 || true
    "$NATIVE_BIN/pg_ctl" -D "$DIR/native" stop -m immediate >/dev/null 2>&1 || true
    rm -rf "$DIR" ;;
  *) echo "usage: $0 up|env|down" >&2; exit 2 ;;
esac

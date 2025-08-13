#!/usr/bin/env bash
set -euo pipefail

# pgdb_ctl.sh - control script for local Postgres
# Usage:
#   ./scripts/pgdb_ctl.sh start [--pg-port 5432] [--data-dir ./.pg_data] [--name xtrader_pg] [--user xtrader] [--password xtrader] [--db xtrader]
#   ./scripts/pgdb_ctl.sh stop  [--name xtrader_pg]
#   ./scripts/pgdb_ctl.sh init  [--sql-dir ./sql/pg] [--name xtrader_pg] [--user xtrader] [--password xtrader] [--db xtrader]

CMD="${1:-}"
shift || true

# Defaults
PG_PORT="5432"
DATA_DIR="${PWD}/.pg_data"
CONTAINER_NAME="xtrader_pg"
SQL_DIR="${PWD}/sql/pg"
PG_USER="xtrader"
PG_PASSWORD="xtrader"
PG_DB="xtrader"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --pg-port) PG_PORT="$2"; shift 2;;
    --data-dir) DATA_DIR="$2"; shift 2;;
    --name) CONTAINER_NAME="$2"; shift 2;;
    --sql-dir) SQL_DIR="$2"; shift 2;;
    --user) PG_USER="$2"; shift 2;;
    --password) PG_PASSWORD="$2"; shift 2;;
    --db) PG_DB="$2"; shift 2;;
    *) echo "Unknown arg: $1"; exit 1;;
  esac
done

start_container() {
  mkdir -p "${DATA_DIR}"
  if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "Container ${CONTAINER_NAME} already exists. Restarting..."
    docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
  fi
  echo "Starting Postgres container ${CONTAINER_NAME}..."
  docker run -d \
    --name "${CONTAINER_NAME}" \
    -p ${PG_PORT}:5432 \
    -v "${DATA_DIR}:/var/lib/postgresql/data" \
    -e POSTGRES_USER="${PG_USER}" \
    -e POSTGRES_PASSWORD="${PG_PASSWORD}" \
    -e POSTGRES_DB="${PG_DB}" \
    postgres:16-alpine >/dev/null

  echo "Postgres is starting on port ${PG_PORT}"
  for i in {1..60}; do
    if docker exec -e PGPASSWORD="${PG_PASSWORD}" "${CONTAINER_NAME}" \
      psql -U "${PG_USER}" -d "${PG_DB}" -c 'select 1' >/dev/null 2>&1; then
      echo "Postgres is ready."
      return 0
    fi
    sleep 1
  done
  echo "Postgres did not become ready in time." >&2
  docker logs "${CONTAINER_NAME}" || true
  return 1
}

stop_container() {
  if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "Stopping ${CONTAINER_NAME}..."
    docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
    echo "Stopped."
  else
    echo "Container ${CONTAINER_NAME} not found."
  fi
}

init_schema() {
  if [[ ! -d "${SQL_DIR}" ]]; then
    echo "SQL directory not found: ${SQL_DIR}" >&2
    exit 1
  fi
  # Ensure running
  if ! docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "Container ${CONTAINER_NAME} is not running. Start it first." >&2
    exit 1
  fi
  echo "Initializing schema using SQL in ${SQL_DIR}"
  for f in "${SQL_DIR}"/*.sql; do
    [[ -e "$f" ]] || continue
    echo "Applying: $(basename "$f")"
    docker exec -i -e PGPASSWORD="${PG_PASSWORD}" "${CONTAINER_NAME}" \
      psql -U "${PG_USER}" -d "${PG_DB}" -v ON_ERROR_STOP=1 -f - < "$f"
  done
  echo "Schema initialization complete."
}

case "$CMD" in
  start) start_container;;
  stop) stop_container;;
  init) init_schema;;
  *) echo "Usage: $0 {start|stop|init} [options]"; { return 1; } 2>/dev/null || exit 1;;
esac

# Propagate function exit code without closing interactive shells when sourced
rc=$?
{ return $rc; } 2>/dev/null || exit $rc



#!/usr/bin/env bash
set -euo pipefail

# questdb_ctl.sh - control script for local/remote QuestDB
# Usage:
#   ./scripts/questdb_ctl.sh start   [--host HOST] [--http-port 9000] [--ilp-port 9009] [--pg-port 8812] [--ilp-http-port 9003] [--data-dir ./.questdb_data] [--name xtrader_questdb]
#   ./scripts/questdb_ctl.sh stop    [--name xtrader_questdb]
#   ./scripts/questdb_ctl.sh status  [--name xtrader_questdb]
#   ./scripts/questdb_ctl.sh init    [--host HOST] [--http-port 9000] [--sql-dir ./sql/questdb]

CMD="${1:-}"
shift || true

# Defaults
HOST="127.0.0.1"
HTTP_PORT="9000"
ILP_PORT="9009"
PG_PORT="8812"
ILP_HTTP_PORT="9003"
DATA_DIR="${PWD}/.questdb_data"
CONTAINER_NAME="xtrader_questdb"
SQL_DIR="${PWD}/sql/questdb"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="$2"; shift 2;;
    --http-port) HTTP_PORT="$2"; shift 2;;
    --ilp-port) ILP_PORT="$2"; shift 2;;
    --pg-port) PG_PORT="$2"; shift 2;;
    --ilp-http-port) ILP_HTTP_PORT="$2"; shift 2;;
    --data-dir) DATA_DIR="$2"; shift 2;;
    --name) CONTAINER_NAME="$2"; shift 2;;
    --sql-dir) SQL_DIR="$2"; shift 2;;
    *) echo "Unknown arg: $1"; exit 1;;
  esac
done

start_container() {
  mkdir -p "${DATA_DIR}"
  if docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "Container ${CONTAINER_NAME} already exists. Restarting..."
    docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
  fi
  echo "Starting QuestDB container ${CONTAINER_NAME}..."
  docker run -d \
    --name "${CONTAINER_NAME}" \
    -p ${HTTP_PORT}:9000 \
    -p ${ILP_PORT}:9009 \
    -p ${PG_PORT}:8812 \
    -p ${ILP_HTTP_PORT}:9003 \
    -v "${DATA_DIR}:/var/lib/questdb" \
    questdb/questdb:latest >/dev/null

  echo "QuestDB is starting on http://${HOST}:${HTTP_PORT}"
  for i in {1..60}; do
    if curl -sS "http://${HOST}:${HTTP_PORT}/exec?query=select%201" | grep -q 'dataset'; then
      echo "QuestDB is ready."
      return 0
    fi
    sleep 1
  done
  echo "QuestDB did not become ready in time." >&2
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

status_container() {
  if docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "${CONTAINER_NAME} is running."
  elif docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "${CONTAINER_NAME} exists but is not running."
  else
    echo "${CONTAINER_NAME} not found."
  fi
}

init_schema() {
  if [[ ! -d "${SQL_DIR}" ]]; then
    echo "SQL directory not found: ${SQL_DIR}" >&2
    exit 1
  fi
  echo "Initializing schema on http://${HOST}:${HTTP_PORT} using SQL in ${SQL_DIR}"
  for f in "${SQL_DIR}"/*.sql; do
    [[ -e "$f" ]] || continue
    echo "Applying: $(basename "$f")"
    q=$(tr '\n' ' ' < "$f")
    url="http://${HOST}:${HTTP_PORT}/exec?query=$(python3 -c 'import sys,urllib.parse; print(urllib.parse.quote(sys.stdin.read()))' <<< "$q")"
    # Fallback to jq-less approach; print errors if any
    http_code=$(curl -s -o /tmp/questdb_out.json -w "%{http_code}" "$url") || true
    if [[ "$http_code" != "200" ]]; then
      echo "Failed to apply $(basename "$f"): HTTP $http_code" >&2
      cat /tmp/questdb_out.json || true
      return 1
    fi
  done
  echo "Schema initialization complete."
}

case "$CMD" in
  start) start_container;;
  stop) stop_container;;
  status) status_container;;
  init) init_schema;;
  *) echo "Usage: $0 {start|stop|status|init} [options]"; { return 1; } 2>/dev/null || exit 1;;
esac

# Propagate function exit code without closing interactive shells when sourced
rc=$?
{ return $rc; } 2>/dev/null || exit $rc



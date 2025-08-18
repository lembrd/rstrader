#!/usr/bin/env bash
set -euo pipefail

# Requires env: DATABASE_URL or explicit PG vars
if [[ -n "${DATABASE_URL:-}" ]]; then
  DB_URL="$DATABASE_URL"
else
  PG_HOST=${POSTGRES_HOST:-127.0.0.1}
  PG_PORT=${POSTGRES_PORT:-5432}
  PG_DB=${POSTGRES_DB:-xtrader}
  PG_USER=${POSTGRES_USER:-xtrader}
  PG_PASSWORD=${POSTGRES_PASSWORD:-xtrader}
  DB_URL="jdbc:postgresql://${PG_HOST}:${PG_PORT}/${PG_DB}"
  export FLYWAY_USER="$PG_USER"
  export FLYWAY_PASSWORD="$PG_PASSWORD"
fi

MIGRATIONS_DIR=/usr/local/share/xtrader/migrations/pg

if [[ -d "$MIGRATIONS_DIR" ]] && [[ -n "$(ls -A "$MIGRATIONS_DIR" 2>/dev/null || true)" ]]; then
  echo "[entrypoint] Running Flyway migrations..."
  flyway -locations=filesystem:"$MIGRATIONS_DIR" -url="$DB_URL" -baselineOnMigrate="${FLYWAY_BASELINE_ON_MIGRATE:-false}" migrate
else
  echo "[entrypoint] No migrations directory found, skipping Flyway."
fi

echo "[entrypoint] Starting xtrader: $*"
exec /usr/local/bin/xtrader "$@"


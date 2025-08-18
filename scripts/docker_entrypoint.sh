#!/usr/bin/env bash
set -euo pipefail

# Requires env: DATABASE_URL or explicit PG vars
PG_HOST=${POSTGRES_HOST:-127.0.0.1}
PG_PORT=${POSTGRES_PORT:-5432}
PG_DB=${POSTGRES_DB:-xtrader}
PG_USER=${POSTGRES_USER:-xtrader}
PG_PASSWORD=${POSTGRES_PASSWORD:-xtrader}
DB_URL="jdbc:postgresql://${PG_HOST}:${PG_PORT}/${PG_DB}"
export FLYWAY_USER="$PG_USER"
export FLYWAY_PASSWORD="$PG_PASSWORD"
export DATABASE_URL="postgres://$PG_USER:$PG_PASSWORD@$PG_HOST:$PG_PORT/$PG_DB"

MIGRATIONS_DIR=/usr/local/share/xtrader/migrations/pg

if [[ -d "$MIGRATIONS_DIR" ]] && [[ -n "$(ls -A "$MIGRATIONS_DIR" 2>/dev/null || true)" ]]; then
  echo "[entrypoint] Running Flyway migrations..."
  flyway -locations=filesystem:"$MIGRATIONS_DIR" -url="$DB_URL" -baselineOnMigrate="${FLYWAY_BASELINE_ON_MIGRATE:-false}" migrate
else
  echo "[entrypoint] No migrations directory found, skipping Flyway."
fi

echo "[entrypoint] Starting DB:$DATABASE_URL xtrader: $*"
exec /usr/local/bin/xtrader "$@"


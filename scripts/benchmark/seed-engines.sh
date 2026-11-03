#!/usr/bin/env bash
# Seed the PostgreSQL and MySQL benchmark fixtures through the compose stack.
#
# Assumes the disposable stack from docker-compose.yml (+ docker-compose.ci.yml
# for isolated names/ports) is running. Uses `docker compose exec`, so the
# host needs no psql/mysql client.
#
# Usage: scripts/benchmark/seed-engines.sh [POSTGRES_SERVICE] [MYSQL_SERVICE]
#   defaults: postgres mysql
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
pg_service="${1:-postgres}"
my_service="${2:-mysql}"
compose_args=(-f "$root/docker-compose.yml")
if [ "${COMPOSE_CI:-0}" = "1" ]; then
  compose_args+=(-f "$root/docker-compose.ci.yml")
fi

echo "seeding PostgreSQL fixture ($pg_service)"
docker compose "${compose_args[@]}" exec -T "$pg_service" \
  psql -v ON_ERROR_STOP=1 -U "${POSTGRES_USER:-demo}" -d "${POSTGRES_DB:-demo}" \
  < "$root/scripts/benchmark/fixtures/postgres.sql"

echo "seeding MySQL fixture ($my_service)"
docker compose "${compose_args[@]}" exec -T "$my_service" \
  mysql -u"${MYSQL_USER:-demo}" -p"${MYSQL_PASSWORD:-demo}" "${MYSQL_DATABASE:-demo}" \
  < "$root/scripts/benchmark/fixtures/mysql.sql"

echo "done"

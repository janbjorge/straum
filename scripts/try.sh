#!/usr/bin/env bash
# Runs Straum against a throwaway PostgreSQL in Docker with the quick
# start's tables, then shows `straum views` and `straum ask`. In a
# terminal it ends in `straum shell`; everything it made goes on leaving.
#
# `scripts/try.sh taxi` does the same on a month of New York taxi trips,
# through bench/bench.py, which also needs uv.
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
    "") ;;
    taxi) exec bench/bench.py shell ;;
    *) echo "usage: scripts/try.sh [taxi]" >&2; exit 2 ;;
esac

for tool in docker cargo; do
    command -v "$tool" >/dev/null || { echo "try.sh: needs $tool" >&2; exit 1; }
done

# Named by this run and on ports the system picks, so two runs never
# take each other's PostgreSQL.
PG=straum-try-$$
STRAUM=target/debug/straum
WORK=$(mktemp -d)

cleanup() {
    [ -n "${STRAUM_PID:-}" ] && kill "$STRAUM_PID" 2>/dev/null && wait "$STRAUM_PID" 2>/dev/null
    docker rm -f "$PG" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

psql() { docker exec -i "$PG" psql -q -v ON_ERROR_STOP=1 -h 127.0.0.1 -U postgres "$@"; }

cargo build -p straum

docker run -d --name "$PG" -p 127.0.0.1::5432 \
    -e POSTGRES_PASSWORD=postgres postgres:18 -c wal_level=logical >/dev/null
echo "waiting for PostgreSQL"
until psql -c 'SELECT 1' >/dev/null 2>&1; do sleep 0.5; done
PG_PORT=$(docker port "$PG" 5432/tcp | sed 's/.*://')
export STRAUM_DSN="postgres://straum:straum@127.0.0.1:$PG_PORT/postgres?sslmode=disable"

psql <<'SQL'
CREATE TABLE customers (id int PRIMARY KEY, country text);
CREATE TABLE orders (
  id bigint PRIMARY KEY,
  placed_at timestamptz NOT NULL,
  shipped_at timestamptz,
  status text NOT NULL,
  customer_id int REFERENCES customers,
  amount numeric(10, 2) NOT NULL
);
INSERT INTO customers VALUES (1, 'NO'), (2, 'SE'), (3, 'NO');
INSERT INTO orders VALUES
  (1, '2026-01-01 09:12+00', '2026-01-02 08:00+00', 'paid', 1, 120.00),
  (2, '2026-01-01 09:40+00', NULL, 'paid', 2, 80.50),
  (3, '2026-01-01 10:05+00', NULL, 'refunded', 3, 42.00),
  (4, '2026-01-01 10:30+00', NULL, 'paid', 3, 19.99);
SQL

cat > "$WORK/orders.sql" <<'SQL'
CREATE VIEW orders WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS bucket,
       orders.status AS status,
       orders.customer_id AS customer_id,
       customers.country AS country,
       count(*) AS orders,
       sum(orders.amount) AS revenue,
       avg(orders.amount) AS avg_amount
FROM public.orders AS orders
LEFT OUTER JOIN public.customers AS customers ON customers.id = orders.customer_id
GROUP BY 1, 2, 3, 4;
SQL

"$STRAUM" plan --spec "$WORK/orders.sql" | psql >/dev/null
"$STRAUM" run --spec "$WORK/orders.sql" --listen 127.0.0.1:0 2>"$WORK/log" &
STRAUM_PID=$!
echo "waiting for Straum"
URL=
until [ -n "$URL" ] && "$STRAUM" views --url "$URL" >/dev/null 2>&1; do
    kill -0 "$STRAUM_PID" 2>/dev/null || { cat "$WORK/log"; exit 1; }
    sleep 0.2
    URL=$(sed -n 's/^straum: listening on /http:\/\//p' "$WORK/log")
done

QUERY='SELECT status, country, orders, revenue, avg_amount FROM orders
  WHERE bucket >= $1 AND bucket < $2 GROUP BY status, country'
RANGE=(2026-01-01T00:00:00Z 2026-01-02T00:00:00Z)

echo; echo "\$ straum views"
"$STRAUM" views --url "$URL"
echo; echo "\$ straum ask '$QUERY' ${RANGE[*]}"
"$STRAUM" ask --url "$URL" "$QUERY" "${RANGE[@]}"
echo; echo "\$ straum ask, with a text parameter for an integer key"
"$STRAUM" ask --url "$URL" \
    'SELECT orders FROM orders WHERE customer_id = $1 AND bucket >= $2 AND bucket < $3' \
    paid "${RANGE[@]}" || true

if [ -t 0 ]; then
    echo
    echo "Change rows from another terminal: docker exec -it $PG psql -U postgres"
    "$STRAUM" shell --url "$URL"
fi

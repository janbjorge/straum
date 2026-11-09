#!/usr/bin/env bash
# M1: one month of NYC yellow trips in a throwaway PostgreSQL 18. Checks that
# asks answered from the taxi.sql rollup, with zones mapped after
# aggregation, equal the same asks over the raw join.
#
# Needs curl, uv and docker. Downloads go to bench/data (gitignored).
set -euo pipefail
cd "$(dirname "$0")"

month=2024-01
tlc=https://d37ci6vzurychx.cloudfront.net
container=straum-bench

mkdir -p data
fetch() {
    [[ -s data/$1 ]] && return
    curl -fsSL -o "data/$1.part" "$2"
    mv "data/$1.part" "data/$1"
}
fetch "yellow_tripdata_$month.parquet" "$tlc/trip-data/yellow_tripdata_$month.parquet"
fetch taxi_zone_lookup.csv "$tlc/misc/taxi_zone_lookup.csv"
if [[ ! -s data/trips_$month.csv ]]; then
    uv run --no-project --with pyarrow python -I trips_csv.py \
        "data/yellow_tripdata_$month.parquet" "data/trips_$month.csv"
fi

docker rm -f "$container" >/dev/null 2>&1 || true
trap 'docker rm -f "$container" >/dev/null' EXIT
docker run -d --name "$container" --shm-size=1g \
    -e POSTGRES_HOST_AUTH_METHOD=trust \
    -v "$PWD/data:/data:ro" postgres:18 >/dev/null
# TCP only answers once the image's init server has stopped.
until docker exec "$container" pg_isready -q -h localhost; do sleep 1; done

sql() { docker exec -i "$container" psql -U postgres -X -q -v ON_ERROR_STOP=1 "$@"; }
sql -v trips="/data/trips_$month.csv" < load.sql
sql < rollup.sql
sql < compare.sql

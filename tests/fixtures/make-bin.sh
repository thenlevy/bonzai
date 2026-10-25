#!/usr/bin/env bash
# Runs a fixture script such as postgres-heap-page.sql in a throwaway Postgres
# cluster with data checksums on, and saves what it prints: the block to
# target/fixtures/postgres-heap-page.bin, to look at with hexyl, and its header
# as pageinspect reads it to postgres-heap-page.header. tests/postgres.rs reads
# both. Run it from the dev shell:
#
#     tests/fixtures/make-bin.sh tests/fixtures/postgres-heap-page.sql
set -euo pipefail

script=$(realpath "${1:?usage: make-bin.sh FIXTURE.sql}")
out_dir=$(realpath "$(dirname "$0")/../..")/target/fixtures
name=$(basename "$script" .sql)
tmp=$(mktemp -d)
trap 'pg_ctl -D "$tmp/data" stop -m immediate >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT

initdb -D "$tmp/data" --data-checksums --no-locale -E UTF8 -U postgres -A trust >/dev/null
pg_ctl -D "$tmp/data" -o "-k '$tmp' -c listen_addresses=" -l "$tmp/log" -w start >/dev/null
output=$(psql -h "$tmp" -U postgres -d postgres -X -q -A -t -v ON_ERROR_STOP=1 -f "$script")

# The script prints the block in hex, then its header.
mkdir -p "$out_dir"
sed -n 1p <<<"$output" | basenc --base16 -d >"$out_dir/$name.bin"
sed -n 2p <<<"$output" >"$out_dir/$name.header"
echo "$out_dir/$name.bin"

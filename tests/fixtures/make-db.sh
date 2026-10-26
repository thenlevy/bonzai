#!/usr/bin/env bash
# Rebuilds postgres/, a Postgres cluster with data checksums on whose files the
# tests read. Runs each tests/fixtures/*.sql in it, saves what each one prints
# (the values the tests can't predict, such as LSNs) to postgres/<name>.facts,
# then stops the server. Run it from the dev shell:
#
#     tests/fixtures/make-db.sh
#
# To look around afterwards:
#
#     pg_ctl -D postgres/data -l postgres/log start
#     psql -h "$PWD/postgres" -U postgres
set -euo pipefail

root=$(realpath "$(dirname "$0")/../..")
dir=$root/postgres
data=$dir/data

# Commands are built as arrays so that each argument can have a comment: bash
# allows none after a `\` line continuation.

# Stops the server in fast mode, the default: it rolls back open
# transactions, disconnects clients, writes a shutdown checkpoint and exits,
# so the cluster starts again without crash recovery.
stop=(
    stop
    -D "$data" # The cluster. pg_ctl finds the server's PID in its postmaster.pid.
    -w         # Return only once the server has exited.
)

# A server left running on the old cluster, from looking around, would have
# its files deleted from under it. Usually none is running: pg_ctl then fails,
# which is fine.
if [ -d "$data" ]; then
    pg_ctl "${stop[@]}" >/dev/null 2>&1 || true
fi
rm -rf "$dir"
mkdir -p "$dir"

initdb_args=(
    -D "$data"       # Where to create the cluster.
    --data-checksums # Off by default before Postgres 18.
    --no-locale      # Compare text byte by byte (the C locale).
    -E UTF8          # Encoding of the databases' text.
    -U postgres      # Name of the superuser.
    -A trust         # Let local connections in without a password.
)
initdb "${initdb_args[@]}" >/dev/null

# Reachable only through a socket in postgres/, so it never clashes with
# another server.
cat >>"$data/postgresql.conf" <<EOF
listen_addresses = ''
unix_socket_directories = '$dir'
EOF

start=(
    start
    -D "$data"
    -l "$dir/log" # Where the server's output goes, instead of the terminal.
    -w            # Return only once the server accepts connections.
)
pg_ctl "${start[@]}" >/dev/null
# Stop cleanly however the script ends, failures included.
trap 'pg_ctl "${stop[@]}" >/dev/null' EXIT

psql_args=(
    -h "$dir"          # Connect through the socket in postgres/.
    -U postgres
    -d postgres        # The database initdb creates for tools to connect to.
    -X                 # Ignore ~/.psqlrc.
    -q                 # Don't print command tags such as CREATE TABLE.
    -A                 # Unaligned: no padding around values.
    -t                 # Tuples only: no column headers or row counts.
    -x                 # Expanded: one line per column, name then value...
    -F =               # ...separated by `=`.
    -v ON_ERROR_STOP=1 # Stop at the first error, and exit with a failure.
)
for script in "$root"/tests/fixtures/*.sql; do
    psql "${psql_args[@]}" -f "$script" >"$dir/$(basename "$script" .sql).facts"
done
echo "$dir"

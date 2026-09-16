#!/usr/bin/env bash
set -euo pipefail

RUNS=${RUNS:-3}
PORT=${PORT:-55432}
PG_BINDIR=${PG_BINDIR:-$(pg_config --bindir)}
PGRUST_BIN=${PGRUST_BIN:-pgrust}
BENCH_ROOT=${BENCH_ROOT:-$(pwd)/results/work}
RESULTS_DIR=${RESULTS_DIR:-$(pwd)/results}
POOL_SIZE=${POOL_SIZE:-$(nproc)}

for executable in "$PG_BINDIR/initdb" "$PG_BINDIR/pg_isready" "$PG_BINDIR/pg_ctl"; do
  [[ -x $executable ]] || { echo "missing executable: $executable" >&2; exit 1; }
done
command -v "$PGRUST_BIN" >/dev/null || { echo "PGRUST_BIN is not executable: $PGRUST_BIN" >&2; exit 1; }
command -v cargo-nextest >/dev/null || { echo "cargo-nextest is required" >&2; exit 1; }

mkdir -p "$BENCH_ROOT" "$RESULTS_DIR"
cargo build --release --tests --bins
cargo nextest list --release >/dev/null

if [[ $EUID -eq 0 ]]; then
  RUN_AS=(runuser -u postgres --)
  chown -R postgres:postgres "$BENCH_ROOT"
else
  RUN_AS=()
fi

COMMON_SETTINGS=(
  -c max_connections=1024
  -c fsync=off
  -c synchronous_commit=off
  -c full_page_writes=off
  -c wal_level=minimal
  -c max_wal_senders=0
  -c autovacuum=off
  -c checkpoint_timeout=1h
  -c max_wal_size=8GB
  -c jit=off
  -c shared_buffers=128MB
)

current_data=
stop_server() {
  if [[ -n ${current_data:-} && -f $current_data/postmaster.pid ]]; then
    "${RUN_AS[@]}" "$PG_BINDIR/pg_ctl" -D "$current_data" -m immediate -w stop >/dev/null 2>&1 || true
  fi
  current_data=
}
trap stop_server EXIT INT TERM

run_once() {
  local engine=$1
  local run=$2
  local data="$BENCH_ROOT/${engine}-${run}"
  local socket="$data/socket"
  local server_log="$RESULTS_DIR/${engine}-${run}.server.log"
  local test_log="$RESULTS_DIR/${engine}-${run}.nextest.log"
  local url="postgres://postgres@127.0.0.1:$PORT/postgres"

  rm -rf "$data"
  mkdir -p "$data" "$socket"
  if [[ $EUID -eq 0 ]]; then
    chown -R postgres:postgres "$data"
  fi
  "${RUN_AS[@]}" "$PG_BINDIR/initdb" -D "$data/pgdata" --no-locale --encoding=UTF8 -U postgres >/dev/null
  current_data="$data/pgdata"

  if [[ $engine == postgres ]]; then
    "${RUN_AS[@]}" "$PG_BINDIR/postgres" -D "$current_data" -p "$PORT" -k "$socket" \
      "${COMMON_SETTINGS[@]}" ${POSTGRES_EXTRA_ARGS:-} >"$server_log" 2>&1 &
  else
    "${RUN_AS[@]}" "$PGRUST_BIN" --profile test -D "$current_data" -p "$PORT" -k "$socket" \
      -c max_connections=1024 ${PGRUST_EXTRA_ARGS:-} >"$server_log" 2>&1 &
  fi

  for _ in $(seq 1 300); do
    if "$PG_BINDIR/pg_isready" -h 127.0.0.1 -p "$PORT" -U postgres >/dev/null 2>&1; then
      break
    fi
    sleep 0.1
  done
  "$PG_BINDIR/pg_isready" -h 127.0.0.1 -p "$PORT" -U postgres >/dev/null

  DATABASE_URL="$url" target/release/setup "$POOL_SIZE"
  echo "engine=$engine run=$run filesystem=$(stat -f -c %T "$data") pool_size=$POOL_SIZE"
  local started finished elapsed
  started=$(date +%s%N)
  DATABASE_URL="$url" cargo nextest run --release --profile benchmark --no-fail-fast ${NEXTEST_FILTER:+-E "$NEXTEST_FILTER"} 2>&1 | tee "$test_log"
  finished=$(date +%s%N)
  elapsed=$(awk -v start="$started" -v finish="$finished" 'BEGIN { printf "%.6f", (finish-start)/1000000000 }')
  printf '{"engine":"%s","run":%s,"wall_seconds":%s,"pool_size":%s}\n' \
    "$engine" "$run" "$elapsed" "$POOL_SIZE" | tee "$RESULTS_DIR/${engine}-${run}.json"
  stop_server
}

"$PG_BINDIR/postgres" --version
"$PGRUST_BIN" --version
for engine in ${ENGINES:-postgres pgrust}; do
  for run in $(seq 1 "$RUNS"); do
    run_once "$engine" "$run"
  done
done

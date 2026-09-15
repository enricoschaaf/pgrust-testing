# Pgrust Nextest reproduction

This repository compares PostgreSQL 18 and Pgrust 0.3 on the same synthetic Rust test suite. It exists to make a test-runner performance regression reproducible without sharing an application schema, migration, fixture, or test.

The narrow question is whether replacing the PostgreSQL server executable with Pgrust test mode makes this workload faster when both the database and Nextest share one machine.

## Synthetic suite

`build.rs` generates 6,000 tests:

- 1,600 database tests
- 4,400 small CPU tests

Every test is a separate Nextest process. The database tests use a synthetic template with 200 tables, 8 baseline tables, and 50 sequences. Most tests lease a prepared database using catalog metadata and an advisory lock, rename it, open two connections, execute separately prepared statements, terminate the connections, restore a generated PL/pgSQL baseline, and return the database to the pool. One database test in 12 creates and drops an isolated template clone.

All names, schemas, rows, SQL, and Rust code in this repository were written specifically for this reproduction.

## Requirements

- Linux
- PostgreSQL 18 tools available through `pg_config`
- Pgrust 0.3 built locally
- `cargo-nextest`
- enough space for one prepared database per logical CPU

Use the same reflink-capable filesystem for both engines. The runner prints the filesystem type for every run.

## Run

```bash
PGRUST_BIN=/path/to/pgrust \
BENCH_ROOT=/path/on/btrfs/pgrust-repro \
RUNS=3 \
./scripts/run.sh
```

The runner executes all PostgreSQL runs followed by all Pgrust runs, with no CPU affinity or quota. It uses every CPU visible to Nextest and defaults the prepared database pool to `nproc`.

PostgreSQL receives the documented non-durable settings expanded by Pgrust's `--profile test`. Pgrust is launched with that profile. Both receive `max_connections=1024`. Server startup, schema creation, prepared-database creation, and Rust compilation happen outside the measured interval.

Each run writes:

- `results/<engine>-<run>.json` with command wall time
- `results/<engine>-<run>.nextest.log` with Nextest's own summary
- `results/<engine>-<run>.server.log` for server-side debugging

## Reference result

On a 96-logical-CPU Linux host with Btrfs, PostgreSQL 18.6 and Pgrust 0.3 produced these Nextest summaries:

| Engine | Run 1 | Run 2 | Run 3 | Median |
| --- | ---: | ---: | ---: | ---: |
| PostgreSQL | 9.402s | 10.363s | 10.357s | **10.357s** |
| Pgrust | 16.005s | 17.073s | 12.991s | **16.005s** |

Pgrust's median was 54.5% slower. All 6,000 tests passed in every run. Server setup and prepared-database creation were excluded from these times.

Useful server overrides:

```bash
POSTGRES_EXTRA_ARGS='-c io_method=worker -c io_workers=32' \
PGRUST_EXTRA_ARGS='-c io_method=worker -c io_workers=32' \
./scripts/run.sh
```

## Why Nextest is part of the reproduction

A single async benchmark client can hide the effect: it owns one runtime and does little work outside PostgreSQL. A Rust test suite creates short-lived test processes and runtimes while the database is busy. That shared-runner scheduling and connection churn are part of the behavior being measured.

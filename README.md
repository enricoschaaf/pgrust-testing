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

On a 96-logical-CPU x86_64 Linux host with Btrfs ([Hetzner AX162](https://www.hetzner.com/de/dedicated-rootserver/ax162/configurator/)), PostgreSQL 18.6 and Pgrust 0.3 produced these Nextest summaries:

| Engine | Run 1 | Run 2 | Run 3 | Median |
| --- | ---: | ---: | ---: | ---: |
| PostgreSQL | 9.402s | 10.363s | 10.357s | **10.357s** |
| Pgrust | 16.005s | 17.073s | 12.991s | **16.005s** |

Pgrust's median was 54.5% slower. All 6,000 tests passed in every run. Server setup and prepared-database creation were excluded from these times.

### Architecture caveat

This is an x86_64 result. Pgrust's README says the project is specifically tuned for AWS Graviton4, its JIT currently targets Graviton4, and its published performance measurements use a Graviton4 `c8g.4xlarge` instance ([Pgrust status and performance notes](https://github.com/malisper/pgrust/blob/v0.3/README.md#status)). The result above demonstrates a reproducible regression for this x86_64 Nextest workload; it should not be read as a performance conclusion for Pgrust on Graviton4.

## Working hypothesis

The likely bottleneck is short-lived session and catalog-management work under a process-per-test runner, rather than database-file I/O.

The main evidence is a control experiment: a single async client running the same broad database lifecycle made Pgrust equal to or faster than PostgreSQL. The regression appeared when the workload became 6,000 separate Nextest processes sharing the host with the server. Each database test creates a runtime, opens short-lived admin, test, and reset connections, and performs catalog operations against `pg_database`. Pgrust uses an operating-system thread per session, so this shape is likely to amplify thread creation, scheduling, connection teardown, and lock-manager costs.

The reusable path is the first place to profile:

1. `acquire_database` repeatedly reads `pg_database`, takes an advisory lock, changes a database comment, renames the database, and enables connections.
2. The test opens two fresh sessions and prepares its statements on a cold connection.
3. `release_database` disables connections, terminates the test sessions, waits for teardown, runs the PL/pgSQL reset, and updates the database comment.

Copy-on-write cloning is unlikely to explain most of the difference. Prepared-database creation is outside the measured interval, and only one database test in 12 uses an isolated clone. Pgrust's native ephemeral-database path is also not involved: the direct comparison deliberately changes only the server executable while keeping the reusable-database harness identical.

Useful source areas and measurements for Pgrust are:

- session thread creation, destruction, and wakeups
- backend teardown after `pg_terminate_backend`
- `LockManager` contention during concurrent `pg_database` reads and `ALTER DATABASE`
- context switches, CPU migrations, futex activity, and total system CPU
- the interaction between the server's session threads and Nextest's short-lived processes

This is a hypothesis, not a demonstrated root cause. The following filters narrow it down without changing the generated suite:

```bash
# Reusable database lifecycle only
DATABASE_URL=... cargo nextest run --release -E 'test(/database_reusable_/)'

# Template create/drop lifecycle only
DATABASE_URL=... cargo nextest run --release -E 'test(/database_isolated_/)'

# Test-process and CPU control, with no database access
cargo nextest run --release -E 'test(/cpu_/)'
```

If the reusable subset retains the regression while the isolated and CPU subsets do not, profiling `acquire_database` and `release_database` should give the shortest path to an actionable Pgrust improvement.

Useful server overrides:

```bash
POSTGRES_EXTRA_ARGS='-c io_method=worker -c io_workers=32' \
PGRUST_EXTRA_ARGS='-c io_method=worker -c io_workers=32' \
./scripts/run.sh
```

## Pgrust test mode

The direct comparison above deliberately keeps the reusable-database harness and only swaps the server executable, so Pgrust's ephemeral-database path is never exercised. `PGRUST_EPHEMERAL=1` switches every database test to that path: the test connects to `tdb_repro_template__<pid>_<index>`, the server mints the database from the sealed template on first connection, and the janitor drops it once it has been idle for the grace period. There is no lease, rename, terminate, restore, or return. Setup seals the template with `pgrust_seal_template` instead of building the prepared-database pool, and `PGRUST_WARM_POOL=<n>` makes setup wait until the janitor has `n` warm spares before the timed interval starts.

```bash
PGRUST_BIN=/path/to/pgrust \
BENCH_ROOT=/path/on/btrfs/pgrust-repro \
ENGINES=pgrust \
PGRUST_EPHEMERAL=1 \
PGRUST_WARM_POOL=192 \
PGRUST_EXTRA_ARGS='-c pgrust.ephemeral_db_pool_size=192 -c pgrust.ephemeral_db_wal_log_threshold=-1' \
./scripts/run.sh
```

The two server settings matter. `pgrust.ephemeral_db_pool_size` defaults to 0, so without it every mint is a cold clone served by the janitor in batches of 32 per 500 ms tick. `pgrust.ephemeral_db_wal_log_threshold=-1` forces file-copy clones; the test profile switches to `STRATEGY wal_log` at 50 relations, which this 200-table template exceeds, and WAL-logging the template on every mint is what produces the "checkpoints are occurring too frequently" warnings in the server log. With file copies each clone is a reflink on a copy-on-write filesystem.

`NEXTEST_FILTER` passes a Nextest filter expression to every run and `ENGINES` limits the run to one engine. `NEXTEST_FILTER='test(/_[0-9]*0$/)'` is a one-in-ten sample (600 tests, 160 of them database tests) that keeps the test mix and finishes both engines in about half a minute.

### Results

AWS c7a.24xlarge (AMD EPYC 9R14, 96 logical CPUs, x86_64, Ubuntu 24.04), PostgreSQL 18.6 from PGDG, Pgrust 0.3 built with `cargo build --release` from the `v0.3` tag, `BENCH_ROOT` on Btrfs. Nextest medians.

Full 6,000-test suite, three runs each, unchanged harness:

| Engine | Run 1 | Run 2 | Run 3 | Median |
| --- | ---: | ---: | ---: | ---: |
| PostgreSQL | 17.623s | 18.430s | 18.711s | **18.430s** |
| Pgrust | 15.767s | 16.617s | 16.470s | **16.470s** |

One-in-ten sample (`NEXTEST_FILTER='test(/_[0-9]*0$/)'`), medians:

| Configuration | Sample | vs PostgreSQL |
| --- | ---: | ---: |
| PostgreSQL, reusable-database harness | 3.843s | 1.0x |
| Pgrust, reusable-database harness | 3.151s | 1.2x faster |
| Pgrust test mode, cold mints, default `wal_log_threshold=50` | 22.629s | 5.9x slower |
| Pgrust test mode, cold mints, `wal_log_threshold=-1` | 4.199s | 1.1x slower |
| Pgrust test mode, 192-spare warm pool, `wal_log_threshold=-1` | 0.550s | **7.0x faster** |

Filling the 192-spare pool takes about 4.5 seconds per server start with file-copy clones (24 seconds with `wal_log`), outside the timed interval. The 440 CPU tests in the sample take about 0.14 seconds on their own, so the database tests went from roughly 3.7 seconds under PostgreSQL to roughly 0.4 seconds.

Two setup notes for a fresh box: Pgrust with `max_connections=1024` needs about 74,000 open file descriptors and refuses to start under the Ubuntu default hard limit of 65,536, and a PGDG install needs `PGRUST_TZDIR=/usr/share/zoneinfo`.

## Why Nextest is part of the reproduction

A single async benchmark client can hide the effect: it owns one runtime and does little work outside PostgreSQL. A Rust test suite creates short-lived test processes and runtimes while the database is busy. That shared-runner scheduling and connection churn are part of the behavior being measured.

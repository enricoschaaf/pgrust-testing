use std::{hint::black_box, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, Config, Connection, NoTls, Socket, tls::NoTlsStream};

const TEMPLATE_DB: &str = "repro_template";
const TABLES: usize = 200;
const SEEDED_TABLES: usize = 8;
const SEQUENCES: usize = 50;
const SEED_ROWS: usize = 4;
const CONNECTIONS_PER_TEST: usize = 2;
const QUERIES_PER_TEST: usize = 16;
const READY_COMMENT: &str = "repro-ready";

#[cfg(test)]
include!(concat!(env!("OUT_DIR"), "/generated_tests.rs"));

type PgConnection = Connection<Socket, NoTlsStream>;
type LiveConnection = (Client, JoinHandle<Result<()>>);

struct Lease {
    admin: Client,
    admin_task: JoinHandle<Result<()>>,
    database: String,
    oid: i64,
}

pub fn run_database_test(index: usize) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(async move {
            let config = database_config()?;
            if std::env::var_os("PGRUST_EPHEMERAL").is_some() {
                run_ephemeral_test(&config, index).await
            } else if index.is_multiple_of(12) {
                run_isolated_test(&config, index).await
            } else {
                run_reusable_test(&config, index).await
            }
        })
        .unwrap();
}

pub fn run_cpu_test(index: usize) {
    let mut state = index as u64 + 0x9e37_79b9_7f4a_7c15;
    for _ in 0..20_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
    }
    black_box(state);
}

pub async fn setup(url: &str, pool_size: usize) -> Result<()> {
    if pool_size == 0 {
        bail!("pool size must be positive");
    }
    let config: Config = url.parse().context("invalid DATABASE_URL")?;
    let (admin, admin_task) = connect(&config, None).await?;
    let databases = admin
        .query(
            "SELECT datname FROM pg_database WHERE datname = $1 OR starts_with(datname, 'repro_db_') OR starts_with(datname, 'repro_isolated_')",
            &[&TEMPLATE_DB],
        )
        .await?;
    for row in databases {
        let database: String = row.get(0);
        admin
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                quote_ident(&database)
            ))
            .await?;
    }
    admin
        .batch_execute(&format!("CREATE DATABASE {TEMPLATE_DB}"))
        .await?;

    let (template, template_task) = connect(&config, Some(TEMPLATE_DB)).await?;
    template.batch_execute(&schema_sql()).await?;
    close(template, template_task).await?;

    if std::env::var_os("PGRUST_EPHEMERAL").is_some() {
        admin
            .query_one("SELECT pgrust_seal_template($1)", &[&TEMPLATE_DB])
            .await
            .context("sealing the template (pgrust test mode)")?;
        // PGRUST_WARM_POOL=<n>: register the template with one mint, then wait until the
        // janitor has <n> spares ready so the timed interval starts with a full warm pool.
        if let Some(target) = std::env::var("PGRUST_WARM_POOL")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
        {
            let (w, wt) = connect(&config, Some(&format!("tdb_{TEMPLATE_DB}__warmup"))).await?;
            close(w, wt).await?;
            let started = std::time::Instant::now();
            loop {
                let spares: i64 = admin
                    .query_one(
                        "SELECT count(*) FROM pg_database WHERE datname LIKE 'tdb_spare_%'",
                        &[],
                    )
                    .await?
                    .get(0);
                if spares >= target {
                    eprintln!(
                        "warm pool ready: {spares} spares in {:?}",
                        started.elapsed()
                    );
                    break;
                }
                if started.elapsed() > Duration::from_secs(300) {
                    bail!("warm pool never reached {target} (have {spares})");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        close(admin, admin_task).await?;
        return Ok(());
    }
    for worker in 0..pool_size {
        let database = format!("repro_db_seed_{worker:03}");
        admin
            .batch_execute(&format!(
                "CREATE DATABASE {database} TEMPLATE {TEMPLATE_DB}"
            ))
            .await
            .with_context(|| format!("creating reusable database {worker}"))?;
        admin
            .batch_execute(&format!(
                "ALTER DATABASE {database} ALLOW_CONNECTIONS false"
            ))
            .await?;
        admin
            .batch_execute(&format!(
                "COMMENT ON DATABASE {database} IS '{READY_COMMENT}'"
            ))
            .await?;
    }
    close(admin, admin_task).await?;
    Ok(())
}

fn database_config() -> Result<Config> {
    std::env::var("DATABASE_URL")
        .context("DATABASE_URL is not set")?
        .parse()
        .context("invalid DATABASE_URL")
}

async fn run_reusable_test(config: &Config, index: usize) -> Result<()> {
    let lease = acquire_database(config, index).await?;
    let connections = open_test_connections(config, &lease.database).await?;
    execute_workload(&connections[0].0, index).await?;
    release_database(config, lease, connections).await
}

async fn run_ephemeral_test(config: &Config, index: usize) -> Result<()> {
    // pgrust test mode: the template is in the name; the database materializes on
    // first connection and the janitor drops it once it has been idle for the grace.
    let database = format!("tdb_{TEMPLATE_DB}__{}_{index}", std::process::id());
    let connections = open_test_connections(config, &database).await?;
    execute_workload(&connections[0].0, index).await?;
    close_all(connections).await
}

async fn run_isolated_test(config: &Config, index: usize) -> Result<()> {
    let database = format!("repro_isolated_{}_{index}", std::process::id());
    let (admin, admin_task) = connect(config, None).await?;
    admin
        .batch_execute(&format!(
            "CREATE DATABASE {} TEMPLATE {TEMPLATE_DB}",
            quote_ident(&database)
        ))
        .await?;
    let connections = open_test_connections(config, &database).await?;
    execute_workload(&connections[0].0, index).await?;
    close_all(connections).await?;
    admin
        .batch_execute(&format!(
            "DROP DATABASE {} WITH (FORCE)",
            quote_ident(&database)
        ))
        .await?;
    close(admin, admin_task).await
}

async fn acquire_database(config: &Config, index: usize) -> Result<Lease> {
    let (admin, admin_task) = connect(config, None).await?;
    let new_name = format!("repro_db_t{index}_{}", std::process::id());
    loop {
        let candidates = admin
            .query(
                "SELECT oid::bigint, datname FROM pg_database WHERE starts_with(datname, 'repro_db_') AND shobj_description(oid, 'pg_database') = $1 ORDER BY oid",
                &[&READY_COMMENT],
            )
            .await?;
        for candidate in candidates {
            let oid: i64 = candidate.get(0);
            let old_name: String = candidate.get(1);
            let lock_key = 0x5250_524f_0000_0000_i64 | oid;
            let locked: bool = admin
                .query_one("SELECT pg_try_advisory_lock($1)", &[&lock_key])
                .await?
                .get(0);
            if !locked {
                continue;
            }
            let still_ready = admin
                .query_opt(
                    "SELECT 1 FROM pg_database WHERE oid = $1::bigint::oid AND datname = $2 AND NOT datallowconn AND shobj_description(oid, 'pg_database') = $3",
                    &[&oid, &old_name, &READY_COMMENT],
                )
                .await?
                .is_some();
            if !still_ready {
                admin
                    .query_one("SELECT pg_advisory_unlock($1)", &[&lock_key])
                    .await?;
                continue;
            }
            admin
                .batch_execute(&format!(
                    "COMMENT ON DATABASE {} IS NULL",
                    quote_ident(&old_name)
                ))
                .await?;
            admin
                .batch_execute(&format!(
                    "ALTER DATABASE {} RENAME TO {}",
                    quote_ident(&old_name),
                    quote_ident(&new_name)
                ))
                .await?;
            admin
                .batch_execute(&format!(
                    "ALTER DATABASE {} ALLOW_CONNECTIONS true",
                    quote_ident(&new_name)
                ))
                .await?;
            return Ok(Lease {
                admin,
                admin_task,
                database: new_name,
                oid,
            });
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn release_database(
    config: &Config,
    lease: Lease,
    connections: Vec<LiveConnection>,
) -> Result<()> {
    let Lease {
        admin,
        admin_task,
        database,
        oid,
    } = lease;
    let (reset_client, reset_task) = connect(config, Some(&database)).await?;
    let reset_pid: i32 = reset_client
        .query_one("SELECT pg_backend_pid()", &[])
        .await?
        .get(0);
    admin
        .batch_execute(&format!(
            "ALTER DATABASE {} ALLOW_CONNECTIONS false",
            quote_ident(&database)
        ))
        .await?;
    admin
        .query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datid = $1::bigint::oid AND pid <> $2",
            &[&oid, &reset_pid],
        )
        .await?;
    loop {
        let remaining: bool = admin
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datid = $1::bigint::oid AND pid <> $2)",
                &[&oid, &reset_pid],
            )
            .await?
            .get(0);
        if !remaining {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    reset_client
        .execute("CALL benchmark_baseline.restore()", &[])
        .await?;
    close(reset_client, reset_task).await?;
    abandon_all(connections).await;
    admin
        .batch_execute(&format!(
            "COMMENT ON DATABASE {} IS '{READY_COMMENT}'",
            quote_ident(&database)
        ))
        .await?;
    close(admin, admin_task).await
}

async fn execute_workload(client: &Client, index: usize) -> Result<()> {
    let id = ((std::process::id() as i64) << 32) | index as i64;
    client.batch_execute("BEGIN").await?;
    client
        .execute(
            "INSERT INTO public.bench_scratch(id, payload) VALUES ($1, md5(($1::bigint)::text))",
            &[&id],
        )
        .await?;
    client
        .execute(
            "INSERT INTO public.bench_table_199(id, payload) VALUES ($1, md5(($1::bigint)::text))",
            &[&id],
        )
        .await?;
    for query in 0..QUERIES_PER_TEST {
        let table = query % SEEDED_TABLES;
        let row_id = (query % SEED_ROWS + 1) as i64;
        client
            .query_opt(
                &format!("SELECT payload FROM public.bench_table_{table:03} WHERE id = $1"),
                &[&row_id],
            )
            .await?;
    }
    client
        .execute(
            "UPDATE public.bench_table_000 SET payload = md5(payload || ($1::bigint)::text) WHERE id = 1",
            &[&id],
        )
        .await?;
    client
        .query_one("SELECT count(*) FROM public.bench_scratch", &[])
        .await?;
    client.batch_execute("COMMIT").await?;
    Ok(())
}

async fn open_test_connections(config: &Config, database: &str) -> Result<Vec<LiveConnection>> {
    let mut connections = Vec::with_capacity(CONNECTIONS_PER_TEST);
    for _ in 0..CONNECTIONS_PER_TEST {
        connections.push(connect(config, Some(database)).await?);
    }
    Ok(connections)
}

async fn connect(config: &Config, database: Option<&str>) -> Result<LiveConnection> {
    let mut config = config.clone();
    if let Some(database) = database {
        config.dbname(database);
    }
    let (client, connection): (Client, PgConnection) = config.connect(NoTls).await?;
    let task = tokio::spawn(async move {
        connection
            .await
            .context("PostgreSQL connection task failed")?;
        Ok(())
    });
    Ok((client, task))
}

async fn close(client: Client, task: JoinHandle<Result<()>>) -> Result<()> {
    drop(client);
    task.await.context("connection task panicked")??;
    Ok(())
}

async fn close_all(connections: Vec<LiveConnection>) -> Result<()> {
    for (client, task) in connections {
        close(client, task).await?;
    }
    Ok(())
}

async fn abandon_all(connections: Vec<LiveConnection>) {
    let mut tasks = Vec::with_capacity(connections.len());
    for (client, task) in connections {
        drop(client);
        tasks.push(task);
    }
    for task in tasks {
        let _ = task.await;
    }
}

fn schema_sql() -> String {
    let mut sql = String::from(
        "CREATE SCHEMA benchmark_baseline;\n\
         CREATE TABLE public.bench_scratch(id bigint PRIMARY KEY, payload text NOT NULL);\n",
    );
    for table in 0..TABLES {
        if table < SEQUENCES {
            sql.push_str(&format!(
                "CREATE TABLE public.bench_table_{table:03}(id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, payload text NOT NULL);\n"
            ));
        } else {
            sql.push_str(&format!(
                "CREATE TABLE public.bench_table_{table:03}(id bigint PRIMARY KEY, payload text NOT NULL);\n"
            ));
        }
        if table < SEEDED_TABLES {
            sql.push_str(&format!(
                "INSERT INTO public.bench_table_{table:03}(payload) SELECT md5(g::text) FROM generate_series(1, {SEED_ROWS}) g;\n\
                 CREATE TABLE benchmark_baseline.bench_table_{table:03} AS TABLE public.bench_table_{table:03};\n"
            ));
        }
    }
    sql.push_str(
        "CREATE OR REPLACE PROCEDURE benchmark_baseline.restore() LANGUAGE plpgsql AS $body$ BEGIN\n\
         PERFORM set_config('session_replication_role', 'replica', true);\n\
         DELETE FROM public.bench_scratch;\n",
    );
    for table in 0..TABLES {
        sql.push_str(&format!("DELETE FROM public.bench_table_{table:03};\n"));
    }
    for table in 0..SEEDED_TABLES {
        sql.push_str(&format!(
            "INSERT INTO public.bench_table_{table:03}(id, payload) OVERRIDING SYSTEM VALUE SELECT id, payload FROM benchmark_baseline.bench_table_{table:03};\n"
        ));
    }
    for table in 0..SEQUENCES {
        let (value, called) = if table < SEEDED_TABLES {
            (SEED_ROWS, true)
        } else {
            (1, false)
        };
        sql.push_str(&format!(
            "PERFORM setval('public.bench_table_{table:03}_id_seq'::regclass, {value}, {called});\n"
        ));
    }
    sql.push_str("PERFORM set_config('session_replication_role', 'origin', true);\nEND $body$;\n");
    sql
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

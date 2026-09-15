use anyhow::{Context, Result};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let url = std::env::var("DATABASE_URL").context("DATABASE_URL is not set")?;
    let pool_size = std::env::args()
        .nth(1)
        .map(|value| value.parse())
        .transpose()
        .context("pool size must be an integer")?
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        });
    pgrust_nextest_repro::setup(&url, pool_size).await
}

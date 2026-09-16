use anyhow::Result;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialise process-wide structured-ish tracing once.
///
/// `RUST_LOG=rust_ai_agent=debug,tower_http=info` is useful during local
/// development. The HTTP layer adds request spans and the agent runtime adds
/// execution spans, so one execution can be followed end-to-end.
pub fn init_tracing() -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("rust_ai_agent=info,tower_http=info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init()
        .ok();

    Ok(())
}

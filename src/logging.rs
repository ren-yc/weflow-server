//! tracing initialization (level from config; `RUST_LOG` overrides).

use std::str::FromStr;

pub fn init(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::from_str(level).unwrap_or_default());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
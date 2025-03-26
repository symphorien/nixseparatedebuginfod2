use std::net::SocketAddr;

use clap::Parser;
use tracing_subscriber::prelude::*;

pub mod build_id;
pub mod cache;
pub mod debuginfod;
pub mod nar;
pub mod server;
pub mod store_path;
pub mod substituter;
pub mod utils;

#[cfg(test)]
pub mod test_utils;

/// A debuginfod implementation that fetches debuginfo and sources from nix binary caches
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Options {
    /// Address for the server
    #[arg(short, long, default_value = "127.0.0.1:1949")]
    listen_address: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let (None, Some(dir)) = (
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("CACHE_DIRECTORY"),
    ) {
        // this env var is set by systemd
        std::env::set_var("XDG_CACHE_HOME", dir);
    }
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "nixseparatedebuginfod2=info,tower_http=debug")
    }
    let args = Options::parse();
    let fmt_layer = tracing_subscriber::fmt::layer().without_time();
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    server::run_server(args).await
}

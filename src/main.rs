//! A debuginfod server suitable to serve debug symbols from nix binary caches.
//!
//! ### Architecture
//!
//! Support for various kinds of binary caches is in [substituter].
//!
//! Substituters should not be queries too often for the same store path so a cache implementation
//! is provided in [cache::FetcherCache].
//!
//! The logic mapping build ids to debug symbols, sources, etc. and which is
//! substituter-independent is in [debuginfod::Debuginfod].
//!
//! Functions in [debuginfod::Debuginfod] are reexposed as a server in [server].

#![warn(missing_docs)]

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
    /// Substituter (aka binary cache) containing the debug symbols
    #[arg(short, long)]
    substituter: String,
    /// Directory where files downloaded from the substituter are stored
    #[arg(short, long, default_value_t = default_cache_directory())]
    cache_dir: String,
}

fn default_cache_directory() -> String {
    std::env::var("XDG_CACHE_HOME")
        .map(|x| x + "/" + env!("CARGO_PKG_NAME"))
        .unwrap_or(std::env::var("CACHE_DIRECTORY").unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|x| x + "/.cache/" + env!("CARGO_PKG_NAME"))
                .unwrap_or("/tmp".into())
        }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

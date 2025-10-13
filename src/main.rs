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

use std::{net::SocketAddr, time::Duration};

use anyhow::Context;
use clap::Parser;
use reqwest::Url;
use tracing_subscriber::prelude::*;

pub mod archive_cache;
pub mod build_id;
pub mod cache;
pub mod debuginfod;
pub mod nar;
pub mod server;
pub mod source_selection;
pub mod store_path;
pub mod substituter;
pub mod utils;
pub mod vfs;

#[cfg(test)]
pub mod test_utils;

/// A debuginfod implementation that fetches debuginfo and sources from nix binary caches
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Options {
    /// Address for the server
    #[arg(short, long, default_value = "127.0.0.1:1949")]
    listen_address: SocketAddr,
    /// Substituter (aka binary cache) containing the debug symbols.
    ///
    /// Can be specified several times, all subsituters will be tried in sequence.
    ///
    /// Supported subsituter URLs:
    ///
    /// - `local:` to serve debug symbols already present in the local store
    ///
    /// - `https://cache.nixos.org` for example for http subsitututers
    ///
    /// - `file:///some/dir` for directories created by `nix copy ... --to
    /// file:///some/dir?index-debug-info`
    #[arg(short, long)]
    substituter: Vec<Url>,
    /// Directory where files downloaded from the substituter are stored
    #[arg(short, long, default_value_t = default_cache_directory())]
    cache_dir: String,
    /// How long a fetched file should be kept in cache. Only a rough indication.
    ///
    /// Accepted syntax: `1 day` `3s` `15 minutes` etc.
    #[arg(short, long, value_parser = humantime::parse_duration)]
    expiration: Duration,
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
    let args = Options::parse();
    let filter = std::env::var("RUST_LOG")
        .unwrap_or("nixseparatedebuginfod2=info,tower_http=debug".to_owned());
    let fmt_layer = tracing_subscriber::fmt::layer().without_time().with_filter(
        tracing_subscriber::EnvFilter::builder()
            .parse(&filter)
            .context("parsing RUST_LOG env var")?,
    );
    let registry = tracing_subscriber::registry().with(fmt_layer);

    #[cfg(feature = "tokio-console")]
    let registry = registry.with(console_subscriber::spawn());
    #[cfg(feature = "tracing-chrome")]
    let (chrome_layer, _guard) = tracing_chrome::ChromeLayerBuilder::new().build();
    #[cfg(feature = "tracing-chrome")]
    let registry = registry.with(chrome_layer);

    registry.init();

    server::run_server(args).await
}

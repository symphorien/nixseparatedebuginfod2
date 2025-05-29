//! Fetching nars from nix substituters, aka binary caches.

/// support for `file://` substituters
pub mod file;
/// support for `http://` and `https://` substituters
pub mod http;
/// serve debuginfo from your own store
pub mod local;
use std::path::Path;

use anyhow::Context;
use file::FileSubstituter;
use http::HttpSubstituter;
use local::LocalStoreSubstituter;
use reqwest::Url;
use serde::Deserialize;

use crate::{build_id::BuildId, store_path::StorePath, utils::Presence};

/// Fetching nars from a nix substituter, aka binary cache.
#[async_trait::async_trait]
pub trait Substituter {
    /// Fetches the debug output containing the files for this build-id.
    ///
    /// `into` should be the root of the extracted nar, not the path to the build id files.
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &Path,
    ) -> anyhow::Result<Presence>;

    /// Fetches a store path.
    ///
    /// `into` should be root of the extracted nar, ie contains `bin`, `lib`, etc rather than
    /// `nix/store/hash-name`.
    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &Path,
    ) -> anyhow::Result<Presence>;
}

/// Structure of the metadata files created by the `index-debug-info` option of substituters
#[derive(Deserialize)]
pub struct DebugInfoRedirectJson {
    /// relative path to the nar.xz
    pub archive: String,
    /// relative path to the file inside of the nar
    pub member: String,
}

/// A substituters of unspecified implementation.
pub type BoxedSubstituter = Box<dyn Substituter + Send + Sync + 'static>;

/// Returns a substituter corresponding to the specified url.
///
/// Query params are ignored
///
/// Returns an error if no implementation can handle this url.
pub async fn substituter_from_url(url: &Url) -> anyhow::Result<BoxedSubstituter> {
    match url.scheme() {
        "file" => {
            let path = Path::new(url.path());
            let _ = tokio::fs::metadata(path).await.with_context(|| {
                format!(
                    "cannot use {} as Substituter: {} does not exist",
                    url,
                    path.display()
                )
            })?;
            Ok(Box::new(FileSubstituter::new(path)))
        }
        "http" | "https" => {
            let substituter = Box::new(HttpSubstituter::new(url.clone()))
                .with_context(|| format!("creating an http substituter from {url}"))?;
            Ok(Box::new(substituter))
        }
        "local" => Ok(Box::new(LocalStoreSubstituter)),
        other => {
            anyhow::bail!(
                "I don't know how to handle this kind of Substituter: {}",
                other
            );
        }
    }
}

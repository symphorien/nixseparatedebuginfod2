//! Fetching nars from nix substituters.
//!
//! About terminology:
//! The glossary of the nix manual says:
//!
//! > An additional store from which Nix can obtain store objects instead of building them. Often the substituter is a binary cache, but any store can serve as substituter.
//!
//! So the [LocalStoreSubstituter] serves a substituters which is not a binary cache, but
//! [HttpSubstituter] and [FileSubstituter] refer to substituters which are binary caches.

/// Common code between substituters which are actually binary caches
pub mod binary_cache;
/// support for `file://` substituters
pub mod file;
/// support for `http://` and `https://` substituters
pub mod http;
/// serve debuginfo from your own store
pub mod local;
/// combine several substituters in one single virtual one
pub mod multiplex;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use file::FileSubstituter;
use http::HttpSubstituter;
use local::LocalStoreSubstituter;
use reqwest::Url;

use crate::{build_id::BuildId, store_path::StorePath, vfs::RestrictedPath};

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
/// Encodes if a substituters should be tried first or last in case several substituters are
/// available
pub enum Priority {
    /// Data is local and already unpacked
    LocalUnpacked,
    /// Data is local but compressed
    Local,
    /// Unknown
    Unknown,
    /// Data must be downloaded from the internet
    Remote,
}

/// Fetching debuginfo from a nix substituter
#[async_trait::async_trait]
pub trait Substituter: std::fmt::Debug {
    /// Fetches the debug output corresponding to this build id and returns the path on the
    /// file-system where this output is cached.
    ///
    /// Until the path is dropped, the cache entry cannot be removed.
    ///
    /// Returns None if the substituter does not contain the requested debug output.
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
    ) -> anyhow::Result<Option<RestrictedPath>>;

    /// Fetches the requested store path and returns the path on the
    /// file-system where this output is cached.
    ///
    /// Until the path is dropped, the cache entry cannot be removed.
    ///
    /// Returns None if the substituter does not contain the requested debug output.
    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
    ) -> anyhow::Result<Option<RestrictedPath>>;

    /// A value indicating if this substituter should be tried first if several are available
    ///
    /// Low values mean first
    fn priority(&self) -> Priority;

    /// Spawn periodic cleaning of caches, if any.
    ///
    /// May leak resources, as the trait does not provide a method to stop them.
    fn spawn_cleanup_task(&self);
}

#[async_trait::async_trait]
impl<S: Substituter + Send + Sync> Substituter for Arc<S> {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
    ) -> anyhow::Result<Option<RestrictedPath>> {
        self.as_ref().build_id_to_debug_output(build_id).await
    }

    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
    ) -> anyhow::Result<Option<RestrictedPath>> {
        self.as_ref().fetch_store_path(store_path).await
    }

    fn priority(&self) -> Priority {
        self.as_ref().priority()
    }

    fn spawn_cleanup_task(&self) {
        self.as_ref().spawn_cleanup_task()
    }
}

/// A substituters of unspecified implementation.
pub type BoxedSubstituter = Box<dyn Substituter + Send + Sync + 'static>;

/// Returns a substituter corresponding to the specified url.
///
/// Query params are ignored
///
/// Returns an error if no implementation can handle this url.
///
/// Cache for this substituter will be stored in `cache_path` (directory, must already exist) and
/// expire after approximately `expiration`.
pub async fn substituter_from_url(
    url: &Url,
    cache_path: PathBuf,
    expiration: Duration,
) -> anyhow::Result<BoxedSubstituter> {
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
            let file_substituter = FileSubstituter::new(path, cache_path, expiration)
                .await
                .with_context(|| format!("creating a file substituter for {path:?}"))?;
            Ok(Box::new(file_substituter))
        }
        "http" | "https" => {
            let http_substituter = HttpSubstituter::new(url.clone(), cache_path, expiration)
                .await
                .with_context(|| format!("creating an http substituter from {url}"))?;
            Ok(Box::new(http_substituter))
        }
        "local" => Ok(Box::new(LocalStoreSubstituter::new())),
        other => {
            anyhow::bail!(
                "I don't know how to handle this kind of Substituter: {}",
                other
            );
        }
    }
}

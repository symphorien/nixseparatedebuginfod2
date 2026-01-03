//! Fetching nars from nix substituters.
//!
//! About terminology:
//! The glossary of the nix manual says:
//!
//! > An additional store from which Nix can obtain store objects instead of building them. Often the substituter is a binary cache, but any store can serve as substituter.
//!
//! So the [LocalStoreSubstituter] serves a substituters which is not a binary cache, but
//! [HttpSubstituter] and [FileSubstituter] refer to substituters which are binary caches.

/// support for `file://` substituters
pub mod file;
/// support for `http://` and `https://` substituters
pub mod http;
/// serve debuginfo from your own store
pub mod local;
/// combine several substituters in one single virtual one
pub mod multiplex;

use std::{path::Path, sync::Arc};

use anyhow::Context;
use file::FileSubstituter;
use http::HttpSubstituter;
use local::LocalStoreSubstituter;
use reqwest::Url;
use serde::Deserialize;

use crate::{build_id::BuildId, store_path::StorePath, utils::Presence};

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
    /// Fetches the debug output containing the files for this build-id.
    ///
    /// `into` should be the root of the extracted nar, not the path to the build id files.
    ///
    /// `into` may be created even in case of error
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &Path,
    ) -> anyhow::Result<Presence>;

    /// Fetches a store path.
    ///
    /// `into` should be root of the extracted nar, ie contains `bin`, `lib`, etc rather than
    /// `nix/store/hash-name`.
    ///
    /// If `store_path` is a subdirectory of the full store path, for example
    /// `/nix/store/hash-name/foo/bar` rather than just `/nix/store/hash-name`,
    /// then the `foo/bar` part must be ignored and all of `/nix/store/hash-name`
    /// must be extracted into `into`.
    ///
    /// `into` may be created even in case of error
    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &Path,
    ) -> anyhow::Result<Presence>;

    /// A value indicating if this substituter should be tried first if several are available
    ///
    /// Low values mean first
    fn priority(&self) -> Priority;
}

/// Structure of the metadata files created by the `index-debug-info` option of substituters
#[derive(Deserialize)]
pub struct DebugInfoRedirectJson {
    /// relative path to the nar.xz
    pub archive: String,
    /// relative path to the file inside of the nar
    pub member: String,
}

#[async_trait::async_trait]
impl<S: Substituter + Send + Sync> Substituter for Arc<S> {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &Path,
    ) -> anyhow::Result<Presence> {
        self.as_ref().build_id_to_debug_output(build_id, into).await
    }

    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &Path,
    ) -> anyhow::Result<Presence> {
        self.as_ref().fetch_store_path(store_path, into).await
    }

    fn priority(&self) -> Priority {
        self.as_ref().priority()
    }
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

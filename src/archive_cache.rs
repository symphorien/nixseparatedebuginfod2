//! Unpacking source archives

use anyhow::Context;

use crate::{
    build_id::BuildId,
    cache::{CachableFetcher, FetcherCacheKey},
    utils::Presence,
    vfs::AsFile,
};

use std::fmt::Debug;

/// An archive (tarball, zip, etc) to be unpacked
pub struct SourceArchive {
    /// path of the file
    file: Box<dyn AsFile + Send + Sync>,
    /// BuildId of which this file is the source
    ///
    /// it is assumed that there is at most one source archive per build id
    build_id: BuildId,
}

impl Debug for SourceArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceArchive")
            .field("build_id", &self.build_id)
            .finish()
    }
}

impl SourceArchive {
    /// two source archives from the same build_id will be considered the same
    pub fn new<F: AsFile + Send + Sync + 'static>(file: F, build_id: BuildId) -> Self {
        Self {
            file: Box::new(file),
            build_id,
        }
    }
}

#[derive(Debug, Clone, Copy)]
/// A helper to unpack archives and cache the unpacking.
pub struct ArchiveUnpacker;

impl FetcherCacheKey for SourceArchive {
    fn as_key(&self) -> &str {
        self.build_id.as_key()
    }
}

impl CachableFetcher<SourceArchive> for ArchiveUnpacker {
    async fn fetch<'a>(
        &'a self,
        key: &'a SourceArchive,
        into: &'a std::path::Path,
    ) -> anyhow::Result<crate::utils::Presence> {
        let mut file = key
            .file
            .open()
            .await
            .with_context(|| format!("opening {key:?} for unpacking"))?;
        compress_tools::tokio_support::uncompress_archive(
            &mut file,
            into,
            compress_tools::Ownership::Ignore,
        )
        .await
        .with_context(|| format!("unpacking {key:?}"))?;
        Ok(Presence::Found)
    }
}

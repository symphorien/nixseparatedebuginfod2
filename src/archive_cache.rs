//! Unpacking source archives

use std::path::PathBuf;

use anyhow::Context;

use crate::{
    build_id::BuildId,
    cache::{CachableFetcher, FetcherCacheKey},
    utils::Presence,
};

/// An archive (tarball, zip, etc) to be unpacked
#[derive(Debug, Clone)]
pub struct SourceArchive {
    /// path of the file
    path: PathBuf,
    /// BuildId of which this file is the source
    ///
    /// it is assumed that there is at most one source archive per build id
    build_id: BuildId,
}

impl SourceArchive {
    /// two source archives from the same build_id will be considered the same
    pub fn new<P: Into<PathBuf>>(path: P, build_id: BuildId) -> Self {
        Self {
            path: path.into(),
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
        let mut file = tokio::fs::File::open(&key.path)
            .await
            .with_context(|| format!("opening {} for unpacking", &key.path.display()))?;
        compress_tools::tokio_support::uncompress_archive(
            &mut file,
            into,
            compress_tools::Ownership::Ignore,
        )
        .await
        .with_context(|| format!("unpacking {}", &key.path.display()))?;
        Ok(Presence::Found)
    }
}

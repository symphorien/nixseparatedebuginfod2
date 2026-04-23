use std::{os::unix::ffi::OsStrExt, path::PathBuf};

use anyhow::Context;
use quick_cache::sync::Cache;

use crate::{
    build_id::BuildId,
    store_path::{StorePath, NIX_STORE},
    vfs::RestrictedPath,
};

use super::{Priority, Substituter};

/// serves store paths directly available locally in `/nix/store`
#[derive(Debug)]
pub struct LocalStoreSubstituter {
    cache: Cache<BuildId, PathBuf>,
}

fn find_buildid_in_store(build_id: &BuildId) -> anyhow::Result<Option<PathBuf>> {
    let expected = build_id.in_debug_output("debug");
    for direntry in std::fs::read_dir(NIX_STORE).context("opening local store")? {
        let direntry = direntry.context("iterating local store")?;
        if !direntry.file_name().as_bytes().ends_with(b"-debug") {
            continue;
        }
        let path = direntry.path();
        if path.join(&expected).exists() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

impl Default for LocalStoreSubstituter {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalStoreSubstituter {
    /// A new `LocalStoreSubstituter` for `/nix/store` (hardcoded)
    pub fn new() -> Self {
        LocalStoreSubstituter {
            cache: Cache::new(100),
        }
    }
}

#[async_trait::async_trait]
impl Substituter for LocalStoreSubstituter {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
    ) -> anyhow::Result<Option<RestrictedPath>> {
        let actual_path = match self.cache.get_value_or_guard_async(build_id).await {
            Ok(actual_path) => actual_path,
            Err(placeholder) => {
                let build_id_copy = build_id.clone();
                match tokio::task::spawn_blocking(move || find_buildid_in_store(&build_id_copy))
                    .await??
                {
                    None => return Ok(None),
                    Some(path) => {
                        if let Err(e) = placeholder.insert(path.clone()) {
                            tracing::debug!(err=?e, ?path, "weird, could not insert path into cache");
                        }
                        path
                    }
                }
            }
        };
        Ok(Some(
            RestrictedPath::new(actual_path.clone(), None)
                .await
                .with_context(|| format!("RestrictedPath::new({actual_path:?})"))?,
        ))
    }

    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
    ) -> anyhow::Result<Option<RestrictedPath>> {
        let store_path = store_path.root();
        match tokio::fs::metadata(store_path.as_ref()).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context(format!("stat({})", store_path.as_ref().display())),
            Ok(_) => Ok(Some(
                RestrictedPath::new(store_path.as_ref().to_path_buf(), None)
                    .await
                    .with_context(|| format!("RestrictedPath::new({store_path:?})"))?,
            )),
        }
    }

    fn priority(&self) -> Priority {
        Priority::LocalUnpacked
    }

    // nothing to do
    fn spawn_cleanup_task(&self) {}

    // nothing to do
    async fn shrink_disk_cache(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

use std::{os::unix::ffi::OsStrExt, path::PathBuf};

use anyhow::Context;

use crate::{
    build_id::BuildId,
    store_path::{StorePath, NIX_STORE},
    vfs::RestrictedPath,
};

use super::{Priority, Substituter};

/// serves store paths directly available locally in `/nix/store`
#[derive(Debug)]
pub struct LocalStoreSubstituter;

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

#[async_trait::async_trait]
impl Substituter for LocalStoreSubstituter {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
    ) -> anyhow::Result<Option<RestrictedPath>> {
        let build_id_copy = build_id.clone();
        match tokio::task::spawn_blocking(move || find_buildid_in_store(&build_id_copy)).await?? {
            None => Ok(None),
            Some(path) => Ok(Some(
                RestrictedPath::new(path.to_path_buf(), None)
                    .await
                    .with_context(|| format!("RestrictedPath::new({path:?})"))?,
            )),
        }
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
}

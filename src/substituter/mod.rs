pub mod file;
use std::path::Path;

use anyhow::Context;
use file::FileSubstituter;
use serde::Deserialize;

use crate::{build_id::BuildId, store_path::StorePath, utils::Presence};

#[async_trait::async_trait]
pub trait Substituter {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &Path,
    ) -> anyhow::Result<Presence>;
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

pub type BoxedSubstituter = Box<dyn Substituter + Send + Sync + 'static>;

pub async fn substituter_from_url(url: &str) -> anyhow::Result<BoxedSubstituter> {
    if let Some(path) = url.strip_prefix("file://") {
        let path = Path::new(path);
        let _ = tokio::fs::metadata(path).await.with_context(|| {
            format!(
                "cannot use {} as Substituter: {} does not exist",
                url,
                path.display()
            )
        })?;
        Ok(Box::new(FileSubstituter::new(path)))
    } else {
        anyhow::bail!(
            "I don't know how to handle this kind of Substituter: {}",
            url
        );
    }
}

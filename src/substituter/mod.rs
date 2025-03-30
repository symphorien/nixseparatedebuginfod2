pub mod file;
use std::path::Path;

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

#[async_trait::async_trait]
impl<'a> Substituter for std::sync::Arc<Box<dyn Substituter + Sync + Send + 'a>> {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &Path,
    ) -> anyhow::Result<Presence> {
        self.build_id_to_debug_output(build_id, into).await
    }
    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &Path,
    ) -> anyhow::Result<Presence> {
        self.fetch_store_path(store_path, into).await
    }
}

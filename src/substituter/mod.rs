mod file;
use std::path::Path;

use serde::Deserialize;

use crate::{build_id::BuildId, store_path::StorePath, utils::Presence};

pub trait Substituter {
    fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &Path,
    ) -> impl std::future::Future<Output = anyhow::Result<Presence>> + Send;
    fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &Path,
    ) -> impl std::future::Future<Output = anyhow::Result<Presence>> + Send;
}

/// Structure of the metadata files created by the `index-debug-info` option of substituters
#[derive(Deserialize)]
pub struct DebugInfoRedirectJson {
    /// relative path to the nar.xz
    pub archive: String,
    /// relative path to the file inside of the nar
    pub member: String,
}

pub type BoxedSubstituter = Box<dyn Substituter>;

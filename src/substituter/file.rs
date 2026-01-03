use std::{
    fmt::Debug,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tokio::io::AsyncBufRead;

use crate::substituter::binary_cache::BinaryCache;

use super::Priority;

/// Fetching from `file://` substituters.
///
/// The substituter must have been created with `?index-debug-info=true`.
#[derive(Debug)]
pub struct FileSubstituter {
    path: PathBuf,
}

impl FileSubstituter {
    /// `path` is where the substituter is, minus `file://`
    pub fn new(path: &Path) -> Self {
        FileSubstituter {
            path: path.to_owned(),
        }
    }

    #[cfg(test)]
    /// Returns a file substituter for `tests/fixtures/file_binary_cache`
    pub fn test_fixture() -> Self {
        let path = crate::test_utils::fixture("file_binary_cache");
        assert!(path.exists());
        FileSubstituter::new(&path)
    }
}

impl BinaryCache for FileSubstituter {
    async fn stream_location(
        &self,
        what: &str,
    ) -> anyhow::Result<Option<impl AsyncBufRead + Send>> {
        let full_path = self.path.join(what);
        let full_path = match tokio::fs::canonicalize(&full_path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(e) => return Err(e).context(format!("canonicalize({})", full_path.display())),
            Ok(path) => path,
        };
        anyhow::ensure!(
            full_path.starts_with(&self.path),
            "redirected to nar path {full_path:?} that escapes the Substituter {:?}",
            &self.path,
        );
        match tokio::fs::File::open(&full_path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context(format!("opening nar {}", full_path.display())),
            Ok(reader) => Ok(Some(tokio::io::BufReader::new(reader))),
        }
    }

    fn priority(&self) -> Priority {
        Priority::Local
    }
}

#[tokio::test]
async fn test_build_id_to_debug_output() {
    use crate::substituter::Substituter;
    use crate::test_utils::file_sha256;
    use crate::test_utils::setup_logging;
    setup_logging();
    let substituter = FileSubstituter::test_fixture();
    let dir = tempfile::tempdir().unwrap();
    let into = dir.path().join("into");
    assert_eq!(
        substituter
            .build_id_to_debug_output(
                &crate::build_id::BuildId::new("b87e34547e94f167f4b737f3a25955477a485cc7").unwrap(),
                &into
            )
            .await
            .unwrap(),
        crate::utils::Presence::Found
    );
    assert_eq!(
        file_sha256(
            &into.join("lib/debug/.build-id/b8/7e34547e94f167f4b737f3a25955477a485cc7.debug")
        )
        .await,
        "b7b38a0c43ec066a034e38f86f5f0926867b9eb2144fd8a7aac88c7c38bf5566"
    );
}

#[tokio::test]
async fn test_fetch_store_path() {
    use crate::substituter::Substituter;
    use crate::test_utils::file_sha256;
    use crate::test_utils::setup_logging;
    setup_logging();
    let substituter = FileSubstituter::test_fixture();
    let dir = tempfile::tempdir().unwrap();
    let into = dir.path().join("into");
    assert_eq!(
        substituter
            .fetch_store_path(
                &crate::store_path::StorePath::new(Path::new(
                    "/nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1"
                ))
                .unwrap(),
                &into
            )
            .await
            .unwrap(),
        crate::utils::Presence::Found
    );
    assert_eq!(
        file_sha256(&into.join("bin/make")).await,
        "bef9ec5e1fe7ccacbf00b1053c6de54de9857ec3d173504190462a01ed3cc52e"
    );
}

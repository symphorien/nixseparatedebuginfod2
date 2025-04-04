use std::{
    fmt::Debug,
    ops::Deref,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tokio::io::{AsyncReadExt, BufReader};

use crate::nar::unpack_nar;
use crate::{
    build_id::BuildId,
    nar::narinfo_to_nar_location,
    store_path::StorePath,
    utils::{DecompressingReader, Presence},
};

use super::{DebugInfoRedirectJson, Substituter};

const SMALL_FILE_SIZE: usize = 1024 * 1024 - 1;

/// Returns the content of the specified file
///
/// Fails if the file is larger than `SMALL_FILE_SIZE`.
///
/// Returns None if the file does not exist.
async fn read_small_file(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let file = match tokio::fs::File::open(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        Ok(file) => file,
    };
    let mut limited = file.take(SMALL_FILE_SIZE as u64 + 1);
    let mut result = Vec::with_capacity(100);
    limited
        .read_to_end(&mut result)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    anyhow::ensure!(
        result.len() <= SMALL_FILE_SIZE,
        "{} is too large; expected file unter {} bytes",
        path.display(),
        SMALL_FILE_SIZE
    );
    Ok(Some(result))
}

#[tokio::test]
async fn test_read_small_file_small() {
    let small = vec![b'a'; SMALL_FILE_SIZE];
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test");
    std::fs::write(&file, &small).unwrap();
    let read = read_small_file(&file).await.unwrap().unwrap();
    assert_eq!(&read, &small);
}

#[tokio::test]
async fn test_read_small_file_big() {
    let small = vec![b'a'; SMALL_FILE_SIZE + 1];
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test");
    std::fs::write(&file, &small).unwrap();
    read_small_file(&file).await.unwrap_err();
}

#[tokio::test]
async fn test_read_small_file_missing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test");
    read_small_file(&file).await.unwrap().ok_or(()).unwrap_err();
}

/// Fetching from `file:://` substituters.
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
    pub fn test_fixture() -> Self {
        let path = crate::test_utils::fixture("file_binary_cache");
        assert!(path.exists());
        FileSubstituter::new(&path)
    }

    async fn return_nar(&self, nar_path: &Path, into: &Path) -> anyhow::Result<Presence> {
        let nar_path = match tokio::fs::canonicalize(nar_path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    "{:?}: got redirected to missing nar {}",
                    &self,
                    nar_path.display()
                );
                return Ok(Presence::NotFound);
            }
            Err(e) => return Err(e).context(format!("canonicalize({})", nar_path.display())),
            Ok(path) => path,
        };
        anyhow::ensure!(
            nar_path.starts_with(&self.path),
            "redirected to nar path that escapes the Substituter: {}",
            nar_path.display()
        );
        let nar_reader = match tokio::fs::File::open(&nar_path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(
                    "{:?}: got redirected to missing nar: failed to open {}",
                    &self,
                    nar_path.display()
                );
                return Ok(Presence::NotFound);
            }
            Err(e) => return Err(e).context(format!("opening nar {}", nar_path.display())),
            Ok(reader) => tokio::io::BufReader::new(reader),
        };
        let decompressing_nar_reader =
            DecompressingReader::new(nar_reader, nar_path.as_os_str().as_bytes())?;
        unpack_nar(decompressing_nar_reader, into).await?;

        Ok(Presence::Found)
    }
}

#[async_trait::async_trait]
impl Substituter for FileSubstituter {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &std::path::Path,
    ) -> anyhow::Result<Presence> {
        let meta = self.path.join(format!("debuginfo/{}.debug", build_id));
        let Some(json) = read_small_file(&meta)
            .await
            .context("looking for json redirect to debuginfo")?
        else {
            tracing::debug!(
                build_id = build_id.deref(),
                "{} is missing from {:?}",
                meta.display(),
                &self
            );
            return Ok(Presence::NotFound);
        };
        let redirect: DebugInfoRedirectJson = serde_json::from_slice(&json)
            .with_context(|| format!("unexpected format for {}", meta.display()))?;
        let nar_path = self.path.join("debuginfo").join(&redirect.archive);
        self.return_nar(&nar_path, into).await
    }

    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &std::path::Path,
    ) -> anyhow::Result<Presence> {
        let narinfo_path = self.path.join(format!("{}.narinfo", store_path.hash()));
        let fd = match tokio::fs::File::open(&narinfo_path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Presence::NotFound),
            Err(e) => return Err(e).context(format!("opening {}", narinfo_path.display())),
            Ok(fd) => BufReader::new(fd),
        };
        let url = narinfo_to_nar_location(fd)
            .await
            .with_context(|| format!("parsing {}", narinfo_path.display()))?;
        let nar_path = self.path.join(url);
        self.return_nar(&nar_path, into).await
    }
}

#[tokio::test]
async fn test_build_id_to_debug_output() {
    use crate::test_utils::file_sha256;
    use crate::test_utils::setup_logging;
    setup_logging();
    let substituter = FileSubstituter::test_fixture();
    let dir = tempfile::tempdir().unwrap();
    let into = dir.path().join("into");
    assert_eq!(
        substituter
            .build_id_to_debug_output(
                &BuildId::new("483bd7f7229bdb06462222e1e353e4f37e15c293").unwrap(),
                &into
            )
            .await
            .unwrap(),
        Presence::Found
    );
    assert_eq!(
        file_sha256(
            &into.join("lib/debug/.build-id/48/3bd7f7229bdb06462222e1e353e4f37e15c293.debug")
        ),
        "e8bcbed1f80e8fcaeb622ad1c1c77a526047ace2f73b75ef1128b47a6d317bb0"
    );
}

#[tokio::test]
async fn test_fetch_store_path() {
    use crate::test_utils::file_sha256;
    use crate::test_utils::setup_logging;
    setup_logging();
    let substituter = FileSubstituter::test_fixture();
    let dir = tempfile::tempdir().unwrap();
    let into = dir.path().join("into");
    assert_eq!(
        substituter
            .fetch_store_path(
                &StorePath::new(Path::new(
                    "/nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1"
                ))
                .unwrap(),
                &into
            )
            .await
            .unwrap(),
        Presence::Found
    );
    assert_eq!(
        file_sha256(&into.join("bin/make")),
        "a7942bdec982d11d0467e84743bee92138038e7a38f37ec08e5cc6fa5e3d18f3"
    );
}

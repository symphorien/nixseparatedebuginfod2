//! Logic to find debuginfo in a substituter
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use tracing::Level;

use crate::{
    archive_cache::{ArchiveUnpacker, SourceArchive},
    build_id::BuildId,
    cache::{CachableFetcher, CachedPath, FetcherCache, FetcherCacheKey},
    source_selection::{get_file_for_source, SourceMatch},
    store_path::StorePath,
    substituter::{BoxedSubstituter, Substituter},
    utils::Presence,
};

impl FetcherCacheKey for BuildId {
    fn as_key(&self) -> &str {
        self.as_ref()
    }
}
impl FetcherCacheKey for StorePath {
    fn as_key(&self) -> &str {
        self.hash()
    }
}

impl<T: Substituter + Send + Sync + ?Sized + 'static> CachableFetcher<BuildId> for Arc<Box<T>> {
    async fn fetch<'a>(&'a self, key: &'a BuildId, into: &'a Path) -> anyhow::Result<Presence> {
        self.build_id_to_debug_output(key, into).await
    }
}

impl<T: Substituter + Send + Sync + ?Sized + 'static> CachableFetcher<StorePath> for Arc<Box<T>> {
    async fn fetch<'a>(&'a self, key: &'a StorePath, into: &'a Path) -> anyhow::Result<Presence> {
        self.fetch_store_path(key, into).await
    }
}

const MAX_SYMLINK_DEPTH: usize = 20;

/// The logic behind a debuginfod server: maps build ids to debug symbols, executables, and source
/// files.
///
/// Cloning it returns a reference to the same debuginfod instance.
#[derive(Clone)]
pub struct Debuginfod {
    debuginfo_fetcher: Arc<FetcherCache<BuildId, Arc<BoxedSubstituter>>>,
    store_fetcher: Arc<FetcherCache<StorePath, Arc<BoxedSubstituter>>>,
    source_unpacker: Arc<FetcherCache<SourceArchive, ArchiveUnpacker>>,
}

/// Returns the input if this file exists or None if it does not
async fn return_if_exists(path: CachedPath) -> anyhow::Result<Option<CachedPath>> {
    match path.as_ref().metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context(format!(
            "testing existence of file {} before returning it from debuginfod",
            path.as_ref().display()
        )),
        Ok(_) => Ok(Some(path)),
    }
}

/// Creates this directory if it does not exist yet.
async fn ensure_dir_exists(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::create_dir(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).context(format!("creating {} for debuginfod implem", path.display())),
    }
}

impl Debuginfod {
    /// Create a [`Debuginfod`] instance which fetches debug symbols from `substituter` and stores
    /// cached files into `cache_path`.
    ///
    /// `duration` is an indication of how long a cached but unread path must be kept
    pub async fn new(
        cache_path: PathBuf,
        substituter: BoxedSubstituter,
        expiration: Duration,
    ) -> anyhow::Result<Self> {
        ensure_dir_exists(&cache_path).await?;
        let debuginfo_path = cache_path.join("debuginfo");
        let store_path = cache_path.join("store");
        let source_path = cache_path.join("sources");
        ensure_dir_exists(&debuginfo_path).await?;
        ensure_dir_exists(&store_path).await?;
        ensure_dir_exists(&source_path).await?;
        let substituter = Arc::new(substituter);
        Ok(Self {
            debuginfo_fetcher: Arc::new(
                FetcherCache::new(debuginfo_path, substituter.clone(), expiration).await?,
            ),
            store_fetcher: Arc::new(
                FetcherCache::new(store_path, substituter.clone(), expiration).await?,
            ),
            source_unpacker: Arc::new(
                FetcherCache::new(source_path, ArchiveUnpacker, expiration).await?,
            ),
        })
    }

    /// Spawns tokio tasks to clear downloaded files from the cache when they have not been queried
    /// for too long.
    pub fn spawn_cleanup_task(&self) {
        self.debuginfo_fetcher.clone().spawn_cleanup_task();
        self.store_fetcher.clone().spawn_cleanup_task();
    }

    /// Returns the path to ELF object with debug symbols for this build id.
    pub async fn debuginfo<'key, 'debuginfod: 'key>(
        &'debuginfod self,
        build_id: &'key BuildId,
    ) -> anyhow::Result<Option<CachedPath>> {
        match self.debuginfo_fetcher.get(build_id.clone()).await {
            Ok(Some(nar)) => {
                let debugfile = nar.join(build_id.in_debug_output("debug"));
                return_if_exists(debugfile).await
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// If the concrete path is a symlink to the store, fetch the store path containing the target, and return the concrete file representing the target.
    ///
    /// Re-resolves the target if the target is still a symlink, until some max depth.
    ///
    /// Return a file guaranteed to exist and not be a symlink.
    #[tracing::instrument(level=Level::DEBUG, skip_all, fields(potential_symlink=%potential_symlink.as_ref().display()))]
    async fn resolve_symlink_to_store(
        &self,
        mut potential_symlink: CachedPath,
    ) -> anyhow::Result<Option<CachedPath>> {
        let mut remaining_depth = MAX_SYMLINK_DEPTH;
        loop {
            match potential_symlink.as_ref().symlink_metadata() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(e).context(format!(
                    "testing existence of potential symlink {} before returning it from debuginfod",
                    potential_symlink.as_ref().display()
                ))
                }
                Ok(m) => {
                    if !m.is_symlink() {
                        return Ok(Some(potential_symlink));
                    } else {
                        if remaining_depth == 0 {
                            anyhow::bail!("{} is still a symlink after {MAX_SYMLINK_DEPTH} readlink() operations", potential_symlink.as_ref().display());
                        }
                        remaining_depth -= 1;
                        let link_content = tokio::fs::read_link(potential_symlink.as_ref())
                            .await
                            .with_context(|| {
                            format!("readlink({})", potential_symlink.as_ref().display())
                        })?;
                        let next_store_path = if link_content.is_relative() {
                            let dir = potential_symlink.clone().parent()?;
                            let path = dir.join(link_content);
                            match tokio::fs::metadata(&path).await {
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                    return Ok(None)
                                }
                                Err(e) => {
                                    return Err(e).context(format!("testing existence of {path:?}"))
                                }
                                Ok(_) => (),
                            };
                            path
                        } else {
                            let target_store_path =
                                StorePath::new(&link_content).with_context(|| {
                                    format!(
                                        "symlink {} does not point to store path but {}",
                                        potential_symlink.as_ref().display(),
                                        link_content.display()
                                    )
                                })?;
                            match self.store_fetcher.get(target_store_path.clone()).await {
                                Err(e) => {
                                    return Err(e).context(format!(
                                        "pointed at by {}",
                                        potential_symlink.as_ref().display()
                                    ))
                                }
                                Ok(None) => return Ok(None),
                                Ok(Some(path)) => path.join(target_store_path.relative()),
                            }
                        };
                        tracing::debug!(
                            "resolved symlink {} to {}",
                            potential_symlink.as_ref().display(),
                            next_store_path.as_ref().display()
                        );
                        potential_symlink = next_store_path;
                    }
                }
            }
        }
    }

    /// Returns the path to the ELF object with this build id.
    ///
    /// It is called executable, but it could also be a share object.
    pub async fn executable<'key, 'debuginfod: 'key>(
        &'debuginfod self,
        build_id: &'key BuildId,
    ) -> anyhow::Result<Option<CachedPath>> {
        match self.debuginfo_fetcher.get(build_id.clone()).await {
            Ok(Some(nar)) => {
                let symlink = nar.join(build_id.in_debug_output("executable"));
                self.resolve_symlink_to_store(symlink).await
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Return the source file matching `path` that led to the compilation of the executable with
    /// the specified build id.
    ///
    /// Matching `path` to actual source file is somewhat fuzzy.
    pub async fn source(
        &self,
        build_id: &BuildId,
        path: &str,
    ) -> anyhow::Result<Option<CachedPath>> {
        // when gdb attempts to show the source of a function that comes
        // from a header in another library, the request is store path made
        // relative to /
        // in this case, let's fetch it
        if path.starts_with("nix/store") {
            let absolute = PathBuf::from("/").join(path);
            let store_path = StorePath::new(&absolute).context("invalid store path")?;
            let demangled = store_path.demangle();
            match self
                .store_fetcher
                .get(demangled.clone())
                .await
                .with_context(|| format!("downloading source {}", demangled.as_ref().display()))?
            {
                None => Ok(None),
                Some(cached_root) => return_if_exists(cached_root.join(demangled.relative())).await,
            }
        } else {
            // as a fallback, have a look at the source of the buildid
            let debug_output = match self.debuginfo_fetcher.get(build_id.clone()).await {
                Ok(Some(nar)) => nar,
                Ok(None) => return Ok(None),
                Err(e) => return Err(e),
            };
            let source_symlink = debug_output
                .clone()
                .join(build_id.in_debug_output("source"));
            let Some(source) = self.resolve_symlink_to_store(source_symlink).await? else {
                return Ok(None);
            };
            let source_dir = if source.as_ref().is_dir() {
                source
            } else {
                let archive = SourceArchive::new(source.as_ref(), build_id.clone());
                match self.source_unpacker.get(archive).await? {
                    None => return Ok(None),
                    Some(x) => x,
                }
            };
            let overlay_symlink = debug_output.join(build_id.in_debug_output("sourceoverlay"));
            let overlay_symlink_path = overlay_symlink.as_ref().to_owned();
            let overlay_dir = self
                .resolve_symlink_to_store(overlay_symlink)
                .await?
                .unwrap_or_else(|| {
                    tracing::warn!("{overlay_symlink_path:?} is missing");
                    source_dir.clone()
                });
            let source_dir_path = source_dir.as_ref().to_path_buf();
            let overaly_dir_path = overlay_dir.as_ref().to_path_buf();
            let request = PathBuf::from(path);
            let matching_file = match tokio::task::spawn_blocking(move || {
                get_file_for_source(&source_dir_path, &overaly_dir_path, &request)
            })
            .await??
            {
                None => return Ok(None),
                Some(SourceMatch::Source(p)) => source_dir.join(p),
                Some(SourceMatch::Overlay(p)) => overlay_dir.join(p),
            };
            self.resolve_symlink_to_store(matching_file).await
        }
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use tempfile::tempdir;

    use crate::{
        build_id::BuildId,
        debuginfod::Debuginfod,
        substituter::file::FileSubstituter,
        test_utils::{file_sha256, setup_logging},
    };

    #[tokio::test]
    async fn test_debuginfo_nominal() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/bin/make
        let debuginfo = debuginfod
            .debuginfo(&BuildId::new("66b33fee92bf535e40d29622ce45b4bd01bebc1f").unwrap())
            .await
            .unwrap()
            .unwrap();
        // /nix/store/w4pl4nw4lygw0sca2q0667fkz5b92lvk-gnumake-4.4.1-debug/lib/debug/make
        assert_eq!(
            file_sha256(debuginfo.as_ref()),
            "c7d7299291732384a47af188410469be6e6cdac3ad8652b93947462489d7f2f9"
        );
    }

    #[tokio::test]
    async fn test_executable_nominal() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("66b33fee92bf535e40d29622ce45b4bd01bebc1f").unwrap();
        let executable = debuginfod.executable(&buildid).await.unwrap().unwrap();
        assert_eq!(
            file_sha256(dbg!(executable.as_ref())),
            "a7942bdec982d11d0467e84743bee92138038e7a38f37ec08e5cc6fa5e3d18f3"
        );
    }

    #[tokio::test]
    async fn test_source_explicit_store_path() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("66b33fee92bf535e40d29622ce45b4bd01bebc1f").unwrap();
        let path = "nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/include/gnumake.h";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        assert_eq!(
            file_sha256(dbg!(source.as_ref())),
            "3e38df96688ba32938ece2070219684616bd157750c8ba5042ccb790a49dcacc"
        );
    }

    #[tokio::test]
    async fn test_source_explicit_mangled_store_path() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("66b33fee92bf535e40d29622ce45b4bd01bebc1f").unwrap();
        let path = "nix/store/6I1HJK6PA24A29SCQHIH4KZ1VFPGDRCD-gnumake-4.4.1/include/gnumake.h";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        assert_eq!(
            file_sha256(dbg!(source.as_ref())),
            "3e38df96688ba32938ece2070219684616bd157750c8ba5042ccb790a49dcacc"
        );
    }

    #[tokio::test]
    async fn test_source_missing_store_path() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("66b33fee92bf535e40d29622ce45b4bd01bebc1f").unwrap();
        let path = "nix/store/6I1H00000000000000004KZ1VFPGDRCD-gnumake-4.4.1/include/gnumake.h";
        let source = debuginfod.source(&buildid, path).await.unwrap();
        assert!(dbg!(source).is_none());
    }

    #[tokio::test]
    async fn test_source_missing_file_in_store_path() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("66b33fee92bf535e40d29622ce45b4bd01bebc1f").unwrap();
        let path = "nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/include/gnumake_does_not_exist.h";
        let source = debuginfod.source(&buildid, path).await.unwrap();
        assert!(dbg!(source).is_none());
    }

    #[tokio::test]
    async fn test_source_in_source_dir() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/anp6npvr7pmh8hdaqk6c9xm57pzrnqw3-ninja-1.12.1/bin/ninja
        let buildid = BuildId::new("483bd7f7229bdb06462222e1e353e4f37e15c293").unwrap();
        let path = "build/source/src/ninja.cc";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        // /nix/store/n11lk1q63864l8vfdl8h8aja1shs3yr7-source/src/ninja.cc
        assert_eq!(
            file_sha256(dbg!(source.as_ref())),
            "5d013f718e1822493a98c5ca0c69fad4ec2279a0005a2cea8d665284563c3480"
        );
    }

    #[tokio::test]
    async fn test_source_in_archive() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/6i1hjk6pa24a29scqhih4kz1vfpgdrcd-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("66b33fee92bf535e40d29622ce45b4bd01bebc1f").unwrap();
        let path = "/build/make-4.4.1/src/main.c";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        // /nix/store/0avnvyc7pkcr4pjqws7hwpy87m6wlnjc-make-4.4.1.tar.gz > make-4.4.1/src/main.c
        assert_eq!(
            file_sha256(dbg!(source.as_ref())),
            "7f0b8a02a6449507c751cdf3315a11bb0e99f22dc75a33a8b82b9e78c9f0bff0"
        );
    }

    #[tokio::test]
    async fn test_source_in_archive_patched() {
        setup_logging();
        let t = tempdir().unwrap();
        let substituter = FileSubstituter::test_fixture();
        let debuginfod = Debuginfod::new(
            t.path().into(),
            Box::new(substituter),
            Duration::from_secs(1000),
        )
        .await
        .unwrap();
        // /nix/store/sbrb2ymlvq2bg7v8nf7p7qkqg5q2ks32-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("45a9ee3e03d0ab4797561a6668e85a5be6a86262").unwrap();
        let path = "/build/make-4.4.1/src/job.c";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        // /nix/store/sj8bfxjk8scdkgmlpan0s8cqccf0ny9j-gnumake-4.4.1-debug/src/overlay/make-4.4.1/src/job.c
        assert_eq!(
            file_sha256(dbg!(source.as_ref())),
            "65c819269ed09f81de1d1659efb76008f23bb748c805409f1ad5f782d15836df"
        );
    }
}

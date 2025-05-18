//! Logic to find debuginfo in a substituter
use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;

use crate::{
    build_id::BuildId,
    cache::{CachableFetcher, CachedPath, FetcherCache, FetcherCacheKey},
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

/// The logic behind a debuginfod server: maps build ids to debug symbols, executables, and source
/// files.
///
/// Cloning it returns a reference to the same debuginfod instance.
#[derive(Clone)]
pub struct Debuginfod {
    debuginfo_fetcher: Arc<FetcherCache<BuildId, Arc<BoxedSubstituter>>>,
    store_fetcher: Arc<FetcherCache<StorePath, Arc<BoxedSubstituter>>>,
    // source_unpacker: FetcherCache,
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
        ensure_dir_exists(&debuginfo_path).await?;
        ensure_dir_exists(&store_path).await?;
        let substituter = Arc::new(substituter);
        Ok(Self {
            debuginfo_fetcher: Arc::new(
                FetcherCache::new(debuginfo_path, substituter.clone(), expiration).await?,
            ),
            store_fetcher: Arc::new(
                FetcherCache::new(store_path, substituter.clone(), expiration).await?,
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
                let debugfile = nar.join(&build_id.in_debug_output("debug"));
                return_if_exists(debugfile).await
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
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
                let symlink = nar.join(&build_id.in_debug_output("executable"));
                match tokio::fs::read_link(symlink.as_ref()).await {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e).context(format!(
                        "dereferencing symlink from debug output to executable {}",
                        symlink.as_ref().display()
                    )),
                    Ok(exe_in_store_path) => {
                        let exe_in_store_path =
                            StorePath::new(&exe_in_store_path).with_context(|| {
                                format!(
                                    "symlink to executable {} is not a store path",
                                    symlink.as_ref().display()
                                )
                            })?;
                        tracing::debug!(
                            build_id = build_id.deref(),
                            "executable symlink points to {}",
                            exe_in_store_path.as_ref().display()
                        );
                        match self.store_fetcher.get(exe_in_store_path.clone()).await {
                            Ok(Some(store_path_root)) => {
                                let actual_file =
                                    store_path_root.join(exe_in_store_path.relative());
                                tracing::debug!(
                                    build_id = build_id.deref(),
                                    "fetched {} as {}",
                                    exe_in_store_path.as_ref().display(),
                                    actual_file.as_ref().display()
                                );
                                // FIXME: what if it still a symlink to another store path
                                return_if_exists(actual_file).await
                            }
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                }
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
            let fetched_root = self
                .store_fetcher
                .get(demangled.clone())
                .await
                .with_context(|| format!("downloading source {}", demangled.as_ref().display()))?;
            Ok(fetched_root.map(|path| path.join(demangled.relative())))
        } else {
            // as a fallback, have a look at the source of the buildid
            todo!()
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
}

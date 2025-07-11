//! Logic to find debuginfo in a substituter
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;

use crate::{
    archive_cache::{ArchiveUnpacker, SourceArchive},
    build_id::BuildId,
    cache::{CachableFetcher, FetcherCache, FetcherCacheKey},
    source_selection::{get_file_for_source, SourceMatch},
    store_path::StorePath,
    substituter::{BoxedSubstituter, Substituter},
    utils::Presence,
    vfs::{ResolvedPath, ResolvedPathKind, RestrictedPath},
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
    source_unpacker: Arc<FetcherCache<SourceArchive, ArchiveUnpacker>>,
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
    ) -> anyhow::Result<Option<ResolvedPath>> {
        match self.debuginfo_fetcher.get(build_id.clone()).await {
            Ok(Some(nar)) => {
                let debugfile = nar.join(build_id.in_debug_output("debug"));
                debugfile.resolve_inside_root().await
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
    ) -> anyhow::Result<Option<ResolvedPath>> {
        match self.debuginfo_fetcher.get(build_id.clone()).await {
            Ok(Some(nar)) => {
                let symlink = nar.join(build_id.in_debug_output("executable"));
                self.resolve_symlinks(symlink).await
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn resolve_symlinks(&self, path: RestrictedPath) -> anyhow::Result<Option<ResolvedPath>> {
        path.resolve(|s| self.store_fetcher.get(s)).await
    }

    /// Return the source file matching `path` that led to the compilation of the executable with
    /// the specified build id.
    ///
    /// Matching `path` to actual source file is somewhat fuzzy.
    pub async fn source(
        &self,
        build_id: &BuildId,
        path: &str,
    ) -> anyhow::Result<Option<ResolvedPath>> {
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
                Some(cached_root) => {
                    let path = cached_root.join(demangled.relative());
                    self.resolve_symlinks(path).await
                }
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
            let Some(source) = self.resolve_symlinks(source_symlink).await? else {
                return Ok(None);
            };
            let source_dir = if source.kind().await? == ResolvedPathKind::Directory {
                source
            } else {
                let archive = SourceArchive::new(source, build_id.clone());
                match self.source_unpacker.get(archive).await? {
                    None => return Ok(None),
                    Some(x) => match x.resolve_inside_root().await? {
                        None => return Ok(None),
                        Some(y) => y,
                    },
                }
            };
            let overlay_symlink = debug_output.join(build_id.in_debug_output("sourceoverlay"));
            // let overlay_symlink_path = overlay_symlink.as_ref().to_owned();
            let overlay_dir = self
                .resolve_symlinks(overlay_symlink.clone())
                .await?
                .unwrap_or_else(|| {
                    // FIXME: temporary, should error
                    tracing::warn!("{overlay_symlink:?} is missing");
                    source_dir.clone()
                });
            let source_dir_clone = source_dir.clone();
            let overlay_dir_clone = overlay_dir.clone();
            let request = PathBuf::from(path);
            let matching_file = match tokio::task::spawn_blocking(move || {
                get_file_for_source(&source_dir_clone, &overlay_dir_clone, &request)
            })
            .await??
            {
                None => return Ok(None),
                Some(SourceMatch::Source(p)) => source_dir.join(p).await?,
                Some(SourceMatch::Overlay(p)) => overlay_dir.join(p).await?,
            };
            self.resolve_symlinks(matching_file).await
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
        // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
        let debuginfo = debuginfod
            .debuginfo(&BuildId::new("0e20481820d3b92468102b35a5e4a29a8695c1af").unwrap())
            .await
            .unwrap()
            .unwrap();
        // /nix/store/dlkw5480vfxdi21rybli43ii782czp94-gnumake-4.4.1-debug/lib/debug/make
        assert_eq!(
            file_sha256(debuginfo).await,
            "8f62cc563915e10f870bd7991ad88e535f842a8dd7afcba30c597b3bb6e728ad"
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
        // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("0e20481820d3b92468102b35a5e4a29a8695c1af").unwrap();
        let executable = debuginfod.executable(&buildid).await.unwrap().unwrap();
        assert_eq!(
            file_sha256(dbg!(executable)).await,
            "bef9ec5e1fe7ccacbf00b1053c6de54de9857ec3d173504190462a01ed3cc52e"
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
        // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("0e20481820d3b92468102b35a5e4a29a8695c1af").unwrap();
        let path = "nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/include/gnumake.h";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        assert_eq!(
            file_sha256(dbg!(source)).await,
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
        // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("0e20481820d3b92468102b35a5e4a29a8695c1af").unwrap();
        let path = "nix/store/34J18R2RPI7JS1WHMVZM9WLIAD55RILR-gnumake-4.4.1/include/gnumake.h";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        assert_eq!(
            file_sha256(dbg!(source)).await,
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
        // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("0e20481820d3b92468102b35a5e4a29a8695c1af").unwrap();
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
        // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("0e20481820d3b92468102b35a5e4a29a8695c1af").unwrap();
        let path = "nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/include/gnumake_does_not_exist.h";
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
        // /nix/store/pbqih0cmbc4xilscj36m80ardhg6kawp-systemd-minimal-257.6/bin/systemctl
        let buildid = BuildId::new("b87e34547e94f167f4b737f3a25955477a485cc7").unwrap();
        let path = "../src/systemctl/systemctl.c";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        // /nix/store/2qw62845796lyx649ck67zbk04pv8xhf-source/src/systemctl/systemctl.c
        assert_eq!(
            file_sha256(dbg!(source)).await,
            "402ec408cd95844108e0c93e6bc249b97941a901fbc89ad8d3f45a07c5916708"
        );
    }

    #[tokio::test]
    async fn test_source_in_source_dir_patched() {
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
        // /nix/store/pbqih0cmbc4xilscj36m80ardhg6kawp-systemd-minimal-257.6/bin/systemctl
        let buildid = BuildId::new("b87e34547e94f167f4b737f3a25955477a485cc7").unwrap();
        let path = "../src/core/manager.c";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        // /nix/store/80nn028rq690b6qk8qprkvfbln38crdx-systemd-minimal-257.6-debug/src/overlay/source/src/core/manager.c
        assert_eq!(
            file_sha256(dbg!(source)).await,
            "62b2117906718dc4b66c128b6f3f81cb24f1c85c2df20de13be7b010087f17f6"
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
        // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("0e20481820d3b92468102b35a5e4a29a8695c1af").unwrap();
        let path = "/build/make-4.4.1/src/main.c";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        // /nix/store/0avnvyc7pkcr4pjqws7hwpy87m6wlnjc-make-4.4.1.tar.gz > make-4.4.1/src/main.c
        assert_eq!(
            file_sha256(dbg!(source)).await,
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
        // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
        let buildid = BuildId::new("0e20481820d3b92468102b35a5e4a29a8695c1af").unwrap();
        let path = "/build/make-4.4.1/src/job.c";
        let source = debuginfod.source(&buildid, path).await.unwrap().unwrap();
        // /nix/store/dlkw5480vfxdi21rybli43ii782czp94-gnumake-4.4.1-debug/src/overlay/make-4.4.1/src/job.c
        assert_eq!(
            file_sha256(dbg!(source)).await,
            "65c819269ed09f81de1d1659efb76008f23bb748c805409f1ad5f782d15836df"
        );
    }
}

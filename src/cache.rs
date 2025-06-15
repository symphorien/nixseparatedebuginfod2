//! Support for caching functions that put stuff into a directory

// avoids higher ranked lifetime errors
#![allow(clippy::manual_async_fn)]
use std::{
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::Context;
use async_lock::{RwLock, RwLockReadGuardArc, RwLockUpgradableReadGuardArc, RwLockWriteGuardArc};
use tracing::{instrument, Instrument, Level};
use weak_table::WeakValueHashMap;

use crate::{
    utils::{remove_recursively_if_exists, touch, Presence},
    vfs::RestrictedPath,
};

/// Fetchers are called to writer in a directory there.
///
/// Only if they complete successfully the output is moved to [`CACHE`]
const PARTIAL: &str = "partial";
/// Directory where finished outputs are stored.
const CACHE: &str = "cache";

/// An argument to a fetcher that can be used with [`FetcherCache`]
pub trait FetcherCacheKey: Debug + Send + Sync {
    /// A text representation of the key suitable as a directory name
    ///
    /// The result must not contain `/`.
    ///
    /// This function must be injective.
    fn as_key(&self) -> &str;
}

struct LockedCacheEntry<Key: FetcherCacheKey, Lock> {
    pub key: Key,
    pub target: PathBuf,
    lock: Lock,
}

impl<Key: FetcherCacheKey, Lock> LockedCacheEntry<Key, Lock> {
    async fn map<NewLock, Fut: Future<Output = NewLock> + Send, F: FnOnce(Lock) -> Fut>(
        self,
        f: F,
    ) -> LockedCacheEntry<Key, NewLock> {
        let LockedCacheEntry { key, target, lock } = self;
        LockedCacheEntry {
            key,
            target,
            lock: f(lock).await,
        }
    }
    fn map_sync<NewLock, F: FnOnce(Lock) -> NewLock>(self, f: F) -> LockedCacheEntry<Key, NewLock> {
        let LockedCacheEntry { key, target, lock } = self;
        LockedCacheEntry {
            key,
            target,
            lock: f(lock),
        }
    }
}

/// While this stucture is not dropped, the directory for this cache key may not be modified.
type ReadLockedCacheEntry<Key> = LockedCacheEntry<Key, RwLockReadGuardArc<()>>;

/// While this stucture is not dropped, the directory for this cache key may not be modified.
type UpgradableReadLockedCacheEntry<Key> = LockedCacheEntry<Key, RwLockUpgradableReadGuardArc<()>>;
/// While this structure is not dropped, only the owner may modify the directory corresponding to
/// this key.
type WriteLockedCacheEntry<Key> = LockedCacheEntry<Key, RwLockWriteGuardArc<()>>;

/// A function that can be cached with [`FetcherCache`].
pub trait CachableFetcher<Key: FetcherCacheKey>: Send + Sync {
    /// Retrieve a file or directory corresponding to `key` and copy it to path `into`.
    ///
    /// The fetcher does not need to ensure that the target is cleaned up in case of failure.
    ///
    /// The fetcher can return `Ok(Presence::NotFound)` to signify that the fetcher determined
    /// successfully that there is no file/directory to be fetched for `key`.
    ///
    /// The debuginfo server will return 404 instead of 5xx.
    fn fetch<'a>(
        &'a self,
        key: &'a Key,
        into: &'a Path,
    ) -> impl Future<Output = anyhow::Result<Presence>> + Send;
}

/// A lock that prevents a temporary directory from being removed
#[derive(Clone)]
pub struct CachedPathLock(#[allow(dead_code)] Arc<RwLockReadGuardArc<()>>);

#[cfg(test)]
impl CachedPathLock {
    pub fn fake() -> Self {
        let lock = Arc::new(RwLock::new(()));
        CachedPathLock(Arc::new(RwLock::read_arc_blocking(&lock)))
    }
}

/// Wraps a [`CachableFetcher`] so that calling [`FetcherCache::get`] only calls
/// [`CachableFetcher::fetch`] once.
pub struct FetcherCache<Key: FetcherCacheKey, Fetcher: CachableFetcher<Key>> {
    root_dir: PathBuf,
    fetcher: Fetcher,
    phantom_key: PhantomData<Key>,
    locks: tokio::sync::Mutex<WeakValueHashMap<String, Weak<RwLock<()>>>>,
    expiration: Duration,
}

impl<Key: FetcherCacheKey + 'static, Fetcher: CachableFetcher<Key> + 'static>
    FetcherCache<Key, Fetcher>
{
    /// Create a directory inside `self.root`, succeeding if it already exists
    async fn ensure_dir_exists(&self, subdir: &str) -> anyhow::Result<()> {
        let path = self.root_dir.join(subdir);
        match tokio::fs::create_dir(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e).context(format!("creating {} for cache", path.display())),
        }
    }

    /// Create a [`FetcherCache`] that stores fetched directories under `root_dir`.
    ///
    /// `expiration` is the order of magnitude of how recently a file must have been requested by [`FetcherCache::get`] to not be deleted by [`FetcherCache::cleanup`].
    pub async fn new(
        root_dir: PathBuf,
        fetcher: Fetcher,
        expiration: Duration,
    ) -> anyhow::Result<Self> {
        let cache = Self {
            root_dir,
            fetcher,
            phantom_key: PhantomData,
            locks: Default::default(),
            expiration,
        };
        cache.ensure_dir_exists(PARTIAL).await?;
        cache.ensure_dir_exists(CACHE).await?;
        Ok(cache)
    }
    #[instrument(level = Level::TRACE, skip(self))]
    async fn entry_lock(&self, key: &str) -> Arc<RwLock<()>> {
        let mut lock_map = self.locks.lock().await;
        lock_map.remove_expired();
        let current = lock_map.get(key);
        let result = match current {
            Some(entry_lock) => entry_lock,
            None => {
                let entry_lock = Arc::new(RwLock::new(()));
                lock_map.insert(key.to_owned(), entry_lock.clone());
                entry_lock
            }
        };
        drop(lock_map);
        result
    }
    #[instrument(level = Level::TRACE, skip_all, fields(key=key.as_key()))]
    async fn read_lock<'a>(&'a self, key: Key) -> ReadLockedCacheEntry<Key> {
        let actual_key = key.as_key();
        let target = self.root_dir.join(CACHE).join(actual_key);
        let entry_lock = self.entry_lock(actual_key).await;
        let lock = entry_lock.read_arc().await;
        ReadLockedCacheEntry { key, target, lock }
    }
    #[instrument(level = Level::TRACE, skip_all, fields(key=lock.key.as_key()))]
    async fn unlock_and_relock_upgradably(
        &self,
        lock: ReadLockedCacheEntry<Key>,
    ) -> UpgradableReadLockedCacheEntry<Key> {
        let LockedCacheEntry { key, target, lock } = lock;
        drop(lock);
        let entry_lock = self.entry_lock(key.as_key()).await;
        let lock = entry_lock.upgradable_read_arc().await;
        UpgradableReadLockedCacheEntry { key, target, lock }
    }
    #[instrument(level = Level::TRACE, skip_all, fields(key=lock.key.as_key()))]
    async fn upgrade_upgradeable_read_lock(
        &self,
        lock: UpgradableReadLockedCacheEntry<Key>,
    ) -> WriteLockedCacheEntry<Key> {
        lock.map(RwLockUpgradableReadGuardArc::upgrade).await
    }
    #[instrument(level = Level::TRACE, skip_all, fields(key=lock.key.as_key()))]
    fn downgrade_write_lock(&self, lock: WriteLockedCacheEntry<Key>) -> ReadLockedCacheEntry<Key> {
        lock.map_sync(RwLockWriteGuardArc::downgrade)
    }
    #[instrument(level = Level::TRACE, skip_all, fields(key=lock.key.as_key()))]
    fn downgrade_upgradeable_read_lock(
        &self,
        lock: UpgradableReadLockedCacheEntry<Key>,
    ) -> ReadLockedCacheEntry<Key> {
        lock.map_sync(RwLockUpgradableReadGuardArc::downgrade)
    }

    #[instrument(level = Level::TRACE, skip(self))]
    async fn try_write_lock<'a, 'b>(&'a self, key: &'b str) -> Option<RwLockWriteGuardArc<()>> {
        let entry_lock = self.entry_lock(key).await;

        entry_lock.try_write_arc()
    }
    /// returns the corresponding directory if it is still in cache
    ///
    /// updates its mtime to remember that it was used, if it is older than some proportion of the cache expiry
    /// time.
    #[instrument(level = Level::TRACE, skip_all, fields(key=key.key.as_key()))]
    async fn cached<'cache, 'key, Lock>(
        &'cache self,
        key: &'key LockedCacheEntry<Key, Lock>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let expiration = self.expiration;
        match tokio::fs::symlink_metadata(&key.target).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context(format!("stat({})", key.target.display())),
            Ok(metadata) => {
                if metadata
                    .modified()
                    .context("no mtime on this platform")?
                    .elapsed()
                    .map(|x| x > expiration / 2)
                    .unwrap_or(true)
                {
                    touch(&key.target)
                        .await
                        .with_context(|| format!("touch({})", key.target.display()))?;
                }
                Ok(Some(key.target.clone()))
            }
        }
    }
    /// when the corresponding directory is not in cache, put it there
    #[instrument(level = Level::TRACE, skip_all, fields(key=key.key.as_key()))]
    async fn fetch<'key, 'cache: 'key>(
        &'cache self,
        key: &'key WriteLockedCacheEntry<Key>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let partial_dir = self.root_dir.join(PARTIAL).join(key.key.as_key());
        // we always clean after us, unless the future stops being polled
        remove_recursively_if_exists(&partial_dir).await?;
        let result = match self.fetcher.fetch(&key.key, &partial_dir).await {
            Ok(Presence::Found) => tokio::fs::rename(&partial_dir, &key.target)
                .await
                .with_context(|| {
                    format!(
                        "renaming {} to {}",
                        partial_dir.display(),
                        key.target.display()
                    )
                })
                .map(|()| Some(key.target.clone())),
            Ok(Presence::NotFound) => Ok(None),
            Err(e) => Err(e),
        };
        remove_recursively_if_exists(&partial_dir).await?;
        result
    }
    /// Returns the location where the file/directory for `key` is stored, fetching it if
    /// necessary.
    pub fn get(
        &self,
        key: Key,
    ) -> impl Future<Output = anyhow::Result<Option<RestrictedPath>>> + Send + use<'_, Key, Fetcher>
    {
        let span = tracing::trace_span!("get", key = key.as_key());
        let future = async move {
            let lock = self.read_lock(key).await;
            let (lock, result) = match self.cached(&lock).await? {
                Some(cached) => (lock, Some(cached)),
                None => {
                    let upgrade_lock = self.unlock_and_relock_upgradably(lock).await;
                    // somebody may have taken the lock and fetched the cache in between so we have
                    // to recheck
                    match self.cached(&upgrade_lock).await? {
                        Some(cached) => (
                            self.downgrade_upgradeable_read_lock(upgrade_lock),
                            Some(cached),
                        ),
                        None => {
                            let write_lock = self.upgrade_upgradeable_read_lock(upgrade_lock).await;
                            let result = self.fetch(&write_lock).await?;
                            (self.downgrade_write_lock(write_lock), result)
                        }
                    }
                }
            };
            match result {
                None => Ok(None),
                Some(path) => {
                    // FIXME: hack to enable returning a symlink to the store instead of copying to the
                    // cache
                    Ok(Some(
                        RestrictedPath::new(path, CachedPathLock(lock.lock.into())).await?,
                    ))
                }
            }
        };
        future.instrument(span)
    }
    /// Removes cache entry that have not been used for some time.
    #[instrument(level = Level::TRACE, skip_all)]
    async fn cleanup(&self) -> anyhow::Result<()> {
        let dir = self.root_dir.join(CACHE);
        let mut dirfd = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("listing {} for cleanup", dir.display()))?;
        loop {
            let entry = match dirfd.next_entry().await {
                Err(e) => {
                    tracing::warn!(
                        "cannot cleanup {}: failed to list entry: {e}",
                        dir.display()
                    );
                    continue;
                }
                Ok(None) => break,
                Ok(Some(entry)) => entry,
            };
            let entry_name = entry.file_name();
            let Some(entry_name) = entry_name.to_str() else {
                tracing::warn!(
                    "unexpected non utf8 file {} in {}",
                    entry_name.to_string_lossy(),
                    dir.display()
                );
                continue;
            };
            let entry_path = entry.path();
            tracing::trace!("attempting to cleanup {}", entry_path.display());
            let Some(write_lock) = self.try_write_lock(entry_name).await else {
                tracing::trace!(
                    "not cleaning up {} because somebody has a lock on it",
                    entry_path.display()
                );
                continue;
            };
            match entry.metadata().await {
                // did some concurrent cleanup remove it ?
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!("cannot cleanup {}: {}", entry_path.display(), e);
                    continue;
                }
                Ok(m) => {
                    let mtime = m.modified().context("mtime not supported on this os")?;
                    if mtime
                        .elapsed()
                        .map(|x| x > self.expiration * 2)
                        .unwrap_or(false)
                    {
                        tracing::debug!("removing expired cache entry {}", entry_path.display());
                        if let Err(e) = remove_recursively_if_exists(&entry_path).await {
                            tracing::warn!(
                                "failed to remove expired cache {}: {e}",
                                entry_path.display()
                            );
                        }
                    } else {
                        tracing::trace!(
                            "not cleaning up {} because it was used recently enough",
                            entry_path.display()
                        );
                    }
                }
            }
            // release write lock
            drop(write_lock);
        }
        Ok(())
    }

    /// Spawns a task that periodically removes unused cached paths
    pub fn spawn_cleanup_task(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(2 * self.expiration).await;
                if let Err(e) = self.cleanup().await {
                    tracing::warn!("failed to cleanup: {e}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use tempfile::tempdir;
    use tokio::io::AsyncReadExt;

    use crate::{test_utils::setup_logging, vfs::AsFile};

    use super::*;

    struct CountingFetcher(AtomicU32);
    impl CountingFetcher {
        fn new() -> Self {
            CountingFetcher(AtomicU32::new(0))
        }
        fn get(&self) -> u32 {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl FetcherCacheKey for String {
        fn as_key(&self) -> &str {
            self
        }
    }
    impl<T: AsRef<CountingFetcher> + Send + Sync> CachableFetcher<String> for T {
        fn fetch<'a>(
            &'a self,
            key: &'a String,
            into: &'a Path,
        ) -> impl Future<Output = anyhow::Result<Presence>> + Send {
            async move {
                let value = self
                    .as_ref()
                    .0
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                tracing::info!(
                    "Running counting fetcher on key {key}, count incremented to {value}"
                );
                tokio::fs::write(&into, format!("{}", value)).await?;
                Ok(Presence::Found)
            }
        }
    }

    struct SymlinkFetcher;
    impl CachableFetcher<String> for SymlinkFetcher {
        fn fetch<'a>(
            &'a self,
            _key: &'a String,
            into: &'a Path,
        ) -> impl Future<Output = anyhow::Result<Presence>> + Send {
            async move {
                tokio::fs::symlink(Path::new("/dev/null"), into)
                    .await
                    .unwrap();
                Ok(Presence::Found)
            }
        }
    }

    async fn read_restricted(r: &RestrictedPath) -> String {
        let mut file = r
            .clone()
            .resolve_inside_root()
            .await
            .unwrap()
            .unwrap()
            .open()
            .await
            .unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn does_not_fetch_twice() {
        let t = tempdir().unwrap();
        let fetcher = Arc::new(CountingFetcher::new());
        let cache = FetcherCache::new(t.path().into(), fetcher.clone(), Duration::from_secs(1000))
            .await
            .unwrap();
        let first = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 1);
        assert_eq!(read_restricted(&first).await, "1");
        let second = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 1);
        assert_eq!(read_restricted(&second).await, "1");
    }

    #[tokio::test]
    async fn cleanup_expired() {
        setup_logging();

        let t = tempdir().unwrap();
        let fetcher = Arc::new(CountingFetcher::new());
        let cache = FetcherCache::new(t.path().into(), fetcher.clone(), Duration::ZERO)
            .await
            .unwrap();
        tracing::info!("fetching key first");
        let first = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 1);
        assert_eq!(read_restricted(&first).await, "1");

        let n1 = count_elements_in_dir(t.path());
        drop(first);
        tracing::info!("cleaning up");
        cache.cleanup().await.unwrap();
        assert_eq!(count_elements_in_dir(t.path()), n1 - 1);

        tracing::info!("fetching key second");
        let second = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 2);
        assert_eq!(read_restricted(&second).await, "2");
    }

    fn count_elements_in_dir(dir: &Path) -> usize {
        #[allow(clippy::suspicious_map)]
        walkdir::WalkDir::new(dir)
            .into_iter()
            .map(|e| e.unwrap())
            .count()
    }

    #[tokio::test]
    async fn cleanup_expired_symlink() {
        setup_logging();

        let t = tempdir().unwrap();
        let cache = FetcherCache::new(t.path().into(), SymlinkFetcher, Duration::ZERO)
            .await
            .unwrap();
        let n1 = count_elements_in_dir(t.path());
        tracing::info!("fetching key first");
        let first = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(count_elements_in_dir(t.path()), n1 + 1);

        drop(first);

        tracing::info!("cleaning up");
        cache.cleanup().await.unwrap();

        assert_eq!(count_elements_in_dir(t.path()), n1);

        tracing::info!("fetching key second");
        let _second = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(count_elements_in_dir(t.path()), n1 + 1);
    }

    #[tokio::test]
    async fn cleanup_expired_but_held_lock() {
        setup_logging();

        let t = tempdir().unwrap();
        let fetcher = Arc::new(CountingFetcher::new());
        let cache = FetcherCache::new(t.path().into(), fetcher.clone(), Duration::ZERO)
            .await
            .unwrap();
        tracing::info!("fetching key first");
        let first = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 1);
        assert_eq!(read_restricted(&first).await, "1");

        tracing::info!("cleaning up");
        let n1 = count_elements_in_dir(t.path());
        cache.cleanup().await.unwrap();
        let n2 = count_elements_in_dir(t.path());
        assert_eq!(n2, n1);

        drop(first);

        tracing::info!("fetching key second");
        let second = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 1);
        assert_eq!(read_restricted(&second).await, "1");
    }

    #[tokio::test]
    async fn cleanup_not_expired() {
        setup_logging();

        let t = tempdir().unwrap();
        let fetcher = Arc::new(CountingFetcher::new());
        let cache = FetcherCache::new(t.path().into(), fetcher.clone(), Duration::from_secs(1000))
            .await
            .unwrap();
        tracing::info!("fetching key first");
        let first = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 1);
        assert_eq!(read_restricted(&first).await, "1");

        let n1 = count_elements_in_dir(t.path());
        drop(first);
        tracing::info!("cleaning up");
        cache.cleanup().await.unwrap();
        let n2 = count_elements_in_dir(t.path());
        assert_eq!(n2, n1);

        tracing::info!("fetching key second");
        let second = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 1);
        assert_eq!(read_restricted(&second).await, "1");
    }

    #[tokio::test]
    async fn cleanup_not_expired_symlink() {
        setup_logging();

        let t = tempdir().unwrap();
        let cache = FetcherCache::new(t.path().into(), SymlinkFetcher, Duration::from_secs(1000))
            .await
            .unwrap();
        tracing::info!("fetching key first");
        let first = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(read_restricted(&first).await, "");

        let n1 = count_elements_in_dir(t.path());
        drop(first);
        tracing::info!("cleaning up");
        cache.cleanup().await.unwrap();
        let n2 = count_elements_in_dir(t.path());
        assert_eq!(n2, n1);

        tracing::info!("fetching key second");
        let second = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(read_restricted(&second).await, "");
    }

    #[tokio::test]
    async fn locking() {
        setup_logging();

        let t = tempdir().unwrap();
        let fetcher = Arc::new(CountingFetcher::new());
        let cache = FetcherCache::new(t.path().into(), fetcher.clone(), Duration::from_secs(1000))
            .await
            .unwrap();
        let cache = Arc::new(cache);

        let fetch_and_use = |key: String| {
            let cache = cache.clone();
            async move {
                let description = format!("fetch_and_use({})", key);
                tracing::info!("starting {}", &description);
                let path = cache.get(key).await.unwrap().unwrap();
                tracing::info!("got a result for {}", &description);
                read_restricted(&path).await;
                tracing::info!("{} -> {path:?}", &description);
                description
            }
        };
        let cleanup = || {
            let cache = cache.clone();
            async move {
                cache.cleanup().await.unwrap();
                "cleanup".to_string()
            }
        };
        let mut futures = tokio::task::JoinSet::new();
        for i in 0..100 {
            futures.spawn(fetch_and_use(format!("key{}", i % 4)));
            futures.spawn(cleanup());
        }
        while let Some(description) = futures.join_next().await {
            // do nothing
            tracing::debug!("{} done", description.unwrap());
        }
    }

    #[tokio::test]
    async fn spawn_cleanup_task() {
        setup_logging();

        let t = tempdir().unwrap();
        let fetcher = Arc::new(CountingFetcher::new());
        let cache = Arc::new(
            FetcherCache::new(t.path().into(), fetcher.clone(), Duration::from_millis(1))
                .await
                .unwrap(),
        );

        cache.clone().spawn_cleanup_task();

        tracing::info!("fetching key first");
        let first = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 1);
        assert_eq!(read_restricted(&first).await, "1");

        let n1 = count_elements_in_dir(t.path());
        drop(first);
        tracing::info!("waiting for cleanup");
        // apparently it takes time for the task to actually spawn so let's have some margin
        tokio::time::sleep(Duration::from_millis(50)).await;
        let n2 = count_elements_in_dir(t.path());
        // first should have been removed
        assert_eq!(n2, n1 - 1);

        tracing::info!("fetching key second");
        let second = cache.get("key".into()).await.unwrap().unwrap();
        assert_eq!(fetcher.get(), 2);
        assert_eq!(read_restricted(&second).await, "2");
    }
}

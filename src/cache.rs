//! Support for caching functions that put stuff into a directory

// avoids higher ranked lifetime errors
#![allow(clippy::manual_async_fn)]
use std::{
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use anyhow::Context;
use async_lock::{RwLock, RwLockReadGuardArc, RwLockWriteGuardArc};
use weak_table::WeakValueHashMap;

use crate::utils::{remove_recursively_if_exists, Presence};

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

/// While this stucture is not dropped, the directory for this cache key may not be modified.
struct ReadLockedCacheEntry<Key: FetcherCacheKey> {
    pub key: Key,
    pub target: PathBuf,
    lock: RwLockReadGuardArc<()>,
}
/// While this structure is not dropped, only the owner may modify the directory corresponding to
/// this key.
struct WriteLockedCacheEntry<Key: FetcherCacheKey> {
    pub key: Key,
    pub target: PathBuf,
    lock: RwLockWriteGuardArc<()>,
}

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

/// Like [`Path`] but ensures that the path is not modified until dropped.
pub struct CachedPath<Key: FetcherCacheKey> {
    path: PathBuf,
    lock: ReadLockedCacheEntry<Key>,
}

impl<Key: FetcherCacheKey> AsRef<Path> for CachedPath<Key> {
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

impl<Key: FetcherCacheKey> CachedPath<Key> {
    /// Wrapper around [`Path::join`].
    pub fn join<T: AsRef<Path>>(self, rest: T) -> Self {
        Self {
            path: self.path.join(rest),
            lock: self.lock,
        }
    }

    fn new(path: PathBuf, lock: ReadLockedCacheEntry<Key>) -> Self {
        Self { path, lock }
    }
}

/// Wraps a [`CachableFetcher`] so that calling [`FetcherCache::get`] only calls
/// [`CachableFetcher::fetch`] once.
pub struct FetcherCache<Key: FetcherCacheKey, Fetcher: CachableFetcher<Key>> {
    root_dir: PathBuf,
    fetcher: Fetcher,
    phantom_key: PhantomData<Key>,
    locks: tokio::sync::Mutex<WeakValueHashMap<String, Weak<RwLock<()>>>>,
}

impl<Key: FetcherCacheKey, Fetcher: CachableFetcher<Key>> FetcherCache<Key, Fetcher> {
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
    pub async fn new(root_dir: PathBuf, fetcher: Fetcher) -> anyhow::Result<Self> {
        let cache = Self {
            root_dir,
            fetcher,
            phantom_key: PhantomData::default(),
            locks: Default::default(),
        };
        cache.ensure_dir_exists(PARTIAL).await?;
        cache.ensure_dir_exists(CACHE).await?;
        Ok(cache)
    }
    async fn entry_lock(&self, key: &Key) -> Arc<RwLock<()>> {
        let actual_key = key.as_key();
        let mut lock_map = self.locks.lock().await;
        lock_map.remove_expired();
        let current = lock_map.get(actual_key);
        match current {
            Some(entry_lock) => entry_lock,
            None => {
                let entry_lock = Arc::new(RwLock::new(()));
                lock_map.insert(actual_key.to_owned(), entry_lock.clone());
                entry_lock
            }
        }
    }
    fn read_lock<'a>(
        &'a self,
        key: Key,
    ) -> impl Future<Output = ReadLockedCacheEntry<Key>> + Send + use<'a, Key, Fetcher> {
        async move {
            let actual_key = key.as_key();
            let target = self.root_dir.join(CACHE).join(&actual_key);
            let entry_lock = self.entry_lock(&key).await;
            let lock = entry_lock.read_arc().await;
            ReadLockedCacheEntry { key, target, lock }
        }
    }
    fn upgrade<'a>(
        &'a self,
        read_lock: ReadLockedCacheEntry<Key>,
    ) -> impl Future<Output = WriteLockedCacheEntry<Key>> + Send + use<'a, Key, Fetcher> {
        async move {
            let entry_lock = self.entry_lock(&read_lock.key).await;
            let upgradeable_read_lock = entry_lock.upgradable_read_arc().await;
            let ReadLockedCacheEntry {
                target,
                key,
                lock: read_lock,
            } = read_lock;
            // at this point, even if we drop the read_lock, nobody can remove the directory,
            // because upgradeable_read_lock exists
            drop(read_lock);
            let write_lock =
                async_lock::RwLockUpgradableReadGuardArc::<()>::upgrade(upgradeable_read_lock)
                    .await;
            WriteLockedCacheEntry {
                key,
                target,
                lock: write_lock,
            }
        }
    }
    fn downgrade<'a>(
        &'a self,
        write_lock: WriteLockedCacheEntry<Key>,
    ) -> impl Future<Output = ReadLockedCacheEntry<Key>> + Send + use<'a, Fetcher, Key> {
        async move {
            let entry_lock = self.entry_lock(&write_lock.key).await;
            let WriteLockedCacheEntry {
                target,
                key,
                lock: write_lock,
            } = write_lock;
            let upgradeable_read_lock = RwLockWriteGuardArc::downgrade_to_upgradable(write_lock);
            // at this point taking a new normal read lock is not a dead lock
            let read_lock = entry_lock.read_arc().await;
            // now we can drop the upgradeable_read_lock while ensuring that the directory always
            // exists
            drop(upgradeable_read_lock);
            ReadLockedCacheEntry {
                key,
                target,
                lock: read_lock,
            }
        }
    }
    /// returns the corresponding directory if it is still in cache
    fn cached<'cache, 'key>(
        &'cache self,
        key: &'key ReadLockedCacheEntry<Key>,
    ) -> impl Future<Output = anyhow::Result<Option<PathBuf>>> + Send + 'key {
        async move {
            if tokio::fs::try_exists(&key.target)
                .await
                .with_context(|| format!("stat({})", key.target.display()))?
            {
                Ok(Some((&key.target).into()))
            } else {
                Ok(None)
            }
        }
    }
    /// when the corresponding directory is not in cache, put it there
    fn fetch<'key, 'cache: 'key>(
        &'cache self,
        key: &'key WriteLockedCacheEntry<Key>,
    ) -> impl Future<Output = anyhow::Result<Option<PathBuf>>> + Send + 'key {
        async move {
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
    }
    /// Returns the location where the file/directory for `key` is stored, fetching it if
    /// necessary.
    pub fn get(
        &self,
        key: Key,
    ) -> impl Future<Output = anyhow::Result<Option<CachedPath<Key>>>> + Send + use<'_, Key, Fetcher>
    {
        async move {
            let lock = self.read_lock(key).await;
            let (lock, result) = match self.cached(&lock).await? {
                Some(cached) => (lock, Some(cached)),
                None => {
                    let write_lock = self.upgrade(lock).await;
                    let result = self.fetch(&write_lock).await?;
                    (self.downgrade(write_lock).await, result)
                }
            };
            Ok(result.map(|path| CachedPath::new(path, lock)))
        }
    }
    #[allow(unused)]
    /// Removes cache entry that have not been used for some time.
    async fn cleanup(&self) {
        // FIXME: actually implement cleanup
        todo!();
    }
}

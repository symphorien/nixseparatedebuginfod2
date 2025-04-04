//! Support for caching functions that put stuff into a directory

// avoids higher ranked lifetime errors
#![allow(clippy::manual_async_fn)]
use std::{
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

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
struct ReadLockedCacheEntry<'cache, Key: FetcherCacheKey> {
    pub key: Key,
    pub target: PathBuf,
    #[allow(unused)]
    lock: RwLockReadGuard<'cache, ()>,
}
/// While this structure is not dropped, only the owner may modify the directory corresponding to
/// this key.
struct WriteLockedCacheEntry<'cache, Key: FetcherCacheKey> {
    pub key: Key,
    pub target: PathBuf,
    #[allow(unused)]
    lock: RwLockWriteGuard<'cache, ()>,
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
pub struct CachedPath<'cache, Key: FetcherCacheKey> {
    path: PathBuf,
    lock: ReadLockedCacheEntry<'cache, Key>,
}

impl<Key: FetcherCacheKey> AsRef<Path> for CachedPath<'_, Key> {
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

impl<'cache, Key: FetcherCacheKey> CachedPath<'cache, Key> {
    /// Wrapper around [`Path::join`].
    pub fn join<T: AsRef<Path>>(self, rest: T) -> Self {
        Self {
            path: self.path.join(rest),
            lock: self.lock,
        }
    }

    fn new(path: PathBuf, lock: ReadLockedCacheEntry<'cache, Key>) -> Self {
        Self { path, lock }
    }
}

/// Wraps a [`CachableFetcher`] so that calling [`FetcherCache::get`] only calls
/// [`CachableFetcher::fetch`] once.
pub struct FetcherCache<Key: FetcherCacheKey, Fetcher: CachableFetcher<Key>> {
    root_dir: PathBuf,
    fetcher: Fetcher,
    phantom_key: PhantomData<Key>,
    lock: RwLock<()>,
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
            lock: Default::default(),
        };
        cache.ensure_dir_exists(PARTIAL).await?;
        cache.ensure_dir_exists(CACHE).await?;
        Ok(cache)
    }
    fn read_lock<'cache>(
        &'cache self,
        key: Key,
    ) -> impl Future<Output = ReadLockedCacheEntry<'cache, Key>> + Send {
        async move {
            // FIXME: actually implement locking
            let actual_key = key.as_key().to_owned();
            let target = self.root_dir.join(CACHE).join(&actual_key);
            let lock = self.lock.read().await;

            ReadLockedCacheEntry { key, target, lock }
        }
    }
    fn upgrade<'cache>(
        &'cache self,
        read_lock: ReadLockedCacheEntry<'cache, Key>,
    ) -> impl Future<Output = WriteLockedCacheEntry<'cache, Key>> + Send {
        async move {
            // FIXME: Racy
            let ReadLockedCacheEntry { target, key, .. } = read_lock;
            let write_lock = self.lock.write().await;
            WriteLockedCacheEntry {
                key,
                target,
                lock: write_lock,
            }
        }
    }
    fn downgrade<'cache>(
        &'cache self,
        write_lock: WriteLockedCacheEntry<'cache, Key>,
    ) -> impl Future<Output = ReadLockedCacheEntry<'cache, Key>> + Send {
        async move {
            // FIXME: Racy
            let WriteLockedCacheEntry { target, key, .. } = write_lock;
            let lock = self.lock.read().await;
            ReadLockedCacheEntry { key, target, lock }
        }
    }
    fn cached<'cache, 'key>(
        &'cache self,
        key: &'key ReadLockedCacheEntry<'cache, Key>,
    ) -> impl Future<Output = anyhow::Result<Option<PathBuf>>> + Send + 'key {
        async move {
            if key.target.exists() {
                Ok(Some((&key.target).into()))
            } else {
                Ok(None)
            }
        }
    }
    fn fetch<'key, 'cache: 'key>(
        &'cache self,
        key: &'key WriteLockedCacheEntry<'cache, Key>,
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
    ) -> impl Future<Output = anyhow::Result<Option<CachedPath<'_, Key>>>> + Send {
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

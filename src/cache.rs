use std::{
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::utils::{remove_recursively_if_exists, Presence};

const PARTIAL: &str = "partial";
const CACHE: &str = "cache";

pub trait FetcherCacheKey: Debug + Send + Sync {
    fn as_key(&self) -> &str;
}

struct ReadLockedCacheEntry<'cache, Key: FetcherCacheKey> {
    pub key: Key,
    pub target: PathBuf,
    #[allow(unused)]
    lock: RwLockReadGuard<'cache, ()>,
}
struct WriteLockedCacheEntry<'cache, Key: FetcherCacheKey> {
    pub key: Key,
    pub target: PathBuf,
    #[allow(unused)]
    lock: RwLockWriteGuard<'cache, ()>,
}

pub trait CachableFetcher<Key: FetcherCacheKey>: Send + Sync {
    fn fetch<'a>(
        &'a self,
        key: &'a Key,
        into: &'a Path,
    ) -> impl Future<Output = anyhow::Result<Presence>> + Send;
}

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

pub struct FetcherCache<Key: FetcherCacheKey, Fetcher: CachableFetcher<Key>> {
    root_dir: PathBuf,
    fetcher: Fetcher,
    phantom_key: PhantomData<Key>,
    lock: RwLock<()>,
}

fn is_send<T: Send>(x: T) -> T {
    x
}
impl<Key: FetcherCacheKey, Fetcher: CachableFetcher<Key>> FetcherCache<Key, Fetcher> {
    async fn ensure_dir_exists(&self, subdir: &str) -> anyhow::Result<()> {
        let path = self.root_dir.join(subdir);
        match tokio::fs::create_dir(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e).context(format!("creating {} for cache", path.display())),
        }
    }

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
    pub fn name(&self) -> &str {
        todo!()
    }
    fn read_lock<'cache>(
        &'cache self,
        key: Key,
    ) -> impl Future<Output = ReadLockedCacheEntry<'cache, Key>> + Send {
        is_send(async move {
            // FIXME: actually implement locking
            let actual_key = key.as_key().to_owned();
            let target = self.root_dir.join(CACHE).join(&actual_key);
            let lock = self.lock.read().await;

            ReadLockedCacheEntry { key, target, lock }
        })
    }
    fn upgrade<'cache>(
        &'cache self,
        read_lock: ReadLockedCacheEntry<'cache, Key>,
    ) -> impl Future<Output = WriteLockedCacheEntry<'cache, Key>> + Send {
        is_send(async move {
            // FIXME: Racy
            let ReadLockedCacheEntry { target, key, .. } = read_lock;
            let write_lock = self.lock.write().await;
            WriteLockedCacheEntry {
                key,
                target,
                lock: write_lock,
            }
        })
    }
    fn downgrade<'cache>(
        &'cache self,
        write_lock: WriteLockedCacheEntry<'cache, Key>,
    ) -> impl Future<Output = ReadLockedCacheEntry<'cache, Key>> + Send {
        is_send(async move {
            // FIXME: Racy
            let WriteLockedCacheEntry { target, key, .. } = write_lock;
            let lock = self.lock.read().await;
            ReadLockedCacheEntry { key, target, lock }
        })
    }
    fn cached<'cache, 'key>(
        &'cache self,
        key: &'key ReadLockedCacheEntry<'cache, Key>,
    ) -> impl Future<Output = anyhow::Result<Option<PathBuf>>> + Send + 'key {
        is_send(async move {
            if key.target.exists() {
                Ok(Some((&key.target).into()))
            } else {
                Ok(None)
            }
        })
    }
    fn fetch<'key, 'cache: 'key>(
        &'cache self,
        key: &'key WriteLockedCacheEntry<'cache, Key>,
    ) -> impl Future<Output = anyhow::Result<Option<PathBuf>>> + Send + 'key {
        is_send(async move {
            let partial_dir = self.root_dir.join(PARTIAL).join(key.key.as_key());
            // we always clean after us, unless the future stops being polled
            remove_recursively_if_exists(&partial_dir).await?;
            let result = match self.fetcher.fetch(&key.key, &partial_dir).await {
                Ok(Presence::Found) => tokio::fs::rename(&partial_dir, &key.target)
                    .await
                    .with_context(|| {
                        format!("fetching key {:?} for cache {}", &key.key, self.name())
                    })
                    .map(|()| Some(key.target.clone())),
                Ok(Presence::NotFound) => Ok(None),
                Err(e) => Err(e),
            };
            remove_recursively_if_exists(&partial_dir).await?;
            result
        })
    }
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
    async fn cleanup(&self) {
        // FIXME: actually implement cleanup
        todo!();
    }
}

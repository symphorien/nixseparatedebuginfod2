use std::{
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    path::{Path, PathBuf},
};

use anyhow::Context;

use crate::utils::{Presence, remove_recursively_if_exists};

const PARTIAL: &str = "partial";
const CACHE: &str = "cache";

pub trait FetcherCacheKey: Debug {
    fn as_key(&self) -> &str;
}

struct ReadLockedCacheEntry<'cache, Key: FetcherCacheKey> {
    pub key: Key,
    pub target: PathBuf,
    cache: PhantomData<&'cache Path>,
}
struct WriteLockedCacheEntry<'cache, Key: FetcherCacheKey> {
    pub key: Key,
    pub target: PathBuf,
    cache: PhantomData<&'cache Path>,
}

pub trait CachableFetcher<Key: FetcherCacheKey>: Sync {
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

impl<'a, Key: FetcherCacheKey> AsRef<Path> for CachedPath<'a, Key> {
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

impl<'cache, Key: FetcherCacheKey> CachedPath<'cache, Key> {
    pub fn join(self, rest: &str) -> Self {
        Self {
            path: self.path.join(rest),
            lock: self.lock,
        }
    }

    pub fn new(path: PathBuf, lock: ReadLockedCacheEntry<'cache, Key>) -> Self {
        Self { path, lock }
    }
}

pub struct FetcherCache<Key: FetcherCacheKey, Fetcher: CachableFetcher<Key>> {
    root_dir: PathBuf,
    fetcher: Fetcher,
    phantom_key: PhantomData<Key>,
}

impl<Key: FetcherCacheKey, Fetcher: CachableFetcher<Key>> FetcherCache<Key, Fetcher> {
    pub fn new(root_dir: PathBuf, fetcher: Fetcher) -> Self {
        Self {
            root_dir,
            fetcher,
            phantom_key: PhantomData::default(),
        }
    }
    pub fn name(&self) -> &str {
        todo!()
    }
    fn read_lock<'cache>(&'cache self, key: Key) -> ReadLockedCacheEntry<'cache, Key> {
        // FIXME: actually implement locking
        let actual_key = key.as_key().to_owned();
        let target = self.root_dir.join(CACHE).join(&actual_key);
        ReadLockedCacheEntry {
            key,
            target,
            cache: Default::default(),
        }
    }
    fn upgrade<'cache>(
        &'cache self,
        read_lock: ReadLockedCacheEntry<'cache, Key>,
    ) -> WriteLockedCacheEntry<'cache, Key> {
        WriteLockedCacheEntry {
            key: read_lock.key,
            target: read_lock.target,
            cache: Default::default(),
        }
    }
    fn downgrade<'cache, 'key: 'cache>(
        &'cache self,
        write_lock: WriteLockedCacheEntry<'key, Key>,
    ) -> ReadLockedCacheEntry<'key, Key> {
        ReadLockedCacheEntry {
            key: write_lock.key,
            target: write_lock.target,
            cache: Default::default(),
        }
    }
    async fn cached<'cache, 'key>(
        &'cache self,
        key: &'key ReadLockedCacheEntry<'cache, Key>,
    ) -> anyhow::Result<Option<PathBuf>> {
        if key.target.exists() {
            Ok(Some((&key.target).into()))
        } else {
            Ok(None)
        }
    }
    async fn fetch<'cache, 'key>(
        &'cache self,
        key: &'key WriteLockedCacheEntry<'cache, Key>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let partial_dir = self.root_dir.join(PARTIAL).join(key.key.as_key());
        // we always clean after us, unless the future stops being polled
        remove_recursively_if_exists(&partial_dir).await?;
        let result = match self.fetcher.fetch(&key.key, &partial_dir).await {
            Ok(Presence::Found) => tokio::fs::rename(&partial_dir, &key.target)
                .await
                .with_context(|| format!("fetching key {:?} for cache {}", &key.key, self.name()))
                .map(|()| Some(key.target.clone())),
            Ok(Presence::NotFound) => Ok(None),
            Err(e) => Err(e),
        };
        remove_recursively_if_exists(&partial_dir).await?;
        result
    }
    pub async fn get(&self, key: Key) -> anyhow::Result<Option<CachedPath<'_, Key>>> {
        let lock = self.read_lock(key);
        let (lock, result) = match self.cached(&lock).await? {
            Some(cached) => (lock, Some(cached)),
            None => {
                let write_lock = self.upgrade(lock);
                let result = self.fetch(&write_lock).await?;
                (self.downgrade(write_lock), result)
            }
        };
        Ok(result.map(|path| CachedPath::new(path, lock)))
    }
    async fn cleanup(&self) {
        // FIXME: actually implement cleanup
        todo!();
    }
}

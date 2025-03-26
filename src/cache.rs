use std::{
    future::Future,
    marker::PhantomData,
    path::{Path, PathBuf},
    pin::Pin,
};

use anyhow::Context;

use crate::utils::{remove_recursively_if_exists, Presence};

const PARTIAL: &str = "partial";
const CACHE: &str = "cache";

struct ReadLockedCacheEntry<'cache> {
    pub key: String,
    pub target: PathBuf,
    cache: PhantomData<&'cache FetcherCache>,
}
struct WriteLockedCacheEntry<'cache> {
    pub key: String,
    pub target: PathBuf,
    cache: PhantomData<&'cache FetcherCache>,
}

type FetchFuture = Pin<Box<dyn Future<Output = anyhow::Result<Presence>> + Send + Sync>>;
type Fetcher = Box<dyn for<'a> Fn(&'a str, &'a Path) -> FetchFuture + Send + Sync>;

pub struct FetcherCache {
    root_dir: PathBuf,
    fetcher: Fetcher,
}

pub struct CachedPath<'cache> {
    path: PathBuf,
    lock: ReadLockedCacheEntry<'cache>,
}

impl<'a> AsRef<Path> for CachedPath<'a> {
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

impl<'cache> CachedPath<'cache> {
    pub fn join(self, rest: &str) -> Self {
        Self {
            path: self.path.join(rest),
            lock: self.lock,
        }
    }
}

impl FetcherCache {
    pub fn name(&self) -> &str {
        todo!()
    }
    fn read_lock<'cache, 'key>(&'cache self, key: &'key str) -> ReadLockedCacheEntry<'cache> {
        // FIXME: actually implement locking
        let target = self.root_dir.join(CACHE).join(key);
        ReadLockedCacheEntry {
            key: key.to_owned(),
            target,
            cache: Default::default(),
        }
    }
    fn upgrade<'cache>(
        &'cache self,
        read_lock: &ReadLockedCacheEntry<'cache>,
    ) -> WriteLockedCacheEntry<'cache> {
        WriteLockedCacheEntry {
            key: read_lock.key.clone(),
            target: read_lock.target.clone(),
            cache: Default::default(),
        }
    }
    fn downgrade<'cache, 'key: 'cache>(
        &'cache self,
        write_lock: WriteLockedCacheEntry<'key>,
    ) -> ReadLockedCacheEntry<'key> {
        ReadLockedCacheEntry {
            key: write_lock.key.clone(),
            target: write_lock.target.clone(),
            cache: Default::default(),
        }
    }
    async fn cached<'cache, 'key>(
        &'cache self,
        key: &'key ReadLockedCacheEntry<'cache>,
    ) -> anyhow::Result<Option<PathBuf>> {
        if key.target.exists() {
            Ok(Some((&key.target).into()))
        } else {
            Ok(None)
        }
    }
    async fn fetch<'cache, 'key>(
        &'cache self,
        key: &'key WriteLockedCacheEntry<'cache>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let partial_dir = self.root_dir.join(PARTIAL).join(&key.key);
        // we always cleam after us, unless the future stops being polled
        remove_recursively_if_exists(&partial_dir).await?;
        let f = &self.fetcher;
        let result = match f(&key.key, &partial_dir).await {
            Ok(Presence::Found) => tokio::fs::rename(&partial_dir, &key.target)
                .await
                .with_context(|| format!("fetching key {} for cache {}", &key.key, self.name()))
                .map(|()| Some(key.target.clone())),
            Ok(Presence::NotFound) => Ok(None),
            Err(e) => Err(e),
        };
        remove_recursively_if_exists(&partial_dir).await?;
        result
    }
    pub async fn get<'key, 'cache>(
        &'cache self,
        key: &'key str,
    ) -> anyhow::Result<Option<CachedPath<'cache>>> {
        let lock = self.read_lock(key);
        let (lock, result) = match self.cached(&lock).await? {
            Some(cached) => (lock, Some(cached)),
            None => {
                let write_lock = self.upgrade(&lock);
                let result = self.fetch(&write_lock).await?;
                (self.downgrade(write_lock), result)
            }
        };
        Ok(result.map(|path| CachedPath { path, lock }))
    }
    async fn cleanup(&self) {
        // FIXME: actually implement cleanup
        todo!();
    }
}

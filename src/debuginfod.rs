use std::{
    path::{Path, PathBuf},
    sync::Arc,
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

pub struct Debuginfod {
    debuginfo_fetcher: FetcherCache<BuildId, Arc<BoxedSubstituter>>,
    store_fetcher: FetcherCache<StorePath, Arc<BoxedSubstituter>>,
    // source_unpacker: FetcherCache,
}

async fn return_if_exists<'a, Key: FetcherCacheKey>(
    path: CachedPath<'a, Key>,
) -> anyhow::Result<Option<CachedPath<'a, Key>>> {
    match path.as_ref().metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context(format!(
            "testing existence of file {} before returning it from debuginfod",
            path.as_ref().display()
        )),
        Ok(_) => Ok(Some(path)),
    }
}

impl Debuginfod {
    pub fn new(cache_path: PathBuf, substituter: BoxedSubstituter) -> Self {
        let debuginfo_path = cache_path.join("debuginfo");
        let store_path = cache_path.join("store");
        let substituter = Arc::new(substituter);
        Self {
            debuginfo_fetcher: FetcherCache::new(debuginfo_path, substituter.clone()),
            store_fetcher: FetcherCache::new(store_path, substituter.clone()),
        }
    }
    pub async fn debuginfo<'key, 'debuginfod: 'key>(
        &'debuginfod self,
        build_id: &'key BuildId,
    ) -> anyhow::Result<Option<CachedPath<'debuginfod, BuildId>>> {
        match self.debuginfo_fetcher.get(build_id.clone()).await {
            Ok(Some(nar)) => {
                let debugfile = nar.join(&build_id.in_debug_output("debug"));
                return_if_exists(debugfile).await
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }
    pub async fn executable<'key, 'debuginfod: 'key>(
        &'debuginfod self,
        build_id: &'key BuildId,
    ) -> anyhow::Result<Option<CachedPath<'key, StorePath>>> {
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
                        match self.store_fetcher.get(exe_in_store_path).await {
                            Ok(Some(exe)) => {
                                // FIXME: what if it still a symlink to another store path
                                return_if_exists(exe).await
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
    pub async fn source(
        &self,
        _build_id: &BuildId,
        _path: &str,
    ) -> anyhow::Result<Option<CachedPath<'_, StorePath>>> {
        // when gdb attempts to show the source of a function that comes
        // from a header in another library, the request is store path made
        // relative to /
        // in this case, let's fetch it
        // if request.starts_with("nix/store") {
        //     let absolute = PathBuf::from("/").join(request);
        //     let demangled = demangle(absolute);
        //     let error: anyhow::Result<()> = todo!()
        //         .await
        //         .with_context(|| format!("downloading source {}", demangled.display()));
        //     return unwrap_file(error.map(|()| Some(demangled)))
        //         .await
        //         .into_response();
        // }
        // as a fallback, have a look at the source of the buildid
        todo!()
    }
}

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncReadExt;

use crate::cache::CachableFetcher;
use crate::cache::FetcherCache;
use crate::cache::FetcherCacheKey;
use crate::nar::unpack_nar;
use crate::store_path::StorePath;
use crate::utils::percent_encode_to_filename;
use crate::utils::DecompressingReader;
use crate::vfs::RestrictedPath;
use crate::{
    build_id::BuildId,
    nar::narinfo_to_nar_location,
    substituter::{Priority, Substituter},
    utils::Presence,
};
/// Structure of the metadata files created by the `index-debug-info` option of binary caches
#[derive(Deserialize)]
pub struct DebugInfoRedirectJson {
    /// relative path to the nar.xz
    pub archive: String,
    /// relative path to the file inside of the nar
    pub member: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// relative path of a NAR inside the substituter
pub struct NarRelativeLocation {
    // the relative path
    location: String,
    // same as location, but mangled to be fit as a filename
    key: String,
}

impl NarRelativeLocation {
    /// constructs a `NarRelativeLocation` from a string representing a relative path inside a
    /// binary cache
    ///
    /// `location` may not start nor end with `/`
    ///
    /// `..` will be resolved logically, which is not 100% correct with symlink semantics, but
    /// `.`, duplicate `/` and trailing `/` will be stripped away.
    /// improves caching.
    pub fn new(location: &str) -> anyhow::Result<Self> {
        let mut resolved_location = PathBuf::new();
        for component in Path::new(location).components() {
            match component {
                std::path::Component::Prefix(_prefix_component) => {
                    anyhow::bail!("unexpected prefix in NarRelativeLocation::new({location:?})")
                }
                std::path::Component::RootDir => {
                    anyhow::bail!(
                        "cannot create NarRelativeLocation from absolute path {location:?}"
                    )
                }
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if !resolved_location.pop() {
                        anyhow::bail!("NarRelativeLocation::new({location:?}): path escapes root");
                    }
                }
                std::path::Component::Normal(os_str) => {
                    if !os_str.is_empty() {
                        resolved_location.push(os_str)
                    }
                }
            }
        }
        let resolved_location = resolved_location
            .to_str()
            .with_context(|| format!("invalid utf8 NarRelativeLocation: {location:?}"))?;
        let key = percent_encode_to_filename(resolved_location);
        Ok(Self {
            location: resolved_location.to_owned(),
            key,
        })
    }
    /// relative path of the NAR
    pub fn location(&self) -> &str {
        &self.location
    }
}

#[test]
fn nar_relative_location_nomimal() {
    let n = NarRelativeLocation::new("a/b").unwrap();
    assert_eq!(n.location(), "a/b");
    assert_eq!(n.as_key(), "a%2Fb");
}

#[test]
fn nar_relative_location_percent() {
    let n = NarRelativeLocation::new("a/b%2F").unwrap();
    assert_eq!(n.location(), "a/b%2F");
    assert_eq!(n.as_key(), "a%2Fb%252F");
}

#[test]
fn nar_relative_location_absolute() {
    NarRelativeLocation::new("/a").unwrap_err();
}

#[test]
fn nar_relative_location_escape() {
    NarRelativeLocation::new("../a").unwrap_err();
}

#[test]
fn nar_relative_location_normalize() {
    let n = NarRelativeLocation::new("a/b/.././/c/").unwrap();
    assert_eq!(n.location(), "a/c");
    assert_eq!(n.as_key(), "a%2Fc");
}

/// A substituter with the well-known-structure of a binary cache
///
/// A binary cache has the following structure:
/// - a derivation output `/nix/store/hash1-name has a corresponding file `hash1.narinfo` at the
///   root of the binary cache
/// - the narinfo points to the path of the corresponding nar file
/// - for each build id contained in the nars, a json file `debug/{buildid}.debug` (or
///   `debug/{buildid}` points to the corresponding nar (it corresponds to [DebugInfoRedirectJson]).
pub trait BinaryCache: std::fmt::Debug + Send + Sync {
    /// Returns a reader for this file as contained by the [BinaryCache], or [Presence::NotFound] if
    /// the [BinaryCache] positively does not contains the requested file.
    ///
    /// `what` is the relative path of the requested file.
    /// This function is responsible for checking that `what` does not escape the root of the binary
    /// cache.
    ///
    /// In the case of `.nar.xz` files, the stream should be the compressed xz data. This function
    /// should not uncompress it.
    fn stream_location(
        &self,
        what: &NarRelativeLocation,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<impl AsyncBufRead + Send>>> + Send;

    /// Same as [Substituter::priority]
    fn priority(&self) -> Priority;
}

const SMALL_FILE_SIZE: u64 = 1024 * 1024 - 1;
/// Returns the content of this stream if it is smaller than [SMALL_FILE_SIZE]
async fn read_small_stream(s: impl AsyncBufRead) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let original = std::pin::pin!(s);
    let mut limited = original.take(SMALL_FILE_SIZE + 1);
    limited.read_to_end(&mut buf).await?;
    anyhow::ensure!(
        buf.len() <= SMALL_FILE_SIZE as usize,
        "stream is too large, refusing to parse"
    );
    Ok(buf)
}

#[tokio::test]
async fn read_small_stream_small() {
    let content = vec![b'A'; SMALL_FILE_SIZE as usize];
    let reader = tokio::io::BufReader::new(&content[..]);
    assert_eq!(read_small_stream(reader).await.unwrap(), content);
}

#[tokio::test]
async fn read_small_stream_big() {
    let content = vec![b'A'; SMALL_FILE_SIZE as usize + 1];
    let reader = tokio::io::BufReader::new(&content[..]);
    read_small_stream(reader).await.unwrap_err();
}

#[tokio::test]
async fn read_small_stream_infinite() {
    let reader = tokio::io::BufReader::new(tokio::io::repeat(b'A'));
    read_small_stream(reader).await.unwrap_err();
}

impl FetcherCacheKey for NarRelativeLocation {
    fn as_key(&self) -> &str {
        &self.key
    }
}

impl<T: BinaryCache> CachableFetcher<NarRelativeLocation> for T {
    /// Fetch a nar by nar location
    ///
    /// `into` must not exist yet, but its parent must be an existing directory.
    ///
    /// In case of error, `into` may contain garbage
    async fn fetch<'a>(
        &'a self,
        key: &'a NarRelativeLocation,
        into: &'a Path,
    ) -> anyhow::Result<Presence> {
        let Some(nar_stream) = self.stream_location(key).await? else {
            tracing::debug!("{} is missing from {:?}", key.location(), &self);
            return Ok(Presence::NotFound);
        };
        let decompressing_nar_reader =
            DecompressingReader::new(nar_stream, key.location().as_bytes())?;
        unpack_nar(decompressing_nar_reader, into).await?;
        Ok(Presence::Found)
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
struct SmallNarRelativeLocation {
    location: String,
}

impl From<NarRelativeLocation> for SmallNarRelativeLocation {
    fn from(value: NarRelativeLocation) -> Self {
        SmallNarRelativeLocation {
            location: value.location,
        }
    }
}

impl From<SmallNarRelativeLocation> for NarRelativeLocation {
    fn from(value: SmallNarRelativeLocation) -> Self {
        let key = percent_encode_to_filename(&value.location);
        NarRelativeLocation {
            location: value.location,
            key,
        }
    }
}

#[test]
fn small_nar_relative_location_roundtrip() {
    let a = NarRelativeLocation::new("a/b%").unwrap();
    let b: SmallNarRelativeLocation = a.clone().into();
    let c: NarRelativeLocation = b.clone().into();
    let d: SmallNarRelativeLocation = c.clone().into();
    assert_eq!(a, c);
    assert_eq!(b, d);
    assert_eq!(a.location(), &b.location);
}

type MemoryCache<K> = quick_cache::sync::Cache<K, SmallNarRelativeLocation>;
const MEMORY_CACHE_SIZE: usize = 1000;
/// A substituter implemented on top of a BinaryCache, with caching so that requesting twice the same
/// store path will not download it twice
pub struct CachedBinaryCache<T: BinaryCache> {
    nar_cache: Arc<FetcherCache<NarRelativeLocation, T>>,
    debuginfo_lookup_cache: MemoryCache<BuildId>,
    store_path_lookup_cache: MemoryCache<StorePath>,
}

impl<T: BinaryCache + 'static> CachedBinaryCache<T> {
    /// turn an uncached BinaryCache into a cached substituter
    ///
    /// cache_dir is where downloaded nars are kept for approximately `expiration`
    pub async fn wrap(inner: T, cache_dir: PathBuf, expiration: Duration) -> anyhow::Result<Self> {
        let nar_cache = Arc::new(FetcherCache::new(cache_dir, inner, expiration).await?);
        let debuginfo_lookup_cache = MemoryCache::new(MEMORY_CACHE_SIZE);
        let store_path_lookup_cache = MemoryCache::new(MEMORY_CACHE_SIZE);
        Ok(Self {
            nar_cache,
            debuginfo_lookup_cache,
            store_path_lookup_cache,
        })
    }

    fn inner(&self) -> &T {
        &self.nar_cache.fetcher
    }
}

impl<T: BinaryCache + 'static> std::fmt::Debug for CachedBinaryCache<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CachedSubstituter")
            .field(&self.inner())
            .finish()
    }
}

#[async_trait::async_trait]
impl<T: BinaryCache + 'static> Substituter for CachedBinaryCache<T> {
    #[tracing::instrument(level=tracing::Level::DEBUG)]
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
    ) -> anyhow::Result<Option<RestrictedPath>> {
        let nar_location = match self
            .debuginfo_lookup_cache
            .get_value_or_guard_async(build_id)
            .await
        {
            Ok(small_location) => small_location.into(),
            Err(placeholder) => {
                let location1 = NarRelativeLocation::new(&format!("debuginfo/{}", build_id))?;
                let location2 = NarRelativeLocation::new(&format!("debuginfo/{}.debug", build_id))?;
                let maybe_json_stream = match self.inner().stream_location(&location1).await {
                    Ok(Some(x)) => Some(x),
                    Err(_) | Ok(None) => self.inner().stream_location(&location2).await?,
                };
                let Some(json_stream) = maybe_json_stream else {
                    tracing::debug!("{location1:?} and {location2:?} are missing from {self:?}");
                    return Ok(None);
                };
                let json_bytes = read_small_stream(json_stream)
                    .await
                    .context("looking for json redirect to debuginfo")?;
                let redirect: DebugInfoRedirectJson = serde_json::from_slice(&json_bytes)
                    .with_context(|| {
                        format!("unexpected format for {location1:?} or {location2:?} in {self:?}")
                    })?;
                let nar_path =
                    NarRelativeLocation::new(&format!("debuginfo/{}", &redirect.archive))?;
                if let Err(e) = placeholder.insert(nar_path.clone().into()) {
                    tracing::trace!(err=?e, nar_path=nar_path.location(), "weird, cannot insert into cache");
                };
                nar_path
            }
        };
        self.nar_cache.get(nar_location).await
    }

    #[tracing::instrument(level=tracing::Level::DEBUG)]
    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
    ) -> anyhow::Result<Option<RestrictedPath>> {
        let nar_location = match self
            .store_path_lookup_cache
            .get_value_or_guard_async(&store_path.root())
            .await
        {
            Ok(small_location) => small_location.into(),
            Err(placeholder) => {
                let narinfo_path =
                    NarRelativeLocation::new(&format!("{}.narinfo", store_path.hash()))?;
                let Some(narinfo_stream) = self.inner().stream_location(&narinfo_path).await?
                else {
                    tracing::debug!("{narinfo_path:?} is missing from {self:?}");
                    return Ok(None);
                };
                let nar_path = narinfo_to_nar_location(narinfo_stream)
                    .await
                    .with_context(|| format!("parsing {narinfo_path:?}"))?;
                let nar_path = NarRelativeLocation::new(&nar_path)?;
                if let Err(e) = placeholder.insert(nar_path.clone().into()) {
                    tracing::trace!(err=?e, nar_path=nar_path.location(), "weird, cannot insert into cache");
                };
                nar_path
            }
        };
        self.nar_cache.get(nar_location).await
    }

    fn priority(&self) -> Priority {
        BinaryCache::priority(self.inner())
    }

    fn spawn_cleanup_task(&self) {
        self.nar_cache.clone().spawn_cleanup_task()
    }

    async fn shrink_disk_cache(&self) -> anyhow::Result<()> {
        self.nar_cache.shrink_cache().await
    }
}

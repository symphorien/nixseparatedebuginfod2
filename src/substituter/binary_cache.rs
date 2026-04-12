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

#[derive(Debug, Clone)]
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
    pub fn new(location: &str) -> Self {
        let key = percent_encode_to_filename(location);
        Self {
            location: location.to_owned(),
            key,
        }
    }
    /// relative path of the NAR
    pub fn location(&self) -> &str {
        &self.location
    }
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

/// A substituter implemented on top of a BinaryCache, with caching so that requesting twice the same
/// store path will not download it twice
pub struct CachedBinaryCache<T: BinaryCache> {
    cache: Arc<FetcherCache<NarRelativeLocation, T>>,
}

impl<T: BinaryCache + 'static> CachedBinaryCache<T> {
    /// turn an uncached BinaryCache into a cached substituter
    ///
    /// cache_dir is where downloaded nars are kept for approximately `expiration`
    pub async fn wrap(inner: T, cache_dir: PathBuf, expiration: Duration) -> anyhow::Result<Self> {
        let cache = FetcherCache::new(cache_dir, inner, expiration).await?;
        Ok(Self {
            cache: Arc::new(cache),
        })
    }

    fn inner(&self) -> &T {
        &self.cache.fetcher
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
        let location1 = NarRelativeLocation::new(&format!("debuginfo/{}", build_id));
        let location2 = NarRelativeLocation::new(&format!("debuginfo/{}.debug", build_id));
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
        let redirect: DebugInfoRedirectJson =
            serde_json::from_slice(&json_bytes).with_context(|| {
                format!("unexpected format for {location1:?} or {location2:?} in {self:?}")
            })?;
        let nar_path = NarRelativeLocation::new(&format!("debuginfo/{}", &redirect.archive));
        self.cache.get(nar_path).await
    }

    #[tracing::instrument(level=tracing::Level::DEBUG)]
    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
    ) -> anyhow::Result<Option<RestrictedPath>> {
        let narinfo_path = NarRelativeLocation::new(&format!("{}.narinfo", store_path.hash()));
        let Some(narinfo_stream) = self.inner().stream_location(&narinfo_path).await? else {
            tracing::debug!("{narinfo_path:?} is missing from {self:?}");
            return Ok(None);
        };
        let nar_path = narinfo_to_nar_location(narinfo_stream)
            .await
            .with_context(|| format!("parsing {narinfo_path:?}"))?;
        self.cache.get(NarRelativeLocation::new(&nar_path)).await
    }

    fn priority(&self) -> Priority {
        BinaryCache::priority(self.inner())
    }

    fn spawn_cleanup_task(&self) {
        self.cache.clone().spawn_cleanup_task()
    }
}

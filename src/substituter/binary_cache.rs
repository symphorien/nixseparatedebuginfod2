use anyhow::Context;
use serde::Deserialize;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncReadExt;

use crate::nar::unpack_nar;
use crate::store_path::StorePath;
use crate::utils::DecompressingReader;
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

/// A substituter with the well-known-structure of a binary cache
///
/// A binary cache has the following structure:
/// - a derivation output `/nix/store/hash1-name has a corresponding file `hash1.narinfo` at the
/// root of the binary cache
/// - the narinfo points to the path of the corresponding nar file
/// - for each build id contained in the nars, a json file `debug/{buildid}.debug` (or
/// `debug/{buildid}` points to the corresponding nar (it corresponds to [DebugInfoRedirectJson]).
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
        what: &str,
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

async fn return_nar<T: BinaryCache>(
    cache: &T,
    nar_path: &str,
    into: &std::path::Path,
) -> anyhow::Result<Presence> {
    let Some(nar_stream) = cache.stream_location(nar_path).await? else {
        tracing::debug!("{nar_path} is missing from {cache:?}");
        return Ok(Presence::NotFound);
    };
    let decompressing_nar_reader = DecompressingReader::new(nar_stream, nar_path.as_bytes())?;
    unpack_nar(decompressing_nar_reader, into).await?;

    Ok(Presence::Found)
}

#[async_trait::async_trait]
impl<T: BinaryCache> Substituter for T {
    #[tracing::instrument(level=tracing::Level::DEBUG)]
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &std::path::Path,
    ) -> anyhow::Result<Presence> {
        let location1 = format!("debuginfo/{}", build_id);
        let location2 = format!("debuginfo/{}.debug", build_id);
        let maybe_json_stream = match self.stream_location(&location1).await {
            Ok(Some(x)) => Some(x),
            Err(_) | Ok(None) => self.stream_location(&location2).await?,
        };
        let Some(json_stream) = maybe_json_stream else {
            tracing::debug!("{location1} and {location2} are missing from {self:?}");
            return Ok(Presence::NotFound);
        };
        let json_bytes = read_small_stream(json_stream)
            .await
            .context("looking for json redirect to debuginfo")?;
        let redirect: DebugInfoRedirectJson =
            serde_json::from_slice(&json_bytes).with_context(|| {
                format!("unexpected format for {location1} or {location2} in {self:?}")
            })?;
        let nar_path = format!("debuginfo/{}", &redirect.archive);
        return_nar(self, &nar_path, into).await
    }

    #[tracing::instrument(level=tracing::Level::DEBUG)]
    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &std::path::Path,
    ) -> anyhow::Result<Presence> {
        let narinfo_path = format!("{}.narinfo", store_path.hash());
        let Some(narinfo_stream) = self.stream_location(&narinfo_path).await? else {
            tracing::debug!("{narinfo_path} is missing from {self:?}");
            return Ok(Presence::NotFound);
        };
        let nar_path = narinfo_to_nar_location(narinfo_stream)
            .await
            .with_context(|| format!("parsing {narinfo_path}"))?;
        return_nar(self, &nar_path, into).await
    }

    fn priority(&self) -> Priority {
        BinaryCache::priority(self)
    }
}

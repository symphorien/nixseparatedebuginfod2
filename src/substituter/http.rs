use std::{fmt::Debug, path::PathBuf, time::Duration};

use anyhow::Context;
use futures::StreamExt;
use http::StatusCode;
use reqwest::{Client, Url};
use tokio::io::AsyncBufRead;
use tokio_util::io::StreamReader;

use crate::substituter::binary_cache::{BinaryCache, CachedBinaryCache, NarRelativeLocation};

use super::Priority;

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Fetching from `http://` and `https://` substituters.
///
/// The substituter must have been created with `?index-debug-info=true`.
pub struct HttpSubstituterInner {
    url: Url,
    client: Client,
}

impl Debug for HttpSubstituterInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSubstituter")
            .field("url", &self.url.as_str())
            .finish()
    }
}

impl HttpSubstituterInner {
    /// Create an http or https substituter with this base url.
    pub fn new(url: Url) -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .with_context(|| format!("creating an http client to connect to {url}"))?;
        Ok(Self { url, client })
    }
    fn make_url(&self, rest: &NarRelativeLocation) -> anyhow::Result<Url> {
        self.url
            .join(rest.location())
            .with_context(|| format!("{}{} is malformed url", &self.url, &rest.location()))
    }
}

impl BinaryCache for HttpSubstituterInner {
    /// sends a get query to this url, and returns the response only if 200
    ///
    /// returns None on 404, an error in other cases.
    async fn stream_location(
        &self,
        what: &NarRelativeLocation,
    ) -> anyhow::Result<Option<impl AsyncBufRead + Send>> {
        let url = self.make_url(what)?;
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("connecting to {url}"))?;
        match response.status() {
            StatusCode::OK => (),
            StatusCode::NOT_FOUND => {
                tracing::trace!("404");
                return Ok(None);
            }
            other => anyhow::bail!("{url} returned {other:?}"),
        };
        let stream = response.bytes_stream();
        let reader = StreamReader::new(stream.map(|r| r.map_err(std::io::Error::other)));

        Ok(Some(reader))
    }

    fn priority(&self) -> Priority {
        Priority::Unknown
    }
}

/// A substituter fetching from `http://` or `https://` binary caches
pub type HttpSubstituter = CachedBinaryCache<HttpSubstituterInner>;

impl CachedBinaryCache<HttpSubstituterInner> {
    /// Constructs a `HttpSubstituter` which downloads from `url` to a cache directory `cache_dir`
    /// where NARs are keps for approximately `expiration`
    pub async fn new(url: Url, cache_dir: PathBuf, expiration: Duration) -> anyhow::Result<Self> {
        let inner = HttpSubstituterInner::new(url)?;
        CachedBinaryCache::wrap(inner, cache_dir, expiration).await
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        build_id::BuildId,
        store_path::StorePath,
        substituter::Substituter,
        test_utils::{file_sha256, HTTP_BINARY_CACHE},
    };
    use std::path::Path;

    use super::*;

    const DEFAULT_EXPIRATION: Duration = Duration::from_hours(1000);

    #[tokio::test]
    async fn test_fetch_store_path_nominal() {
        let cache_dir = tempfile::tempdir().unwrap();
        let substituter = HttpSubstituter::new(
            HTTP_BINARY_CACHE.clone(),
            cache_dir.path().to_path_buf(),
            DEFAULT_EXPIRATION,
        )
        .await
        .unwrap();
        let store_path = StorePath::new(Path::new(
            "/nix/store/2qw62845796lyx649ck67zbk04pv8xhf-source/src/systemctl/systemctl.c",
        ))
        .unwrap();
        let out = substituter
            .fetch_store_path(&store_path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            file_sha256(
                out.join("src/systemctl/systemctl.c")
                    .resolve_inside_root()
                    .await
                    .unwrap()
                    .unwrap()
            )
            .await,
            "402ec408cd95844108e0c93e6bc249b97941a901fbc89ad8d3f45a07c5916708"
        );
    }

    #[tokio::test]
    async fn test_fetch_store_path_missing() {
        let cache_dir = tempfile::tempdir().unwrap();
        let substituter = HttpSubstituter::new(
            HTTP_BINARY_CACHE.clone(),
            cache_dir.path().to_path_buf(),
            DEFAULT_EXPIRATION,
        )
        .await
        .unwrap();
        let store_path = StorePath::new(Path::new(
            "/nix/store/n11lk1q63oooooooooooooja1shs3yr7-source/src/systemctl/systemctl.c",
        ))
        .unwrap();
        let out = substituter.fetch_store_path(&store_path).await.unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_fetch_store_path_bad_host() {
        let url = Url::parse("https://255.255.255.255/doesnotexist").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let substituter =
            HttpSubstituter::new(url, cache_dir.path().to_path_buf(), DEFAULT_EXPIRATION)
                .await
                .unwrap();
        let store_path = StorePath::new(Path::new(
            "/nix/store/n11lk1q63oooooooooooooja1shs3yr7-source/src/systemctl/systemctl.c",
        ))
        .unwrap();
        substituter
            .fetch_store_path(&store_path)
            .await
            .expect_err("it's impossible to connect to this url");
    }

    #[tokio::test]
    async fn test_build_id_to_debug_output_nominal() {
        let cache_dir = tempfile::tempdir().unwrap();
        let substituter = HttpSubstituter::new(
            HTTP_BINARY_CACHE.clone(),
            cache_dir.path().to_path_buf(),
            DEFAULT_EXPIRATION,
        )
        .await
        .unwrap();

        // /nix/store/pbqih0cmbc4xilscj36m80ardhg6kawp-systemd-minimal-257.6/bin/systemctl
        let out = substituter
            .build_id_to_debug_output(
                &BuildId::new("b87e34547e94f167f4b737f3a25955477a485cc7").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            file_sha256(
                out.join("lib/debug/.build-id/b8/7e34547e94f167f4b737f3a25955477a485cc7.debug")
                    .resolve_inside_root()
                    .await
                    .unwrap()
                    .unwrap()
            )
            .await,
            "b7b38a0c43ec066a034e38f86f5f0926867b9eb2144fd8a7aac88c7c38bf5566"
        );
    }

    #[tokio::test]
    async fn test_build_id_to_debug_output_missing() {
        let cache_dir = tempfile::tempdir().unwrap();
        let substituter = HttpSubstituter::new(
            HTTP_BINARY_CACHE.clone(),
            cache_dir.path().to_path_buf(),
            DEFAULT_EXPIRATION,
        )
        .await
        .unwrap();

        assert!(substituter
            .build_id_to_debug_output(
                &BuildId::new("483bd7f7229bdb00000000000000e4f37e15c293").unwrap(),
            )
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_build_id_to_debug_output_error() {
        let url = Url::parse("https://255.255.255.255/doesnotexist").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let substituter =
            HttpSubstituter::new(url, cache_dir.path().to_path_buf(), DEFAULT_EXPIRATION)
                .await
                .unwrap();

        substituter
            .build_id_to_debug_output(
                &BuildId::new("483bd7f7229bdb00000000000000e4f37e15c293").unwrap(),
            )
            .await
            .unwrap_err();
    }
}

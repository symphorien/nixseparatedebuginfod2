use std::fmt::Debug;

use anyhow::Context;
use futures::StreamExt;
use http::StatusCode;
use reqwest::{Client, Url};
use tokio::io::AsyncBufRead;
use tokio_util::io::StreamReader;

use crate::substituter::binary_cache::BinaryCache;

use super::Priority;

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Fetching from `http://` and `https://` substituters.
///
/// The substituter must have been created with `?index-debug-info=true`.
pub struct HttpSubstituter {
    url: Url,
    client: Client,
}

impl Debug for HttpSubstituter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSubstituter")
            .field("url", &self.url.as_str())
            .finish()
    }
}

impl HttpSubstituter {
    /// Create an http or https substituter with this base url.
    pub fn new(url: Url) -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .with_context(|| format!("creating an http client to connect to {url}"))?;
        Ok(Self { url, client })
    }
    fn make_url(&self, rest: &str) -> anyhow::Result<Url> {
        self.url
            .join(rest)
            .with_context(|| format!("{}{rest} is malformed url", &self.url))
    }
}

impl BinaryCache for HttpSubstituter {
    /// sends a get query to this url, and returns the response only if 200
    ///
    /// returns None on 404, an error in other cases.
    async fn stream_location(
        &self,
        what: &str,
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

#[cfg(test)]
mod tests {
    use crate::{
        build_id::BuildId,
        store_path::StorePath,
        substituter::Substituter,
        test_utils::{file_sha256, HTTP_BINARY_CACHE},
        utils::Presence,
    };
    use std::path::Path;

    use super::*;

    #[tokio::test]
    async fn test_fetch_store_path_nominal() {
        let substituter = HttpSubstituter::new(HTTP_BINARY_CACHE.clone()).unwrap();
        let store_path = StorePath::new(Path::new(
            "/nix/store/2qw62845796lyx649ck67zbk04pv8xhf-source/src/systemctl/systemctl.c",
        ))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("target");
        assert_eq!(
            substituter
                .fetch_store_path(&store_path, &into)
                .await
                .unwrap(),
            Presence::Found
        );
        assert_eq!(
            file_sha256(&into.join("src/systemctl/systemctl.c")).await,
            "402ec408cd95844108e0c93e6bc249b97941a901fbc89ad8d3f45a07c5916708"
        );
    }

    #[tokio::test]
    async fn test_fetch_store_path_missing() {
        let substituter = HttpSubstituter::new(HTTP_BINARY_CACHE.clone()).unwrap();
        let store_path = StorePath::new(Path::new(
            "/nix/store/n11lk1q63oooooooooooooja1shs3yr7-source/src/systemctl/systemctl.c",
        ))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("target");
        assert_eq!(
            substituter
                .fetch_store_path(&store_path, &into)
                .await
                .unwrap(),
            Presence::NotFound
        );
    }

    #[tokio::test]
    async fn test_fetch_store_path_bad_host() {
        let url = Url::parse("https://255.255.255.255/doesnotexist").unwrap();
        let substituter = HttpSubstituter::new(url).unwrap();
        let store_path = StorePath::new(Path::new(
            "/nix/store/n11lk1q63oooooooooooooja1shs3yr7-source/src/systemctl/systemctl.c",
        ))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("target");
        substituter
            .fetch_store_path(&store_path, &into)
            .await
            .expect_err("it's impossible to connect to this url");
    }

    #[tokio::test]
    async fn test_build_id_to_debug_output_nominal() {
        let substituter = HttpSubstituter::new(HTTP_BINARY_CACHE.clone()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("target");
        // /nix/store/pbqih0cmbc4xilscj36m80ardhg6kawp-systemd-minimal-257.6/bin/systemctl
        assert_eq!(
            substituter
                .build_id_to_debug_output(
                    &BuildId::new("b87e34547e94f167f4b737f3a25955477a485cc7").unwrap(),
                    &into
                )
                .await
                .unwrap(),
            Presence::Found
        );
        assert_eq!(
            file_sha256(
                &into.join("lib/debug/.build-id/b8/7e34547e94f167f4b737f3a25955477a485cc7.debug")
            )
            .await,
            "b7b38a0c43ec066a034e38f86f5f0926867b9eb2144fd8a7aac88c7c38bf5566"
        );
    }

    #[tokio::test]
    async fn test_build_id_to_debug_output_missing() {
        let substituter = HttpSubstituter::new(HTTP_BINARY_CACHE.clone()).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("target");
        assert_eq!(
            substituter
                .build_id_to_debug_output(
                    &BuildId::new("483bd7f7229bdb00000000000000e4f37e15c293").unwrap(),
                    &into
                )
                .await
                .unwrap(),
            Presence::NotFound
        );
    }

    #[tokio::test]
    async fn test_build_id_to_debug_output_error() {
        let url = Url::parse("https://255.255.255.255/doesnotexist").unwrap();
        let substituter = HttpSubstituter::new(url).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("target");
        substituter
            .build_id_to_debug_output(
                &BuildId::new("483bd7f7229bdb00000000000000e4f37e15c293").unwrap(),
                &into,
            )
            .await
            .unwrap_err();
    }
}

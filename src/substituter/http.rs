use std::{fmt::Debug, path::Path};

use anyhow::Context;
use futures::StreamExt;
use http::StatusCode;
use http_body_util::{BodyExt, Limited};
use reqwest::{Client, Response, Url};
use tokio_util::io::StreamReader;
use tracing::Level;

use crate::{
    build_id::BuildId,
    nar::{narinfo_to_nar_location, unpack_nar},
    store_path::StorePath,
    utils::{DecompressingReader, Presence},
};

use super::{DebugInfoRedirectJson, Priority, Substituter};

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

    async fn return_nar(&self, nar_url: Url, into: &Path) -> anyhow::Result<Presence> {
        let Some(response) = self.query(&nar_url).await? else {
            return Ok(Presence::NotFound);
        };
        let stream = response.bytes_stream();
        let nar_reader = StreamReader::new(stream.map(|r| r.map_err(std::io::Error::other)));
        let decompressing_nar_reader = DecompressingReader::new(
            tokio::io::BufReader::new(nar_reader),
            nar_url.as_str().as_bytes(),
        )?;
        unpack_nar(decompressing_nar_reader, into).await?;

        Ok(Presence::Found)
    }

    /// sends a get query to this url, and returns the response only if 200
    ///
    /// returns None on 404, an error in other cases.
    #[tracing::instrument(level=Level::TRACE, err, skip(url), fields(url=%url))]
    async fn query(&self, url: &Url) -> anyhow::Result<Option<Response>> {
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
        Ok(Some(response))
    }
}

const SMALL_BODY_SIZE: usize = 1024 * 1024 - 1;

#[async_trait::async_trait]
impl Substituter for HttpSubstituter {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &std::path::Path,
    ) -> anyhow::Result<Presence> {
        // for cache.nixos.org
        let url1 = self.make_url(&format!("debuginfo/{}", build_id))?;
        // for file:// binary caches
        let url2 = self.make_url(&format!("debuginfo/{}.debug", build_id))?;
        let (url, response) = match self.query(&url1).await? {
            Some(r) => (url1, r),
            None => match self.query(&url2).await? {
                Some(r) => (url2, r),
                None => return Ok(Presence::NotFound),
            },
        };
        let body = Limited::new(
            std::convert::Into::<reqwest::Body>::into(response),
            SMALL_BODY_SIZE,
        );
        let json_bytes = body
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("downloading {url}: {e:#}"))?
            .to_bytes();
        let redirect: DebugInfoRedirectJson = serde_json::from_slice(&json_bytes)
            .with_context(|| format!("unexpected format for {}", url))?;
        let nar_url = self.make_url(&format!("debuginfo/{}", &redirect.archive))?;
        self.return_nar(nar_url, into).await
    }

    async fn fetch_store_path(
        &self,
        store_path: &StorePath,
        into: &std::path::Path,
    ) -> anyhow::Result<Presence> {
        let narinfo_url = self.make_url(&format!("{}.narinfo", store_path.hash()))?;
        let Some(response) = self.query(&narinfo_url).await? else {
            return Ok(Presence::NotFound);
        };
        let stream = response.bytes_stream();
        let reader = StreamReader::new(stream.map(|r| r.map_err(std::io::Error::other)));
        let url = narinfo_to_nar_location(reader)
            .await
            .with_context(|| format!("parsing {}", narinfo_url))?;
        let nar_url = self.make_url(&url)?;
        self.return_nar(nar_url, into).await
    }

    fn priority(&self) -> Priority {
        Priority::Unknown
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{file_sha256, HTTP_BINARY_CACHE};

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

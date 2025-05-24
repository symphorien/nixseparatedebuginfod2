use std::path::Path;

use anyhow::Context;
use futures::StreamExt;
use http::StatusCode;
use http_body_util::{BodyExt, Limited};
use reqwest::{Client, Url};
use tokio_util::io::StreamReader;

use crate::{
    build_id::BuildId,
    nar::{narinfo_to_nar_location, unpack_nar},
    store_path::StorePath,
    utils::{DecompressingReader, Presence},
};

use super::{DebugInfoRedirectJson, Substituter};

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Fetching from `http://` and `https://` substituters.
///
/// The substituter must have been created with `?index-debug-info=true`.
pub struct HttpSubstituter {
    url: Url,
    client: Client,
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
        let response = self
            .client
            .get(nar_url.clone())
            .send()
            .await
            .with_context(|| format!("connecting to {nar_url}"))?;
        match response.status() {
            StatusCode::OK => (),
            StatusCode::NOT_FOUND => return Ok(Presence::NotFound),
            other => anyhow::bail!("{nar_url} returned {other:?}"),
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
}

const SMALL_BODY_SIZE: usize = 1024 * 1024 - 1;

#[async_trait::async_trait]
impl Substituter for HttpSubstituter {
    async fn build_id_to_debug_output(
        &self,
        build_id: &BuildId,
        into: &std::path::Path,
    ) -> anyhow::Result<Presence> {
        let url = self.make_url(&format!("debuginfo/{}.debug", build_id))?;
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("connecting to {url}"))?;
        match response.status() {
            StatusCode::OK => (),
            StatusCode::NOT_FOUND => return Ok(Presence::NotFound),
            other => anyhow::bail!("{url} returned {other:?}"),
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
        let response = self
            .client
            .get(narinfo_url.clone())
            .send()
            .await
            .with_context(|| format!("connecting to {narinfo_url}"))?;
        match response.status() {
            StatusCode::OK => (),
            StatusCode::NOT_FOUND => return Ok(Presence::NotFound),
            other => anyhow::bail!("{narinfo_url} returned {other:?}"),
        };
        let stream = response.bytes_stream();
        let reader = StreamReader::new(stream.map(|r| r.map_err(std::io::Error::other)));
        let url = narinfo_to_nar_location(reader)
            .await
            .with_context(|| format!("parsing {}", narinfo_url))?;
        let nar_url = self.make_url(&url)?;
        self.return_nar(nar_url, into).await
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
            "/nix/store/n11lk1q63864l8vfdl8h8aja1shs3yr7-source/src/getopt.c",
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
            file_sha256(&into.join("src/getopt.c")),
            "a0e40f26252d08a127b7c6fb16499447c543252f883154322207fa8b1d8d460a"
        );
    }

    #[tokio::test]
    async fn test_fetch_store_path_missing() {
        let substituter = HttpSubstituter::new(HTTP_BINARY_CACHE.clone()).unwrap();
        let store_path = StorePath::new(Path::new(
            "/nix/store/n11lk1q63oooooooooooooja1shs3yr7-source/src/getopt.c",
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
            "/nix/store/n11lk1q63oooooooooooooja1shs3yr7-source/src/getopt.c",
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
        assert_eq!(
            substituter
                .build_id_to_debug_output(
                    &BuildId::new("483bd7f7229bdb06462222e1e353e4f37e15c293").unwrap(),
                    &into
                )
                .await
                .unwrap(),
            Presence::Found
        );
        assert_eq!(
            file_sha256(
                &into.join("lib/debug/.build-id/48/3bd7f7229bdb06462222e1e353e4f37e15c293.debug")
            ),
            "e8bcbed1f80e8fcaeb622ad1c1c77a526047ace2f73b75ef1128b47a6d317bb0"
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

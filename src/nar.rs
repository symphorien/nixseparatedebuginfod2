//! utilities about NAR files (nix archives)
use anyhow::Context;
use futures::StreamExt;
use nix_nar::Decoder;
use std::path::Path;
use std::pin::pin;
use tokio::io::{AsyncBufRead, AsyncRead};
use tokio_util::codec::{FramedRead, LinesCodec};

/// Unpacks the nar passed in argument to the specified path.
///
/// The path must not exist yet, but its parent must be an existing directory.
///
/// In case of error no guarantee is given that destination is clean.
pub async fn unpack_nar<'a, T: AsyncRead + Send + std::fmt::Debug + 'a>(
    nar: T,
    destination: &'a Path,
) -> anyhow::Result<()> {
    let nar_name = format!("{nar:?}");
    let mut async_reader = pin!(nar);
    let (static_async_reader, mut static_async_writer) = tokio::io::simplex(1_000_000);
    let sync_reader = tokio_util::io::SyncIoBridge::new(static_async_reader);
    let destination2 = destination.to_path_buf();
    let handle = tokio::task::spawn_blocking(move || {
        let decoder = Decoder::new(sync_reader)?;
        decoder.unpack(destination2)
    });
    let result = tokio::io::copy(&mut async_reader, &mut static_async_writer).await;
    handle
        .await
        .context("failed to join handle")?
        .with_context(|| format!("failed to unpack nar {nar_name}"))?;
    result.with_context(|| format!("copying data to unpacker of {nar_name}"))?;
    Ok(())
}

const NAR_URL_KEY: &str = "URL: ";

const NAR_MAX_LINES_LENGTH: usize = 1024;

/// Parses a narinfo to find the relative location of the corresponing nar.
pub async fn narinfo_to_nar_location<T: AsyncBufRead>(narinfo: T) -> anyhow::Result<String> {
    let narinfo = pin!(narinfo);
    let decoder = LinesCodec::new_with_max_length(NAR_MAX_LINES_LENGTH);
    let mut lines = pin!(FramedRead::new(narinfo, decoder));
    while let Some(line) = lines.next().await {
        let line = line.context("parsing narinfo line")?;
        if let Some(suffix) = line.strip_prefix(NAR_URL_KEY) {
            return Ok(suffix.to_owned());
        }
    }
    anyhow::bail!("narinfo dit not have an URL:")
}

#[tokio::test]
async fn test_narinfo_to_nar_location() {
    let narinfo =
        crate::test_utils::fixture("file_binary_cache/8avg418ydn50ha9wlyrv2f5pj4qccldg.narinfo");
    let fd = tokio::fs::File::open(&narinfo).await.unwrap();
    let bufread = tokio::io::BufReader::new(fd);
    let url = narinfo_to_nar_location(bufread).await.unwrap();
    assert_eq!(
        url,
        "nar/078h1d26cqf628a2qy8660q6a5v5ga38mh036w5c0y49k9bxsaq9.nar.xz"
    );
}

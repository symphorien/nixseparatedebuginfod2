//! utilities about NAR files (nix archives)
use anyhow::Context;
use futures::StreamExt;
use std::fmt::Debug;
use std::path::Path;
use std::pin::pin;
use std::process::Stdio;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWriteExt};
use tokio_util::codec::{FramedRead, LinesCodec};

/// Unpacks the nar passed in argument to the specified path.
///
/// The path must not exist yet.
///
/// In case of error no guarantee is given that destination is clean.
///
/// Currently not a native implementation, actually shells out to `nix-store --restore`.
pub async fn unpack_nar<T: AsyncRead + Debug>(nar: T, destination: &Path) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new("nix-store");
    command.arg("--restore");
    command.arg(destination);
    command.stdin(Stdio::piped());
    command.kill_on_drop(true);
    let mut process = command
        .spawn()
        .with_context(|| format!("failed to spawn {:?}", &command))?;
    let Some(mut stdin) = process.stdin.take() else {
        anyhow::bail!("running nix-store --restore without stdin");
    };
    let mut input_reader = pin!(nar);
    let result = tokio::io::copy(&mut input_reader, &mut stdin).await;
    let result2 = stdin.flush().await;
    match result.and(result2) {
        Ok(()) => {
            let status = process
                .wait()
                .await
                .context("waiting for nix-store --restore")?;
            anyhow::ensure!(status.success(), "nix-store --restore failed");
            Ok(())
        }
        Err(e) => {
            process
                .kill()
                .await
                .with_context(|| format!("killing nix-store --restore because of {e:#}"))?;
            Err(e).context(format!(
                "piping {:?} into nix-store --restore",
                &input_reader
            ))
        }
    }
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

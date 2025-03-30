use anyhow::Context;
use std::fmt::Debug;
use std::path::Path;
use std::pin::pin;
use std::process::Stdio;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt};

/// Unpacks the nar passed in argument to the specified path.
///
/// The path must not exist yet.
///
/// In case of error no guarantee is given that destination is clean.
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

pub async fn narinfo_to_nar_location<T: AsyncBufRead>(narinfo: T) -> anyhow::Result<String> {
    // FIXME: protect against large file with no newline
    let narinfo = pin!(narinfo);
    let mut lines = pin!(narinfo.lines());
    while let Some(line) = lines.next_line().await.context("parsing narinfo line")? {
        if let Some(suffix) = line.strip_prefix(NAR_URL_KEY) {
            return Ok(suffix.to_owned());
        }
    }
    anyhow::bail!("narinfo dit not have an URL:")
}

#[tokio::test]
async fn test_narinfo_to_nar_location() {
    let narinfo =
        crate::test_utils::fixture("file_binary_cache/m3kjnkzvsj983fkzam6hc6vg3sjdcj19.narinfo");
    let fd = tokio::fs::File::open(&narinfo).await.unwrap();
    let bufread = tokio::io::BufReader::new(fd);
    let url = narinfo_to_nar_location(bufread).await.unwrap();
    assert_eq!(
        url,
        "nar/02zi1rwab3ff5m1k7c85abbdy717x1487fxd5j0b7kbmybk992x0.nar.xz"
    );
}

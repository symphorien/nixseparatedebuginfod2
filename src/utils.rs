//! Misc utils
use std::path::Path;
use std::pin::pin;
use std::{fmt::Debug, time::SystemTime};

use async_compression::tokio::bufread::{XzDecoder, ZstdDecoder};
use pin_project::pin_project;
use tokio::io::{AsyncBufRead, AsyncRead};

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
#[must_use]
/// Whether the requested file exists in the substituter or not
pub enum Presence {
    /// Yes the substituter has this file or directory
    Found,
    /// No the substituter does not have this file or directory
    NotFound,
}

/// Sets the mtime of this path to current time
///
/// the path must exist.
pub async fn touch(path: &Path) -> std::io::Result<()> {
    let std_file = std::fs::File::open(path)?;
    tokio::task::spawn_blocking(move || std_file.set_modified(SystemTime::now())).await?
}

#[tokio::test]
async fn test_touch() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("file");
    std::fs::write(&f, "contents").unwrap();
    let mtime_before = f.metadata().unwrap().modified().unwrap();
    let time1 = SystemTime::now();
    touch(&f).await.unwrap();
    let time2 = SystemTime::now();
    let mtime_after = f.metadata().unwrap().modified().unwrap();
    assert!(mtime_before <= time1);
    assert!(time1 <= mtime_after);
    assert!(mtime_after <= time2);
}

/// Ensure that `path` does not exists.
///
/// Does not dereference symlinks, and does not fail if path already does not exists.
pub async fn remove_recursively_if_exists(path: &Path) -> std::io::Result<()> {
    let meta = match tokio::fs::symlink_metadata(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        other => other,
    };
    let result = if meta?.is_dir() {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    };
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[tokio::test]
async fn test_remove_recursively_if_exists_nonempty_dir() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("test");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("file"), "hello").unwrap();
    remove_recursively_if_exists(&dir).await.unwrap();
    assert!(!dir.exists());
}
#[tokio::test]
async fn test_remove_recursively_if_exists_nonexisting_dir() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("test");
    remove_recursively_if_exists(&dir).await.unwrap();
    assert!(!dir.exists());
}

#[tokio::test]
async fn test_remove_recursively_if_exists_file() {
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join("test");
    std::fs::write(&file, "hello").unwrap();
    remove_recursively_if_exists(&file).await.unwrap();
    assert!(!file.exists());
}

#[tokio::test]
async fn test_remove_recursively_if_exists_symlink() {
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join("test");
    std::fs::write(&file, "hello").unwrap();
    let symlink = t.path().join("symlink");
    std::os::unix::fs::symlink(&file, &symlink).unwrap();
    remove_recursively_if_exists(&symlink).await.unwrap();
    assert!(file.exists());
    assert!(!symlink.exists());
}

#[pin_project(project = DecompressingReaderInnerProjected)]
enum DecompressingReaderInner<R: AsyncBufRead> {
    XZ(#[pin] XzDecoder<R>),
    Zstd(#[pin] ZstdDecoder<R>),
    NoCompression(#[pin] R),
}
/// A wrapper arount an [`AsyncBufRead`] that transparently decompresses it
#[pin_project]
pub struct DecompressingReader<R: AsyncBufRead> {
    name: Vec<u8>,
    #[pin]
    reader: DecompressingReaderInner<R>,
}

impl<R: AsyncBufRead> DecompressingReader<R> {
    /// Wraps an [`AsyncBufRead`] whose content is compressed.
    ///
    /// Reading from the [`DecompressingReader`] will yield the decompressed bytes.
    ///
    /// The format of the compression is guessed from the extension of `path_or_url`.
    pub fn new(reader: R, path_or_url: &[u8]) -> anyhow::Result<Self> {
        let reader = if path_or_url.ends_with(b".nar") {
            DecompressingReaderInner::NoCompression(reader)
        } else if path_or_url.ends_with(b".nar.xz") {
            DecompressingReaderInner::XZ(XzDecoder::new(reader))
        } else if path_or_url.ends_with(b".nar.zst") || path_or_url.ends_with(b".nar.zstd") {
            DecompressingReaderInner::Zstd(ZstdDecoder::new(reader))
        } else {
            anyhow::bail!(
                "don't support compression for extension of {}",
                &String::from_utf8_lossy(&path_or_url)
            );
        };
        let name = path_or_url.to_owned();
        Ok(DecompressingReader { name, reader })
    }
}

impl<R: AsyncBufRead> Debug for DecompressingReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecompressingReader")
            .field("name", &String::from_utf8_lossy(&self.name))
            .finish()
    }
}

impl<R: AsyncBufRead> AsyncRead for DecompressingReader<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let inner = self.project();
        let inner2 = inner.reader.project();
        match inner2 {
            DecompressingReaderInnerProjected::XZ(reader) => reader.poll_read(cx, buf),
            DecompressingReaderInnerProjected::Zstd(reader) => reader.poll_read(cx, buf),
            DecompressingReaderInnerProjected::NoCompression(reader) => reader.poll_read(cx, buf),
        }
    }
}

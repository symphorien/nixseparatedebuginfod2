//! Misc utils
use std::path::Path;
use std::pin::pin;
use std::{fmt::Debug, time::Duration};

use anyhow::Context;
use async_compression::tokio::bufread::{XzDecoder, ZstdDecoder};
use nix::fcntl::AT_FDCWD;
use nix::sys::time::TimeSpec;
use pin_project::pin_project;
use tokio::io::{AsyncBufRead, AsyncRead};
use tracing::Level;

#[cfg(test)]
use crate::test_utils::count_elements_in_dir;

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
///
/// does not dereference symlinks
pub async fn touch(path: &Path) -> anyhow::Result<()> {
    let copy = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        nix::sys::stat::utimensat(
            AT_FDCWD,
            &copy,
            &TimeSpec::UTIME_NOW,
            &TimeSpec::UTIME_NOW,
            nix::sys::stat::UtimensatFlags::NoFollowSymlink,
        )
    })
    .await?
    .with_context(|| format!("touch({})", path.display()))?;
    Ok(())
}

#[tokio::test]
async fn test_touch() {
    use std::time::Duration;
    use std::time::SystemTime;
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("file");
    std::fs::write(&f, "contents").unwrap();
    let mtime_before = f.symlink_metadata().unwrap().modified().unwrap();
    let time1 = SystemTime::now();
    std::thread::sleep(Duration::from_millis(20));
    touch(&f).await.unwrap();
    nix::unistd::sync();
    std::thread::sleep(Duration::from_millis(20));
    let time2 = SystemTime::now();
    let mtime_after = f.symlink_metadata().unwrap().modified().unwrap();
    assert!(mtime_before <= time1);
    assert!(time1 <= mtime_after);
    assert!(mtime_after <= time2);
}

#[tokio::test]
async fn test_touch_symlink() {
    use std::time::Duration;
    use std::time::SystemTime;
    let d = tempfile::tempdir().unwrap();
    let l = d.path().join("link");
    let target = d.path().join("target that does not exist");
    std::os::unix::fs::symlink(&target, &l).unwrap();
    let time_before = SystemTime::now();
    std::thread::sleep(Duration::from_millis(20));
    touch(&l).await.unwrap();
    nix::unistd::sync();
    let mtime_after = l.symlink_metadata().unwrap().modified().unwrap();
    assert!(dbg!(time_before) <= dbg!(mtime_after));
}

/// Ensure that `path` does not exists.
///
/// Does not dereference symlinks, and does not fail if path already does not exists.
pub async fn remove_recursively_if_exists(path: &Path) -> std::io::Result<()> {
    let meta = match tokio::fs::symlink_metadata(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        other => other,
    };
    tracing::trace!(?path, "removing");
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

/// Removes elements older than `expiration` in this cache directory.
///
/// Does not remove the directory itself, which must exist.
///
/// This is a way to migrate the cache directory from one layout in a version to a different layout
/// in later versions.
#[tracing::instrument(level=Level::DEBUG)]
pub fn clean_cache_dir(path: &Path, expiration: Duration) -> anyhow::Result<()> {
    for entry in walkdir::WalkDir::new(path)
        .min_depth(1)
        .follow_links(false)
        .contents_first(true)
    {
        let entry = entry?;
        if entry.file_type().is_dir() {
            let path = entry.path();
            if std::fs::read_dir(path)
                .with_context(|| format!("failed to list cache dir {path:?}"))?
                .next()
                .is_none()
            {
                // empty dir
                std::fs::remove_dir(path)
                    .with_context(|| format!("failed to remove unused cache dir {path:?}"))?;
            }
        } else {
            let meta = entry
                .metadata()
                .with_context(|| format!("stat({:?}", entry.path()))?;
            let mut most_recent_time = meta.accessed();
            let other_time = meta.modified();
            // only report errors if neither time could be obtained
            // keep only the most recent
            most_recent_time = match (&most_recent_time, &other_time) {
                (Ok(ref t), Ok(ref t2)) if t > t2 => most_recent_time,
                (Ok(_), Ok(_)) => other_time,
                (Err(_), Ok(_)) => other_time,
                (Ok(_), Err(_)) => most_recent_time,
                (Err(_), Err(_)) => other_time,
            };
            let most_recent_time = most_recent_time
                .with_context(|| format!("failed to get time of {:?}", entry.path()))?;

            let age = most_recent_time
                .elapsed()
                .with_context(|| format!("cannot compute age of {:?}", entry.path()))?;
            if age > expiration {
                std::fs::remove_file(entry.path()).with_context(|| {
                    format!("cannot remove expired cache file {:?}", entry.path())
                })?;
            }
        }
    }
    Ok(())
}

#[test]
fn clean_cache_dir_clean_everything() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("nixseparatedebuginfod2/b")).unwrap();
    let path = t.path().join("nixseparatedebuginfod2");
    assert_eq!(count_elements_in_dir(&path), 2);
    clean_cache_dir(&path, Duration::from_secs(0)).unwrap();
    assert_eq!(count_elements_in_dir(&path), 1);
}

#[test]
fn clean_cache_dir_clean_broken_symlink() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("nixseparatedebuginfod2/b")).unwrap();
    let path = t.path().join("nixseparatedebuginfod2");
    let symlink = path.join("symlink");
    std::os::unix::fs::symlink("broken", &symlink).unwrap();
    assert_eq!(count_elements_in_dir(&path), 3);
    clean_cache_dir(&path, Duration::from_secs(0)).unwrap();
    assert_eq!(count_elements_in_dir(&path), 1);
}

#[test]
fn clean_cache_dir_nominal() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("nixseparatedebuginfod2/b")).unwrap();
    let path = t.path().join("nixseparatedebuginfod2");
    std::fs::create_dir_all(path.join("a/b")).unwrap();
    std::fs::create_dir_all(path.join("c/d")).unwrap();
    std::fs::write(path.join("a/b/old"), "old").unwrap();
    std::fs::write(path.join("c/d/new"), "new").unwrap();
    let expiration = Duration::from_hours(48);
    let pivot = std::time::SystemTime::now() - expiration;
    let before = pivot - Duration::from_secs(20);
    let after = pivot + Duration::from_secs(20);
    let convert = |when: std::time::SystemTime| -> TimeSpec {
        when.duration_since(std::time::UNIX_EPOCH).unwrap().into()
    };
    let before_ts = convert(before);
    let after_ts = convert(after);
    nix::sys::stat::utimensat(
        AT_FDCWD,
        &path.join("a/b/old"),
        &before_ts,
        &before_ts,
        nix::sys::stat::UtimensatFlags::NoFollowSymlink,
    )
    .unwrap();
    nix::sys::stat::utimensat(
        AT_FDCWD,
        &path.join("c/d/new"),
        &after_ts,
        &after_ts,
        nix::sys::stat::UtimensatFlags::NoFollowSymlink,
    )
    .unwrap();
    clean_cache_dir(&path, expiration).unwrap();
    assert!(!path.join("a").exists());
    assert!(path.join("c/d/new").exists());
}

const CONTROLS_AND_SLASH_AND_PERCENT: percent_encoding::AsciiSet =
    percent_encoding::CONTROLS.add(b'/').add(b'%');

/// urlencode special characters so that this string is a valid filename
///
/// injective
pub fn percent_encode_to_filename(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, &CONTROLS_AND_SLASH_AND_PERCENT).to_string()
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
                &String::from_utf8_lossy(path_or_url)
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

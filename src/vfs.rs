//! Manipulation of paths with untrusted symlinks

use std::fmt::Debug;
use std::{
    future::Future,
    path::{Component, Path, PathBuf},
};

use anyhow::Context;
use tracing::Instrument;
use tracing::Level;

use crate::{
    cache::CachedPathLock,
    store_path::{StorePath, NIX_STORE},
};

/// A path with untrusted symlinks.
///
/// The underlying cache directory will not be dropped until this value is dropped.
///
/// If the path contains symlinks, they may only be pointing to:
/// * store paths
/// * inside the current root of the `RestrictedPath`
///
/// One intentionnally cannot access the underlying path to prevent bypassing the checks.
/// To use, convert to a [`ResolvedPath`] first.
#[derive(Clone)]
pub struct RestrictedPath {
    /// non store path symlinks in `inner` may not escape this prefix
    ///
    /// must be absolute and the last component must not be a symlink
    root: PathBuf,
    /// the absolute untrusted path
    inner: PathBuf,
    lock: CachedPathLock,
}

impl Debug for RestrictedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RestrictedPath")
            .field(&self.inner.display())
            .finish()
    }
}

/// A path where all untrusted symlinks have been resolved
///
/// The underlying cache directory will not be dropped until this value is dropped.
///
/// Guaranteed to exist.
///
/// One intentionnally cannot access the underlying path to prevent bypassing the checks.
/// To use, open the file with the implementation of [`AsFile`].
#[derive(Clone)]
pub struct ResolvedPath {
    path: PathBuf,
    lock: CachedPathLock,
}

impl Debug for ResolvedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ResolvedPath")
            .field(&self.path.display())
            .finish()
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
/// The possible file types of a [`ResolvedPath`]
///
/// A [`ResolvedPath`] cannot be a symlink, and there is no reason not to error on special files
/// (fifos, sockets, devices)
pub enum ResolvedPathKind {
    /// a regular file
    File,
    /// a directory
    Directory,
}

impl ResolvedPath {
    /// returns whether this path is a file or a directory
    pub async fn kind(&self) -> anyhow::Result<ResolvedPathKind> {
        let m = tokio::fs::symlink_metadata(&self.path)
            .await
            .with_context(|| format!("lstat({self:?})"))?;
        if m.is_file() {
            Ok(ResolvedPathKind::File)
        } else if m.is_dir() {
            Ok(ResolvedPathKind::Directory)
        } else {
            anyhow::bail!(
                "unexpected file type {:?} for resolved path {self:?}",
                m.file_type()
            )
        }
    }

    /// Appends a relative path to this path to access a transitive child file.
    ///
    /// Makes only sense if self is a directory.
    ///
    /// The result is a [`RestrictedPath`] bound not to escape `self`.
    ///
    /// Not expected to error in practice.
    pub async fn join(self, rest: impl AsRef<Path>) -> anyhow::Result<RestrictedPath> {
        Ok(RestrictedPath::new(self.path, self.lock).await?.join(rest))
    }
}

/// Stuff on which one can call [`tokio::fs::File::open`]
#[async_trait::async_trait]
pub trait AsFile {
    /// call [`tokio::fs::File::open`] on the underlying path
    ///
    /// without revealing the path
    async fn open(&self) -> std::io::Result<tokio::fs::File>;
}

#[async_trait::async_trait]
impl AsFile for ResolvedPath {
    async fn open(&self) -> std::io::Result<tokio::fs::File> {
        tokio::fs::File::open(&self.path).await
    }
}

#[async_trait::async_trait]
impl<T: AsRef<Path> + Sync> AsFile for T {
    async fn open(&self) -> std::io::Result<tokio::fs::File> {
        tokio::fs::File::open(self.as_ref()).await
    }
}

/// One can iterate the files in a directory.
pub trait WalkableDirectory: Sized + Debug {
    /// Returns an iterator of the relative paths of all files contained in this directory
    ///
    /// omits non files, follows no symlinks.
    fn list_files_recursively(&self) -> impl Iterator<Item = anyhow::Result<PathBuf>>;
}

impl<T: AsRef<Path> + Sync + Sized + Debug> WalkableDirectory for T {
    fn list_files_recursively(&self) -> impl Iterator<Item = anyhow::Result<PathBuf>> {
        let walkdir = walkdir::WalkDir::new(self.as_ref())
            .follow_links(false)
            .follow_root_links(false);
        walkdir.into_iter().filter_map(|entry| match entry {
            Err(e) => Some(Err(e.into())),
            Ok(entry) => {
                if entry.file_type().is_file() {
                    match entry.path().strip_prefix(self.as_ref()) {
                        Err(e) => Some(Err(anyhow::anyhow!(
                            "child file {} should be relative to {}: {e}",
                            entry.path().display(),
                            self.as_ref().display()
                        ))),
                        Ok(relative) => Some(Ok(relative.to_path_buf())),
                    }
                } else {
                    None
                }
            }
        })
    }
}

impl WalkableDirectory for ResolvedPath {
    fn list_files_recursively(&self) -> impl Iterator<Item = anyhow::Result<PathBuf>> {
        self.path.list_files_recursively()
    }
}

const MAX_SYMLINK_DEPTH: u32 = 20;

impl RestrictedPath {
    /// Creates a `RestrictedPath` with itself as root
    ///
    /// Fails if the `root` does not exist.
    ///
    /// If `root` is a symlink, then this symlink will be followed without check.
    ///
    /// However, if the resulting path is modified then
    /// symlinks that are introduced will be checked.
    pub async fn new(root: PathBuf, lock: CachedPathLock) -> anyhow::Result<Self> {
        let root = tokio::fs::canonicalize(&root)
            .await
            .with_context(|| format!("canonicalize({})", root.display()))?;
        Ok(Self {
            inner: root.clone(),
            root,
            lock,
        })
    }

    /// Like `[Path.join]`
    ///
    /// Keeps the same root
    ///
    /// If the path is empty, does not add a trailing `/`
    pub fn join<T: AsRef<Path>>(self, rest: T) -> Self {
        let path = rest.as_ref();
        if path == Path::new("") {
            self
        } else {
            Self {
                inner: self.inner.join(path),
                ..self
            }
        }
    }

    /// Resolves all symlinks in the path
    ///
    /// symlinks must either:
    /// * not escape the original root
    /// * be store paths, in which case `resolver` is called an the symlink is resolved in
    /// the resulting `RestrictedPath`
    #[tracing::instrument(level=Level::TRACE, skip(resolver), ret)]
    pub async fn resolve<
        F: Future<Output = anyhow::Result<Option<RestrictedPath>>> + Sized,
        R: Fn(StorePath) -> F,
    >(
        self,
        resolver: R,
    ) -> anyhow::Result<Option<ResolvedPath>> {
        // can change when the symlink resolves to a different store path
        let mut current_root = &self.root;
        // absolute path of a potential symlink inside current_root
        let mut to_be_resolved = self.inner.clone();
        // how many symlinks we have resolved until now
        let mut depth = 0;
        // if we resolve a symlink to a different store path, we will start
        // exploring a different restricted path. This variable contains Some
        // of this restricted path in this case
        let mut current_restricted_path = None;
        'symlinks: loop {
            anyhow::ensure!(
                depth <= MAX_SYMLINK_DEPTH,
                "failed to resolve {}: more than {MAX_SYMLINK_DEPTH} symlinks",
                self.inner.display()
            );
            let relative = to_be_resolved.strip_prefix(current_root).with_context(|| {
                format!(
                    "{} escaped out of {}",
                    self.inner.display(),
                    current_root.display()
                )
            })?;

            // invariant: the path we need is resolved_path.join(remaining_components), and resolved_path contains no symlink
            let mut resolved_path = current_root.to_path_buf();
            let mut remaining_components = relative.components();
            while let Some(component) = remaining_components.next() {
                anyhow::ensure!(
                    resolved_path.starts_with(current_root),
                    "{:?} escaped out of {current_root:?}",
                    &self.inner
                );

                match component {
                    Component::CurDir => continue,
                    Component::ParentDir => {
                        let m = tokio::fs::symlink_metadata(&resolved_path).await.with_context(|| format!("lstat({resolved_path:?}) but this path was already successfully resolved"))?;
                        anyhow::ensure!(
                            m.file_type().is_dir(),
                            "{resolved_path:?} is not a directory but {:?}",
                            m.file_type()
                        );

                        // apparently /.. is / so no need to check the return value
                        resolved_path.pop();
                        continue;
                    }
                    Component::RootDir | Component::Prefix(_) => {
                        anyhow::bail!("unreachable: relative path should not contain {component:?}")
                    }
                    Component::Normal(name) => resolved_path.push(name),
                }
                match tokio::fs::read_link(&resolved_path).await {
                    Err(e) => {
                        match e.kind() {
                            std::io::ErrorKind::NotFound => return Ok(None),
                            // not a symlink
                            std::io::ErrorKind::InvalidInput => (),
                            _ => {
                                return Err(e)
                                    .context(format!("lstat({})", resolved_path.display()))
                            }
                        }
                    }
                    Ok(path) => {
                        resolved_path.pop();
                        let mut to_be_resolved_ = resolved_path;
                        if path != Path::new("") {
                            to_be_resolved_.push(path)
                        }
                        if remaining_components.as_path() != Path::new("") {
                            to_be_resolved_.push(remaining_components.as_path())
                        }
                        to_be_resolved = to_be_resolved_;
                        tracing::trace!("symlink points to {}", to_be_resolved.display());
                        depth += 1;
                        if to_be_resolved.starts_with(NIX_STORE) {
                            let store_path =
                                StorePath::new(&to_be_resolved).with_context(|| {
                                    format!(
                                        "{} resolves to malformed store path {}",
                                        self.inner.display(),
                                        to_be_resolved.display()
                                    )
                                })?;
                            let fetched_store_path = match resolver(store_path.clone()).instrument(tracing::trace_span!("calling resolver", store_path= ?store_path)).await {
                                Err(e) => {
                                    return Err(e).context(format!(
                                        "fetching {store_path:?} the symlink target of {}",
                                        self.inner.display()
                                    ))
                                }
                                Ok(None) => return Ok(None),
                                Ok(Some(x)) => x,
                            };
                            to_be_resolved = fetched_store_path.root.join(store_path.relative());
                            current_restricted_path = Some(fetched_store_path);
                            current_root = &current_restricted_path.as_ref().unwrap().root;
                        }
                        continue 'symlinks;
                    }
                }
            }
            // we iterated on all components, so the target is now resolved_path
            return Ok(Some(ResolvedPath {
                path: resolved_path,
                lock: match current_restricted_path {
                    Some(x) => x.lock,
                    None => self.lock,
                },
            }));
        }
    }

    /// Like `[RestrictedPath::resolve]` except that symlinks tot the store result in an error
    pub async fn resolve_inside_root(self) -> anyhow::Result<Option<ResolvedPath>> {
        self.resolve(|path| async move {
            Err(anyhow::anyhow!(
                "not allowed to point to store path {path:?}"
            ))
        })
        .await
    }
}

#[cfg(test)]
mod test {
    use tokio::io::AsyncReadExt;

    use crate::test_utils::setup_logging;

    use super::*;

    fn make_test_dir(files: Vec<&str>, links: Vec<(&str, &str)>) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for file in files {
            let path = d.path().join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, file).unwrap();
        }
        for (link, target) in links {
            let path = d.path().join(link);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::os::unix::fs::symlink(target, path).unwrap()
        }
        d
    }

    async fn assert_contains(path: &ResolvedPath, contents: &str) {
        let mut file = path.open().await.unwrap();
        let mut buf = String::new();
        file.read_to_string(&mut buf).await.unwrap();
        assert_eq!(&buf, contents);
    }

    #[tokio::test]
    async fn test_resolve_dotdot_no_symlink() {
        let d = make_test_dir(vec!["a/b/c/d", "e"], vec![]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root.join("a/b/c/../../../e");
        let resolved = subject.resolve_inside_root().await.unwrap().unwrap();
        assert_contains(&resolved, "e").await;
    }

    #[tokio::test]
    async fn test_resolve_dot_no_symlink() {
        let d = make_test_dir(vec!["a/b/c/d", "e"], vec![]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root.join("a/b/c/./././d");
        let resolved = subject.resolve_inside_root().await.unwrap().unwrap();
        assert_contains(&resolved, "a/b/c/d").await;
    }

    #[tokio::test]
    async fn test_resolve_dotdot_file() {
        let d = make_test_dir(vec!["a/b/c/d", "e"], vec![]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        // cannot use .. when parent is a file
        let subject = root.join("a/b/c/d/../d");
        subject.resolve_inside_root().await.unwrap_err();
    }

    #[tokio::test]
    async fn test_resolve_dotdot_escape() {
        let d = make_test_dir(vec!["a/b/c/d", "e"], vec![]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root
            .join("..")
            .join(d.path().file_name().unwrap())
            .join("e");
        subject.resolve_inside_root().await.unwrap_err();
    }

    #[tokio::test]
    async fn test_resolve_dotdot_after_symlink() {
        let d = make_test_dir(vec!["a/b/c/d", "e"], vec![("link", "a/b")]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root.join("link/../../e");
        let resolved = subject.resolve_inside_root().await.unwrap().unwrap();
        assert_contains(&resolved, "e").await;
    }

    #[tokio::test]
    async fn test_resolve_symlink_to_dir() {
        let d = make_test_dir(
            vec!["a/b/c/d", "a/b/C"],
            vec![("link", "a/b"), ("a/b/c/link2", "../C")],
        );
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root.join("link/c/link2");
        let resolved = subject.resolve_inside_root().await.unwrap().unwrap();
        assert_contains(&resolved, "a/b/C").await;
    }

    #[tokio::test]
    async fn test_resolve_symlink_dotdot_escape() {
        let d = make_test_dir(vec!["a/b/c/d", "e"], vec![("link", "..")]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root
            .join("link")
            .join(d.path().file_name().unwrap())
            .join("e");
        subject.resolve_inside_root().await.unwrap_err();
    }

    #[tokio::test]
    async fn test_resolve_symlink_loop() {
        let d = make_test_dir(vec!["e"], vec![("a/link", "../a/link")]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root.join("a/link");
        subject.resolve_inside_root().await.unwrap_err();
    }

    #[tokio::test]
    async fn test_resolve_absolute_symlink_escape() {
        setup_logging();
        let d2 = make_test_dir(vec!["escape"], vec![]);
        let d = make_test_dir(
            vec![],
            vec![("link", d2.path().join("escape").to_str().unwrap())],
        );
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root.join("link");
        subject.resolve_inside_root().await.unwrap_err();
    }

    #[tokio::test]
    async fn test_resolve_use_file_as_dir() {
        setup_logging();
        let d = make_test_dir(vec!["a"], vec![]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root.join("a/b");
        subject.resolve_inside_root().await.unwrap_err();
    }

    #[tokio::test]
    async fn test_resolve_missing_file() {
        setup_logging();
        let d = make_test_dir(vec!["a"], vec![]);
        let root = RestrictedPath::new(d.path().to_path_buf(), CachedPathLock::fake())
            .await
            .unwrap();
        let subject = root.join("b");
        assert!(dbg!(subject.resolve_inside_root().await.unwrap()).is_none())
    }
}

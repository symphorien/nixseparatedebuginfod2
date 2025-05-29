//! Utils to work with Nix store paths, i.e. `/nix/store/xxx`.

use std::{
    ffi::OsStr,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

/// `/nix/store`
pub const NIX_STORE: &str = "/nix/store";
const HASH_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
/// A Nix store path (not necessarily its root)
///
/// Currently it hard codes `/nix/store`. Other store locations are not supported.
///
/// A store path upholds the following invariants
/// - starts with /store/path
/// - 3rd components starts with HASH_LEN chars, then at least two others (minus and another)
/// - the HASH_LEN other chars are ascii
pub struct StorePath(PathBuf);

impl AsRef<Path> for StorePath {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

impl StorePath {
    /// Validates that the store path is indeed a store path.
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(
            path.starts_with(Path::new(NIX_STORE)),
            "does not start with {}",
            NIX_STORE
        );
        let Some(std::path::Component::Normal(name)) = path.components().nth(3) else {
            anyhow::bail!("path is just {}, not a store path inside it", NIX_STORE)
        };
        anyhow::ensure!(
            name.len() >= HASH_LEN + 2,
            "store path does not have a hash"
        );
        anyhow::ensure!(name.as_bytes()[..HASH_LEN].is_ascii());
        Ok(Self(path.into()))
    }

    /// Returns the `hash-name` part of the path (after `/nix/store`)
    pub fn name(&self) -> &OsStr {
        match self.0.components().nth(3) {
            Some(std::path::Component::Normal(name)) => name,
            _ => unreachable!(),
        }
    }

    /// Returns the hash part of the path
    pub fn hash(&self) -> &str {
        let os_hash = &self.name().as_bytes()[..HASH_LEN];
        std::str::from_utf8(os_hash).unwrap()
    }

    /// Returns the suffix of the path, excluding `/nix/store/hash-name/`
    pub fn relative(&self) -> &Path {
        self.0
            .strip_prefix(NIX_STORE)
            .unwrap()
            .strip_prefix(self.name())
            .unwrap()
    }
}

#[test]
fn test_store_path_relative_path() {
    StorePath::new(Path::new(
        "./nix/store/hbqzhmrscihnl9vgvw9nqhlzc64r1gwl-sl-5.05/bin/sl",
    ))
    .unwrap_err();
}
#[test]
fn test_store_path_escape() {
    StorePath::new(Path::new(
        "/nix/store/../hbqzhmrscihnl9vgvw9nqhlzc64r1gwl-sl-5.05/bin/sl",
    ))
    .unwrap_err();
}
#[test]
fn test_store_path_storedir() {
    StorePath::new(Path::new("/nix/store")).unwrap_err();
}
#[test]
fn test_store_path_storedir2() {
    StorePath::new(Path::new("/nix/store/")).unwrap_err();
}
#[test]
fn test_store_path_truncated() {
    StorePath::new(Path::new("/nix/store/hbqzhmrscihnl9vgvw9nqhlzc64r1")).unwrap_err();
}
#[test]
fn test_store_path_badhash() {
    StorePath::new(&PathBuf::from(OsStr::from_bytes(
        &b"/nix/store/hbqzhmrsci\xffnl9vgvw9nqhlzc64r1gwl-sl-5.05/bin/sl"[..],
    )))
    .unwrap_err();
}
#[test]
fn test_store_path_name() {
    let path = StorePath::new(Path::new(
        "/nix/store/hbqzhmrscihnl9vgvw9nqhlzc64r1gwl-sl-5.05/bin/sl",
    ))
    .unwrap();
    assert_eq!(path.name(), "hbqzhmrscihnl9vgvw9nqhlzc64r1gwl-sl-5.05");
}
#[test]
fn test_store_path_hash() {
    let path = StorePath::new(Path::new(
        "/nix/store/hbqzhmrscihnl9vgvw9nqhlzc64r1gwl-sl-5.05/bin/sl",
    ))
    .unwrap();
    assert_eq!(path.hash(), "hbqzhmrscihnl9vgvw9nqhlzc64r1gwl");
}

#[test]
fn test_store_path_relative() {
    let path = StorePath::new(Path::new(
        "/nix/store/hbqzhmrscihnl9vgvw9nqhlzc64r1gwl-sl-5.05/bin/sl",
    ))
    .unwrap();
    assert_eq!(path.relative(), Path::new("bin/sl"));
}
#[test]
fn test_store_path_relative_bare_path() {
    let path = StorePath::new(Path::new(
        "/nix/store/hbqzhmrscihnl9vgvw9nqhlzc64r1gwl-sl-5.05",
    ))
    .unwrap();
    assert_eq!(path.relative(), Path::new(""));
}

impl StorePath {
    /// To remove references, gcc is patched to replace the hash part
    /// of store path by an uppercase version in debug symbols.
    ///
    /// Store paths are embedded in debug symbols for example as the location
    /// of template instantiation from libraries that live in other derivations.
    ///
    /// This function undoes the mangling.
    pub fn demangle(self) -> StorePath {
        let mut as_bytes = self.0.into_os_string().into_vec();
        let len = as_bytes.len();
        let store_len = NIX_STORE.as_bytes().len();
        as_bytes[len.min(store_len + 1)..len.min(store_len + 1 + HASH_LEN)].make_ascii_lowercase();
        StorePath::new(OsStr::from_bytes(&as_bytes).as_ref()).unwrap()
    }
}

#[test]
fn test_demangle_nominal() {
    assert_eq!(
        StorePath::new(Path::new(
            "/nix/store/JW65XNML1FGF4BFGZGISZCK3LFJWXG6L-GCC-12.3.0/include/c++/12.3.0/bits/vector.tcc"
        )).unwrap().demangle(),
        StorePath::new(Path::new(
            "/nix/store/jw65xnml1fgf4bfgzgiszck3lfjwxg6l-GCC-12.3.0/include/c++/12.3.0/bits/vector.tcc"
        )).unwrap()
    );
}

#[test]
fn test_demangle_noop() {
    assert_eq!(
        StorePath::new(Path::new(
            "/nix/store/jw65xnml1fgf4bfgzgiszck3lfjwxg6l-gcc-12.3.0/include/c++/12.3.0/bits/vector.tcc"
        )).unwrap().demangle(),
        StorePath::new(Path::new(
            "/nix/store/jw65xnml1fgf4bfgzgiszck3lfjwxg6l-gcc-12.3.0/include/c++/12.3.0/bits/vector.tcc"
        )).unwrap()
    );
}

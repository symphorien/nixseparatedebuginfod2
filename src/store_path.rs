use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt as _},
    path::{Path, PathBuf},
};

const NIX_STORE: &str = "/nix/store";
const HASH_LEN: usize = 32;

#[derive(Debug)]
pub struct StorePath(PathBuf);

impl AsRef<Path> for StorePath {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

impl StorePath {
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

    pub fn name(&self) -> &OsStr {
        match self.0.components().nth(3) {
            Some(std::path::Component::Normal(name)) => name,
            _ => unreachable!(),
        }
    }

    pub fn hash(&self) -> &str {
        let os_hash = &self.name().as_bytes()[..HASH_LEN];
        std::str::from_utf8(os_hash).unwrap()
    }
}

#[test]
fn test_store_path_relative() {
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
/// To remove references, gcc is patched to replace the hash part
/// of store path by an uppercase version in debug symbols.
///
/// Store paths are embedded in debug symbols for example as the location
/// of template instantiation from libraries that live in other derivations.
///
/// This function undoes the mangling.
pub fn demangle(storepath: PathBuf) -> PathBuf {
    if !storepath.starts_with(NIX_STORE) {
        return storepath;
    }
    let mut as_bytes = storepath.into_os_string().into_vec();
    let len = as_bytes.len();
    let store_len = NIX_STORE.as_bytes().len();
    as_bytes[len.min(store_len + 1)..len.min(store_len + 1 + 32)].make_ascii_lowercase();
    OsString::from_vec(as_bytes).into()
}

#[test]
fn test_demangle_nominal() {
    assert_eq!(
        demangle(PathBuf::from(
            "/nix/store/JW65XNML1FGF4BFGZGISZCK3LFJWXG6L-GCC-12.3.0/include/c++/12.3.0/bits/vector.tcc"
        )),
        PathBuf::from(
            "/nix/store/jw65xnml1fgf4bfgzgiszck3lfjwxg6l-GCC-12.3.0/include/c++/12.3.0/bits/vector.tcc"
        )
    );
}

#[test]
fn test_demangle_noop() {
    assert_eq!(
        demangle(PathBuf::from(
            "/nix/store/jw65xnml1fgf4bfgzgiszck3lfjwxg6l-gcc-12.3.0/include/c++/12.3.0/bits/vector.tcc"
        )),
        PathBuf::from(
            "/nix/store/jw65xnml1fgf4bfgzgiszck3lfjwxg6l-gcc-12.3.0/include/c++/12.3.0/bits/vector.tcc"
        )
    );
}

#[test]
fn test_demangle_empty() {
    assert_eq!(demangle(PathBuf::from("/")), PathBuf::from("/"));
}

#[test]
fn test_demangle_incomplete() {
    assert_eq!(
        demangle(PathBuf::from("/nix/store/JW65XNML1FGF4B")),
        PathBuf::from("/nix/store/jw65xnml1fgf4b")
    );
}

#[test]
fn test_demangle_non_storepath() {
    assert_eq!(
        demangle(PathBuf::from("/build/src/FOO.C")),
        PathBuf::from("/build/src/FOO.C")
    );
}

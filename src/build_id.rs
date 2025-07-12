//! Parsing and utils about Build Ids

use std::{fmt::Display, ops::Deref};

/// A unique identifier for an elf executable or shared object.
///
/// The build id of an executable can be obtained as follows:
///
/// ```text
/// file -L /bin/sh
/// /bin/sh: ELF 64-bit LSB executable, x86-64, version 1 (SYSV), dynamically linked, interpreter /nix/store/maxa3xhmxggrc5v2vc0c3pjb79hjlkp9-glibc-2.40-66/lib/ld-linux-x86-64.so.2, BuildID[sha1]=094b9da7911246c32c8962fe4573d52165304991, for GNU/Linux 3.10.0, not stripped
/// ```
#[derive(Debug, Clone)]
pub struct BuildId(String);

impl BuildId {
    /// Parses a string into a build id
    ///
    /// Fails if the string is not composed of 40 hexadecimal characters.
    pub fn new(str: &str) -> anyhow::Result<Self> {
        if let Some(bad_char) = str.chars().find(|&c| !c.is_ascii_hexdigit()) {
            Err(anyhow::anyhow!(format!(
                "bad character {:?} in build_id",
                bad_char
            )))
        } else if str.len() != 40 {
            Err(anyhow::anyhow!(format!(
                "bad build_id length {}",
                str.len()
            )))
        } else {
            Ok(BuildId(str.into()))
        }
    }

    /// Returns the relative path in a debug output where files related to this build id should be
    /// located.
    pub fn in_debug_output(&self, extension: &str) -> String {
        format!(
            "lib/debug/.build-id/{}/{}.{}",
            &self.0[..2],
            &self.0[2..],
            extension
        )
    }
}

#[test]
fn test_build_id_ok() {
    let str = "483bd7f7229bdb06462222e1e353e4f37e15c293";
    let build_id = BuildId::new(str).unwrap();
    assert_eq!(
        build_id.in_debug_output("debug"),
        "lib/debug/.build-id/48/3bd7f7229bdb06462222e1e353e4f37e15c293.debug"
    );
}

#[test]
fn test_build_id_bad_char() {
    let str = "483bd7f72_9bdb06462222e1e353e4f37e15c293";
    BuildId::new(str).unwrap_err();
}

#[test]
fn test_build_id_short() {
    let str = "4";
    BuildId::new(str).unwrap_err();
}

impl Deref for BuildId {
    fn deref(&self) -> &Self::Target {
        &self.0
    }

    type Target = str;
}

impl Display for BuildId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

use std::{fmt::Display, ops::Deref};

#[derive(Debug)]
pub struct BuildId(String);

impl BuildId {
    pub fn new(str: &str) -> anyhow::Result<Self> {
        if let Some(bad_char) = str.chars().find(|&c| {
            !(('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z') || ('0' <= c && c <= '9'))
        }) {
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

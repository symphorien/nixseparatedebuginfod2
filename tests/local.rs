//! integration tests for the local: store
//!
//! they use bwrap to fake some store path being in /nix/store

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use assert_cmd::assert::OutputAssertExt;
use assert_cmd::cargo_bin;
use rand::Rng;
use tempfile::TempDir;

/// Path to the `tests/fixture` folder of the repo.
fn fixture(path: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path);
    assert!(path.exists());
    path
}

/// prepares a temporary directory containing the `file_binary_cache` binary cache expanded as a
/// chroot store
///
/// specifically, the store is at the `nix/store` subdir
fn prepare_store() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let binary_cache = format!(
        "file://{}",
        fixture("file_binary_cache")
            .canonicalize()
            .unwrap()
            .display()
    );
    Command::new("nix")
        .arg("copy")
        .arg("--store")
        .arg(root.path())
        .arg("--extra-experimental-features")
        .arg("nix-command")
        .arg("--from")
        .arg(binary_cache)
        .arg("--all")
        .arg("--no-check-sigs")
        .assert()
        .success();
    root
}

struct Server {
    process: Child,
    port: u16,
    cache: TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.process.id() as i32),
            nix::sys::signal::Signal::SIGINT,
        )
        .unwrap();
        if self.process.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        self.process.kill().unwrap();
        self.process.wait().unwrap();
    }
}

impl Server {
    /// runs a nixseparatedebuginfod2 serving this store on a random port
    ///
    /// store is a root filesystem whose subdir `nix/store` contains the store
    fn new(store: &Path) -> Server {
        let port = rand::rng().random_range(50_000u16..u16::MAX);
        let addr = format!("127.0.0.1:{port}");
        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir(cache.path().join("server")).unwrap();
        std::fs::create_dir(cache.path().join("client")).unwrap();
        let mut command = Command::new("bwrap");
        command.env("RUST_LOG", "nixseparatedebuginfod2=trace,tower_http=debug");
        command
            .args([
                "--die-with-parent",
                "--bind",
                "/",
                "/",
                "--overlay-src",
                "/nix/store",
                "--overlay-src",
            ])
            .arg(store.join("nix/store"))
            .args(["--ro-overlay", "/nix/store", "--"])
            .arg(cargo_bin!("nixseparatedebuginfod2"))
            .arg("--listen-address")
            .arg(&addr)
            .arg("--substituter")
            .arg("local:")
            .arg("--cache-dir")
            .arg(cache.path().join("server"))
            .arg("--expiration")
            .arg("1h");
        let child = command.spawn().unwrap();
        let mut result = Server {
            process: child,
            port,
            cache,
        };
        // wait for the server to start
        let mut i = 0;
        loop {
            if dbg!(reqwest::blocking::get(format!(
                "http://{addr}/non-existent"
            )))
            .is_ok()
            {
                break;
            }
            if let Some(status) = result.process.try_wait().unwrap() {
                panic!("{command:?} failed to spawn: {status:?}")
            }
            if i > 100 {
                panic!("timeout")
            }
            i += 1;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        result
    }

    fn request(&self, args: &[&str]) -> PathBuf {
        let mut cmd = Command::new("debuginfod-find");
        cmd.env("HOME", self.cache.path().join("client"));
        cmd.env("DEBUGINFOD_URLS", format!("http://127.0.0.1:{}", self.port));
        cmd.args(args);
        let result = cmd.assert().success();
        let out = &result.get_output().stdout;
        let path = PathBuf::from(OsStr::from_bytes(out.trim_ascii_end()));
        assert!(dbg!(&path).starts_with(self.cache.path()));
        assert!(path.exists());
        path
    }
}

/// Returns the sha256sum of this file in a lowecase hex string
pub fn file_sha256(file: &Path) -> String {
    let mut std_file = std::fs::File::open(file).unwrap();
    let mut buf = [0; 4096];
    let mut hash = hmac_sha256::Hash::new();
    loop {
        let n = std_file.read(&mut buf).unwrap();
        if n == 0 {
            break;
        } else {
            hash.update(&buf[..n]);
        }
    }
    let digest = hash.finalize();
    let mut result = String::new();
    for &byte in digest.iter() {
        result.push_str(&format!("{:0>2x}", byte))
    }
    result
}

#[test]
fn local_debuginfo_nominal() {
    let store = prepare_store();
    let server = Server::new(store.path());
    // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
    let debuginfo = server.request(&["debuginfo", "0e20481820d3b92468102b35a5e4a29a8695c1af"]);
    // /nix/store/dlkw5480vfxdi21rybli43ii782czp94-gnumake-4.4.1-debug/lib/debug/make
    assert_eq!(
        file_sha256(&debuginfo),
        "8f62cc563915e10f870bd7991ad88e535f842a8dd7afcba30c597b3bb6e728ad"
    );
}

#[test]
fn local_source_in_archive_patched() {
    let store = prepare_store();
    let server = Server::new(store.path());
    // /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
    let source = server.request(&[
        "source",
        "0e20481820d3b92468102b35a5e4a29a8695c1af",
        "/build/make-4.4.1/src/job.c",
    ]);
    // /nix/store/dlkw5480vfxdi21rybli43ii782czp94-gnumake-4.4.1-debug/src/overlay/make-4.4.1/src/job.c
    assert_eq!(
        file_sha256(dbg!(&source)),
        "65c819269ed09f81de1d1659efb76008f23bb748c805409f1ad5f782d15836df"
    );
}

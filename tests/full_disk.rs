//! integration tests for the feature of detecting ENOSPC and dropping the cache
//!
//! they use a tmpfs in a mount namespace to enforce size limits

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use assert_cmd::assert::OutputAssertExt;
use assert_cmd::cargo_bin;
use tempfile::TempDir;

/// Path to the `tests/fixture` folder of the repo.
fn fixture(path: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path);
    assert!(path.exists());
    path
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

fn with_cache_size(cache_dir: &Path, cache_size_inodes: usize) -> Command {
    let options = format!("size=100M,nr_inodes={cache_size_inodes}");
    let mount = shlex::try_join([
        "mount",
        "-t",
        "tmpfs",
        "-o",
        &options,
        "tmpfs",
        cache_dir.to_str().unwrap(),
    ])
    .unwrap();
    let mut cmd = Command::new("unshare");
    cmd.arg("-r")
        .arg("-m")
        .arg("sh")
        .arg("-exc")
        .arg(format!(r#"{mount}; exec "$@""#))
        .arg("wrapper");
    cmd
}

impl Server {
    /// runs a nixseparatedebuginfod2 serving this store on a random port
    fn new(cache_size_inodes: usize) -> Server {
        let port = port_check::free_local_ipv4_port().unwrap();
        let addr = format!("127.0.0.1:{port}");
        let cache = tempfile::tempdir().unwrap();
        let server_cache_dir = cache.path().join("server");
        std::fs::create_dir(&server_cache_dir).unwrap();
        std::fs::create_dir(cache.path().join("client")).unwrap();
        let mut check_command = with_cache_size(&server_cache_dir, cache_size_inodes);
        check_command.arg("true");
        if !check_command.status().unwrap().success() {
            // write to /dev/store to bypass cargo capturing
            std::fs::write(
                "/dev/stderr",
                format!(
                    r#"
                @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
                failed to run command `true` in a user namespace
                as {check_command:?}
                giving up on running the test
                @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
                "#
                ),
            )
            .unwrap();
            std::process::exit(0);
        };
        let mut command = with_cache_size(&server_cache_dir, cache_size_inodes);
        command.env("RUST_LOG", "nixseparatedebuginfod2=trace,tower_http=debug");
        command
            .arg(cargo_bin!("nixseparatedebuginfod2"))
            .arg("--listen-address")
            .arg(&addr)
            .arg("--substituter")
            .arg(format!(
                "file://{}",
                fixture("file_binary_cache").to_str().unwrap()
            ))
            .arg("--cache-dir")
            .arg(&server_cache_dir)
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

    fn run_debuginfod_find(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("debuginfod-find");
        cmd.env("HOME", self.cache.path().join("client"));
        cmd.env("DEBUGINFOD_URLS", format!("http://127.0.0.1:{}", self.port));
        cmd.args(args);
        cmd
    }

    /// returns the path output by debuginfod-find
    fn debuginfod_find_path(&self, args: &[&str]) -> PathBuf {
        let mut cmd = self.run_debuginfod_find(args);
        let result = cmd.assert().success();
        let out = &result.get_output().stdout;
        let path = PathBuf::from(OsStr::from_bytes(out.trim_ascii_end()));
        assert!(dbg!(&path).starts_with(self.cache.path()));
        assert!(path.exists());
        path
    }
}

// /nix/store/dlkw5480vfxdi21rybli43ii782czp94-gnumake-4.4.1-debug
const SMALL_DEBUG_OUTPUT_SIZE_INODES: usize = 34;
// /nix/store/34j18r2rpi7js1whmvzm9wliad55rilr-gnumake-4.4.1/bin/make
const SMALL_DEBUG_OUTPUT_BUILDID: &str = "0e20481820d3b92468102b35a5e4a29a8695c1af";
#[allow(unused)]
const SMALL_DEBUG_OUTPUT_SOURCE_INODES: usize = 416;
const SMALL_DEBUG_OUTPUT_SOURCE_REQUEST: &str = "/src/ar.c";
// /nix/store/80nn028rq690b6qk8qprkvfbln38crdx-systemd-minimal-257.6-debug
// on my system it is impossible to copy this store path to a tmpfs of size 26670000 but 26680000,
// despite its size according to du -s --bytes being 25587684
const BIG_DEBUG_OUTPUT_SIZE_INODES: usize = 928;
// /nix/store/pbqih0cmbc4xilscj36m80ardhg6kawp-systemd-minimal-257.6/lib/systemd/systemd-binfmt
const BIG_DEBUG_OUTPUT_BUILDID: &str = "455de628442731ead5357ea8b70bdcd14b8029a9";

// empty cache has 9 directories, plus 1 for the root of the fs, plus 5 for the test to pass, no
// idea why
const OVERHEAD: usize = 15;

#[test]
fn enospc_debuginfo() {
    let server =
        Server::new(BIG_DEBUG_OUTPUT_SIZE_INODES + SMALL_DEBUG_OUTPUT_SIZE_INODES / 2 + OVERHEAD);
    server.debuginfod_find_path(&["debuginfo", BIG_DEBUG_OUTPUT_BUILDID]);
    // fetching the small debug output cannot happen if we don't flush the cache
    server.debuginfod_find_path(&["debuginfo", SMALL_DEBUG_OUTPUT_BUILDID]);
}

#[test]
fn enospc_source() {
    let server =
        Server::new(BIG_DEBUG_OUTPUT_SIZE_INODES + SMALL_DEBUG_OUTPUT_SIZE_INODES + OVERHEAD);
    server.debuginfod_find_path(&["debuginfo", BIG_DEBUG_OUTPUT_BUILDID]);
    server.debuginfod_find_path(&["debuginfo", SMALL_DEBUG_OUTPUT_BUILDID]);
    // fetching the small debug source cannot happen if we don't flush the cache
    server.debuginfod_find_path(&[
        "source",
        SMALL_DEBUG_OUTPUT_BUILDID,
        SMALL_DEBUG_OUTPUT_SOURCE_REQUEST,
    ]);
}

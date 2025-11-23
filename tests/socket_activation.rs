use assert_cmd::cargo_bin;
use command_fds::{CommandFdExt, FdMapping};
use std::ffi::CStr;
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt as _;
use std::process::Command;

unsafe fn unlocked_setenv(key: &CStr, value: impl AsRef<CStr>) -> std::io::Result<()> {
    let r = nix::libc::setenv(key.as_ptr(), value.as_ref().as_ptr(), 1);
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    };
    Ok(())
}

/// Write this number in ascii to this buffer without allocating
fn itoa(n: u32, buf: &mut [u8]) -> std::io::Result<&CStr> {
    let mut cursor = &mut buf[..];
    write!(cursor, "{}\x00", n)?;
    CStr::from_bytes_until_nul(buf).map_err(|_| std::io::Error::other("no null byte in itoa"))
}

#[test]
fn socket_activation_on_two_ports() {
    let cache = tempfile::tempdir().unwrap();
    let listener1 = TcpListener::bind("127.0.0.1:0").unwrap();
    let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr1 = listener1.local_addr().unwrap();
    let addr2 = listener2.local_addr().unwrap();
    assert_ne!(addr1, addr2);
    let mut command = Command::new(cargo_bin!("nixseparatedebuginfod2"));
    command
        .arg("--substituter")
        .arg("local:")
        .arg("--cache-dir")
        .arg(cache.path().join("server"))
        .arg("--expiration")
        .arg("1h");
    command
        .fd_mappings(vec![
            FdMapping {
                parent_fd: listener1.as_fd().try_clone_to_owned().unwrap(),
                child_fd: 3,
            },
            FdMapping {
                parent_fd: listener2.as_fd().try_clone_to_owned().unwrap(),
                child_fd: 4,
            },
        ])
        .unwrap();
    unsafe {
        command.pre_exec(move || -> std::io::Result<()> {
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)?;
            // we only know the pid once fork has happened, so we need to set the environment now
            let buf = &mut [0; 20][..];
            let value = itoa(std::process::id(), buf)?;
            unlocked_setenv(c"LISTEN_PID", value)?;
            // unfortunately, if we also had used command.env(...) then our enviroment change of
            // LISTEN_PID would be ignored, so we need to also set LISTEN_FDS this way.
            unlocked_setenv(c"LISTEN_FDS", c"2")?;
            unlocked_setenv(
                c"RUST_LOG",
                c"nixseparatedebuginfod2=trace,tower_http=debug",
            )?;
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();

    // query the server on both ports to check that the implementation of socket activation is
    // complete
    for addr in [addr1, addr2] {
        dbg!(addr);
        assert_eq!(
            reqwest::blocking::get(format!(
                "http://{addr}/buildid/0000000000000000000000000000000000000000/debuginfo"
            ))
            .unwrap()
            .status(),
            404
        );
    }
    child.kill().unwrap();
    child.wait().unwrap();
}

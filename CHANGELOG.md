v2.0.0:

- breaking change: omitting `--listen-address` now means that systemd socket activation should be used.
- breaking change: when no substituter is specified, refuse to run instead of running a server that always returns 404.
- add support for being a Type=notify systemd service. Disable by disabling the `systemd` feature at compilation time.
- fix caching logic. It used to be the case that querying debuginfo for two libraries in the same derivation would fetch the corresponding debug output twice.
- it is now unnecessary to have the `nix-store` command on `PATH` at runtime.
- if the server encounters a "No space left on device" or "Disk quota exceeded"
  error while writing to its cache directory, it will attempt to shrink its
  cache before retrying.

v1.0.1:
- fix flaky test

v1.0.0:
- support several substituters at the same time

v0.1.0:
- initial release

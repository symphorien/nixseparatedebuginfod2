v2.0.0:

- breaking change: omitting `--listen-address` now means that systemd socket activation should be used.
- breaking change: when no substituter is specified, refuse to run instead of running a server that always returns 404.
- add support for being a Type=notify systemd service

v1.0.1:
- fix flaky test

v1.0.0:
- support several substituters at the same time

v0.1.0:
- initial release

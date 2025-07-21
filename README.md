# proof of concept debuginfod server for nix

Contrary to nixseparatedebuginfod, this works by relying on the indexation hydra does only.

It proxifies the debuginfo stored in a substituter, storing temporary data in a cache directory.

substituters must be created with the `index-debug-info` options:


```
nix copy ... --to file://...?index-debug-info=true
```

then you can run

```
cargo run -- --substituter file://... --expiration "1 day"
```
or
```
cargo run -- --substituter https://cache.nixos.org --expiration "1 day"
```
or
```
cargo run -- --substituter local: --expiration "1 day"
```

and set the environment variable `DEBUGINFOD_URLS=http://127.0.0.1:1949`.

### Warning

Does not check signatures from the upstream cache. Don't use `http` substituters, only `https`.

If you expose this server to the public, be aware that anybody can request
files from very big archives, and the server will unpack them on demand,
possibly leading to very large resource usage.

### License

The source is GPL v3 only

The directory `./tests/fixtures/file_binary_cache` contains compiled free software described in `./tests/fixtures/README.md` and has the license of the corresponding software.

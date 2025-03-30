# proof of concept debuginfod server for nix

Contrary to nixseparatedebuginfod, this works by relying on the indexation hydra does only.

It proxifies the debuginfo stored in a substituter, storing temporary data in a cache directory.
Currently only supports file:// substituters.

substituters must be created with the `index-debug-info` options:


```
nix copy ... --to file://...?index-debug-info=true
```

then you can run

```
cargo run -- --substituter file://...
```

and set the environment variable `DEBUGINFOD_URLS=http://127.0.0.1:1949`.


### License

The source is GPL v3 only

The directory `./tests/fixtures/file_binary_cache` contains compiled free software described in `./tests/fixtures/README.md` and has the license of the corresponding software.

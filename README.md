# Debuginfod server for Nixpkgs

nixseparatedebuginfod2 is the successor of nixseparatedebuginfod.
Contrary to nixseparatedebuginfod, this one works by relying on the indexation hydra does only.
It proxifies the debuginfo stored in a substituter, storing temporary data in a cache directory.

substituters must be created with the `index-debug-info` options:


```
nix copy ... --to file://...?index-debug-info=true
```

then you can run

```
cargo run -- --listen-address 127.0.0.1:1949 --substituter file://... --expiration "1 day"
```
or
```
cargo run -- --listen-address 127.0.0.1:1949 --substituter https://cache.nixos.org --expiration "1 day"
```
or
```
cargo run -- --listen-address 127.0.0.1:1949 --substituter local: --expiration "1 day"
```

and set the environment variable `DEBUGINFOD_URLS=http://127.0.0.1:1949`.

### Source files

nixseparatedebuginfod2 can provide source files for packages built from nixos-25.11 (nixos-unstable at the time I write this) only.
Package built with older stdenv will only provide debuginfo. Source files which
are patched during the build should be served patched correctly in most cases.

### Using the NixOS module

`nixseparatedebuginfod2` is available on NixOS starting with version 25.11.

[![Packaging status](https://repology.org/badge/vertical-allrepos/nixseparatedebuginfod2.svg)](https://repology.org/project/nixseparatedebuginfod2/versions)

If you are not using custom substituters, then this configuration in `/etc/nixos/configuration.nix` should be enough:
```nix
{ config, pkgs, lib, ... }: {
    config = {
      /* rest of your config */
      services.nixseparatedebuginfod2.enable = true;
    };
}
```

### Migration from nixseparatedebuginfod

If you only use the default binary cache then this invocation is a drop-in replacement:
```
nixseparatedebuginfod2 --listen-address 127.0.0.1:1949 --substituter local: --substituter https://cache.nixos.org --expiration "1 day"
```
or on NixOS >=25.11:
```nix
{ config, pkgs, lib, ... }: {
    config = {
      /* rest of your config */
      services.nixseparatedebuginfod2.enable = true;
    };
}
```

If you use other http caches, add them to `--substituter` on the CLI or in the `services.nixseparatedebuginfod2.substituters` NixOS option. If you use ssh substituters, then nixseparatedebuginfod2 cannot handle them directly. Consider running nixseparatedebuginfod2 on the substituters themselves.

### Warning

Does not check signatures from the upstream cache. Don't use `http` substituters, only `https`.

If you expose this server to the public, be aware that anybody can request
files from very big archives, and the server will unpack them on demand,
possibly leading to very large resource usage.

If you point nixseparatedebuginfod2 to the local store (`--substituter local:`)
it will happily serve any file in your store. Of course you don't have secrets
in your store, do you?

### License

The source is GPL v3 only

The directory `./tests/fixtures/file_binary_cache` contains compiled free software described in `./tests/fixtures/README.md` and has the license of the corresponding software.

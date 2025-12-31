# Debuginfod server for Nixpkgs

Downloads and provides debug symbols and source code for nix derivations to `gdb` and other `debuginfod`-capable debuggers as needed.

## Overview

Most software in `nixpkgs` is stripped, so hard to debug. But some key packages are built with `separateDebugInfo = true`: debug symbols are put in a separate output `debug` which is not downloaded by default (and that's for the best, debug symbols can be huge). But when you do need the debug symbols, for example for `gdb`, you need to download this `debug` output and point `gdb` to it. This can be done manually, but is quite cumbersome. `nixseparatedebuginfod2` does that for you on the fly, for separate debug outputs and even for the source!

## Setup

[![Packaging status](https://repology.org/badge/vertical-allrepos/nixseparatedebuginfod2.svg)](https://repology.org/project/nixseparatedebuginfod2/versions)

### On NixOS

A NixOS module has been present in NixOS upstream since 25.11.

If you are not using custom substituters, then this configuration in `/etc/nixos/configuration.nix` should be enough:
```nix
{ config, pkgs, lib, ... }: {
    config = {
      /* rest of your config */
      services.nixseparatedebuginfod2.enable = true;
    };
}
```

As the module sets the environment variable `DEBUGINFOD_URLS` you might have to log out and login again.

### Running manually

```
cargo run -- --listen-address 127.0.0.1:1949 --substituter local: --substituter https://cache.nixos.org --expiration "1 day"
```

and set the environment variable `DEBUGINFOD_URLS=http://127.0.0.1:1949`.

#### `gdb`
In `~/.gdbinit` put
```
set debuginfod enabled on
```
otherwise, it will ask for confirmation every time.

#### `valgrind`

`valgrind` needs `debuginfod-find` on `$PATH` to use `nixseparatedebuginfod2`.
Add `(lib.getBin pkgs.elfutils)` to `environment.systemPackages` or `home.packages`.

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

### Checking that it all works

For example, `gnumake` is compiled with `separateDebugInfo = true` as of NixOS 25.11:
```
$  gdb $(command -v make)                                                                              
GNU gdb (GDB) 16.3
Copyright (C) 2024 Free Software Foundation, Inc.
License GPLv3+: GNU GPL version 3 or later <http://gnu.org/licenses/gpl.html>
This is free software: you are free to change and redistribute it.
There is NO WARRANTY, to the extent permitted by law.
Type "show copying" and "show warranty" for details.
This GDB was configured as "x86_64-unknown-linux-gnu".
Type "show configuration" for configuration details.
For bug reporting instructions, please see:
<https://www.gnu.org/software/gdb/bugs/>.
Find the GDB manual and other documentation resources online at:
    <http://www.gnu.org/software/gdb/documentation/>.

For help, type "help".
Type "apropos word" to search for commands related to "word"...
Reading symbols from /home/symphorien/.nix-profile/bin/make...
Downloading 379.82 K separate debug info for /nix/store/jlv0a6iyh8vb7pfyhjj93xy245xlmgh5-gnumake-4.4.1/bin/make
Reading symbols from /home/symphorien/.cache/debuginfod_client/5ba4a279aeaa0f717a07b1b5298cbdef3210ca4e/debuginfo...                                                                                                                                                                                                                                                                         
(gdb) start
Downloading 118.28 K source file /build/make-4.4.1/src/main.c
Temporary breakpoint 1 at 0xb940: file src/main.c, line 1174.                                                                                                                                                                                                                                                                                                                                
Starting program: /nix/store/j6yb06v6xcz70wg1f9ivpp9c9kw4146l-home-manager-path/bin/make 
Downloading 547.93 K separate debug info for /nix/store/xx7cm72qy2c0643cm1ipngd87aqwkcdp-glibc-2.40-66/lib/ld-linux-x86-64.so.2
Downloading 4.13 M separate debug info for /nix/store/xx7cm72qy2c0643cm1ipngd87aqwkcdp-glibc-2.40-66/lib/libc.so.6                                                                                                                                                                                                                                                                           
[Thread debugging using libthread_db enabled]                                                                                                                                                                                                                                                                                                                                                
Using host libthread_db library "/nix/store/xx7cm72qy2c0643cm1ipngd87aqwkcdp-glibc-2.40-66/lib/libthread_db.so.1".

Temporary breakpoint 1, main (argc=1, argv=0x7fffffffc9a8, envp=0x7fffffffc9b8) at src/main.c:1174
1174	{
(gdb) l
1169	main (int argc, char **argv)
1170	#else
1171	int
1172	main (int argc, char **argv, char **envp)
1173	#endif
1174	{
1175	  int makefile_status = MAKE_SUCCESS;
1176	  struct goaldep *read_files;
1177	  PATH_VAR (current_directory);
1178	  unsigned int restarts = 0;
(gdb)
```

## Capabilities

### Supported substituters

`nixseparatedebuginfod2` supports using debug info present in:
- the local store with the special value `local:`
- any `file://` or `https://` (or `http://` but insecure!) substituter created with the `index-debug-info` option set:

```
nix copy ... --to file://...?index-debug-info=true
```
This is the case of the official binary cache, `https://cache.nixos.org`.

By default the NixOS module only uses the local store and official binary cache; if you use other ones, you must add them to the `services.nixseparatedebuginfod2.substituters`.

### Source files

`nixseparatedebuginfod2` can provide source files for packages built from nixpkgs-25.11 or later only.
Package built with older stdenv will only provide debuginfo. Source files which
are patched during the build should be served patched correctly in most cases.

## Migration from nixseparatedebuginfod
`nixseparatedebuginfod2` is the spiritual successor of `nixseparatedebuginfod`, but is built on a completely different principle.
Contrary to `nixseparatedebuginfod`, this one works by relying on the indexation hydra does only.
It proxifies the debuginfo stored in a substituter, storing temporary data in a cache directory.

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

If you use other http caches, add them to `--substituter` on the CLI or in the `services.nixseparatedebuginfod2.substituters` NixOS option. If you use ssh substituters, then `nixseparatedebuginfod2` cannot handle them directly. Consider running nixseparatedebuginfod2 on the substituters themselves and adding the corresponding url to `environment.debuginfodServers` on the client machine.


## Warning

Does not check signatures from the upstream cache. Don't use `http` substituters, only `https`.

If you expose this server to the public, be aware that anybody can request
files from very big archives, and the server will unpack them on demand,
possibly leading to very large resource usage.

If you point nixseparatedebuginfod2 to the local store (`--substituter local:`)
it will happily serve any file in your store. Of course, you don't have secrets
in your store, do you?

## License

The source is GPL v3 only.

The directory `./tests/fixtures/file_binary_cache` contains compiled free software described in `./tests/fixtures/README.md` and has the license of the corresponding software.

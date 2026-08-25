---
title: Making packages
description: Three ways to author ply packages — deb2pkg conversion, ply craft sessions, and plain directories.
section: Guides
order: 16
---

# Making packages

A ply package is an inert image: files under its own prefix plus a tiny
manifest describing its `PATH`/`LD_LIBRARY_PATH` contributions. No install
scripts — ever. That makes packages easy to mint.

## deb2pkg — convert Debian packages

The `deb2pkg` tool converts a Debian package (plus its runtime library
closure) into a single self-contained ply package:

```sh
deb2pkg redis-server --name redis -o ./out
# → out/redis-8.0.2-linux-x64.img
```

Debian's `Depends` graph is too coarse to vendor (postgres would drag in
perl), so deb2pkg reads the **binaries** instead: it unpacks the package,
walks every ELF's `DT_NEEDED`, maps each soname to its owning package via
the Contents index, and recurses — vendoring exactly the libraries the code
loads into one keg (`/opt/redis-8.0.2/`). Maintainer scripts are never
read: no install hooks, ever, by construction.

Three flags cover what scripts would have done:

```sh
deb2pkg postgresql-17 --name postgresql17 --skip-so llvmjit.so   # drop a dlopen plugin (–150 MiB of LLVM)
deb2pkg nodejs --with node-cjs-module-lexer                      # vendor non-ELF runtime data the walk can't see
deb2pkg python3.13 --name python3 --symlink python3=python3.13   # symlinks update-alternatives would have made
```

`--arch arm64` converts for the other arch from any host — conversion only
unpacks files. Names and versions normalize to ply's grammar
(`postgresql-17` → `postgresql17`, epochs stripped: `5:8.0.2-2` → `8.0.2`).

This is the machinery behind the [official registry](https://registry.plybox.sh)
— mainstream packages pre-converted from Debian trixie (glibc, so npm
prebuilts, pip wheels, and JNI libraries work untouched), served from a
CDN. Most of the time you don't run deb2pkg at all. The earlier Alpine/musl
catalog (`apk2pkg`) is frozen: still served, no longer grown.

## ply craft — author interactively

For anything that isn't an Alpine package, `craft` turns a shell session
into a package. The overlay upper layer *is* the layer:

```sh
ply craft new --base debian@13 mytools      # opens a shell on the base
# …inside: install things, copy files, configure…
ply craft changes mytools                   # what did the session add?
ply craft commit mytools --version 0.1.0    # → mytools-0.1.0-linux-x64.img
```

Sessions persist between shells (`ply craft shell`), can be listed
(`ply craft ls`), discarded (`ply craft rm`), and — because a committed
package is just an image — resumed anywhere from the artifact itself
(`ply craft edit`). The result is a normal, inert, content-addressed
package.

## Plain directories

The lowest-tech path: an app-layer dependency can be a static binary you
vendor straight into your app directory before `ply build`. What npm's
`ffmpeg-static` does today needs zero ply support.

## Publishing

However a package is made, publishing is copying a file — see
[Registries & publishing](/docs/registries/). Upload to GitHub Releases,
any bucket, or a directory, add an `index.json` if you want range
resolution, done.

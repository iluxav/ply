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

For anything that isn't a Debian package, `craft` turns a shell session
into a package. The overlay upper layer *is* the layer:

```sh
sudo ply craft new --from debian@13 mytools  # opens a shell on the base
# …inside: apt-get install things, copy files, configure…
sudo ply craft changes mytools               # what did the session add?
sudo ply craft commit mytools --version 0.1.0  # → mytools-0.1.0-linux-x64.img
```

It needs root (it mounts an overlay) and the base comes from the official
registry unless `--source` says otherwise.

`commit` leaves two kinds of thing out, and says so. What a package
manager regenerates — apt's or apk's package lists and download caches —
is reported as one total, because it is usually most of the weight: an
`apt-get install jq` session packs to about half a megabyte instead of
sixteen. The session's own records — anything under `/tmp`, the package
manager's logs and lock files, the shell history — are reported **by
name**, because a tool you unpacked into `/tmp` and meant to keep would
otherwise vanish quietly; move it somewhere else and commit again. The
dpkg database itself ships, so a session resumed from the image with
`craft edit` still knows what is installed; it just needs `apt-get update`
before installing something new, exactly as a Dockerfile does.

### `[layer]` — what a keg adds to the apps that depend on it

A keg can contribute to the environment of every app that depends on it:

```toml
[layer]
path = ["/opt/ruby-3.3.8/usr/bin"]
ld_library_path = ["/opt/ruby-3.3.8/usr/lib/aarch64-linux-gnu"]
env = { RUBYLIB = "/opt/ruby-3.3.8/usr/lib/ruby/3.3.0" }
```

`path` and `ld_library_path` are joined across the dependency closure;
`env` sets plain variables, base first so a dependent's value wins, and
the app's own `[env]` wins over all of them. This is how the registry's
`ruby` finds its standard library from a relocated prefix without every
app spelling out `RUBYLIB`.

`env` arrived in ply 0.1.81, and the check is strict on both sides: a
**host** running an older ply refuses a keg that carries it when the app
starts — not when the image is built, because building never reads a
dependency's layer. If you build on a current laptop against such a keg
and ship the image to a server, update the server first.

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

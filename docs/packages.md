---
title: Making packages
description: Three ways to author ply packages — apk2pkg conversion, ply craft sessions, and plain directories.
section: Guides
order: 16
---

# Making packages

A ply package is an inert image: files under its own prefix plus a tiny
manifest describing its `PATH`/`LD_LIBRARY_PATH` contributions. No install
scripts — ever. That makes packages easy to mint.

## apk2pkg — convert Alpine packages

The `apk2pkg` tool converts any Alpine package (plus its dependency
closure) into a single self-contained ply package:

```sh
apk2pkg ffmpeg --alpine 3.20 -o ./out
# → out/ffmpeg-6.1.1-linux-x64.img
```

It downloads the apk and everything it needs, vendors the closure into one
keg (`/opt/ffmpeg-6.1.1/`), resolves Alpine's virtual `provides`
(so things like ICU data land where binaries expect them), and emits a
canonical, content-addressed image.

Names and versions are normalized to ply's grammar: `g++` → `gpp`,
`_` → `-`, versions padded to `x.y.z`.

This is the machinery behind the [official registry](https://registry.plybox.sh)
— mainstream Alpine packages, pre-converted and served from a CDN, so most
of the time you don't run apk2pkg at all.

## ply craft — author interactively

For anything that isn't an Alpine package, `craft` turns a shell session
into a package. The overlay upper layer *is* the layer:

```sh
ply craft new --base alpine@3.20 mytools    # opens a shell on the base
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

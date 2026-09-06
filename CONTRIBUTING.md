# Contributing to ply

Thanks for looking. ply is pre-1.0 and small enough that one careful report
or change makes a visible difference.

## The most useful contribution

Try ply on an application you already run, and say what happened. Open an
issue with the **feedback** template: the app, what you ran, where it got
awkward or stopped, and whether you kept using ply. Successful deployments
are as useful as failures — they tell us what to keep.

For a defect, use the **bug report** template. `ply --version`, the OS and
architecture, rootless or rootful, the exact commands, and what you expected
versus what you saw. `ply why APP` and `ply logs APP` usually hold the
evidence; strip credentials and private data before pasting manifests or
logs.

## Development setup

You need a Linux machine (x86_64 or arm64) and stable Rust; the repository
is built with the current stable toolchain. Clone, then:

```sh
cargo build --workspace         # debug build of every crate
make check                      # fmt --check, clippy -D warnings, cargo test
make static                     # the release binary, statically linked (musl)
make install                    # → /usr/local/bin/ply (asks for sudo)
```

`make check` is what CI runs on Linux; a change is ready when it passes with
no warnings. CI also compiles the workspace for `aarch64-apple-darwin` to
keep the platform seam honest. You can run that gate locally with
`make check-darwin` if you have `cargo-zigbuild` and `zig`; otherwise let CI
do it.

To run apps rootless on your own machine, run `sudo ply setup` once. On
Ubuntu 24.04 and later it installs an AppArmor profile that grants user
namespaces to the **installed** binary by path, so rootless runs must use
`/usr/local/bin/ply`, not `target/release/ply`. Rootful runs (`sudo ply run …`)
work from either.

Layout, briefly: `ply-core` is the library (manifest, resolver, image
format, runtime, policies); `ply-cli` is the binary; `docs/` is the site
documentation; `bench/` holds the benchmark and live-check scripts;
`examples/` holds runnable examples; `.github/` holds CI and the release
workflow.

## Making a change

1. For anything larger than a fix, open an issue first and say what you
   intend. It saves both of us a rewrite.
2. Keep the change focused. One reason per pull request.
3. Add or adjust tests. Test names in this repository are sentences that
   state the rule being protected (`a_hair_over_target_is_not_a_scale_up`);
   follow that style. Pure logic lives in functions that need no root and no
   network to test.
4. Comments explain *why*, not what. Error messages name the remedy.
5. Runtime changes need a live check on a real Linux host, rootless and
   rootful where both apply. Describe in the pull request what you ran and
   what you saw; `bench/` has scripts that show the shape.
6. `make check` must pass. CI runs the same checks on the pull request.

In the pull request description, state the reason for the change and how you
verified it. That is the review's starting point.

## Releases

Maintainers cut releases with `make release-cli`. It refuses a dirty tree or
a branch other than `main`, runs the checks, bumps the version, tags, and
the release workflow builds both Linux binaries and publishes the GitHub
release with them attached.

Every release carries a short, human-written entry in [`CHANGELOG.md`](CHANGELOG.md):
what changed, fixes, breaking changes, and known limitations. Write it under
`## Unreleased` as you go; `make release-cli` turns that heading into the
version and date, and the release workflow uses the entry as the release
notes. A release without an entry is refused.

## Security

If you believe you have found a security problem, do not open a public
issue with the details. Contact the maintainer through the GitHub profile
listed on the repository, with enough to reproduce, and allow time for a fix
before disclosure.

## License

By contributing you agree that your contribution is licensed under the
repository's [MIT license](LICENSE).

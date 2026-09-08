# hello-bun

The [hello-http](../hello-http) example in Bun: one file served by
`Bun.serve`, packaged with an explicit base system and the `bun` runtime
from the registry.

```sh
ply build .
ply run --publish 127.0.0.1:8080:3000 hello-bun-0.1.0-linux-*.img
curl http://127.0.0.1:8080        # in another terminal on the same host
# Hello from Bun on Ply!
```

`ply build` resolves `bun = "1.4"` and `debian@13` from the public registry,
writes `ply.lock` with their versions and content hashes, and produces a
4 KiB image holding `index.ts`, `index.html` and the lock; the Bun binary
lives in its own keg, fetched once and shared by every app that uses it.
`index.ts` reads `PORT` because rootless runs may hand the app a port;
otherwise it listens on 3000.

The manifest is also what `ply run .` infers for a directory that holds a
`bun.lock` and no `ply.toml`, so this example is the written-down form of
the zero-config run.

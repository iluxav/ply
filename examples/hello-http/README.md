# hello-http

The example from the [README](../../README.md): one HTML file served by a
ten-line Node server, packaged with an explicit base system and runtime.

```sh
ply build .
ply run --publish 127.0.0.1:8080:8000 hello-0.1.0-linux-*.img
curl http://127.0.0.1:8080        # in another terminal on the same host
```

`ply build` resolves `node = "22"` and `debian@13` from the public registry,
writes `ply.lock` with their versions and content hashes, and produces a
4 KiB image holding `server.js`, `index.html` and the lock. The dependencies
are fetched once into the host's store and shared by every app that uses
them. `server.js` reads `PORT` because rootless runs may hand the app a port;
otherwise it listens on 8000.

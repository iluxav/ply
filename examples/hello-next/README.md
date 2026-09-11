# hello-next

A minimal [Next.js](https://nextjs.org) app packaged as a single ply image.
Next needs a build step, so unlike [hello-http](../hello-http) you run your
own toolchain first (`next build`) and ply packages the result — ply has no
Dockerfile and no build cache; it assumes your build produced the files.

## Build and run

```sh
npm install
npm run build                                    # next build, then copies static into standalone

ply build .                                      # → hello-next-0.1.0-linux-<arch>.img (~4 MiB)
ply run --publish 127.0.0.1:8080:3000 hello-next-0.1.0-linux-*.img
curl http://127.0.0.1:8080                       # in another terminal on the same host
# <!DOCTYPE html> … Hello from Next.js on ply
```

## Why standalone, and the copy step

`next.config.js` sets `output: "standalone"`, so `next build` emits a
self-contained server at `.next/standalone/server.js` with only the
`node_modules` Next actually traced — that is the small artifact ply ships.
The standalone server `chdir`s to its own directory and serves assets from
`.next/standalone/.next/static`, which Next does not copy for you — the same
step every Next.js container image does. Here the `postbuild` script in
`package.json` does it, so `npm run build` leaves a complete tree; `ply build`
then packages `.next/standalone/` per the `include` in `ply.toml`.

`node` and `debian@13` are resolved from the public registry, written to
`ply.lock` with their content hashes, and kept as shared kegs — so the image
holds only your built app, not a copy of Node. That is why a whole Next.js
app lands in a few MiB.

`HOSTNAME=0.0.0.0` in `[env]` matters: Next's standalone server otherwise
binds `localhost`, which nothing outside the instance could reach.

## Building on a small box

The build above can run on the same small server you deploy to. ply keeps
nothing resident, so a 512 MB droplet has its whole memory budget free to
build; for a memory-hungry `next build`, `sudo ply setup --swap 2G` lets the
memory-fenced builder spill to swap instead of OOM-killing the build or
evicting an app you are already serving. See the
[deployments guide](https://plybox.sh/docs/deployments/) for a build-on-host
deployment file (`repo = …` + `build = "npm ci && npm run build"`).

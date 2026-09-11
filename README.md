# ply

**A daemonless container runtime and package manager for Linux.**

*Not the [BPF tracer](https://github.com/iovisor/ply) or the [lex-yacc library](https://github.com/dabeaz/ply).* This **ply** packages your app with explicit, versioned dependencies, resolves them into one deterministic image, and runs it as a foreground process — no daemon, no Dockerfile, no registry to operate.

**How it isolates.** Not a wrapper around Docker or runc: `ply run` forks straight into its own user, mount, PID, network, UTS and IPC namespaces, pivots root with `pivot_root`, applies a seccomp filter, drops every Linux capability (a native package keeps none; imported OCI images get Docker's default set), and sets `no_new_privs`. Rootful runs additionally bound each instance with a cgroup v2 slice for memory and CPU; rootless enters the user namespace first, so none of this needs root (and skips cgroup limits).

[Website](https://plybox.sh/) · [Documentation](https://plybox.sh/docs/) · [Releases](https://github.com/iluxav/ply/releases) · [Report an issue](https://github.com/iluxav/ply/issues)

**Status:** pre-1.0. The CLI and image format may change. Linux x86_64 and arm64 are the primary targets; the native Apple Silicon backend is experimental — the same installer installs it, each instance runs in its own microVM. See the [macOS guide](https://plybox.sh/docs/macos/) for what it does not do yet.

## Quickstart

```sh
curl -fsSL https://plybox.sh/install.sh | sh   # one small binary, no daemon
cd my-node-or-python-app
ply init                                        # detects the project, writes ply.toml
ply build .                                      # → one deterministic .img + ply.lock
ply run --publish 8080 myapp-1.0.0-linux-x64.img # foreground; Ctrl-C stops it
```

Nothing is resident between deploys. The full walkthrough — with a manifest you can copy — is [below](#try-it-on-linux).

## Why ply?

Ply is designed for developers deploying applications to individual Linux hosts who want explicit dependencies and a small runtime they can operate with familiar shell tools.

- **Named dependencies.** Declare your base system and runtimes in `ply.toml`; `ply.lock` records resolved versions and content hashes.
- **Shared packages.** Applications reference dependency packages stored separately on the host. Apps using the same package content can reuse it.
- **File-based distribution.** Copy an application image with `scp` or serve packages over HTTP. Missing dependencies are fetched from the configured sources and checked against their locked hashes.
- **Foreground execution.** Run without a central Ply daemon. Logs go to your terminal, signals reach the application, and exit codes propagate.

## Dockerfile → ply.toml

A typical multi-stage Node Dockerfile:

```dockerfile
FROM node:22 AS build
WORKDIR /app
COPY package*.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM node:22-slim
COPY --from=build /app/dist ./dist
COPY --from=build /app/node_modules ./node_modules
CMD ["node", "server.js"]
```

The same app in ply — no build steps, no layers, no `build-essential` baked into the image:

```toml
[package]
name = "web"
version = "1.0.0"
entrypoint = ["node", "server.js"]
include = ["dist/", "package.json"]
base = "debian@13"

[dependencies]
node = "22"
```

`node` is a content-hashed package resolved once and shared by every app on the host; the image ships only your files, and `ply.lock` pins the closure so a rebuild is byte-identical.

## Builds on a 512 MB box

Docker keeps a daemon resident; a typical self-hosted PaaS wants ~2 GB before you deploy anything. Ply keeps nothing running between deploys — a small binary that exits — so a 512 MB droplet has its whole memory budget free to *build*.

For memory-hungry JS builds, `sudo ply setup --swap 2G` gives the memory-fenced builder somewhere to spill: it stays inside a cgroup and spills to swap rather than OOM-killing the build or evicting the app you are already serving. The same small box builds and runs, with no registry hop.

A complete, runnable Next.js app — building to a ~4 MiB image — is in [`examples/hello-next`](examples/hello-next).

## Try it on Linux

Install Ply:

```sh
curl -fsSL https://plybox.sh/install.sh | sh
```

The installer uses `~/.local/bin` for a regular user or `/usr/local/bin` when run as root or with sudo. Follow its PATH instructions. If it requests host preparation, run `sudo ply setup` once; this prepares host facilities needed by the runtime, including networking and storage, and prints a short to-do list for rootless use on this host, such as installing `passt` so rootless apps have outbound network. The example below needs none of the to-dos.

Create a directory containing a page to serve:

```sh
mkdir ply-hello
cd ply-hello
printf 'Hello from Ply!\n' > index.html
```

Save this as `server.js` in the same directory:

```js
const http = require("http");
const fs = require("fs");

const port = Number(process.env.PORT) || 8000;
http
  .createServer((req, res) => {
    res.setHeader("Content-Type", "text/plain; charset=utf-8");
    res.end(fs.readFileSync("index.html"));
  })
  .listen(port, "0.0.0.0", () => console.log(`hello-http listening on ${port}`));
```

And this as `ply.toml`:

```toml
[package]
name = "hello"
version = "0.1.0"
base = "debian@13"
entrypoint = ["node", "server.js"]
include = ["server.js", "index.html"]

[dependencies]
node = "22"

[ports]
web = 8000

[sources]
default = "https://registry.plybox.sh/ply/{package}"
```

The same three files are in [`examples/hello-http`](examples/hello-http).

Build and run it:

```sh
ply build .
ply run --publish 127.0.0.1:8080:8000 hello-0.1.0-linux-*.img
```

Open [localhost:8080](http://localhost:8080), or run this in another terminal on the same machine:

```sh
curl http://127.0.0.1:8080
# Hello from Ply!
```

The example serves one file with Node's built-in `http` module and publishes it on the host's loopback address. If 8080 is already taken on your machine, ply says so; pick another host port, for example `127.0.0.1:8081:8000`, and use it in the `curl` command too. If you are working over SSH, run the `curl` command on that Linux host.

Press **Ctrl-C** in the original terminal to stop it. Edit `index.html`, then repeat the build and run commands to serve your updated package. Downloaded dependencies remain cached for reuse.

## What gets shipped?

`ply build` produces an application image and a lockfile. The image contains your application files and locked dependency references. The base system and the Node.js runtime remain separate packages.

On another Linux host with Ply installed and set up, copy and run the image:

```sh
scp hello-0.1.0-linux-*.img server:
ssh -t server 'ply run --publish 127.0.0.1:8080:8000 hello-0.1.0-linux-*.img'
```

Replace `server` with your SSH destination. Build for the destination's CPU architecture; an x86_64 image needs an x86_64 host, and an arm64 image needs an arm64 host.

The destination fetches any missing dependencies from the image's configured sources. An application image's size therefore excludes those dependency downloads. Use [`ply bundle`](https://plybox.sh/docs/cli/) when you need an artifact with dependencies included for offline use.

For services that should survive logout and reboot, follow the [supervision guide](https://plybox.sh/docs/running/#supervision).

## How it fits alongside Docker and Podman

Ply uses its own package and image model, with named dependencies composed at runtime. It targets deployment on individual hosts.

Docker and Podman provide established OCI workflows and a much larger ecosystem. [Docker supports distributing images as files](https://docs.docker.com/reference/cli/docker/image/save/), and [Podman runs without a central daemon](https://docs.podman.io/en/latest/). Ply's main distinction is its package composition model.

Ply can [import Docker/OCI images](https://plybox.sh/docs/docker/) as flattened snapshots. Importing an image does not turn its contents into separately versioned Ply dependencies. Compatibility varies by image; see the documented tested images and limitations.

If your workflow depends on Kubernetes, Docker Compose files, Dev Containers, or Testcontainers, account for that tooling before considering a migration.

Read the [architecture](https://plybox.sh/docs/architecture/) and [tradeoffs](https://plybox.sh/docs/ply-vs-docker/) for more detail.

## Next steps

- [Run databases and services](https://plybox.sh/docs/services/)
- [Run a stack of applications](https://plybox.sh/docs/stacks/)
- [Configure volumes and persistent data](https://plybox.sh/docs/volumes/)
- [Deploy updates with health checks](https://plybox.sh/docs/deploy/)
- [Understand rootless operation and security](https://plybox.sh/docs/security/)
- [Use Ply with a coding agent](https://plybox.sh/docs/agents/)

## Feedback and contributions

Trying Ply on an existing application is a useful way to contribute. [Open an issue](https://github.com/iluxav/ply/issues/new) with what you tried, what happened, and what you expected. Reports of successful deployments are welcome too: describe the app and whether Ply made your workflow easier.

For bugs, include your Ply version, OS and architecture, whether the run was rootless or rootful, and a minimal reproduction. Remove credentials and private data from manifests and logs before sharing them.

For code changes, include the reason for the change and how you verified it. Run the repository checks from the project root:

```sh
make check
```

## License

[MIT](https://github.com/iluxav/ply/blob/main/LICENSE).

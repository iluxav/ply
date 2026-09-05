use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// ply — npm for containers.
///
/// Your app is a package, its OS-level dependencies are packages, an image is
/// a resolved lockfile, and the runtime mounts the closure and execs. One
/// static binary, no daemon, no registry, zero config. An image is a single
/// file you can scp around.
#[derive(Parser)]
#[command(name = "ply", version, about, long_about = None, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Write a starter ply.toml (npm init for containers)
    ///
    /// Detects Node/Python projects for defaults, asks a few questions
    /// (Enter accepts the default), and writes the manifest the quickstart shows.
    Init(InitArgs),

    /// Build an image from a directory containing ply.toml
    ///
    /// Resolves dependencies (writing ply.lock) and produces a deterministic
    /// squashfs image named <name>-<version>-<os>-<arch>.img.
    Build(BuildArgs),

    /// Search a source's package catalog (cargo search for containers)
    ///
    /// Reads `state.json` at the source prefix: --source, else the
    /// `[sources] default` of ./ply.toml, else the official registry.
    Search(SearchArgs),

    /// Add a dependency to ./ply.toml (cargo add for containers)
    ///
    /// `ply add ffmpeg` takes the latest major.minor from the source's
    /// catalog; `ply add ffmpeg@6.0` writes that range without a lookup.
    Add(AddArgs),

    /// Run an image (foreground; SIGTERM works; exit code propagates)
    ///
    /// Boxed: `run` is the flag-richest verb by a wide margin, and an
    /// inline `RunArgs` makes every other variant of this enum as big as
    /// it (clippy::large_enum_variant). Boxing it costs one allocation per
    /// process and clap's own `Args for Box<T>` handles the rest.
    Run(Box<RunArgs>),

    /// Start a stack — several `ply run`s from one file ([[app]] blocks), in
    /// dependency order. `ply up` runs the stack in `-C dir`; `ply up
    /// <namespace>/<name>` fetches and runs a published stack; `ply up
    /// ./x.stack.toml` runs a stack file.
    Up(UpArgs),

    /// Run a command inside a running instance
    Exec(ExecArgs),

    /// Make systemd agree with /var/lib/ply/deployments/ (a deployment is
    /// a file: `<name>.toml` naming a registry app, an image, a GitHub
    /// release stream, a repo to build, or a published stack
    /// (`stack = "<ns>/<name>"`, expanded into one unit per member and
    /// re-fetched every beat) — plus env, publish, domains).
    /// Fleet hosts sync the dir from git first. Fired automatically by
    /// ply-deployments.path and the 1-minute timer; safe to run by hand.
    /// Root only.
    Reconcile(ReconcileArgs),

    /// Set an app's instance count (the run parent grows/shrinks the pool)
    ///
    /// Writes the app's control file — commands are files; the parent acts
    /// within ~2s. Works from anything that can write the dir: this CLI,
    /// the dashboard, `echo 4 > …/control/scale` over ssh.
    Scale(ScaleArgs),

    /// Rolling restart on the current image (health-gated, like a deploy)
    Restart(RestartArgs),

    /// Show an app's recent output (bounded ring; journald keeps history)
    ///
    /// The run parent tees every instance's stdout+stderr into
    /// <run-dir>/logs/<app>.<n>.log (512 KiB x2 per instance). This reads
    /// those files — works identically foreground, under systemd, rootless.
    Logs(LogsArgs),

    /// What an app's instances reached: the egress audit log as a table
    Egress(EgressArgs),

    /// List running instances
    Ps(PsArgs),

    /// Live per-instance usage: CPU, memory, pids, network, throttling
    ///
    /// Reads the kernel's cgroup v2 files and veth counters — no agent.
    Stats(StatsArgs),

    /// Validate an image, optionally against a host runtime policy
    Check(CheckArgs),

    /// Show a package's record: type, owner, volumes, links, dependencies,
    /// and params — from a registry ref, a local .img, a .toml, or a dir
    Inspect(InspectArgs),

    /// Import an OCI/Docker image as a self-sufficient (fat) ply image
    Import(ImportArgs),

    /// Flatten an image and its dependencies into one self-sufficient image
    Bundle(BundleArgs),

    /// Author a package interactively: shell in, install, commit the diff
    ///
    /// The overlay upperdir is the layer: `craft new` opens a persistent
    /// session on a base, `changes` shows what you added, `commit` packs it
    /// as a normal, inert, content-addressed package image.
    #[command(subcommand)]
    Craft(CraftCommand),

    /// Rolling-deploy a new image version to a running app
    ///
    /// Writes the deploy pointer, signals the app's run parent (SIGHUP),
    /// and watches the roll: instances restart one at a time from the new
    /// image, gated by [health]. A failed gate aborts the roll and reverts
    /// that slot; untouched instances keep serving the old version.
    Deploy(DeployArgs),

    /// Swap a runtime under an app without rebuilding it
    ///
    /// Fleet security patching as a metadata operation: only the embedded
    /// lockfile changes; the manifest's version constraint still applies.
    Rebase(RebaseArgs),

    /// Emit a systemd unit file for an image (supervision is systemd's job)
    Systemd(SystemdArgs),

    /// Emit reverse-proxy / load-balancer config for running apps
    Proxy(ProxyArgs),

    /// One-time host preparation (idempotent; run with sudo)
    ///
    /// Installs an AppArmor profile enabling rootless `ply run` on kernels
    /// that restrict unprivileged user namespaces (Ubuntu 24.04+), then
    /// reports what else rootless still needs (subuid range, newuidmap,
    /// privileged ports). Safe to re-run.
    Setup(SetupArgs),
    /// Update ply itself to the newest release
    SelfUpdate(SelfUpdateArgs),
    /// Log in to the registry (GitHub device flow — your namespace is your login)
    Login,
    /// Show who you are logged in as
    Whoami,

    /// Registry keys: mint one for CI, list them, revoke one
    ///
    /// A CI runner cannot do the GitHub device flow, so it publishes with a
    /// key: mint one here (or on the account page), store it as a secret,
    /// and set PLY_TOKEN in the workflow.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Publish an app, a keg or a stack to the registry under your namespace
    ///
    /// The manifest inside the artifact IS what gets published: `ply push`
    /// uploads the bytes (unless `--src` says where they already live) and
    /// sends the record — manifest text, its JSON rendering, and the
    /// artifact's arch/sha256/bytes.
    Push(PushArgs),

    /// Pre-fetch every package in the host policy into the store
    ///
    /// Reads /etc/ply/runtimes.toml (or --policy FILE). A freshly synced
    /// host deploys with zero fetches.
    Sync(SyncArgs),

    /// Delete store entries unreferenced by any installed or running app
    Gc(GcArgs),

    /// Remove an app (volumes are kept unless --volumes)
    Rm(RmArgs),

    /// List and delete app data volumes (plain host directories)
    ///
    /// Volumes survive `ply rm` and deleted deployments on purpose — this
    /// is where you see what survived and, deliberately, delete it.
    /// Wiping a database volume before a BACKUP_RESTORE redeploy lives here.
    #[command(subcommand)]
    Volume(VolumeCommand),

    /// List and set secrets (the file store `ply up` mints into)
    ///
    /// A secret is one 0600 file named `<member>.<param>` under
    /// `.ply/secrets/` (stack-local, `-C DIR`) or a deployment's
    /// `.secrets/<stack>/` (`--deployments`). `ply up` mints ordinary
    /// secrets automatically; external secrets
    /// (`{ secret = true, external = true }`) are yours to provide, which
    /// is what `set` is for. Values never appear in output.
    #[command(subcommand)]
    Secret(SecretCommand),

    /// Report shared volumes, deprecated runtimes, and other risk surface
    Audit(AuditArgs),

    /// List dependencies with newer versions available
    Outdated(OutdatedArgs),
}

#[derive(Args)]
pub struct BuildArgs {
    /// Directory containing ply.toml
    #[arg(value_name = "DIR", default_value = ".")]
    pub dir: PathBuf,

    /// Output image path (defaults to the canonical filename in DIR)
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Target architecture: x64 or arm64 (default: the host's). Packing is
    /// arch-independent; dependencies resolve for the target arch.
    #[arg(long, value_name = "ARCH")]
    pub arch: Option<String>,

    /// Allow plain-http sources on public hosts (hash still verifies content)
    #[arg(long)]
    pub insecure_source: bool,

    /// Pack credential-shaped files (.env, *.key, .npmrc …) that were swept
    /// in implicitly. Refused by default: an image is distributable.
    #[arg(long)]
    pub allow_secrets: bool,
}

#[derive(Args)]
pub struct ReconcileArgs {
    /// Converge every deployment NOW, ignoring the failure back-off and
    /// `auto = false` pins for this run. The timer never passes this; it is
    /// for a person who has just fixed something and does not want to wait.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct InitArgs {
    /// Directory to initialise (defaults to the current one)
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    /// Accept every default without asking
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Overwrite an existing ply.toml
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct SearchArgs {
    /// Substring to match against package names and descriptions
    pub query: String,
    /// List every published version and arch instead of one line per package
    #[arg(long)]
    pub versions: bool,
    /// Maximum packages to show (0 = all)
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Search this source instead of the manifest's default (any `[sources]` spec)
    #[arg(long)]
    pub source: Option<String>,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct AddArgs {
    /// Package name, optionally with a range: `ffmpeg` or `ffmpeg@6.1`
    pub spec: String,
    /// Take the package from this `[sources]` entry instead of `default`
    #[arg(long)]
    pub source: Option<String>,
}

#[derive(Args)]
pub struct RunArgs {
    /// Image to run: a .img path, a registry name (`postgres`, `myapp@1.2` —
    /// newest matching version is fetched and cached), or `docker://name:tag`
    /// to import an OCI image on demand (cached after the first run)
    #[arg(value_name = "IMAGE")]
    pub image: String,

    /// Join the stack's network instead of the caller's: a network
    /// namespace (`/proc/<pid>/ns/net`) on Linux. `ply up` passes the one it
    /// made for the stack, so members share a network; not something to type
    /// by hand.
    ///
    /// `--vswitch PATH` is the second spelling, and the one `ply up` uses
    /// off Linux: there is no namespace to join on a Mac, so the stack's
    /// network is a userspace switch in the `ply up` process and this is
    /// the unix socket that reaches it. One field, because a run joins one
    /// network and only the platform decides what shape it has.
    #[arg(long = "netns", alias = "vswitch", value_name = "PATH", hide = true)]
    pub network: Option<std::path::PathBuf>,

    /// A sibling in `--netns`/`--vswitch`, so `<name>.ply` resolves: to
    /// loopback inside the container on Linux, to the sibling's own address
    /// on the switch in a microVM. Repeatable; set by `ply up`.
    #[arg(long = "netns-peer", value_name = "NAME", hide = true)]
    pub netns_peer: Vec<String>,

    /// The resolver reachable inside `--netns`. Set by `ply up` when it has
    /// attached a user-mode router.
    #[arg(long = "netns-dns", value_name = "IP", hide = true)]
    pub netns_dns: Option<String>,

    /// Name this running app instead of using the image's own name. Sets its
    /// state pool, its `<name>.ply` address, and what `--after` waits on — so
    /// two runs of one image (two databases, say) don't collide.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Registry source for name references (any `[sources]` spec)
    #[arg(long, value_name = "SPEC", default_value = ply_core::catalog::OFFICIAL_RUN_SOURCE)]
    pub source: String,

    /// Re-import a `docker://` reference even if it is already cached
    #[arg(long)]
    pub pull: bool,

    /// Number of identical instances to start
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub scale: u32,

    /// Set an environment variable (repeatable, KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Read environment variables from a file (for secrets — never bake them into images)
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,

    /// Bind-mount a host path into the instance (dev mode), HOST:CONTAINER
    #[arg(long, value_name = "HOST:CONTAINER")]
    pub link: Vec<String>,

    /// Expose the pool on a real host port: the run parent binds it and
    /// L4-balances connections across instances (TCP only — hostnames and
    /// TLS are the edge's job).
    ///
    /// PORT | HOST_PORT:INSTANCE_PORT | ADDR:PORT[:INSTANCE_PORT], where ADDR
    /// is `internal` (reachable only by ply apps on this host), `public`
    /// (0.0.0.0, the default) or an IPv4 address. Use `internal` for
    /// databases and internal APIs: `--publish 5432` puts postgres on every
    /// interface, `--publish internal:5432` does not.
    ///
    /// Repeatable — each spec gets its own listener and backend pool, so an
    /// edge can hold `--publish 80:80 --publish 443:443`. The first spec is
    /// the app's canonical address (what `--after` gives dependants).
    #[arg(long, value_name = "[ADDR:]PORT[:INSTANCE_PORT]")]
    pub publish: Vec<String>,

    /// Outbound policy: `off`, `audit` (log everything, mark what the
    /// manifest did not declare) or `enforce` (block it). Defaults to
    /// `audit` when the manifest declares `[network] egress`, else `off`.
    #[arg(long, value_name = "MODE")]
    pub egress: Option<String>,

    /// Replace the manifest's declared egress list with these entries
    /// (repeatable: a hostname, `*.suffix`, an IPv4 address or CIDR, or
    /// `*`). Pass `--egress-allow ""` for an empty list.
    #[arg(long = "egress-allow", value_name = "ENTRY")]
    pub egress_allow: Vec<String>,

    /// Start only once a condition holds (repeatable). Three forms:
    ///
    /// `APP` — an instance of APP is alive and, when its manifest declares
    /// [health] port, that port is accepting connections (just running is
    /// enough if it has none).
    ///
    /// `APP.PARAM` — APP has published PARAM under its live params tree
    /// (readable at /run/ply/APP/PARAM from inside a container).
    ///
    /// `APP.PARAM == 'value'` (or "value") — APP has published PARAM and its
    /// current value is exactly `value`. The published file is the truth
    /// for this form and the one above: neither requires APP itself to
    /// still be alive.
    ///
    /// Also service discovery: if APP is published, <APP>_ADDR, <APP>_HOST and
    /// <APP>_PORT are injected into this app's environment, pointing at APP's
    /// load-balanced endpoint (correct for rootless and rootful alike). An
    /// explicit [env] entry or -e always wins.
    #[arg(long, value_name = "COND")]
    pub after: Vec<String>,

    /// How long to wait for --after apps before giving up
    #[arg(long, value_name = "DURATION", default_value = "60s")]
    pub after_timeout: String,

    /// Skip rights stripping: keep all capabilities, no_new_privs off, seccomp
    /// off. For debugging and for imported Docker images whose entrypoints
    /// expect Docker's retained capabilities (`chown … && exec gosu …`).
    /// Never for anything you did not write or import yourself.
    #[arg(long)]
    pub privileged: bool,

    /// Mount the host links the image's [requests] section asks for. Never
    /// automatic: a manifest ships inside the image, so host access needs
    /// this explicit yes. Without it, requests are listed and NOT mounted.
    #[arg(long)]
    pub grant_links: bool,

    /// Serve this app at a hostname via the ply-managed edge (repeatable).
    /// Records the domain in instance state; `ply proxy --watch` (installed
    /// by `sudo ply setup --edge`) renders it into Caddy config and Caddy
    /// obtains the certificate. Point the domain's DNS at this host first.
    #[arg(long, value_name = "HOST")]
    pub domain: Vec<String>,

    /// Back a container path with a managed, chowned volume (repeatable).
    /// For imported apps that write a data dir as a non-root user but whose
    /// image never declared a VOLUME (n8n's /home/node/.n8n).
    #[arg(long, value_name = "PATH")]
    pub volume: Vec<String>,
}

#[derive(Args)]
pub struct UpArgs {
    /// Optional stack source first (`<namespace>/<name>` or a `.toml`/dir
    /// path), then members to start; a bare word is a member. Empty = the
    /// stack in `-C dir`, all members.
    #[arg(value_name = "[SOURCE] [MEMBER...]")]
    pub members: Vec<String>,

    /// Directory containing the stack ply.toml
    #[arg(short = 'C', long, value_name = "DIR", default_value = ".")]
    pub dir: PathBuf,

    /// Registry source for `run =` members (any `[sources]` spec)
    #[arg(long, value_name = "SPEC", default_value = ply_core::catalog::OFFICIAL_RUN_SOURCE)]
    pub source: String,

    /// Re-resolve `run =` members instead of honoring the stack lock
    #[arg(long)]
    pub refresh: bool,

    /// How long each member may wait for its `after` dependencies
    #[arg(long, value_name = "DURATION", default_value = "60s")]
    pub after_timeout: String,

    /// Values for `$VAR` holes in the stack (KEY=VALUE lines); the process
    /// environment is consulted too. An undefined `$VAR` is a hard error.
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,

    /// Resolve every member's params and env, and print the composed
    /// result — no minting, no spawn, no lock write. Exits non-zero on any
    /// resolution error (an undeclared param, a live param in env, a
    /// missing external secret) — plan is the validator.
    #[arg(long)]
    pub plan: bool,
}

#[derive(Args)]
pub struct ScaleArgs {
    /// App to scale
    #[arg(value_name = "APP")]
    pub app: String,
    /// Target instance count
    #[arg(value_name = "N")]
    pub n: u32,
}

#[derive(Args)]
pub struct RestartArgs {
    /// App to rolling-restart
    #[arg(value_name = "APP")]
    pub app: String,
}

#[derive(Args)]
pub struct LogsArgs {
    /// App (or app.<n> instance); omit to list what has logs
    #[arg(value_name = "APP")]
    pub app: Option<String>,

    /// Follow: keep printing as new output arrives
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Lines of history to show first
    #[arg(short = 'n', long, value_name = "N", default_value_t = 100)]
    pub lines: usize,
}

#[derive(Args)]
pub struct EgressArgs {
    /// App whose instances to show
    #[arg(value_name = "APP")]
    pub app: String,
    /// Keep printing new records as they arrive
    #[arg(short = 'f', long)]
    pub follow: bool,
    /// Only blocked connections and refused names
    #[arg(long)]
    pub blocked: bool,
    /// Raw JSON records instead of the table
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ExecArgs {
    /// App (or app.<n> instance) to enter
    #[arg(value_name = "APP")]
    pub app: String,

    /// Command to run inside the instance
    #[arg(value_name = "CMD", trailing_var_arg = true, required = true)]
    pub cmd: Vec<String>,
}

#[derive(Args)]
pub struct PsArgs {
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct StatsArgs {
    /// Limit to one app (`myapp`) or instance (`myapp.2`)
    #[arg(value_name = "APP")]
    pub app: Option<String>,

    /// Machine-readable output
    #[arg(long)]
    pub json: bool,

    /// CPU sampling window in milliseconds
    #[arg(long, default_value_t = 500)]
    pub sample_ms: u64,
}

#[derive(Args)]
pub struct CheckArgs {
    /// Image file to check
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// Runtime policy file to check against (pure function — CI-friendly)
    #[arg(long, value_name = "POLICY")]
    pub against: Option<PathBuf>,
}

#[derive(Args)]
pub struct InspectArgs {
    /// What to inspect: a registry ref (`postgres@17`, `owner/name@1.2`),
    /// a local `.img`, a `.toml` manifest, or a directory containing one
    #[arg(value_name = "TARGET")]
    pub target: String,

    /// Print the record as JSON (what `ply push` sends)
    #[arg(long)]
    pub json: bool,

    /// Print the embedded manifest text verbatim
    #[arg(long)]
    pub manifest: bool,
}

#[derive(Args)]
pub struct ImportArgs {
    /// Source, e.g. docker://nginx:1.27
    #[arg(value_name = "SOURCE")]
    pub source: String,

    /// Output image path
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,
}

#[derive(Args)]
pub struct BundleArgs {
    /// Image to flatten
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// Output image path
    #[arg(short, long, value_name = "FILE")]
    pub output: PathBuf,
}

#[derive(Subcommand)]
pub enum CraftCommand {
    /// Start a new session on a base and open a shell in it
    New(CraftNewArgs),
    /// Re-enter an existing session (state persists between shells)
    Shell(CraftShellArgs),
    /// Reconstruct a session from a committed package image (resume anywhere)
    Edit(CraftEditArgs),
    /// List files added/modified/deleted by the session
    Changes(CraftNameArg),
    /// Pack the session's changes as a package image
    Commit(CraftCommitArgs),
    /// List sessions
    Ls,
    /// Discard a session (its rw layer is deleted; committed images stay)
    Rm(CraftNameArg),
}

#[derive(Args)]
pub struct CraftNewArgs {
    /// Session name (becomes the package name at commit)
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Base to build on, as pkg@constraint (e.g. debian@13)
    #[arg(long, value_name = "PKG@CONSTRAINT")]
    pub from: String,

    /// Where to fetch the base from (default: the official registry)
    #[arg(long, value_name = "URL")]
    pub source: Option<String>,

    /// Command to run instead of /bin/sh
    #[arg(value_name = "CMD", trailing_var_arg = true)]
    pub cmd: Vec<String>,

    /// Allow plain-http sources on public hosts
    #[arg(long)]
    pub insecure_source: bool,
}

#[derive(Args)]
pub struct CraftShellArgs {
    /// Session name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Command to run instead of /bin/sh
    #[arg(value_name = "CMD", trailing_var_arg = true)]
    pub cmd: Vec<String>,
}

#[derive(Args)]
pub struct CraftEditArgs {
    /// A package image previously produced by `ply craft commit`
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// Override the base's source URL recorded in the image
    #[arg(long, value_name = "URL")]
    pub source: Option<String>,

    /// Allow plain-http sources on public hosts
    #[arg(long)]
    pub insecure_source: bool,
}

#[derive(Args)]
pub struct CraftNameArg {
    /// Session name
    #[arg(value_name = "NAME")]
    pub name: String,
}

#[derive(Args)]
pub struct CraftCommitArgs {
    /// Session name (= package name)
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Version of the resulting package
    #[arg(long, value_name = "SEMVER")]
    pub version: semver::Version,

    /// Output image path (defaults to the canonical filename in cwd)
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

#[derive(Args)]
pub struct DeployArgs {
    /// The new image version to roll out
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// Seconds to wait for the roll before reporting partial progress
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,
}

#[derive(Args)]
pub struct RebaseArgs {
    /// Image to rebase
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// Runtime to swap in, as name@exact.version (e.g. node@24.6.1)
    #[arg(long, value_name = "NAME@VERSION")]
    pub runtime: String,

    /// Output image path (defaults to rewriting IMAGE in place)
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Allow plain-http sources on public hosts
    #[arg(long)]
    pub insecure_source: bool,
}

#[derive(Args)]
pub struct SystemdArgs {
    /// Image to generate a unit file for
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    /// Number of identical instances (baked into ExecStart)
    #[arg(long, value_name = "N")]
    pub scale: Option<u32>,

    /// Publish the pool on a host port (baked into ExecStart). Repeatable.
    #[arg(long, value_name = "[ADDR:]PORT[:INSTANCE_PORT]")]
    pub publish: Vec<String>,

    /// Environment variables for the app (repeatable, KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Environment file read at start (secrets stay out of the unit)
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,

    /// Serve at a hostname via the ply-managed edge (repeatable; baked
    /// into ExecStart — see `ply run --domain`)
    #[arg(long, value_name = "HOST")]
    pub domain: Vec<String>,

    /// Expand the image's [requests] links into explicit --link flags in the
    /// unit (auditable in the unit file; the grant happens here, once)
    #[arg(long)]
    pub grant_links: bool,

    /// Start after APP is healthy (repeatable): ordered in [Unit] and
    /// gated by `--after` in ExecStart
    #[arg(long, value_name = "APP")]
    pub after: Vec<String>,

    /// Emit a systemd **user** unit (~/.config/systemd/user) instead of a
    /// system one. Rootless apps must be supervised by the user that owns
    /// their store, subuid range and AppArmor profile — a system unit runs
    /// them as root, which is a different mode entirely.
    #[arg(long)]
    pub user: bool,
}

#[derive(Args)]
pub struct ProxyArgs {
    /// Apps to emit config for (default: every running app)
    #[arg(value_name = "APP")]
    pub apps: Vec<String>,

    /// Config format
    #[arg(long, value_name = "FORMAT", default_value = "caddy")]
    pub format: String,

    /// Keep the config current: re-render on instance-state changes, write
    /// --out on difference, reload Caddy. This is the edge's engine —
    /// `sudo ply setup --edge` installs it as a systemd unit.
    #[arg(long)]
    pub watch: bool,

    /// Write here instead of stdout (default under --watch:
    /// /etc/ply/edge/apps/ply.caddy)
    #[arg(long, value_name = "FILE")]
    pub out: Option<String>,
}

#[derive(Args)]
pub struct SyncArgs {
    /// Policy file (defaults to /etc/ply/runtimes.toml)
    #[arg(long, value_name = "FILE")]
    pub policy: Option<PathBuf>,

    /// Allow plain-http sources on public hosts
    #[arg(long)]
    pub insecure_source: bool,
}

#[derive(Args)]
pub struct GcArgs {
    /// Show what would be deleted without deleting
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct RmArgs {
    /// App to remove
    #[arg(value_name = "APP")]
    pub app: String,

    /// Also destroy the app's volumes (data deletion is always explicit)
    #[arg(long)]
    pub volumes: bool,
}

#[derive(Args)]
pub struct AuditArgs {}

#[derive(Subcommand)]
pub enum KeyCommand {
    /// Mint a key and print it once (CI: store it as PLY_TOKEN)
    New(KeyNewArgs),
    /// List this account's keys — ids and last use, never the keys
    Ls,
    /// Revoke a key by id; anything using it stops immediately
    Rm(KeyRmArgs),
}

#[derive(Args)]
pub struct KeyNewArgs {
    /// What it is for, e.g. "ci: ply-web" — shown in `ply key ls`
    #[arg(long)]
    pub note: Option<String>,
}

#[derive(Args)]
pub struct KeyRmArgs {
    /// Key id from `ply key ls`
    pub id: i64,
}

#[derive(Subcommand)]
pub enum VolumeCommand {
    /// List volumes: app, size, in use / idle / orphaned
    Ls(VolumeLsArgs),
    /// Delete a volume (refused while its instance runs)
    Rm(VolumeRmArgs),
}

#[derive(Args)]
pub struct VolumeLsArgs {
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct VolumeRmArgs {
    /// Exactly as `ply volume ls` shows it: <app>/<name>.<slot>
    #[arg(value_name = "APP/VOLUME.SLOT")]
    pub target: Option<String>,

    /// Delete every volume no installed app claims (lists first, confirms)
    #[arg(long)]
    pub orphans: bool,

    /// Skip the confirmation (for scripts)
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Subcommand)]
pub enum SecretCommand {
    /// List secret names — never values
    Ls(SecretLsArgs),
    /// Set a secret's value (external secrets need this before `ply up` will run)
    Set(SecretSetArgs),
}

#[derive(Args)]
pub struct SecretLsArgs {
    /// Directory containing the stack (its secrets live in `.ply/secrets/`)
    #[arg(
        short = 'C',
        long,
        value_name = "DIR",
        default_value = ".",
        conflicts_with = "deployments"
    )]
    pub dir: PathBuf,

    /// Manage the deployments-side store instead, keyed by stack name: files
    /// live under the deployments dir's `.secrets/<stack>/`
    /// (/var/lib/ply/deployments/.secrets/<stack>/ by default) — that root
    /// is normally only writable as root.
    #[arg(long, value_name = "STACK", conflicts_with = "dir")]
    pub deployments: Option<String>,
}

#[derive(Args)]
pub struct SecretSetArgs {
    /// Secret name: MEMBER.PARAM, e.g. db.password
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Value to store; omit to read one line from stdin (keeps it out of
    /// shell history)
    #[arg(value_name = "VALUE")]
    pub value: Option<String>,

    /// Directory containing the stack (its secrets live in `.ply/secrets/`)
    #[arg(
        short = 'C',
        long,
        value_name = "DIR",
        default_value = ".",
        conflicts_with = "deployments"
    )]
    pub dir: PathBuf,

    /// Manage the deployments-side store instead, keyed by stack name: files
    /// live under the deployments dir's `.secrets/<stack>/`
    /// (/var/lib/ply/deployments/.secrets/<stack>/ by default) — that root
    /// is normally only writable as root.
    #[arg(long, value_name = "STACK", conflicts_with = "dir")]
    pub deployments: Option<String>,
}

#[derive(Args)]
pub struct OutdatedArgs {}

/// A Docker verb ply deliberately doesn't have → a pointer to the ply way.
/// Shown instead of clap's generic "unrecognized subcommand" so switchers
/// learn the model at exactly the moment they reach for the old habit.
pub fn docker_hint(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "pull" => "dependencies fetch on demand, pinned by ply.lock — there is nothing to pull.\n(`ply sync` pre-fetches a host policy's packages; docs: https://plybox.sh/docs/dependencies/)",
        "tag" => "no tags, on purpose: published versions are immutable — nothing like `:latest` can move.\nBump [package] version and `ply build` instead.",
        "logout" => "signing out is deleting ~/.config/ply/credentials — `ply login` mints a fresh token.\nInstalling never needs an account; only publishing does.",
        "compose" => "compose is `ply up`: a stack file lists [[app]] members (registry apps, local dirs, or URLs) wired with `after` and env — each block is one `ply run`.\n(docs: https://plybox.sh/docs/running/)",
        "stop" | "kill" => "Ctrl-C the foreground run, `ply rm <app>`, or `systemctl stop ply-<app>` under systemd.",
        "start" | "restart" => "`ply run IMAGE` starts instances; crash restarts are the [restart] policy's job, reboots are systemd's.\n(docs: https://plybox.sh/docs/deploy/)",
        "inspect" => "`ply check IMAGE` validates an image; `ply ps` / `ply stats` show running instances.",
        "cp" => "volumes are plain host directories (ply.toml [volumes]); dev mode: `ply run --link HOST:CONTAINER`.",
        "save" | "load" | "export" => "an image already IS a single file — scp it; `ply run app.img` on the other side.",
        "network" => "every instance gets its own IP and a `<app>.ply` name on the bridge — no port mappings, no network objects.",
        "commit" => "turning a live session into an image is `ply craft` (new → commit).\n(docs: https://plybox.sh/docs/packages/)",
        "rmi" => "`ply gc` deletes store entries no app references; `rm -rf /var/lib/ply` is the factory reset.",
        // `ply lb` was a second name for the same thing: emit reverse-proxy
        // config. The only difference was scope, which is now an argument.
        "lb" => "folded into `ply proxy`: `ply proxy <app>` for one, `ply proxy` for every running app.\n(--format caddy|nginx|haproxy; docs: https://plybox.sh/docs/running/)",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn docker_verbs_get_hints_ply_verbs_do_not() {
        assert!(docker_hint("pull").is_some());
        assert!(docker_hint("compose").is_some());
        assert!(docker_hint("run").is_none());
        assert!(docker_hint("deploy").is_none());
    }

    /// A hint must never shadow a real subcommand: if a hinted verb becomes
    /// a real ply command one day, this forces removing the stale hint.
    #[test]
    fn hints_only_cover_nonexistent_subcommands() {
        for verb in [
            "pull", "tag", "logout", "compose", "stop", "kill", "start", "restart", "inspect",
            "cp", "save", "load", "export", "network", "commit", "rmi",
        ] {
            assert!(
                docker_hint(verb).is_some(),
                "{verb} missing from hint table"
            );
            assert!(
                Cli::try_parse_from(["ply", verb]).is_err(),
                "`ply {verb}` is a real subcommand now — remove its docker hint"
            );
        }
    }

    #[test]
    fn run_after_is_repeatable_with_a_default_timeout() {
        let cli = Cli::try_parse_from([
            "ply", "run", "--after", "pgdb", "--after", "redis", "app.img",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(args.after, vec!["pgdb", "redis"]);
        assert_eq!(args.after_timeout, "60s");
        let cli = Cli::try_parse_from(["ply", "systemd", "--after", "pgdb", "app.img"]).unwrap();
        let Command::Systemd(args) = cli.command else {
            panic!("expected systemd")
        };
        assert_eq!(args.after, vec!["pgdb"]);
    }
}

#[derive(Args)]
pub struct PushArgs {
    /// What to publish: an app/keg directory (built first, as `ply build`),
    /// a built .img, or a stack (stack.toml, or a directory whose ply.toml
    /// carries `[[app]]`). The manifest inside it IS the record.
    #[arg(value_name = "TARGET")]
    pub target: String,

    /// Publish under a granted namespace instead of your own login
    /// (the official `ply`/`apps` shelves, a shared org). `ply whoami`
    /// lists what your key may publish to. Sets `owner` when the manifest
    /// declares none; conflicts with a different `[package] owner`.
    #[arg(long = "as", value_name = "NAMESPACE")]
    pub as_namespace: Option<String>,

    /// Publish an image that lives elsewhere (a release asset, any static
    /// host): the record points at URL and no bytes are uploaded. `{version}`
    /// and `{arch}` expand; the artifact is recorded unverified.
    #[arg(long, value_name = "URL")]
    pub src: Option<String>,

    /// Target architecture: x64 or arm64 (default: the host's). A directory
    /// cross-builds exactly as `ply build --arch` does; the artifact is
    /// appended to the version.
    #[arg(long, value_name = "ARCH")]
    pub arch: Option<String>,

    /// Print the record that would be published — upload nothing, publish
    /// nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub struct SelfUpdateArgs {
    /// Only report whether a newer release exists
    #[arg(long)]
    pub check: bool,
}

#[derive(clap::Args, Debug)]
pub struct SetupArgs {
    /// Lower net.ipv4.ip_unprivileged_port_start so rootless instances can
    /// bind privileged ports (default 80: nginx, httpd, caddy and traefik all
    /// bind it themselves). HOST-WIDE — every unprivileged process on this
    /// machine gains the same right. Persisted in /etc/sysctl.d.
    #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "80")]
    pub unprivileged_ports: Option<u16>,

    /// Create and enable a swapfile (e.g. --swap 2G) — small droplets need
    /// it for JS builds: the memory-fenced builder spills here instead of
    /// OOMing or evicting your running apps. Persisted in /etc/fstab.
    #[arg(long, value_name = "SIZE")]
    pub swap: Option<String>,

    /// Install the HTTPS edge: Caddy (downloaded if absent) + a ply-managed
    /// Caddyfile + two systemd units — `ply-edge` (Caddy) and `ply-proxy`
    /// (`ply proxy --watch`). After this, `ply run --domain app.example.com`
    /// is all an app needs for a certificate and HTTPS serving.
    #[arg(long)]
    pub edge: bool,

    /// GitOps fleet: sync this host's deployments from a git repo. Every
    /// reconcile beat pulls the repo and applies `shared/*.toml` +
    /// `hosts/<host>/*.toml` into the deployments dir (content-compared, so
    /// only real changes count as touches; git-managed files are tracked —
    /// local deployments coexist untouched).
    #[arg(long, value_name = "GIT_URL")]
    pub fleet: Option<String>,

    /// Which hosts/<name>/ dir this host follows (default: the hostname).
    #[arg(long, value_name = "NAME")]
    pub fleet_host: Option<String>,

    /// Read-only deploy key for a private fleet repo (ssh URLs).
    #[arg(long, value_name = "PATH")]
    pub fleet_key: Option<String>,
}

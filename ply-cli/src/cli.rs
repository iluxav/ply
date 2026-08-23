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
    Run(RunArgs),

    /// Run a command inside a running instance
    Exec(ExecArgs),

    /// List running instances
    Ps(PsArgs),

    /// Live per-instance usage: CPU, memory, pids, network, throttling
    ///
    /// Reads the kernel's cgroup v2 files and veth counters — no agent.
    Stats(StatsArgs),

    /// Validate an image, optionally against a host runtime policy
    Check(CheckArgs),

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

    /// Emit load-balancer config for one app
    Lb(LbArgs),

    /// One-time host preparation (idempotent; run with sudo)
    ///
    /// Installs an AppArmor profile enabling rootless `ply run` on kernels
    /// that restrict unprivileged user namespaces (Ubuntu 24.04+). On hosts
    /// without the restriction it does nothing.
    Setup,

    /// Pre-fetch every package in the host policy into the store
    ///
    /// Reads /etc/ply/runtimes.toml (or --policy FILE). A freshly synced
    /// host deploys with zero fetches.
    Sync(SyncArgs),

    /// Delete store entries unreferenced by any installed or running app
    Gc(GcArgs),

    /// Remove an app (volumes are kept unless --volumes)
    Rm(RmArgs),

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

    /// Allow plain-http sources on public hosts (hash still verifies content)
    #[arg(long)]
    pub insecure_source: bool,
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
    /// Image file to run
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

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
    /// TLS are the edge's job). PORT, or HOST_PORT:INSTANCE_PORT.
    #[arg(long, value_name = "PORT[:INSTANCE_PORT]")]
    pub publish: Option<String>,
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

    /// Base to build on, as pkg@constraint (e.g. alpine@3.20)
    #[arg(long, value_name = "PKG@CONSTRAINT")]
    pub from: String,

    /// Where to fetch the base from (e.g. http://127.0.0.1:8321)
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

    /// Publish the pool on a host port (baked into ExecStart)
    #[arg(long, value_name = "PORT[:INSTANCE_PORT]")]
    pub publish: Option<String>,

    /// Environment variables for the app (repeatable, KEY=VALUE)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// Environment file read at start (secrets stay out of the unit)
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,
}

#[derive(Args)]
pub struct ProxyArgs {
    /// Proxy to emit config for
    #[arg(long, value_name = "BACKEND", default_value = "caddy")]
    pub backend: String,
}

#[derive(Args)]
pub struct LbArgs {
    /// App to emit load-balancer config for
    #[arg(value_name = "APP")]
    pub app: String,

    /// Config format
    #[arg(long, value_name = "FORMAT", default_value = "nginx")]
    pub format: String,
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

#[derive(Args)]
pub struct OutdatedArgs {}

/// A Docker verb ply deliberately doesn't have → a pointer to the ply way.
/// Shown instead of clap's generic "unrecognized subcommand" so switchers
/// learn the model at exactly the moment they reach for the old habit.
pub fn docker_hint(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "pull" => "dependencies fetch on demand, pinned by ply.lock — there is nothing to pull.\n(`ply sync` pre-fetches a host policy's packages; docs: https://plybox.sh/docs/dependencies/)",
        "push" => "publishing is copying a file: upload the .img to any file host (GitHub Releases, a bucket, a directory).\n(docs: https://plybox.sh/docs/registries/)",
        "tag" => "no tags, on purpose: published versions are immutable — nothing like `:latest` can move.\nBump [package] version and `ply build` instead.",
        "login" | "logout" => "registries have no accounts — any file host your network can GET works; the sha256 in ply.lock is the trust, not a login.",
        "compose" | "up" => "no compose file: every app is its own ply.toml, and apps reach each other by `<app>.ply` names.\n(docs: https://plybox.sh/docs/running/)",
        "logs" => "logs are stdout — `ply run` is a foreground process. Under systemd: journalctl -u ply-<app>",
        "stop" | "kill" => "Ctrl-C the foreground run, `ply rm <app>`, or `systemctl stop ply-<app>` under systemd.",
        "start" | "restart" => "`ply run IMAGE` starts instances; crash restarts are the [restart] policy's job, reboots are systemd's.\n(docs: https://plybox.sh/docs/deploy/)",
        "inspect" => "`ply check IMAGE` validates an image; `ply ps` / `ply stats` show running instances.",
        "cp" => "volumes are plain host directories (ply.toml [volumes]); dev mode: `ply run --link HOST:CONTAINER`.",
        "save" | "load" | "export" => "an image already IS a single file — scp it; `ply run app.img` on the other side.",
        "network" => "every instance gets its own IP and a `<app>.ply` name on the bridge — no port mappings, no network objects.",
        "volume" => "declare [volumes] in ply.toml; data lives as plain directories under the volumes dir.\n(docs: https://plybox.sh/docs/volumes/)",
        "commit" => "turning a live session into an image is `ply craft` (new → commit).\n(docs: https://plybox.sh/docs/packages/)",
        "rmi" => "`ply gc` deletes store entries no app references; `rm -rf /var/lib/ply` is the factory reset.",
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
            "pull", "push", "tag", "login", "logout", "compose", "up", "logs", "stop", "kill",
            "start", "restart", "inspect", "cp", "save", "load", "export", "network", "volume",
            "commit", "rmi",
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
}

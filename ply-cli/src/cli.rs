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
    /// Build an image from a directory containing ply.toml
    ///
    /// Resolves dependencies (writing ply.lock) and produces a deterministic
    /// squashfs image named <name>-<version>-<os>-<arch>.img.
    Build(BuildArgs),

    /// Run an image (foreground; SIGTERM works; exit code propagates)
    Run(RunArgs),

    /// Run a command inside a running instance
    Exec(ExecArgs),

    /// List running instances
    Ps(PsArgs),

    /// Validate an image, optionally against a host runtime policy
    Check(CheckArgs),

    /// Import an OCI/Docker image as a self-sufficient (fat) ply image
    Import(ImportArgs),

    /// Flatten an image and its dependencies into one self-sufficient image
    Bundle(BundleArgs),

    /// Emit a systemd unit file for an image (supervision is systemd's job)
    Systemd(SystemdArgs),

    /// Emit reverse-proxy / load-balancer config for running apps
    Proxy(ProxyArgs),

    /// Emit load-balancer config for one app
    Lb(LbArgs),

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

#[derive(Args)]
pub struct SystemdArgs {
    /// Image to generate a unit file for
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,
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

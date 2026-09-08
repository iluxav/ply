//! ply-core — library behind the `ply` CLI.
//!
//! Library-first shape: everything the CLI does goes through here.

pub mod apps;
pub mod autoscale;
pub mod build;
pub mod bundle;
pub mod catalog;
#[cfg(target_os = "linux")]
pub mod craft;
pub mod deployments;
pub mod dev;
pub mod digest;
pub mod egress;
pub mod env;
pub mod error;
pub mod github;
pub mod image;
pub mod lifecycle;
pub mod lockfile;
pub mod manifest;
pub mod oci;
pub mod params;
pub mod paths;
pub mod policy;
pub mod rebase;
pub mod record;
pub mod resolve;
pub mod runtime;
pub mod sealed;
pub mod secrets;
pub mod source;
pub mod stack;
pub mod stats;
pub mod store;
pub mod why;

pub use error::{Error, Result};

/// Rust ignores SIGPIPE by default, which turns `ply … | head` into a
/// panic on stdout. CLIs want the Unix default (die quietly).
pub fn restore_default_sigpipe() {
    unsafe {
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_DFL);
    }
}

/// The opposite, for the processes that must outlive a closed pipe: a run
/// parent, a `ply up`, a microVM worker. They write to sockets whose other
/// end can go away at any moment — a readiness probe that dropped its
/// connection before the app's greeting arrived, a worker that died — and
/// with the CLI default a single such write kills the supervisor (exit 141)
/// and every instance with it. Ignored, the write returns `EPIPE`, which
/// every writer here already treats as "they hung up".
pub fn ignore_sigpipe() {
    unsafe {
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_IGN);
    }
}

//! ply-core — library behind the `ply` CLI.
//!
//! Library-first shape: everything the CLI does goes through here.

pub mod apps;
pub mod build;
pub mod bundle;
pub mod catalog;
pub mod craft;
pub mod dev;
pub mod digest;
pub mod env;
pub mod error;
pub mod image;
pub mod lifecycle;
pub mod lockfile;
pub mod manifest;
pub mod oci;
pub mod paths;
pub mod policy;
pub mod rebase;
pub mod resolve;
pub mod runtime;
pub mod source;
pub mod stack;
pub mod stats;
pub mod store;

pub use error::{Error, Result};

/// Rust ignores SIGPIPE by default, which turns `ply … | head` into a
/// panic on stdout. CLIs want the Unix default (die quietly).
pub fn restore_default_sigpipe() {
    unsafe {
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_DFL);
    }
}

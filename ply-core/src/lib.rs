//! ply-core — library behind the `ply` CLI.
//!
//! Library-first shape: everything the CLI does goes through here.

pub mod apps;
pub mod build;
pub mod digest;
pub mod env;
pub mod error;
pub mod image;
pub mod lifecycle;
pub mod lockfile;
pub mod manifest;
pub mod policy;
pub mod resolve;
pub mod runtime;
pub mod source;
pub mod store;

pub use error::{Error, Result};

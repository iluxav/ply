//! ply-core — library behind the `ply` CLI.
//!
//! Library-first shape: everything the CLI does goes through here.

pub mod build;
pub mod digest;
pub mod error;
pub mod image;
pub mod manifest;

pub use error::{Error, Result};

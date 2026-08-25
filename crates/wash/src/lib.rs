// Raised for the same reason as main.rs: computing the layout of the deeply nested
// async fn in cli::dev::handle() exceeds the default limit of 128. main.rs already
// carried this, but lib.rs is a separate crate root and does not inherit it, so the
// bin target built while the lib target failed with "queries overflow the depth
// limit". Whether it trips depends on the rustc version: 1.94.1 stays under the
// default, newer stables do not.
#![recursion_limit = "512"]
#![doc = include_str!("../../../README.md")]

/// The current version of the wash package, set at build time
pub const CARGO_PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Command line interface implementations for wash
pub mod cli;
/// Configuration management for wash
pub mod config;
/// Component inspection and analysis
pub mod inspect;
/// Create new wash projects
pub mod new;
/// Manage WebAssembly Interface Types (WIT) for wash components
pub(crate) mod wit;

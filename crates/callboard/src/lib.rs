//! Linux service lifecycle support, shared by the binary's future commands.

#[cfg(target_os = "linux")]
pub mod lifecycle;

pub mod api;
pub mod client;
pub mod server;
pub mod setup;
#[cfg(target_os = "linux")]
pub mod uninstall;
#[cfg(target_os = "linux")]
pub mod upgrade;
pub type Error = Box<dyn std::error::Error + Send + Sync>;

mod events;

/// The package version, reported by `/health`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// This build's identity (DESIGN.md §7.4): the GUI compares it with the service's.
pub const BUILD: &str = env!("CALLBOARD_BUILD");

//! Linux service lifecycle support, shared by the binary's future commands.

#[cfg(target_os = "linux")]
pub mod lifecycle;

pub mod client;
pub mod server;
pub mod setup;
pub type Error = Box<dyn std::error::Error + Send + Sync>;

//! Operating-system boundaries for the portable scheduler model.

#[cfg(windows)]
mod background;
pub mod client;
pub mod credentials;
pub mod framing;
pub mod ipc;
pub mod paths;
pub mod process;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use background::{BackgroundProcess, spawn_background};
pub use client::*;
pub use credentials::*;
pub use framing::*;
pub use ipc::*;
pub use paths::*;
pub use process::*;

//! Application orchestration and provider adapters.

mod claude_inbox;
mod claude_session;
pub mod codex_desktop;
pub mod codex_host;
pub mod parent;
pub mod scheduler;
pub mod server;
pub mod worker;
pub mod workers;

pub use claude_session::{check_claude_version, claude_supports_native_inbox, run_managed_claude};
pub use codex_desktop::*;
pub use parent::*;
pub use scheduler::*;
pub use server::*;
pub use worker::*;
pub use workers::*;

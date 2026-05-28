//! # aura-terminal
//!
//! Layer: surface
//!
//! Cyber-retro terminal UI library for AURA CLI.
//!
//! This crate provides a standalone terminal UI that can be embedded in any
//! Rust application wanting the AURA terminal experience.
//!
//! ## Features
//!
//! - **Cyber Aesthetic**: Neon colors, ASCII art, box-drawing UI
//! - **Rich Components**: Messages, tool cards, diffs, modals
//! - **Themes**: Cyber (default), Matrix, Synthwave, Minimal
//! - **Animations**: Spinners, progress bars, streaming text
//! - **Responsive Layout**: Adapts from 40-char to 200-char terminals
//! - **Input System**: History, autocomplete, slash commands
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use aura_terminal::{Terminal, App, Theme, TerminalError};
//!
//! fn main() -> Result<(), TerminalError> {
//!     let theme = Theme::cyber();
//!     let mut terminal = Terminal::new(theme)?;
//!     let mut app = App::new();
//!
//!     terminal.run(&mut app)?;
//!     Ok(())
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod animation;
pub mod components;
pub mod events;
pub mod input;
pub mod layout;
pub mod themes;

mod app;
mod renderer;
mod terminal;

// Re-exports for convenience
pub use app::{App, AppState};
pub use events::{UiCommand, UiEvent};
pub use terminal::Terminal;
pub use themes::Theme;

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("render error: {0}")]
    Render(String),
    #[error("{0}")]
    Internal(String),
}

// Component re-exports
pub use components::{
    DiffLine, DiffView, HeaderBar, InputField, Message, MessageRole, ProgressBar, StatusBar,
    ToolCard, ToolStatus,
};

/// Library version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

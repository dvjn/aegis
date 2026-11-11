//! `ags setup` on a terminal: a full-screen, keyboard-driven screen with three steps (Aegis,
//! Accounts, Profiles) whose elements are edited in place. Every change is saved at once, so
//! quitting at any point keeps what was done.
mod app;
mod data;
mod run;
mod view;

pub use app::Tab;
pub use run::run;

pub struct Options {
    /// Server named on the command line.
    pub server: Option<String>,
    pub no_browser: bool,
    /// `--provider CLI=ID` flags: applied when pairing, or once at start when paired.
    pub defaults: Vec<String>,
}

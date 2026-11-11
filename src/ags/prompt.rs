//! Terminal prompts (demand) with a plain theme, and telling Esc from Ctrl-C.
//!
//! demand reports both Esc and Ctrl-C as `io::ErrorKind::Interrupted`. A Ctrl-C also raises
//! SIGINT, so a flag set by our SIGINT handler tells them apart: [`Ask::ask`] returns
//! [`Cancel::Back`] for Esc and [`Cancel::Quit`] for Ctrl-C, both still `Interrupted`. Outside a
//! prompt, SIGINT keeps its default action and ends the process.
use demand::{Confirm, Dialog, Input, MultiSelect, Select, Theme};
use std::{
    io,
    sync::{
        Arc, LazyLock, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

/// The Aegis look (the dashboard's accent and muted grey), readable on light and dark terminals:
/// only the accent, grey and status colors are set, plain text keeps the terminal's own color.
pub static THEME: LazyLock<Theme> = LazyLock::new(|| {
    let accent = Theme::color_rgb(47, 111, 237);
    let muted = Theme::color_rgb(121, 130, 143);
    let ok = Theme::color_rgb(63, 185, 80);
    let mut title = demand::Theme::new().title;
    title.set_bold(true);
    let mut focused_button = Theme::color_rgb(255, 255, 255);
    focused_button.set_bg(accent.fg().copied()).set_bold(true);
    let mut theme = Theme::new();
    theme.title = title;
    theme.description = muted.clone();
    theme.cursor = accent.clone();
    theme.cursor_str = "›".into();
    theme.selected_option = accent.clone();
    theme.selected_prefix = " ◉".into();
    theme.selected_prefix_fg = ok;
    theme.unselected_prefix = " ○".into();
    theme.unselected_prefix_fg = muted.clone();
    theme.input_prompt = accent.clone();
    theme.input_cursor = accent;
    theme.input_placeholder = muted.clone();
    theme.help_key = muted.clone();
    theme.help_desc = muted.clone();
    theme.help_sep = muted;
    theme.focused_button = focused_button;
    theme.blurred_button = demand::Theme::new().title;
    theme.error_indicator = Theme::color_rgb(239, 107, 115);
    theme
});

struct Flags {
    /// A SIGINT arrived during the current prompt.
    pressed: Arc<AtomicBool>,
    /// No prompt is running: SIGINT ends the process.
    outside: Arc<AtomicBool>,
}

static FLAGS: OnceLock<Flags> = OnceLock::new();

/// Installs the SIGINT handling; call once before any prompt.
pub fn init() {
    FLAGS.get_or_init(|| {
        let flags = Flags {
            pressed: Arc::new(AtomicBool::new(false)),
            outside: Arc::new(AtomicBool::new(true)),
        };
        #[cfg(unix)]
        {
            use signal_hook::{consts::SIGINT, flag};
            let _ = flag::register_conditional_default(SIGINT, flags.outside.clone());
            let _ = flag::register(SIGINT, flags.pressed.clone());
        }
        flags
    });
}

/// How a prompt was cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Cancel {
    #[error("cancelled")]
    Back,
    #[error("cancelled")]
    Quit,
}

fn guard<T>(run: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    init();
    let flags = FLAGS.get().expect("initialized");
    flags.pressed.store(false, Ordering::SeqCst);
    flags.outside.store(false, Ordering::SeqCst);
    let result = run();
    flags.outside.store(true, Ordering::SeqCst);
    match result {
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            let cancel = if flags.pressed.swap(false, Ordering::SeqCst) {
                Cancel::Quit
            } else {
                Cancel::Back
            };
            Err(io::Error::new(io::ErrorKind::Interrupted, cancel))
        }
        other => other,
    }
}

/// While alive, SIGINT does not end AGS: for a vendor command that owns the terminal and
/// handles Ctrl-C itself.
pub struct Shield;

impl Shield {
    pub fn new() -> Self {
        init();
        let flags = FLAGS.get().expect("initialized");
        flags.outside.store(false, Ordering::SeqCst);
        Self
    }
}

impl Drop for Shield {
    fn drop(&mut self) {
        let flags = FLAGS.get().expect("initialized");
        flags.pressed.store(false, Ordering::SeqCst);
        flags.outside.store(true, Ordering::SeqCst);
    }
}

/// Runs a prompt with the plain theme.
pub trait Ask {
    type Output;
    fn ask(self) -> io::Result<Self::Output>;
}

impl<T> Ask for Select<'_, T> {
    type Output = T;
    fn ask(self) -> io::Result<T> {
        guard(|| self.theme(&THEME).run())
    }
}

impl<T> Ask for MultiSelect<'_, T> {
    type Output = Vec<T>;
    fn ask(self) -> io::Result<Vec<T>> {
        guard(|| self.theme(&THEME).run())
    }
}

impl Ask for Input<'_> {
    type Output = String;
    fn ask(self) -> io::Result<String> {
        guard(|| self.theme(&THEME).run())
    }
}

impl Ask for Confirm<'_> {
    type Output = bool;
    fn ask(self) -> io::Result<bool> {
        guard(|| self.theme(&THEME).run())
    }
}

impl Ask for Dialog<'_> {
    type Output = String;
    fn ask(self) -> io::Result<String> {
        guard(|| self.theme(&THEME).run())
    }
}

#[cfg(test)]
mod tests;

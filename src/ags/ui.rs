//! The Aegis palette (the dashboard's) for ags terminal output. Colors are dropped when the
//! output is not a terminal or `NO_COLOR` is set; plain text keeps the terminal's own color.
use console::{Style, StyledObject};

const ACCENT: (u8, u8, u8) = (47, 111, 237);
const MUTED: (u8, u8, u8) = (121, 130, 143);
const OK: (u8, u8, u8) = (63, 185, 80);

/// Where styled text goes; colors follow that stream's terminal detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    Stdout,
    Stderr,
}

fn paint<D>(out: Out, text: D, style: Style) -> StyledObject<D> {
    let style = match out {
        Out::Stdout => style.for_stdout(),
        Out::Stderr => style.for_stderr(),
    };
    style.apply_to(text)
}

fn rgb((r, g, b): (u8, u8, u8)) -> Style {
    Style::new().true_color(r, g, b)
}

pub fn accent<D>(out: Out, text: D) -> StyledObject<D> {
    paint(out, text, rgb(ACCENT))
}
pub fn muted<D>(out: Out, text: D) -> StyledObject<D> {
    paint(out, text, rgb(MUTED))
}
pub fn ok<D>(out: Out, text: D) -> StyledObject<D> {
    paint(out, text, rgb(OK))
}
pub fn bold<D>(out: Out, text: D) -> StyledObject<D> {
    paint(out, text, Style::new().bold())
}

/// Keeps a path note short: `~/Workspace/x` stays, a long one keeps its last two components.
pub fn short_path(path: &str) -> String {
    if path.chars().count() <= 40 {
        return path.to_owned();
    }
    let parts: Vec<_> = path.rsplit('/').take(2).collect();
    format!("…/{}/{}", parts[1], parts[0])
}

#[cfg(test)]
mod tests;

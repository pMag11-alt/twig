//! ANSI colours, enabled only when writing to a terminal (and NO_COLOR isn't set).

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn paint(code: &str, text: &str) -> String {
    if enabled() && !text.is_empty() { format!("\x1b[{code}m{text}\x1b[m") } else { text.to_string() }
}

pub const RED: &str = "31";
pub const GREEN: &str = "32";
pub const YELLOW: &str = "33";
pub const CYAN: &str = "36";
pub const BOLD: &str = "1";
pub const BOLD_CYAN: &str = "1;36";
pub const BOLD_GREEN: &str = "1;32";
pub const BOLD_RED: &str = "1;31";
pub const BOLD_YELLOW: &str = "1;33";

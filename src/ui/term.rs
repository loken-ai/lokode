//! Terminal lifecycle.
//!
//! Entering raw mode and the alternate screen are changes to the user's shell, not to
//! this process, so undoing them cannot depend on the happy path being taken. A panic in
//! a draw call would otherwise hand back a terminal with no echo and no cursor, printing
//! the message the user needs in a mode where it is unreadable.

use std::io;

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

/// Undo everything [`TerminalGuard::new`] turned on.
///
/// Every step is attempted and its failure discarded: this runs from a panic hook, where
/// there is nobody left to report to, and a terminal restored in part beats one left in
/// raw mode. Calling it twice is harmless, which is what lets the guard and the hook both
/// own the job without coordinating.
pub fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        Show
    );
}

/// Owns the terminal's raw mode and alternate screen for as long as it lives.
pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(Self { _private: () })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Restore the terminal before the default hook prints the panic.
///
/// Install this before taking the terminal, so that the window in which a panic could
/// escape uncovered never opens.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        default(info);
    }));
}

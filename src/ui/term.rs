//! Terminal lifecycle.
//!
//! Entering raw mode and the alternate screen are changes to the user's shell, not to
//! this process, so undoing them cannot depend on the happy path being taken. A panic in
//! a draw call would otherwise hand back a terminal with no echo and no cursor, printing
//! the message the user needs in a mode where it is unreadable.

use std::io;

use anyhow::Result;
use crossterm::cursor::Show;
use std::io::Write;
use std::sync::OnceLock;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
};

/// Undo everything [`TerminalGuard::new`] turned on.
///
/// Every step is attempted and its failure discarded: this runs from a panic hook, where
/// there is nobody left to report to, and a terminal restored in part beats one left in
/// raw mode. Calling it twice is harmless, which is what lets the guard and the hook both
/// own the job without coordinating.
pub fn restore() {
    progress(Progress::Off);
    let _ = execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableFocusChange
    );
    // The title the terminal had before, taken back off the stack it was saved on.
    let _ = write!(io::stdout(), "\x1b[23;0t");
    let _ = io::stdout().flush();
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        Show
    );
}

/// Owns the terminal's raw mode and alternate screen for as long as it lives.
pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    /// The wheel scrolls the pane, and the pane does its own selecting, which is what a
    /// captured mouse buys and costs.
    ///
    /// Paste is asked for as well. Without it a terminal hands pasted text over one
    /// keystroke at a time, so every newline in it reads as the send key: pasting 10
    /// lines of a stack trace asked 10 questions, each of them a fragment.
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        // Keys the legacy encoding confuses arrive told apart: Esc from the start of an
        // Alt chord, Shift-Enter from Enter. A terminal that does not know the protocol
        // ignores the request, and one that cannot be sent it refuses it, which changes
        // nothing either.
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        // Told when the window gains and loses focus, so a question waiting on the person
        // can reach them when they are elsewhere.
        let _ = execute!(io::stdout(), EnableFocusChange);
        // The terminal's own title, saved on its stack to be given back on leaving.
        let _ = write!(io::stdout(), "\x1b[22;0t");
        let _ = io::stdout().flush();
        Ok(Self { _private: () })
    }
}

/// Hold what is drawn until the frame is complete, so a terminal that knows the request
/// shows each frame whole. One that does not ignores it.
pub fn begin_frame() {
    let _ = execute!(io::stdout(), BeginSynchronizedUpdate);
}

/// Show the frame held since [`begin_frame`].
pub fn end_frame() {
    let _ = execute!(io::stdout(), EndSynchronizedUpdate);
}

/// Name the window, in the title bar and the list of tabs.
pub fn set_title(title: &str) {
    let _ = execute!(io::stdout(), crossterm::terminal::SetTitle(title));
}

/// What the terminal can be asked beyond drawing, as its environment says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reach {
    /// Whether it shows a progress state in its tab or task bar.
    pub progress: bool,
    pub notify: Notify,
}

/// How the terminal puts a notification on the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    /// `OSC 777;notify`, as Ghostty, WezTerm and foot take it.
    Osc777,
    /// `OSC 9`, as iTerm2 takes it.
    Osc9,
    /// `OSC 99`, kitty's own.
    Osc99,
    /// The bell, which every terminal turns into an alert of its own kind.
    Bell,
}

impl Reach {
    /// Read from the variables a terminal sets about itself.
    ///
    /// Under tmux the sequences beyond the bell never reach the terminal, so only the bell
    /// is used there. iTerm2 shows progress from 3.6 on: before it, the same sequence is a
    /// notification.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        let program = var("TERM_PROGRAM").unwrap_or_default();
        let term = var("TERM").unwrap_or_default();
        if var("TMUX").is_some() {
            return Self {
                progress: false,
                notify: Notify::Bell,
            };
        }
        let iterm_progress = program == "iTerm.app"
            && var("TERM_PROGRAM_VERSION").is_some_and(|v| at_least(&v, &[3, 6]));
        let progress = program == "ghostty"
            || program == "WezTerm"
            || var("WT_SESSION").is_some()
            || iterm_progress;
        let notify = if program == "ghostty" || program == "WezTerm" || term.starts_with("foot") {
            Notify::Osc777
        } else if program == "iTerm.app" {
            Notify::Osc9
        } else if var("KITTY_WINDOW_ID").is_some() {
            Notify::Osc99
        } else {
            Notify::Bell
        };
        Self { progress, notify }
    }
}

/// Whether a dotted version is at least `floor`, compared part by part.
fn at_least(version: &str, floor: &[u32]) -> bool {
    let parts: Vec<u32> = version
        .split('.')
        .map(|p| p.trim().parse().unwrap_or(0))
        .collect();
    for (i, want) in floor.iter().enumerate() {
        let have = parts.get(i).copied().unwrap_or(0);
        if have != *want {
            return have > *want;
        }
    }
    true
}

static REACH: OnceLock<Reach> = OnceLock::new();

/// What this terminal can be asked, read once.
pub fn reach() -> Reach {
    *REACH.get_or_init(|| Reach::from_env(|k| std::env::var(k).ok()))
}

/// The state a progress indicator in the tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Off,
    /// Working, with no telling how far along.
    Busy,
    /// Stopped on something the person has to answer.
    Waiting,
}

/// Show `state` in the terminal's tab or task bar, where it has one.
pub fn progress(state: Progress) {
    if !reach().progress {
        return;
    }
    let code = match state {
        Progress::Off => "0;0",
        Progress::Busy => "3;0",
        Progress::Waiting => "4;100",
    };
    let _ = write!(io::stdout(), "\x1b]9;4;{code}\x07");
    let _ = io::stdout().flush();
}

/// Put `body` in front of the person, on the desktop where the terminal can, or as a bell.
pub fn notify(title: &str, body: &str) {
    let clean = |s: &str| s.replace(['\x07', '\x1b', ';'], " ");
    let (title, body) = (clean(title), clean(body));
    let out = match reach().notify {
        Notify::Osc777 => format!("\x1b]777;notify;{title};{body}\x07"),
        Notify::Osc9 => format!("\x1b]9;{title}: {body}\x07"),
        Notify::Osc99 => format!("\x1b]99;;{title}: {body}\x1b\\"),
        Notify::Bell => "\x07".to_string(),
    };
    let _ = write!(io::stdout(), "{out}");
    let _ = io::stdout().flush();
}

/// Take the mouse, for a wheel that scrolls the pane instead of the terminal.
pub fn capture_mouse(on: bool) -> Result<()> {
    match on {
        true => execute!(io::stdout(), EnableMouseCapture)?,
        false => execute!(io::stdout(), DisableMouseCapture)?,
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reach_of(vars: &[(&str, &str)]) -> Reach {
        Reach::from_env(|k| {
            vars.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        })
    }

    /// A terminal known to show progress gets it, and one that would read the sequence as
    /// a notification does not.
    #[test]
    fn progress_goes_only_where_it_is_shown() {
        assert!(reach_of(&[("TERM_PROGRAM", "ghostty")]).progress);
        assert!(reach_of(&[("WT_SESSION", "x")]).progress);
        assert!(
            reach_of(&[
                ("TERM_PROGRAM", "iTerm.app"),
                ("TERM_PROGRAM_VERSION", "3.6.6")
            ])
            .progress
        );
        assert!(
            !reach_of(&[
                ("TERM_PROGRAM", "iTerm.app"),
                ("TERM_PROGRAM_VERSION", "3.5.14")
            ])
            .progress
        );
        assert!(!reach_of(&[("TERM", "xterm-256color")]).progress);
    }

    /// A notification takes the terminal's own sequence, and the bell under tmux or where
    /// nothing better is known.
    #[test]
    fn a_notification_takes_the_terminals_own_way() {
        assert_eq!(
            reach_of(&[("TERM_PROGRAM", "WezTerm")]).notify,
            Notify::Osc777
        );
        assert_eq!(reach_of(&[("KITTY_WINDOW_ID", "1")]).notify, Notify::Osc99);
        assert_eq!(
            reach_of(&[("TERM_PROGRAM", "iTerm.app")]).notify,
            Notify::Osc9
        );
        assert_eq!(
            reach_of(&[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/s")]).notify,
            Notify::Bell
        );
        assert_eq!(reach_of(&[]).notify, Notify::Bell);
    }
}

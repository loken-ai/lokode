//! The line being typed: completing it, recalling it, and what a key means for it.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use ratatui_textarea::TextArea;

use crate::sandbox::approval::ApprovalDecision;
use crate::ui::commands::command_choices;

/// Whether the cursor is at the very start of the input.
///
/// The test for "the arrows are free to mean history": anywhere else they are how you move
/// around what you are writing, and taking them would throw away a half-written line.
pub(crate) fn at_start(input: &TextArea) -> bool {
    input.cursor() == (0, 0)
}

/// An input holding `text`, with the cursor after it.
///
/// After it, because everything that puts text here puts it there to be carried on with:
/// a recalled line to be edited, a command just completed to be given an argument. A
/// cursor left at the start makes the next keystroke insert in front of what is there and
/// backspace do nothing at all.
pub(crate) fn fresh_input(text: &str) -> TextArea<'static> {
    let mut input = TextArea::from(text.lines().map(String::from).collect::<Vec<_>>());
    // The line being typed is not underlined: the cursor says where it is.
    input.set_cursor_line_style(ratatui::style::Style::default());
    // An empty line says what it is for, every time it is empty.
    input.set_placeholder_text("ask for something   / commands   @ files   ! a command of yours");
    input.set_placeholder_style(crate::ui::theme::theme().dim);
    input.move_cursor(ratatui_textarea::CursorMove::End);
    input.move_cursor(ratatui_textarea::CursorMove::Bottom);
    input.move_cursor(ratatui_textarea::CursorMove::End);
    input
}

/// How long an offer to leave stands before it has to be made again.
///
/// Long enough to be a deliberate second press, short enough that a Ctrl-C typed minutes
/// apart is two interruptions rather than an exit.
pub(crate) const ASK_AGAIN_WITHIN: Duration = Duration::from_secs(3);

/// What a key says about leaving.
pub(crate) enum Quit {
    /// Stop what is running, clear what is typed, or offer to leave - in that order,
    /// because that is the order of what is in front of the person.
    Ask,
    /// The end-of-input key: on an empty line it means what it has always meant.
    OnEmptyLine,
    /// Asked for in a way that leaves no doubt.
    Now,
}

pub(crate) fn quit_intent(k: &KeyEvent) -> Option<Quit> {
    if k.kind != KeyEventKind::Press || !k.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match k.code {
        KeyCode::Char('c') => Some(Quit::Ask),
        KeyCode::Char('d') => Some(Quit::OnEmptyLine),
        KeyCode::Char('q') => Some(Quit::Now),
        _ => None,
    }
}

/// Hand the line being typed to the editor named by the environment, and take back what
/// comes out.
///
/// The terminal is given up for as long as it runs and taken again afterwards, because two
/// programs drawing on one screen is one program's output. `None` when nothing came back
/// that was worth taking: an editor closed without saving has said no.
pub(crate) async fn write_elsewhere(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    carried: &str,
) -> Result<Option<String>> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let path = std::env::temp_dir().join(format!("lokode-{}.md", std::process::id()));
    std::fs::write(&path, carried)?;

    crate::ui::term::restore();
    let ran = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status()
        .await;
    // Taken back whatever the editor did, including failing to start: the alternative is a
    // terminal in the state an editor left it and an interface drawing into it.
    let _ = crate::ui::term::TerminalGuard::new();
    let _ = term.clear();
    ran?;

    let written = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    let written = written.trim_end_matches('\n').to_string();
    Ok((written != carried && !written.trim().is_empty()).then_some(written))
}

/// The list the line being typed calls for, carrying over the one already open.
///
/// Rebuilt from the line rather than driven by keystrokes, so the list and the text can
/// never disagree about what is being completed.
pub(crate) fn open_picker(
    input: &TextArea,
    open: Option<crate::ui::picker::Picker>,
    root: &std::path::Path,
    known: &Known<'_>,
) -> Option<crate::ui::picker::Picker> {
    use crate::ui::picker::Kind;
    let line = input.lines().join("\n");
    let (kind, _, token) = crate::ui::picker::token_at(&line)?;
    let mut picker = match open {
        Some(p) if *p.kind() == kind => p,
        _ => crate::ui::picker::Picker::new(
            kind.clone(),
            match &kind {
                Kind::Command => command_choices(root),
                Kind::Path => path_choices(root),
                Kind::Argument(verb) => argument_choices(verb, known),
            },
        ),
    };
    picker.narrow(&token);
    (!picker.is_empty()).then_some(picker)
}

/// What the interface can offer as the argument of a command.
pub(crate) struct Known<'a> {
    pub(crate) models: &'a [String],
    pub(crate) mcp: &'a [std::sync::Arc<crate::mcp::client::McpClient>],
}

/// What a command may be given, for finishing it as it is typed.
///
/// A command whose argument is a name nobody keeps in their head is a command that can
/// only be used by somebody who does. The names are all known here already.
pub(crate) fn argument_choices(verb: &str, known: &Known<'_>) -> Vec<crate::ui::picker::Choice> {
    use crate::ui::picker::Choice;
    match verb {
        "model" | "retry" => known
            .models
            .iter()
            .map(|m| Choice::new(m.clone(), ""))
            .collect(),
        "mcp" => known
            .mcp
            .iter()
            .map(|c| Choice::new(c.server.clone(), "tool server"))
            .collect(),
        "digest" => vec![Choice::new("forget", "throw away what was worked out")],
        "copy" => vec![Choice::new("last", "the answer alone")],
        _ => Vec::new(),
    }
}

/// Whether what is typed already is what is highlighted.
///
/// A command typed in full and matched exactly has nothing left to complete, so the key
/// that sends should send rather than replace the line with itself.
pub(crate) fn nothing_left_to_complete(
    input: &TextArea,
    picker: &Option<crate::ui::picker::Picker>,
) -> bool {
    let Some(picker) = picker else { return true };
    let line = input.lines().join("\n");
    let Some((_, _, token)) = crate::ui::picker::token_at(&line) else {
        return true;
    };
    picker.chosen().is_some_and(|c| c == token)
}

/// The line with the token being completed replaced by what was chosen.
pub(crate) fn accept_choice(
    input: &TextArea,
    picker: Option<&crate::ui::picker::Picker>,
) -> Option<String> {
    use crate::ui::picker::Kind;
    let line = input.lines().join("\n");
    let chosen = picker?.chosen()?;
    let (kind, at, _) = crate::ui::picker::token_at(&line)?;
    // A command is the whole line and is ready to send; an argument finishes one; a file
    // is one word in a sentence and the sentence carries on after it.
    Some(match kind {
        Kind::Command => format!("/{chosen}"),
        Kind::Argument(_) => format!("{}{chosen}", &line[..at]),
        Kind::Path => format!("{}@{chosen}", &line[..at]),
    })
}

/// The files and directories under the root, for naming one with `@`.
///
/// Bounded and gitignore-aware, like everything else that walks the project: a list is for
/// choosing from, and a list of every file in a large repository is for scrolling through.
pub(crate) fn path_choices(root: &std::path::Path) -> Vec<crate::ui::picker::Choice> {
    const MOST: usize = 400;
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .git_ignore(true)
        .require_git(false)
        .hidden(true)
        .build()
        .flatten()
    {
        if entry.depth() == 0 {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(crate::ui::picker::Choice::new(
            match is_dir {
                true => format!("{}/", rel.display()),
                false => rel.display().to_string(),
            },
            "",
        ));
        if out.len() >= MOST {
            break;
        }
    }
    out.sort_by(|a, b| a.value.cmp(&b.value));
    out
}

/// Which decision a keystroke stands for at the approval modal.
///
/// `Enter` is deliberately absent. It is also the send key, so a user mid-sentence when
/// the modal appears would approve a shell command they never read - the one outcome an
/// approval flow exists to prevent. Approving is a letter you have to reach for.
pub(crate) fn approval_key(k: &KeyEvent) -> Option<ApprovalDecision> {
    if k.kind != KeyEventKind::Press {
        return None;
    }
    match k.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::Once),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            Some(ApprovalDecision::Deny(None))
        }
        KeyCode::Char('a') | KeyCode::Char('A') => Some(ApprovalDecision::Always),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Enter is the send key. If it also approves, a user who is mid-sentence when a
    /// prompt appears authorises a shell command they never read.
    #[test]
    fn enter_does_not_approve() {
        assert_eq!(approval_key(&press(KeyCode::Enter)), None);
    }

    /// A refusal with nothing said is still a refusal: the keys that back out keep
    /// working, and what they mean does not change.
    #[test]
    fn refusing_says_nothing_unless_something_was_said() {
        assert_eq!(
            approval_key(&press(KeyCode::Char('n'))),
            Some(ApprovalDecision::Deny(None))
        );
        // The key that asks for a reason is not itself a decision: the decision is made
        // when what was typed is sent.
        assert_eq!(approval_key(&press(KeyCode::Char('e'))), None);
        assert_eq!(
            crate::sandbox::approval::denial(Some("use the test runner".into())),
            "user denied: use the test runner"
        );
        assert_eq!(crate::sandbox::approval::denial(None), "user denied");
    }

    /// Declining must stay reachable by the two gestures people already use to back out.
    #[test]
    fn declining_is_reachable() {
        assert_eq!(
            approval_key(&press(KeyCode::Esc)),
            Some(ApprovalDecision::Deny(None))
        );
        assert_eq!(
            approval_key(&press(KeyCode::Char('n'))),
            Some(ApprovalDecision::Deny(None))
        );
    }

    #[test]
    fn approving_takes_a_letter() {
        assert_eq!(
            approval_key(&press(KeyCode::Char('y'))),
            Some(ApprovalDecision::Once)
        );
        assert_eq!(
            approval_key(&press(KeyCode::Char('Y'))),
            Some(ApprovalDecision::Once)
        );
        assert_eq!(
            approval_key(&press(KeyCode::Char('a'))),
            Some(ApprovalDecision::Always)
        );
    }

    /// A key with no meaning at the modal must leave it up rather than pick for the user.
    #[test]
    fn an_unrelated_key_decides_nothing() {
        assert_eq!(approval_key(&press(KeyCode::Char('k'))), None);
        assert_eq!(approval_key(&press(KeyCode::Tab)), None);
    }

    /// What each of the leaving keys means, and what an ordinary key does not mean.
    ///
    /// Ctrl-C asks rather than ends: it used to end a session outright, in the middle of a
    /// turn, taking with it a change history nobody had looked at. Ctrl-D is the
    /// end-of-input key and means what it means everywhere, on an empty line. Ctrl-Q is
    /// the one that leaves no doubt.
    #[test]
    fn the_keys_that_leave_are_told_apart() {
        assert!(matches!(quit_intent(&ctrl('c')), Some(Quit::Ask)));
        assert!(matches!(quit_intent(&ctrl('d')), Some(Quit::OnEmptyLine)));
        assert!(matches!(quit_intent(&ctrl('q')), Some(Quit::Now)));
        // A letter is a letter.
        assert!(quit_intent(&press(KeyCode::Char('c'))).is_none());
        assert!(quit_intent(&press(KeyCode::Char('d'))).is_none());
        assert_eq!(approval_key(&ctrl('c')), None);
    }
}

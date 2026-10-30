//! The styles every part of the interface draws with.
//!
//! Text keeps to the terminal's own named colours.
//! Its theme already made them legible on its background.
//! What cannot be named is computed from the background itself.
//! That covers the tint that sets a surface apart and the colour of code.
//! Both need the terminal to say what its colours are and to take any colour asked of it.
//! Without that they fall back to named colours and an untinted surface.

use std::sync::OnceLock;
use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};

/// How long the terminal is given to say what its colours are.
const ASK_FOR: Duration = Duration::from_millis(200);

/// How far a raised surface moves from the background towards the text, out of 255.
const TINT: u8 = 14;

/// How far code moves from the text towards the accent, out of 255.
const CODE_TOWARDS_ACCENT: u8 = 60;

/// How far a list or a page over the rest moves from the background, out of 255.
const RAISED: u8 = 26;

/// The colour the highlighted row and the person's band lean towards.
const ACCENT: (u8, u8, u8) = (86, 182, 194);

/// How far the highlighted row of a list moves towards the accent, out of 255.
const CHOSEN: u8 = 80;

/// How far the band under the person's words moves towards the accent, out of 255.
const BAND: u8 = 24;

/// The colours an added and a removed line lean towards, when the colours are known.
const ADDED: (u8, u8, u8) = (70, 190, 110);
const REMOVED: (u8, u8, u8) = (220, 80, 80);

/// How far a changed line moves from the background towards its colour, out of 255.
const LINE_TINT: u8 = 36;

/// How far a changed word moves from the background towards its colour, out of 255.
const WORD_TINT: u8 = 90;

/// The accent the colour of code leans towards, when the colours are known.
const CODE_ACCENT: (u8, u8, u8) = (90, 160, 220);

/// The styles, by what they are used for.
#[derive(Debug, Clone)]
pub struct Theme {
    /// The status line and anything else in the program's own voice at full strength.
    pub text: Color,
    /// What the program says about itself, evidence, hints: present and receding.
    pub dim: Style,
    /// The prompt, the person's own words, what is highlighted.
    pub accent: Color,
    /// A tool's name and what it acts on.
    pub tool: Color,
    /// Something waiting on the person, or getting close to a limit.
    pub warn: Color,
    /// Something that went wrong.
    pub bad: Color,
    /// Something that went right, and lines a diff adds.
    pub good: Color,
    /// A heading in the model's prose.
    pub heading: Style,
    /// Inline code in the model's prose.
    pub inline_code: Style,
    /// A line inside a fenced block.
    pub code: Style,
    /// The hunk header of a diff.
    pub hunk: Color,
    /// Text drawn on a coloured badge or highlight.
    pub ink: Color,
    /// A key named on the line of keys.
    pub key: Style,
    /// What the mouse has selected.
    pub selected: Style,
    /// What a search found.
    pub found: Style,
    /// The background of a raised surface, when the terminal can show one.
    pub tint: Option<Color>,
    /// The background of a list or a page opened over the rest, raised above a tint.
    pub raised: Option<Color>,
    /// The highlighted row of a list.
    pub chosen_row: Style,
    /// The band under the person's own words.
    pub band: Option<Color>,
    /// The colours of code, by what each part of it is.
    pub syntax: Syntax,
    /// How a diff's added and removed lines are drawn.
    pub diff: DiffStyle,
}

/// How a diff's added and removed lines are drawn, and the words that changed in them.
#[derive(Debug, Clone)]
pub struct DiffStyle {
    /// Whether the lines sit on a tint, which leaves the code on them its own colours.
    pub tinted: bool,
    pub added: Style,
    pub removed: Style,
    pub added_word: Style,
    pub removed_word: Style,
}

impl Default for DiffStyle {
    /// Named colours for the lines, and the changed words in bold and underlined.
    fn default() -> Self {
        let word = Modifier::BOLD | Modifier::UNDERLINED;
        Self {
            tinted: false,
            added: Style::default().fg(Color::Green),
            removed: Style::default().fg(Color::Red),
            added_word: Style::default().fg(Color::Green).add_modifier(word),
            removed_word: Style::default().fg(Color::Red).add_modifier(word),
        }
    }
}

/// The colours of code, in the terminal's named colours.
#[derive(Debug, Clone)]
pub struct Syntax {
    pub comment: Style,
    pub string: Style,
    pub number: Style,
    pub keyword: Style,
    pub function: Style,
    /// Type and class names.
    pub kind: Style,
}

impl Default for Syntax {
    fn default() -> Self {
        Self {
            comment: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            string: Style::default().fg(Color::Green),
            number: Style::default().fg(Color::Yellow),
            keyword: Style::default().fg(Color::Magenta),
            function: Style::default().fg(Color::Blue),
            kind: Style::default().fg(Color::Cyan),
        }
    }
}

impl Default for Theme {
    /// The named colours alone, which is what a terminal that says nothing gets.
    fn default() -> Self {
        Self {
            text: Color::Gray,
            dim: Style::default().fg(Color::DarkGray),
            accent: Color::Cyan,
            tool: Color::Yellow,
            warn: Color::Yellow,
            bad: Color::Red,
            good: Color::Green,
            heading: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            inline_code: Style::default().fg(Color::Yellow),
            code: Style::default().fg(Color::Rgb(180, 200, 220)),
            hunk: Color::Cyan,
            ink: Color::Black,
            key: Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
            selected: Style::default()
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            found: Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            tint: None,
            raised: None,
            chosen_row: Style::default().fg(Color::Black).bg(Color::Cyan),
            band: None,
            syntax: Syntax::default(),
            diff: DiffStyle::default(),
        }
    }
}

/// A colour as its 3 channels.
type Rgb = (u8, u8, u8);

/// `a` moved towards `b` by `by` out of 255.
fn mix(a: Rgb, b: Rgb, by: u8) -> Rgb {
    let one = |x: u8, y: u8| -> u8 {
        let (x, y, by) = (x as u32, y as u32, by as u32);
        ((x * (255 - by) + y * by) / 255) as u8
    };
    (one(a.0, b.0), one(a.1, b.1), one(a.2, b.2))
}

/// Whether a background is a light one, by its perceived brightness.
fn is_light(bg: Rgb) -> bool {
    let (r, g, b) = (bg.0 as u32, bg.1 as u32, bg.2 as u32);
    (299 * r + 587 * g + 114 * b) / 1000 > 128
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

impl Theme {
    /// The colour of receding text, for places that take a colour rather than a style.
    pub fn faint(&self) -> Color {
        self.dim.fg.unwrap_or(Color::Reset)
    }

    /// The theme for a terminal whose text and background colours are known.
    ///
    /// `truecolor` says whether it takes any colour asked of it.
    /// One that does not would round a tint to the nearest of its own colours.
    /// That colour is rarely near.
    pub fn for_colours(fg: Rgb, bg: Rgb, truecolor: bool) -> Self {
        let mut theme = Self::default();
        if is_light(bg) {
            // On a light background the bright named colours are the faint ones.
            theme.heading = Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);
            theme.key = Style::default().add_modifier(Modifier::BOLD);
            theme.text = Color::Reset;
            theme.ink = Color::White;
            theme.code = Style::default().fg(Color::Blue);
        }
        if truecolor {
            theme.tint = Some(rgb(mix(bg, fg, TINT)));
            theme.raised = Some(rgb(mix(bg, fg, RAISED)));
            theme.chosen_row = Style::default()
                .fg(rgb(fg))
                .bg(rgb(mix(bg, ACCENT, CHOSEN)));
            theme.band = Some(rgb(mix(bg, ACCENT, BAND)));
            let on = |towards: Rgb, by: u8| Style::default().bg(rgb(mix(bg, towards, by)));
            theme.diff = DiffStyle {
                tinted: true,
                added: on(ADDED, LINE_TINT),
                removed: on(REMOVED, LINE_TINT),
                added_word: on(ADDED, WORD_TINT),
                removed_word: on(REMOVED, WORD_TINT),
            };
            theme.code = Style::default().fg(rgb(mix(fg, CODE_ACCENT, CODE_TOWARDS_ACCENT)));
        }
        theme
    }

    /// Ask the terminal what its colours are, and build the theme for them.
    ///
    /// Asked before the screen is taken, since the answer arrives on the input the event loop
    /// reads afterwards.
    /// A terminal that does not answer gets the named colours.
    pub fn detect() -> Self {
        let mut options = terminal_colorsaurus::QueryOptions::default();
        options.timeout = ASK_FOR;
        match terminal_colorsaurus::color_palette(options) {
            Ok(p) => {
                let eight = |c: terminal_colorsaurus::Color| -> Rgb {
                    let (r, g, b) = c.scale_to_8bit();
                    (r, g, b)
                };
                Self::for_colours(eight(p.foreground), eight(p.background), truecolor())
            }
            // No answer, which is what tmux and many terminals give.
            // The background is read off `COLORFGBG` where the terminal sets it.
            // Otherwise it is taken to be a dark one.
            // A terminal that takes any colour can still be given tints.
            Err(_) if truecolor() => {
                let bg = std::env::var("COLORFGBG")
                    .ok()
                    .and_then(|v| background_of_colorfgbg(&v))
                    .unwrap_or(ASSUMED_DARK);
                let fg = match is_light(bg) {
                    true => (30, 30, 30),
                    false => (208, 208, 208),
                };
                Self::for_colours(fg, bg, true)
            }
            Err(_) => Self::default(),
        }
    }
}

/// The background taken when a terminal that takes any colour does not say which it has.
const ASSUMED_DARK: Rgb = (24, 26, 30);

/// The background `COLORFGBG` names, as `fg;bg` with ANSI colour numbers.
/// 0 to 6 and 8 are dark backgrounds, 7 and 15 light ones.
fn background_of_colorfgbg(value: &str) -> Option<Rgb> {
    let bg: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    Some(match bg {
        7 | 15 => (250, 250, 250),
        0..=6 | 8 => ASSUMED_DARK,
        _ => return None,
    })
}

/// Whether the terminal takes any colour asked of it, as it says through the environment.
fn truecolor() -> bool {
    takes_any_colour(|k| std::env::var(k).ok())
}

/// Whether the variables a terminal sets say it takes any colour.
/// `COLORTERM` says so when it is set.
/// Otherwise the terminals known to take any colour without saying it.
fn takes_any_colour(var: impl Fn(&str) -> Option<String>) -> bool {
    let colorterm = var("COLORTERM").unwrap_or_default();
    let term = var("TERM").unwrap_or_default();
    let program = var("TERM_PROGRAM").unwrap_or_default();
    colorterm == "truecolor"
        || colorterm == "24bit"
        || term.ends_with("-direct")
        || term.contains("truecolor")
        || matches!(
            program.as_str(),
            "ghostty" | "WezTerm" | "iTerm.app" | "vscode"
        )
        || [
            "WT_SESSION",
            "KITTY_WINDOW_ID",
            "ALACRITTY_SOCKET",
            "KONSOLE_VERSION",
        ]
        .iter()
        .any(|k| var(k).is_some())
        || var("VTE_VERSION")
            .and_then(|v| v.parse::<u32>().ok())
            .is_some_and(|v| v >= 3600)
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Settle the theme for the rest of the process.
/// Only the first call counts.
pub fn set(theme: Theme) {
    let _ = THEME.set(theme);
}

/// The theme in force: the one settled, or the named colours when none was.
pub fn theme() -> &'static Theme {
    THEME.get_or_init(Theme::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing_moves_from_one_colour_towards_the_other() {
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 0), (0, 0, 0));
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 255), (255, 255, 255));
        assert_eq!(mix((0, 100, 200), (255, 100, 0), 51), (51, 100, 160));
    }

    #[test]
    fn light_and_dark_backgrounds_are_told_apart() {
        assert!(is_light((250, 250, 245)));
        assert!(is_light((238, 232, 213)));
        assert!(!is_light((0, 43, 54)));
        assert!(!is_light((15, 18, 23)));
    }

    /// A tint sits just off the background, on the side of the text.
    #[test]
    fn a_tint_is_a_little_towards_the_text() {
        let dark = Theme::for_colours((200, 200, 200), (10, 10, 10), true);
        let Some(Color::Rgb(r, _, _)) = dark.tint else {
            panic!("a terminal that takes any colour gets a tint")
        };
        assert!(r > 10 && r < 40, "{r}");
        let light = Theme::for_colours((30, 30, 30), (250, 250, 250), true);
        let Some(Color::Rgb(r, _, _)) = light.tint else {
            panic!("a terminal that takes any colour gets a tint")
        };
        assert!(r < 250 && r > 220, "{r}");
    }

    /// A terminal that takes any colour is recognised whether or not it says so.
    #[test]
    fn a_terminal_that_takes_any_colour_is_recognised() {
        let of = |vars: &[(&str, &str)]| {
            takes_any_colour(|k| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            })
        };
        assert!(of(&[("COLORTERM", "truecolor")]));
        assert!(of(&[("TERM_PROGRAM", "ghostty")]));
        assert!(of(&[("TERM", "xterm-direct")]));
        assert!(of(&[("VTE_VERSION", "7600")]));
        assert!(!of(&[("VTE_VERSION", "3400")]));
        assert!(!of(&[("TERM", "xterm-256color")]));
    }

    /// `COLORFGBG` says whether the background is light or dark.
    #[test]
    fn colorfgbg_names_the_background() {
        assert!(is_light(background_of_colorfgbg("0;15").unwrap()));
        assert!(!is_light(background_of_colorfgbg("15;0").unwrap()));
        assert!(!is_light(background_of_colorfgbg("7;default;8").unwrap()));
        assert_eq!(background_of_colorfgbg("nonsense"), None);
    }

    /// Without any colour on offer there is no tint, and named colours stand in.
    #[test]
    fn a_terminal_of_named_colours_gets_no_tint() {
        let t = Theme::for_colours((30, 30, 30), (250, 250, 250), false);
        assert_eq!(t.tint, None);
        assert_eq!(t.code.fg, Some(Color::Blue));
    }
}

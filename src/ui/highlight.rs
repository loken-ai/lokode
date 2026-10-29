//! Code coloured by what each part of it is: comments, strings, numbers, keywords,
//! functions and types, each in the theme's colour for it.
//!
//! The grammars are the TextMate grammars syntaxmate bundles, read once on first use. A
//! language they do not know is left to the caller to draw plain.

use std::sync::OnceLock;

use ratatui::style::Style;
use ratatui::text::Span;
use syntaxmate::{Highlighter, Scopes};

use crate::ui::theme::theme;

static HIGHLIGHTER: OnceLock<Option<Highlighter>> = OnceLock::new();

fn highlighter() -> Option<&'static Highlighter> {
    HIGHLIGHTER
        .get_or_init(|| Highlighter::bundled().ok())
        .as_ref()
}

/// Read the grammars now, so the first code block does not wait for them.
pub fn warm() {
    highlighter();
}

/// The rows of `code` in `lang`, each a list of spans over `base`, or `None` when the
/// language is not one the grammars know.
///
/// A line the grammars could only partly read keeps the colours they found and the rest
/// of it in `base`.
pub fn rows(lang: &str, code: &str, base: Style) -> Option<Vec<Vec<Span<'static>>>> {
    if lang.is_empty() {
        return None;
    }
    let document = highlighter()?.tokenize(lang, code).ok()?;
    let mut tokenized = document.lines().iter();
    let mut out = Vec::new();
    for line in code.lines() {
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut at = 0usize;
        if let Some(tokens) = tokenized.next() {
            for token in tokens.tokens() {
                let range = token.range();
                if range.start < at || range.end > line.len() {
                    break;
                }
                push(&mut row, &line[at..range.start], base);
                push(
                    &mut row,
                    &line[range.clone()],
                    style_of(token.scopes(), base),
                );
                at = range.end;
            }
        }
        push(&mut row, &line[at..], base);
        out.push(row);
    }
    Some(out)
}

fn push(row: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if !text.is_empty() {
        row.push(Span::styled(text.to_string(), style));
    }
}

/// What the innermost scope that names a kind of code says the text is.
fn style_of(scopes: Scopes<'_>, base: Style) -> Style {
    let syntax = &theme().syntax;
    let scopes: Vec<&str> = scopes.collect();
    for scope in scopes.into_iter().rev() {
        let style = match kind_of(scope) {
            Kind::Comment => syntax.comment,
            Kind::String => syntax.string,
            Kind::Number => syntax.number,
            Kind::Keyword => syntax.keyword,
            Kind::Function => syntax.function,
            Kind::Type => syntax.kind,
            Kind::Plain => continue,
        };
        return base.patch(style);
    }
    base
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Comment,
    String,
    Number,
    Keyword,
    Function,
    Type,
    Plain,
}

/// The kind of code a scope names, from the first atoms of its name.
fn kind_of(name: &str) -> Kind {
    let starts = |p: &str| {
        name.strip_prefix(p)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    };
    if starts("comment") {
        Kind::Comment
    } else if starts("string") || starts("constant.character") {
        Kind::String
    } else if starts("constant.numeric") || starts("constant.language") {
        Kind::Number
    } else if starts("keyword") || starts("storage") {
        Kind::Keyword
    } else if starts("entity.name.function") || starts("support.function") {
        Kind::Function
    } else if starts("entity.name.type")
        || starts("entity.name.class")
        || starts("entity.name.struct")
        || starts("entity.name.enum")
        || starts("support.type")
        || starts("support.class")
    {
        Kind::Type
    } else {
        Kind::Plain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lang: &str, code: &str) -> Vec<(String, Style)> {
        rows(lang, code, Style::default())
            .expect("a known language")
            .into_iter()
            .flatten()
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    /// A keyword, a string and a comment each take their own colour, and the text comes
    /// out whole.
    #[test]
    fn rust_is_coloured_by_what_each_part_is() {
        let syntax = &theme().syntax;
        let spans = kinds("rust", "let s = \"hi\"; // greet\n");
        let joined: String = spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "let s = \"hi\"; // greet");
        let style_of = |text: &str| {
            spans
                .iter()
                .find(|(t, _)| t.contains(text))
                .map(|(_, s)| *s)
                .unwrap_or_else(|| panic!("{text} in {spans:?}"))
        };
        assert_eq!(style_of("let"), syntax.keyword);
        assert_eq!(style_of("hi"), syntax.string);
        assert_eq!(style_of("greet"), syntax.comment);
    }

    /// Every line comes back, in order, with nothing added or lost.
    #[test]
    fn every_line_comes_back_whole() {
        let code = "fn main() {\n    println!(\"é\");\n}";
        let rows = rows("rust", code, Style::default()).expect("a known language");
        let text: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text, code.lines().collect::<Vec<_>>());
    }

    /// The common short names of a language are known, TypeScript and TOML included.
    #[test]
    fn short_names_are_known() {
        for lang in [
            "rs", "py", "sh", "js", "ts", "toml", "json", "c", "go", "yaml",
        ] {
            assert!(rows(lang, "x\n", Style::default()).is_some(), "{lang}");
        }
    }

    /// A language the grammars do not know is left to be drawn plain.
    #[test]
    fn an_unknown_language_is_left_plain() {
        assert!(rows("klingon", "x\n", Style::default()).is_none());
        assert!(rows("", "x\n", Style::default()).is_none());
    }
}

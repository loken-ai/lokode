//! Columns as a person counts them, and as a language server counts them.
//!
//! The tools speak in characters, 1-indexed.
//! A server counts code units of the encoding it agreed to at startup.
//! The encoding is UTF-16 unless the server says otherwise.
//!
//! On a line with an accent or an emoji the 2 counts differ.
//! A column passed through unconverted points at the wrong place.

/// The unit a server counts columns in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    Utf8,
    #[default]
    Utf16,
    Utf32,
}

impl Encoding {
    /// The encoding a server names in its capabilities, UTF-16 for any other name.
    pub fn from_name(name: &str) -> Self {
        match name {
            "utf-8" => Encoding::Utf8,
            "utf-32" => Encoding::Utf32,
            _ => Encoding::Utf16,
        }
    }

    fn width(self, c: char) -> usize {
        match self {
            Encoding::Utf8 => c.len_utf8(),
            Encoding::Utf16 => c.len_utf16(),
            Encoding::Utf32 => 1,
        }
    }
}

/// The units `enc` counts for the first `chars` characters of `line`.
/// A count past the end of the line stops at its end.
pub fn units(line: &str, chars: usize, enc: Encoding) -> u32 {
    line.chars()
        .take(chars)
        .map(|c| enc.width(c))
        .sum::<usize>() as u32
}

/// How many characters of `line` the first `units` units of `enc` cover.
/// A unit count that falls inside a character counts that character.
pub fn chars(line: &str, units: u32, enc: Encoding) -> usize {
    let mut seen = 0usize;
    let mut out = 0usize;
    for c in line.chars() {
        if seen >= units as usize {
            break;
        }
        seen += enc.width(c);
        out += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An accent takes 2 bytes and one UTF-16 unit.
    /// An emoji takes 4 bytes and 2 units.
    #[test]
    fn a_column_is_counted_in_the_server_s_units() {
        let line = "é🙂x";
        assert_eq!(units(line, 2, Encoding::Utf8), 6);
        assert_eq!(units(line, 2, Encoding::Utf16), 3);
        assert_eq!(units(line, 2, Encoding::Utf32), 2);
        assert_eq!(chars(line, 6, Encoding::Utf8), 2);
        assert_eq!(chars(line, 3, Encoding::Utf16), 2);
        assert_eq!(chars(line, 2, Encoding::Utf32), 2);
        // Past the end, both stop at the line's end.
        assert_eq!(units(line, 10, Encoding::Utf16), 4);
        assert_eq!(chars(line, 10, Encoding::Utf16), 3);
    }
}

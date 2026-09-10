//! Putting text on the clipboard through the terminal itself.
//!
//! The pane is on an alternate screen, so there is no scrollback to select from, and the
//! program cannot reach a clipboard the way a windowed application would - least of all
//! over ssh, which is where a local-first agent is often driven from.
//!
//! The terminal can, and has been able to since OSC 52: a control sequence carrying
//! base64, which the terminal puts on its own clipboard, on whichever machine it is
//! running. No dependency, no daemon, and it works through ssh and tmux.

use std::io::Write;

/// How much may be sent. Terminals bound what they will accept and a conversation is not a
/// file transfer; past this the copy is written to disk instead.
pub const MOST: usize = 100_000;

/// Put `text` on the terminal's clipboard.
///
/// Best effort by construction: the terminal either understands the sequence or ignores
/// it, and says nothing either way, so the caller must always offer another way out.
pub fn put(text: &str) -> std::io::Result<()> {
    let encoded = base64(text.as_bytes());
    let mut out = std::io::stdout();
    // tmux passes the sequence through only when told to; the wrapper is what tells it.
    let inner = format!("\x1b]52;c;{encoded}\x07");
    if std::env::var_os("TMUX").is_some() {
        write!(out, "\x1bPtmux;\x1b{inner}\x1b\\")?;
    } else {
        write!(out, "{inner}")?;
    }
    out.flush()
}

/// Base64, written out rather than depended on: one table and three shifts.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(match chunk.len() > 1 {
            true => TABLE[(n >> 6) as usize & 63] as char,
            false => '=',
        });
        out.push(match chunk.len() > 2 {
            true => TABLE[n as usize & 63] as char,
            false => '=',
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The examples from the specification, including both shapes of padding.
    #[test]
    fn base64_matches_the_specification() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn text_that_is_not_ascii_survives_the_encoding() {
        assert_eq!(base64("é".as_bytes()), "w6k=");
    }
}

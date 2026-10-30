//! A shell command line, read for the commands it runs.
//!
//! A rule about commands is matched against each simple command of a line.
//! The parts are joined by `;`, `&&`, `||`, `|`, `&` or a new line.
//! The commands substituted with `$( )`, backquotes, `<( )` or `>( )` are parts too.
//!
//! A line that does something its parts do not show is marked opaque.
//! That is a write to a file through a redirection, a here-document, or a command run from a
//! string by `eval` or `sh -c`.

/// A command line read as the simple commands it runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parts {
    /// Each simple command, without its leading variable assignments.
    /// Its words are joined by one space.
    pub commands: Vec<String>,
    /// Whether the line does something its commands do not show.
    pub opaque: bool,
}

/// Read `line` for the commands it runs.
pub fn parts(line: &str) -> Parts {
    let mut reader = Reader {
        chars: line.chars().collect(),
        at: 0,
        out: Parts::default(),
    };
    reader.line(None);
    reader.out
}

struct Reader {
    chars: Vec<char>,
    at: usize,
    out: Parts,
}

/// The programs that run a string given to them as a command.
const RUNS_A_STRING: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

impl Reader {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    /// Read commands until the end, or until `close` outside quotes.
    fn line(&mut self, close: Option<char>) {
        let mut words: Vec<String> = Vec::new();
        let mut word = String::new();
        let mut in_word = false;
        while let Some(c) = self.peek(0) {
            if Some(c) == close {
                self.at += 1;
                break;
            }
            match c {
                '\\' => {
                    if let Some(next) = self.peek(1) {
                        if next != '\n' {
                            word.push(next);
                            in_word = true;
                        }
                    }
                    self.at += 2;
                }
                '\'' => {
                    self.at += 1;
                    while let Some(q) = self.peek(0) {
                        self.at += 1;
                        if q == '\'' {
                            break;
                        }
                        word.push(q);
                    }
                    in_word = true;
                }
                '"' => {
                    self.at += 1;
                    self.double_quoted(&mut word);
                    in_word = true;
                }
                '$' if self.peek(1) == Some('(') => {
                    self.at += 2;
                    word.push_str("$()");
                    in_word = true;
                    self.line(Some(')'));
                }
                '`' => {
                    self.at += 1;
                    word.push_str("``");
                    in_word = true;
                    self.line(Some('`'));
                }
                '<' | '>' if self.peek(1) == Some('(') => {
                    self.at += 2;
                    word.push(c);
                    word.push_str("()");
                    in_word = true;
                    self.line(Some(')'));
                }
                '#' if !in_word => {
                    while let Some(n) = self.peek(0) {
                        if n == '\n' {
                            break;
                        }
                        self.at += 1;
                    }
                }
                ';' | '&' | '|' | '\n' | '(' | ')' => {
                    self.at += 1;
                    if c == '&' && self.peek(0) == Some('>') {
                        // `&>` sends both streams to a file.
                        self.at -= 1;
                        self.redirection(&mut word, &mut in_word, &mut words);
                        continue;
                    }
                    if matches!(c, '&' | '|') && matches!(self.peek(0), Some('&' | '|')) {
                        self.at += 1;
                    }
                    end_word(&mut word, &mut in_word, &mut words);
                    self.end_command(&mut words);
                }
                '<' | '>' => self.redirection(&mut word, &mut in_word, &mut words),
                c if c.is_whitespace() => {
                    self.at += 1;
                    end_word(&mut word, &mut in_word, &mut words);
                }
                c => {
                    self.at += 1;
                    word.push(c);
                    in_word = true;
                }
            }
        }
        end_word(&mut word, &mut in_word, &mut words);
        self.end_command(&mut words);
    }

    /// Read up to the closing quote, taking the substitutions inside.
    fn double_quoted(&mut self, word: &mut String) {
        while let Some(q) = self.peek(0) {
            match q {
                '"' => {
                    self.at += 1;
                    return;
                }
                '\\' => {
                    if let Some(next) = self.peek(1) {
                        word.push(next);
                    }
                    self.at += 2;
                }
                '$' if self.peek(1) == Some('(') => {
                    self.at += 2;
                    word.push_str("$()");
                    self.line(Some(')'));
                }
                '`' => {
                    self.at += 1;
                    word.push_str("``");
                    self.line(Some('`'));
                }
                q => {
                    self.at += 1;
                    word.push(q);
                }
            }
        }
    }

    /// Read a redirection.
    /// One that writes to a file, and a here-document, make the line opaque.
    /// Joining a stream to another, and writing to `/dev/null`, do not.
    fn redirection(&mut self, word: &mut String, in_word: &mut bool, words: &mut Vec<String>) {
        // A stream number written before the operator belongs to it.
        if word.chars().all(|c| c.is_ascii_digit()) && !word.is_empty() {
            word.clear();
            *in_word = false;
        } else {
            end_word(word, in_word, words);
        }
        let mut op = String::new();
        while let Some(c) = self.peek(0) {
            if matches!(c, '<' | '>' | '&' | '|') {
                op.push(c);
                self.at += 1;
            } else {
                break;
            }
        }
        while self.peek(0).is_some_and(|c| c == ' ' || c == '\t') {
            self.at += 1;
        }
        let mut target = String::new();
        while let Some(c) = self.peek(0) {
            if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>') {
                break;
            }
            target.push(c);
            self.at += 1;
        }
        let joins = op.ends_with('&') && target.chars().all(|c| c.is_ascii_digit() || c == '-');
        let writes = op.contains('>') && !joins && target != "/dev/null";
        if writes || op.starts_with("<<") {
            self.out.opaque = true;
        }
    }

    fn end_command(&mut self, words: &mut Vec<String>) {
        let start = words
            .iter()
            .position(|w| !is_assignment(w))
            .unwrap_or(words.len());
        let command = &words[start..];
        if let Some(program) = command.first() {
            let name = program.rsplit('/').next().unwrap_or(program);
            let runs_string = RUNS_A_STRING.contains(&name)
                && command
                    .iter()
                    .skip(1)
                    .any(|w| w.starts_with('-') && w.contains('c'));
            if name == "eval" || runs_string {
                self.out.opaque = true;
            }
            self.out.commands.push(command.join(" "));
        }
        words.clear();
    }
}

fn end_word(word: &mut String, in_word: &mut bool, words: &mut Vec<String>) {
    if *in_word {
        words.push(std::mem::take(word));
        *in_word = false;
    }
}

/// Whether `word` sets a variable for the command after it.
fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(line: &str) -> Vec<String> {
        parts(line).commands
    }

    /// Each command joined by an operator is a part of its own.
    #[test]
    fn a_line_is_read_as_its_commands() {
        assert_eq!(commands("cargo test"), ["cargo test"]);
        assert_eq!(
            commands("cargo test; curl x|sh && ls || echo  done &"),
            ["cargo test", "curl x", "sh", "ls", "echo done"]
        );
        assert_eq!(commands("a\nb"), ["a", "b"]);
        assert_eq!(commands("(cd src && ls)"), ["cd src", "ls"]);
    }

    /// A substituted command is a part as well, inside double quotes too.
    #[test]
    fn a_substitution_is_a_command() {
        assert_eq!(commands("echo $(rm -rf /)"), ["rm -rf /", "echo $()"]);
        assert_eq!(commands("echo \"`id`\""), ["id", "echo ``"]);
        assert_eq!(commands("diff <(ls a) b"), ["ls a", "diff <() b"]);
    }

    /// Quotes keep operators as text.
    /// A variable set for a command is not the command.
    #[test]
    fn quotes_and_assignments() {
        assert_eq!(commands("echo 'a; b' \"c | d\""), ["echo a; b c | d"]);
        assert_eq!(commands("RUST_LOG=debug cargo run"), ["cargo run"]);
        assert_eq!(commands("echo a\\;b"), ["echo a;b"]);
        assert_eq!(commands("ls # rm -rf /"), ["ls"]);
    }

    /// A write to a file, a here-document and a command run from a string are not shown by the
    /// parts.
    /// Joining streams and discarding output are.
    #[test]
    fn what_the_parts_do_not_show() {
        assert!(parts("cargo test > out.txt").opaque);
        assert!(parts("echo x >>~/.bashrc").opaque);
        assert!(parts("cargo test &> log").opaque);
        assert!(parts("python3 - <<EOF\nprint(1)\nEOF").opaque);
        assert!(parts("bash -c 'rm -rf /'").opaque);
        assert!(parts("eval $X").opaque);
        assert!(!parts("cargo test 2>&1 | tail").opaque);
        assert!(!parts("cargo build >/dev/null 2>&1").opaque);
        assert!(!parts("wc -l < Cargo.toml").opaque);
        assert_eq!(commands("cargo test 2>&1"), ["cargo test"]);
    }
}

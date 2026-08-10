//! Separating a model's scratchpad from its answer.
//!
//! A reasoning model produces two kinds of text and the same semantic thing arrives four
//! ways: inline `<think>…</think>` in the content (loken, and any server that does not
//! split), a `reasoning` field beside it (ollama, OpenRouter), a `reasoning_content` field
//! (DeepSeek), or its own block type (Anthropic). None of those is in the OpenAI
//! specification, which is why every vendor chose differently.
//!
//! A generic client absorbs that here, once, so the rest of the agent sees one event and
//! the difference never reaches the loop, the transcript or the screen.
//!
//! Reasoning is shown and then dropped. It is never fed back to the model, because it is
//! not what the model said - it is what the model was working through on the way to saying
//! it, and replaying it as though it were the answer would teach the next turn that the
//! scratchpad is part of the conversation.

/// The tags a ChatML reasoning family opens and closes its scratchpad with.
const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// Splits inline `<think>` blocks out of a stream of content fragments.
///
/// Written as a state machine over fragments rather than over a whole string, because the
/// tags arrive a few characters at a time and can be split across chunks - `<thi` at the
/// end of one and `nk>` at the start of the next is ordinary.
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    /// Text seen but not yet classifiable, because it might be the start of a tag.
    held: String,
    inside: bool,
}

/// What one fragment turned out to contain.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Split {
    pub answer: String,
    pub reasoning: String,
}

impl ThinkSplitter {
    /// Feed the next fragment and take whatever is now unambiguous.
    pub fn push(&mut self, fragment: &str) -> Split {
        self.held.push_str(fragment);
        let mut out = Split::default();

        loop {
            let (looking_for, other) = if self.inside {
                (CLOSE, OPEN)
            } else {
                (OPEN, CLOSE)
            };
            match self.held.find(looking_for) {
                Some(at) => {
                    let before: String = self.held.drain(..at).collect();
                    self.held.drain(..looking_for.len());
                    if self.inside {
                        out.reasoning.push_str(&before);
                    } else {
                        out.answer.push_str(&before);
                    }
                    self.inside = !self.inside;
                }
                None => {
                    // Everything is emittable except a tail that could still become a tag.
                    // A closing tag is longer, so hold enough for whichever might come.
                    let keep = partial_tag_len(&self.held, looking_for)
                        .max(partial_tag_len(&self.held, other));
                    let take = self.held.len() - keep;
                    let ready: String = self.held.drain(..take).collect();
                    if self.inside {
                        out.reasoning.push_str(&ready);
                    } else {
                        out.answer.push_str(&ready);
                    }
                    break;
                }
            }
        }
        out
    }

    /// Nothing more is coming: release whatever is still held.
    ///
    /// An unclosed `<think>` means the model was cut off mid-thought, and what it was
    /// thinking is reasoning rather than answer - showing it as the answer would present a
    /// scratchpad as a conclusion.
    pub fn finish(&mut self) -> Split {
        let rest = std::mem::take(&mut self.held);
        if self.inside {
            Split {
                answer: String::new(),
                reasoning: rest,
            }
        } else {
            Split {
                answer: rest,
                reasoning: String::new(),
            }
        }
    }
}

/// How many trailing characters of `s` could be the beginning of `tag`.
fn partial_tag_len(s: &str, tag: &str) -> usize {
    let max = (tag.len() - 1).min(s.len());
    (1..=max)
        .rev()
        .find(|&n| {
            s.is_char_boundary(s.len() - n)
                && tag.as_bytes().starts_with(&s.as_bytes()[s.len() - n..])
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a whole string one character at a time - the worst case a stream can produce.
    fn char_by_char(s: &str) -> Split {
        let mut sp = ThinkSplitter::default();
        let mut out = Split::default();
        for c in s.chars() {
            let got = sp.push(&c.to_string());
            out.answer.push_str(&got.answer);
            out.reasoning.push_str(&got.reasoning);
        }
        let got = sp.finish();
        out.answer.push_str(&got.answer);
        out.reasoning.push_str(&got.reasoning);
        out
    }

    #[test]
    fn text_with_no_tags_is_all_answer() {
        let out = char_by_char("just an ordinary answer");
        assert_eq!(out.answer, "just an ordinary answer");
        assert_eq!(out.reasoning, "");
    }

    #[test]
    fn a_think_block_is_separated_from_the_answer() {
        let out = char_by_char("<think>let me see</think>the answer");
        assert_eq!(out.reasoning, "let me see");
        assert_eq!(out.answer, "the answer");
    }

    #[test]
    fn text_on_both_sides_of_a_block_is_kept() {
        let out = char_by_char("before <think>middle</think> after");
        assert_eq!(out.answer, "before  after");
        assert_eq!(out.reasoning, "middle");
    }

    /// The case this is a state machine for: a tag split across two fragments.
    #[test]
    fn a_tag_split_across_fragments_is_still_recognised() {
        let mut sp = ThinkSplitter::default();
        let a = sp.push("hello <thi");
        assert_eq!(a.answer, "hello ", "held back too little");
        assert_eq!(a.reasoning, "");
        let b = sp.push("nk>secret</thi");
        assert_eq!(b.reasoning, "secret");
        let c = sp.push("nk>done");
        assert_eq!(c.answer, "done");
    }

    /// A model cut off mid-thought was thinking, not answering.
    #[test]
    fn an_unclosed_block_is_reasoning_not_answer() {
        let out = char_by_char("<think>I was interrupted");
        assert_eq!(out.reasoning, "I was interrupted");
        assert_eq!(out.answer, "");
    }

    /// A lone angle bracket is text, not the start of anything.
    #[test]
    fn ordinary_angle_brackets_survive() {
        let out = char_by_char("if a < b and c > d");
        assert_eq!(out.answer, "if a < b and c > d");
        assert_eq!(out.reasoning, "");
    }

    #[test]
    fn a_close_tag_with_no_open_is_left_alone() {
        // Nothing opened, so there is nothing to close - the text is the answer.
        let out = char_by_char("plain </think> text");
        assert_eq!(out.reasoning, "");
        assert!(out.answer.contains("plain"), "{}", out.answer);
    }

    #[test]
    fn several_blocks_are_all_separated() {
        let out = char_by_char("<think>one</think>A<think>two</think>B");
        assert_eq!(out.reasoning, "onetwo");
        assert_eq!(out.answer, "AB");
    }

    /// Splitting must never lose or reorder a character.
    #[test]
    fn every_character_is_accounted_for() {
        for body in [
            "plain",
            "<think>a</think>b",
            "x<think>y</think>z",
            "<think>unclosed",
            "a < b",
            "",
        ] {
            let out = char_by_char(body);
            let stripped: String = body.replace(OPEN, "").replace(CLOSE, "");
            assert_eq!(
                format!("{}{}", out.reasoning, out.answer).len(),
                stripped.len(),
                "characters lost or duplicated for {body:?}"
            );
        }
    }

    /// A multi-byte character must not be split by the hold-back logic.
    #[test]
    fn multibyte_text_survives() {
        let out = char_by_char("réponse <think>réflexion</think> fin");
        assert_eq!(out.reasoning, "réflexion");
        assert_eq!(out.answer, "réponse  fin");
    }
}

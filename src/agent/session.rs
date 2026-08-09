use uuid::Uuid;

use crate::agent::transcript::{Record, Transcript};
use crate::backend::wire::Message;

/// Roughly how many characters one token covers on English prose and code.
///
/// The agent has no tokenizer, and acquiring one per model would tie it to a backend it
/// deliberately does not depend on. The figure only decides *when* to compact, so being
/// off by a fifth moves the trigger point and never corrupts the conversation.
const CHARS_PER_TOKEN: usize = 4;

/// Fraction of the estimated capacity actually used.
///
/// The estimate is corrected against the server's own count, but a correction only
/// arrives after a request has already been sent - so the figure that decides whether to
/// send is deliberately pessimistic. Being early to compact costs a few dropped steps;
/// being late costs the system message, which is the failure this module exists for.
const SAFETY: f64 = 0.9;

/// Bounds on the corrected ratio.
///
/// A ratio outside these says the arithmetic went wrong rather than that the tokenizer is
/// unusual - one malformed usage report should not be able to collapse the budget to
/// nothing or inflate it past the window.
const MIN_RATIO: f64 = 1.5;
const MAX_RATIO: f64 = 8.0;

/// How much of a new observation to believe.
///
/// Each request's ratio varies with what happens to be in the history, so corrections are
/// smoothed rather than taken whole.
const SMOOTHING: f64 = 0.3;

/// Note left in place of what was dropped, so the model can tell that the conversation
/// it can see is not the whole of it.
const ELISION: &str = "[earlier steps of this session were elided to fit the context window]";

/// Conversation state for a single TUI session.
pub struct Session {
    pub id: String,
    pub history: Vec<Message>,
    /// Characters the history may occupy before older steps are dropped.
    budget_chars: usize,
    /// Tokens the history may occupy, which is the figure the budget is derived from.
    usable_tokens: usize,
    /// Characters one token is currently believed to cover.
    ///
    /// Starts at the constant and is corrected from what the server reports. The guess
    /// under-counts on dense code, and under-counting is the direction that hurts: the
    /// client believes the history fits, sends it anyway, and the server truncates from
    /// the front - taking the preamble.
    chars_per_token: f64,
    /// Durable record of everything said, if one is being kept.
    ///
    /// Attached here because `push` is the single place a message enters the
    /// conversation, so nothing can join the history without also being written down.
    transcript: Option<Transcript>,
}

/// What a single tool result may contribute to the conversation.
///
/// A quarter of the history's budget, so the several results of one step can coexist
/// with the exchange that asked for them, instead of one of them displacing all of it.
/// Derived here beside the history budget rather than chosen inside each tool: a result
/// is only ever large relative to the window it has to fit in.
pub fn tool_output_chars(context_tokens: usize, reserve_tokens: usize) -> usize {
    context_tokens.saturating_sub(reserve_tokens) * CHARS_PER_TOKEN / 4
}

impl Session {
    /// `context_tokens` is the window the backend is expected to offer, and
    /// `reserve_tokens` what to leave free for the answer.
    pub fn new(system_prompt: String, context_tokens: usize, reserve_tokens: usize) -> Self {
        let usable_tokens = context_tokens.saturating_sub(reserve_tokens);
        Self {
            id: Uuid::new_v4().to_string(),
            history: vec![Message::system(system_prompt)],
            budget_chars: (usable_tokens as f64 * CHARS_PER_TOKEN as f64 * SAFETY) as usize,
            usable_tokens,
            chars_per_token: CHARS_PER_TOKEN as f64,
            transcript: None,
        }
    }

    /// Keep a durable record of this session, and adopt the identity it was written under.
    ///
    /// The id comes from the transcript rather than the other way round so that a resumed
    /// session keeps appending to the file it came from, instead of leaving a trail of
    /// fragments across as many files as it had lives.
    pub fn with_transcript(mut self, transcript: Transcript, id: String) -> Self {
        self.id = id;
        self.transcript = Some(transcript);
        self
    }

    /// Put back the messages of an earlier session, after the system message.
    pub fn restore(&mut self, messages: Vec<Message>) {
        // Straight onto the history: these are already in the transcript, and writing
        // them again would double the file on every resume.
        self.history.extend(messages);
    }

    pub fn transcript(&self) -> Option<&Transcript> {
        self.transcript.as_ref()
    }

    /// Replace the preamble in place.
    ///
    /// Used when the protocol changes mid-session: the system message states the contract
    /// the model is being held to, so it has to change with it. Rewritten rather than
    /// appended, because two contradictory contracts is worse than either.
    pub fn set_system_prompt(&mut self, prompt: String) {
        if let Some(first) = self.history.first_mut() {
            if first.role == "system" {
                first.content = prompt;
                return;
            }
        }
        self.history.insert(0, Message::system(prompt));
    }

    pub fn push(&mut self, m: Message) {
        if let Some(t) = &self.transcript {
            t.append(Record::Message(m.clone()));
        }
        self.history.push(m);
    }

    pub fn size_chars(&self) -> usize {
        self.history
            .iter()
            .map(|m| m.content.len() + m.role.len())
            .sum()
    }

    /// Characters the history may currently occupy.
    pub fn budget_chars(&self) -> usize {
        self.budget_chars
    }

    /// Correct the estimate from what the server actually counted.
    ///
    /// `sent_chars` is what this client measured before the request; `prompt_tokens` is
    /// what the server made of it. Their ratio is the one measurement of the model's
    /// tokenizer available without shipping one, and it is the whole reason the budget
    /// can be wrong in the dangerous direction without anyone noticing.
    ///
    /// Returns a note for the user when the correction moves the budget enough to matter.
    pub fn observe(&mut self, sent_chars: usize, prompt_tokens: u32) -> Option<String> {
        if sent_chars == 0 || prompt_tokens == 0 {
            return None;
        }
        let measured = sent_chars as f64 / prompt_tokens as f64;
        if !measured.is_finite() || !(MIN_RATIO..=MAX_RATIO).contains(&measured) {
            return None;
        }

        let before = self.budget_chars;
        self.chars_per_token = self.chars_per_token * (1.0 - SMOOTHING) + measured * SMOOTHING;
        self.budget_chars = (self.usable_tokens as f64 * self.chars_per_token * SAFETY) as usize;

        // Only worth saying when the change is large enough to alter behaviour; the ratio
        // drifts a little on every request and narrating that would be noise.
        let moved = before.abs_diff(self.budget_chars);
        (moved * 10 > before.max(1)).then(|| {
            format!(
                "context budget now {} chars ({:.1} chars/token, measured against the server)",
                self.budget_chars, self.chars_per_token
            )
        })
    }

    /// Drop the oldest steps until the history fits the budget, and report how many
    /// messages went.
    ///
    /// Two invariants make this safe to do behind the model's back. The system message
    /// stays: it carries the tool manifest and the response contract, and a server that
    /// has to truncate keeps the tail, so leaving the job to the server is precisely
    /// what removes the preamble. And a step is dropped whole - an assistant turn
    /// together with the tool results it produced - because a tool result whose call is
    /// gone describes an action the model cannot see itself having taken.
    pub fn compact(&mut self) -> usize {
        if self.budget_chars == 0 || self.size_chars() <= self.budget_chars {
            return 0;
        }
        let mut dropped = 0;
        // Everything from index 1 on is fair game; stop while a user turn remains, so
        // the conversation the model reads still opens on somebody asking for something.
        while self.size_chars() > self.budget_chars {
            let Some(next) = self.next_user_boundary() else {
                break;
            };
            if next >= self.history.len() {
                break;
            }
            self.history.drain(1..next);
            dropped += next - 1;
            if self.history.get(1).map(|m| m.content.as_str()) != Some(ELISION) {
                self.history.insert(1, Message::user(ELISION));
            }
        }
        dropped
    }

    /// Index of the second retained user message, i.e. where the history would resume
    /// after dropping the oldest step. `None` when there is nothing left to drop.
    fn next_user_boundary(&self) -> Option<usize> {
        let first = (1..self.history.len())
            .find(|&i| self.history[i].role == "user" && self.history[i].content != ELISION)?;
        (first + 1..self.history.len()).find(|&i| self.history[i].role == "user")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, n: usize) -> Message {
        let body = "x".repeat(n);
        match role {
            "user" => Message::user(body),
            "assistant" => Message::assistant(body),
            _ => Message::tool(body, "id"),
        }
    }

    /// One result must never be able to fill the window on its own, or compaction has
    /// nothing left to drop and the server truncates - taking the preamble.
    #[test]
    fn a_single_tool_result_cannot_fill_the_budget() {
        let (ctx_tok, reserve) = (8192, 4096);
        let s = Session::new("sys".into(), ctx_tok, reserve);
        assert!(tool_output_chars(ctx_tok, reserve) * 2 < s.budget_chars());
    }

    /// The ratio is corrected towards what the server counted, not replaced by it: one
    /// request's ratio depends on what happens to be in the history.
    #[test]
    fn the_budget_follows_the_server_count() {
        let mut s = Session::new("sys".into(), 8192, 4096);
        let started_at = s.budget_chars();

        // A tokenizer that gets ~2 chars to the token: dense code, the case the constant
        // is wrong about and the direction that loses the preamble.
        for _ in 0..20 {
            s.observe(20_000, 10_000);
        }
        assert!(
            s.budget_chars() < started_at,
            "a denser tokenizer must shrink the budget: {} vs {started_at}",
            s.budget_chars()
        );
        assert!(
            (s.chars_per_token - 2.0).abs() < 0.1,
            "ratio: {}",
            s.chars_per_token
        );
    }

    /// A nonsense report must not be able to collapse the budget or inflate it.
    #[test]
    fn an_impossible_measurement_is_ignored() {
        let mut s = Session::new("sys".into(), 8192, 4096);
        let before = s.budget_chars();
        for (chars, tokens) in [(0, 100), (100, 0), (1, 1_000_000), (1_000_000, 1)] {
            s.observe(chars, tokens);
            assert_eq!(
                s.budget_chars(),
                before,
                "{chars} chars / {tokens} tokens moved the budget"
            );
        }
    }

    /// The estimate is used to decide whether to send, and the correction only arrives
    /// after sending - so it has to sit under the window rather than on it.
    #[test]
    fn the_budget_leaves_room_for_being_wrong() {
        let s = Session::new("sys".into(), 8192, 4096);
        assert!(s.budget_chars() < (8192 - 4096) * CHARS_PER_TOKEN);
    }

    /// A drift too small to change behaviour is not worth interrupting the user over.
    #[test]
    fn only_a_meaningful_correction_is_reported() {
        let mut s = Session::new("sys".into(), 8192, 4096);
        // Exactly the ratio already believed: nothing moves, nothing is said.
        assert_eq!(s.observe(40_000, 10_000), None);
        // A tokenizer half as generous moves the budget a long way.
        assert!(s.observe(20_000, 10_000).is_some());
    }

    /// A generous budget must leave the conversation exactly as it was.
    #[test]
    fn under_budget_nothing_moves() {
        let mut s = Session::new("sys".into(), 10_000, 0);
        for _ in 0..3 {
            s.push(msg("user", 10));
            s.push(msg("assistant", 10));
        }
        let before = s.history.len();
        assert_eq!(s.compact(), 0);
        assert_eq!(s.history.len(), before);
    }

    /// The system message carries the tool manifest and the response contract. Losing it
    /// is the failure this whole mechanism exists to prevent, so it survives any budget.
    #[test]
    fn the_system_message_is_never_dropped() {
        let mut s = Session::new("THE CONTRACT".into(), 100, 0);
        for _ in 0..8 {
            s.push(msg("user", 400));
            s.push(msg("assistant", 400));
        }
        s.compact();
        assert_eq!(s.history[0].role, "system");
        assert_eq!(s.history[0].content, "THE CONTRACT");
    }

    /// A tool result whose call was dropped reports an action with no antecedent, so a
    /// step leaves whole or not at all.
    #[test]
    fn no_tool_result_is_left_without_its_call() {
        let mut s = Session::new("sys".into(), 200, 0);
        for _ in 0..6 {
            s.push(msg("user", 300));
            s.push(msg("assistant", 300));
            s.push(msg("tool", 300));
        }
        s.compact();
        // Past the system line and the elision note, the transcript opens on a request.
        let first_real = s
            .history
            .iter()
            .skip(1)
            .find(|m| m.content != ELISION)
            .unwrap();
        assert_eq!(
            first_real.role,
            "user",
            "history: {:?}",
            s.history
                .iter()
                .map(|m| m.role.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// Compaction says how much it removed, and leaves one note rather than one per pass.
    #[test]
    fn it_reports_what_went_and_marks_the_gap_once() {
        let mut s = Session::new("sys".into(), 150, 0);
        for _ in 0..10 {
            s.push(msg("user", 200));
            s.push(msg("assistant", 200));
        }
        let dropped = s.compact();
        assert!(
            dropped > 0,
            "nothing was dropped despite exceeding the budget"
        );
        assert_eq!(s.history.iter().filter(|m| m.content == ELISION).count(), 1);
        assert!(s.history.len() < 21);
    }

    /// The last exchange is what the model needs most; compaction must not eat into it
    /// even when one message alone exceeds the budget.
    #[test]
    fn the_most_recent_turn_survives_an_impossible_budget() {
        let mut s = Session::new("sys".into(), 10, 0);
        s.push(msg("user", 5_000));
        s.push(msg("assistant", 5_000));
        s.compact();
        assert!(s.history.iter().any(|m| m.role == "user"));
        assert!(s.history.iter().any(|m| m.role == "assistant"));
    }
}

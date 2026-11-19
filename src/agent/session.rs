use uuid::Uuid;

use crate::agent::transcript::{Record, Transcript};
use crate::backend::wire::Message;

/// Roughly how many characters one token covers on English prose and code.
/// The agent has no tokenizer, and does not depend on a backend that would supply one.
/// The figure only decides when to compact.
/// Being off moves the trigger point and never corrupts the conversation.
pub(crate) const CHARS_PER_TOKEN: usize = 4;

/// Fraction of the estimated capacity actually used.
/// The estimate is corrected against the server's own count.
/// A correction arrives only after a request has been sent.
/// The figure that decides whether to send is therefore pessimistic.
/// Compacting early costs a few dropped steps.
/// Compacting late costs the system message, which this module exists to keep.
const SAFETY: f64 = 0.9;

/// Bounds on the corrected ratio.
/// A ratio outside these bounds means the arithmetic went wrong, not that the tokenizer is
/// unusual.
/// One malformed usage report cannot collapse the budget to nothing or inflate it past the
/// window.
const MIN_RATIO: f64 = 1.5;
const MAX_RATIO: f64 = 8.0;

/// How far below the budget a compaction trims.
/// Compacting invalidates the server's prefix cache, so the whole prompt is processed again.
/// Trimming to just under the line makes the next step cross it and pay that cost again.
/// Trimming well under it lets several steps fit before the next compaction.
const COMPACT_TO: f64 = 0.6;

/// How much of a new observation to believe.
/// Each request's ratio varies with what is in the history, so corrections are smoothed.
const SMOOTHING: f64 = 0.3;

/// Messages at the end of the history that are never dropped.
///
/// A turn that cannot see the step it just took cannot carry on from it.
/// It re-reads what it read, or answers about something it can no longer see.
/// The kept messages are the last call, its result, and the exchange before them.
const KEEP_RECENT: usize = 4;

/// Note left in place of what was dropped.
/// It tells the model that the conversation it can see is not the whole of it.
const ELISION: &str = "[earlier steps of this session were elided to fit the context window]";

/// What the history holds against its budget, in characters, and who wrote it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Occupancy {
    /// The share of the budget in use, as [`Session::fullness_percent`] says it.
    pub percent: u8,
    pub used: usize,
    pub budget: usize,
    /// The standing instructions, what the person said, what the model said and what the
    /// tools returned, in that order.
    pub parts: Vec<(&'static str, usize)>,
}

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
    /// Starts at the constant and is corrected from what the server reports.
    /// The guess under-counts on dense code.
    /// Under-counting hurts: the client believes the history fits and sends it.
    /// The server then truncates from the front, which takes the preamble.
    chars_per_token: f64,
    /// Durable record of everything said, if one is being kept.
    ///
    /// Attached here because `push` is the single place a message enters the conversation.
    /// Nothing can join the history without also being written down.
    transcript: Option<Transcript>,
    /// What the elision note carries besides saying so.
    carry: Option<String>,
}

/// The most of the window the answer may be promised, as a fraction of it.
///
/// The reserve is subtracted from the window before anything else is budgeted.
/// A reserve equal to the window leaves no history, and every tool result is clamped to its
/// own truncation notice.
/// The client then cannot read, and the model answers from what it invents.
/// Asking for a shorter answer than configured is the recoverable half of that trade.
const RESERVE_OF_WINDOW: usize = 2;

/// What the answer may be promised, given the window it has to be promised out of.
///
/// The configured figure, unless the window cannot afford it.
pub fn answer_reserve(context_tokens: usize, asked: usize) -> usize {
    asked.min(context_tokens / RESERVE_OF_WINDOW)
}

/// What a single tool result may contribute to the conversation.
///
/// A fixed share of the history's budget, so the several results of one step coexist with the
/// exchange that asked for them.
/// One result never displaces all of it.
/// The share is derived here beside the history budget, not chosen inside each tool.
pub fn tool_output_chars(context_tokens: usize, reserve_tokens: usize) -> usize {
    context_tokens.saturating_sub(reserve_tokens) * CHARS_PER_TOKEN / 4
}

/// What a whole file or a command's output may give one step: the history's budget shared
/// among the steps a turn is expected to take.
///
/// The budget is in characters at the measured ratio, so it follows the window and the
/// tokenizer.
/// The expected steps are the stated step limit, or its default when the turn ends by its
/// progress.
/// A turn that reads one whole file a step takes that many steps before it fills the window.
pub fn step_share_chars(budget_chars: usize, expected_steps: usize) -> usize {
    budget_chars / expected_steps.max(1)
}

/// What a reply's token count said about the request that produced it.
///
/// The count is the only thing a backend says about how it read the prompt.
/// It carries 2 facts: the tokenizer's ratio, which sizes the history, and whether the prompt
/// arrived whole.
pub enum Observed {
    /// Nothing that changes what the caller does.
    Quiet,
    /// The history budget moved enough to be worth saying.
    Budget(String),
    /// The backend read only this many tokens of what was sent, so it cut the prompt.
    /// What it read is the window it serves.
    Cut(usize),
}

/// What the standing instructions may hold of the window.
///
/// A fixed share of the budget, and half of what one tool result may take.
/// This text is in the system message, which compaction never drops.
/// It is paid on every request of the session.
pub fn instruction_chars(context_tokens: usize, reserve_tokens: usize) -> usize {
    context_tokens.saturating_sub(reserve_tokens) * CHARS_PER_TOKEN / 8
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
            carry: None,
        }
    }

    /// Keep a durable record of this session, and adopt the identity it was written under.
    ///
    /// The id comes from the transcript.
    /// A resumed session therefore keeps appending to the file it came from.
    pub fn with_transcript(mut self, transcript: Transcript, id: String) -> Self {
        self.id = id;
        self.transcript = Some(transcript);
        self
    }

    /// Take over an earlier session: its record, its identity and what was said in it.
    ///
    /// The conversation is replaced, not added to: coming back to a session means being in
    /// that one.
    pub fn adopt(&mut self, transcript: Transcript, id: String, messages: Vec<Message>) {
        self.clear();
        self.id = id;
        self.transcript = Some(transcript);
        self.history.extend(messages);
    }

    /// Put back the messages of an earlier session, after the system message.
    pub fn restore(&mut self, messages: Vec<Message>) {
        // Straight onto the history: these are already in the transcript.
        // Writing them again would double the file on every resume.
        self.history.extend(messages);
    }

    /// Start the conversation again, keeping the contract and what was measured.
    ///
    /// The record keeps what was said, and choosing to start again does not unsay anything.
    /// The record carries a mark so that resuming it later does not bring back what was
    /// dropped.
    ///
    /// The note left where steps were elided carries the given text besides saying so, as of
    /// the next elision.
    pub fn set_carry(&mut self, carry: Option<String>) {
        self.carry = carry;
    }

    /// The note left where steps were elided.
    fn elision(&self) -> String {
        match &self.carry {
            Some(carry) => format!("{ELISION}\n\n{carry}"),
            None => ELISION.to_string(),
        }
    }

    pub fn clear(&mut self) -> usize {
        let dropped = self.history.len().saturating_sub(1);
        self.history.truncate(1);
        if let Some(t) = &self.transcript {
            t.append(Record::Cleared);
        }
        dropped
    }

    pub fn transcript(&self) -> Option<&Transcript> {
        self.transcript.as_ref()
    }

    /// Take the window of another model, and forget what the last one taught.
    ///
    /// The chars-per-token ratio was measured against one tokenizer.
    /// It goes back to the constant and is corrected again from the first reply the new model
    /// sends.
    pub fn set_window(&mut self, context_tokens: usize, reserve_tokens: usize) {
        self.usable_tokens = context_tokens.saturating_sub(reserve_tokens);
        self.chars_per_token = CHARS_PER_TOKEN as f64;
        self.budget_chars = (self.usable_tokens as f64 * self.chars_per_token * SAFETY) as usize;
    }

    /// Replace the preamble in place.
    ///
    /// Used when the protocol changes mid-session.
    /// The system message states the contract the model is held to, so it changes with it.
    /// It is rewritten, not appended, because 2 contradictory contracts are worse than either.
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

    /// How full the window is, as a percentage of the budget.
    ///
    /// It is measured against the budget, not the raw window, because compaction defends the
    /// budget.
    /// At 100% the next step starts dropping messages.
    pub fn fullness_percent(&self) -> u8 {
        if self.budget_chars == 0 {
            return 0;
        }
        ((self.size_chars() * 100 / self.budget_chars) as u64).min(255) as u8
    }

    /// What the history holds against its budget, and who wrote it.
    pub fn measure(&self) -> Occupancy {
        let mut parts = [
            ("instructions", 0usize),
            ("you", 0),
            ("model", 0),
            ("tools", 0),
        ];
        for m in &self.history {
            let at = match m.role.as_str() {
                "system" => 0,
                "user" => 1,
                "assistant" => 2,
                _ => 3,
            };
            parts[at].1 += m.content.len() + m.role.len();
        }
        Occupancy {
            percent: self.fullness_percent(),
            used: self.size_chars(),
            budget: self.budget_chars,
            parts: parts.to_vec(),
        }
    }

    /// What is holding the window, by kind, largest first.
    pub fn occupancy(&self) -> Vec<(String, usize)> {
        let mut by_kind: std::collections::BTreeMap<&str, usize> = Default::default();
        for m in &self.history {
            let kind = match m.role.as_str() {
                "system" => "standing instructions and the tool manifest",
                "user" => "what you said",
                "assistant" => "what the model said",
                "tool" => "tool results",
                other => other,
            };
            *by_kind.entry(kind).or_default() += m.content.len();
        }
        let mut rows: Vec<(String, usize)> = by_kind
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        rows.sort_by_key(|(_, chars)| std::cmp::Reverse(*chars));
        rows
    }

    /// Take the conversation back to just before the last thing the user asked, and hand
    /// that back.
    ///
    /// What the agent did after it goes, so the question is asked again from where it was
    /// asked the first time.
    /// The transcript keeps both, because what was tried is part of what happened.
    pub fn rewind_to_last_ask(&mut self) -> Option<String> {
        let at = self
            .history
            .iter()
            .rposition(|m| m.role == "user" && !m.content.is_empty())?;
        let asked = self.history[at].content.clone();
        self.history.truncate(at);
        Some(asked)
    }

    /// Compact now, whatever the budget says, down to the same target a full window would be
    /// taken to.
    ///
    /// Returns how many messages went.
    pub fn compact_now(&mut self) -> usize {
        let budget = self.budget_chars;
        // Compaction is a comparison against the budget.
        // Asking for it early lowers the budget for one pass.
        self.budget_chars = (self.size_chars() as f64 * COMPACT_TO) as usize;
        let dropped = self.compact();
        self.budget_chars = budget;
        dropped
    }

    /// Correct the estimate from what the server actually counted.
    ///
    /// `sent_chars` is what this client measured before the request.
    /// `prompt_tokens` is what the server made of it.
    /// Their ratio is the one measurement of the model's tokenizer available without shipping
    /// one.
    ///
    /// Returns a note for the user when the correction moves the budget enough to matter.
    pub fn observe(&mut self, sent_chars: usize, prompt_tokens: u32) -> Observed {
        if sent_chars == 0 || prompt_tokens == 0 {
            return Observed::Quiet;
        }
        let measured = sent_chars as f64 / prompt_tokens as f64;
        if !measured.is_finite() {
            return Observed::Quiet;
        }
        // Far more characters than the tokens the server counted: no text tokenises that
        // compactly, so the server did not read all of what was sent.
        // It cuts from the front, which is where the standing instructions are.
        // What it did read is the window it serves.
        if measured > MAX_RATIO {
            return Observed::Cut(prompt_tokens as usize);
        }
        if measured < MIN_RATIO {
            return Observed::Quiet;
        }

        let before = self.budget_chars;
        self.chars_per_token = self.chars_per_token * (1.0 - SMOOTHING) + measured * SMOOTHING;
        self.budget_chars = (self.usable_tokens as f64 * self.chars_per_token * SAFETY) as usize;

        // Only worth saying when the change is large enough to alter behaviour.
        // The ratio drifts a little on every request.
        let moved = before.abs_diff(self.budget_chars);
        match moved * 10 > before.max(1) {
            true => Observed::Budget(format!(
                "context budget now {} chars ({:.1} chars/token, measured against the server)",
                self.budget_chars, self.chars_per_token
            )),
            false => Observed::Quiet,
        }
    }

    /// Whether compacting now would cut into the messages from `from` on.
    ///
    /// Compaction drops whole earlier turns first, down to a share of the budget.
    /// Only what is still over that share costs the messages from `from` on.
    pub fn would_cut_from(&self, from: usize) -> bool {
        if self.budget_chars == 0 || self.size_chars() <= self.budget_chars {
            return false;
        }
        let target = (self.budget_chars as f64 * COMPACT_TO) as usize;
        let kept: usize = self
            .history
            .iter()
            .take(1)
            .chain(self.history.iter().skip(from.max(1)))
            .map(|m| m.content.len() + m.role.len())
            .sum();
        kept > target
    }

    /// Drop the oldest steps until the history is comfortably inside the budget, and
    /// report how many messages went.
    ///
    /// 2 invariants make this safe to do behind the model's back.
    ///
    /// The system message stays.
    /// It carries the tool manifest and the response contract.
    /// A server that has to truncate keeps the tail, which removes the preamble.
    ///
    /// A step is dropped whole: an assistant turn together with the tool results it produced.
    /// A tool result whose call is gone describes an action the model cannot see itself
    /// having taken.
    ///
    /// It trims well past the line, not to it.
    /// The server keeps a prefix cache keyed on the conversation.
    /// Dropping anything from the front changes the prefix at the second message.
    /// Every compaction therefore throws that cache away, and the whole prompt is processed
    /// again.
    /// Trimming well past the line makes compaction rare.
    pub fn compact(&mut self) -> usize {
        if self.budget_chars == 0 || self.size_chars() <= self.budget_chars {
            return 0;
        }
        let target = (self.budget_chars as f64 * COMPACT_TO) as usize;
        let mut dropped = 0;
        // Whole turns first.
        // The conversation the model reads still opens on somebody asking for something.
        while self.size_chars() > target {
            let Some(next) = self.next_user_boundary() else {
                break;
            };
            if next >= self.history.len() {
                break;
            }
            self.history.drain(1..next);
            dropped += next - 1;
            self.mark_elision();
        }
        // Then the steps of the turn still running.
        // One long turn has no second user message to cut at.
        if self.size_chars() > target {
            dropped += self.drop_oldest_steps();
        }
        dropped
    }

    /// Drop the oldest steps of the turn in one cut, keeping the ask and the last few.
    ///
    /// One cut rather than a loop.
    /// A loop has to leave the history well formed after every pass, and can fail to make
    /// progress.
    ///
    /// A tool result whose call has gone is a reply to nothing, which some backends refuse.
    /// The cut lands where the next message is not one.
    fn drop_oldest_steps(&mut self) -> usize {
        // Index 0 is the contract and index 1 is what was asked, and neither can go.
        // The last few steps are what the turn is carrying on from.
        const KEEP_FIRST: usize = 2;
        let keep_from = self.history.len().saturating_sub(KEEP_RECENT);
        if keep_from <= KEEP_FIRST {
            return 0;
        }
        let mut cut = keep_from;
        while cut < self.history.len() && self.history[cut].role == "tool" {
            cut += 1;
        }
        if cut <= KEEP_FIRST {
            return 0;
        }
        let dropped = cut - KEEP_FIRST;
        self.history.drain(KEEP_FIRST..cut);
        let note = self.elision();
        match self.history.get_mut(KEEP_FIRST) {
            Some(m) if m.content.starts_with(ELISION) => m.content = note,
            _ => self.history.insert(KEEP_FIRST, Message::user(note)),
        }
        dropped
    }

    /// Say once, where the cut was made, that something was cut.
    fn mark_elision(&mut self) {
        let note = self.elision();
        match self.history.get_mut(1) {
            Some(m) if m.content.starts_with(ELISION) => m.content = note,
            _ => self.history.insert(1, Message::user(note)),
        }
    }

    /// Index of the second retained user message, where the history would resume after
    /// dropping the oldest step.
    /// `None` when there is nothing left to drop.
    fn next_user_boundary(&self) -> Option<usize> {
        let first = (1..self.history.len()).find(|&i| {
            self.history[i].role == "user" && !self.history[i].content.starts_with(ELISION)
        })?;
        (first + 1..self.history.len()).find(|&i| self.history[i].role == "user")
    }
}

#[cfg(test)]
mod tests {
    /// Starting again keeps the contract and drops everything said under it.
    /// A cleared session still carrying the conversation is not cleared.
    #[test]
    fn starting_again_keeps_the_contract_and_nothing_else() {
        let mut s = Session::new("the contract".into(), 8192, 1024);
        s.push(Message::user("first"));
        s.push(Message::assistant("second"));
        assert_eq!(s.clear(), 2);
        assert_eq!(s.size_chars(), "the contract".len() + "system".len());
        // And again, with nothing said, is not an error.
        assert_eq!(s.clear(), 0);
    }

    /// A prompt the server only partly read is said, not swallowed.
    ///
    /// The client cannot always learn the window a backend serves.
    /// The remaining sign is the token count coming back for a prompt that should have
    /// produced far more.
    /// The server cuts from the front, which is where the standing instructions are.
    /// An answer to a truncated prompt is fluent and about the wrong thing.
    #[test]
    fn a_prompt_the_backend_only_partly_read_is_reported() {
        let mut s = Session::new("sys".into(), 32_768, 4096);
        // 10 characters a token: no text does that, so the prompt was cut.
        // What was read is the window the backend serves.
        assert!(matches!(s.observe(100_000, 10_000), Observed::Cut(10_000)));
        // An ordinary ratio is measured, not read as damage.
        let ordinary = Session::new("sys".into(), 32_768, 4096).observe(40_000, 10_000);
        assert!(!matches!(ordinary, Observed::Cut(_)));
    }

    /// A reserve equal to the window leaves nothing to read with.
    /// The window is subtracted to zero, every tool result clamps to its own truncation
    /// notice, and nothing says so.
    #[test]
    fn the_answer_never_takes_the_whole_window() {
        // What a backend offering 4096 tokens did with the default reserve.
        assert_eq!(answer_reserve(4096, 4096), 2048);
        assert!(tool_output_chars(4096, answer_reserve(4096, 4096)) > 0);
        // A window that can afford what was asked keeps it.
        assert_eq!(answer_reserve(32_768, 4096), 4096);
    }

    use super::*;

    /// One long turn has no second user message to cut at.
    /// The history must not grow past the budget and stay there.
    #[test]
    fn a_single_long_turn_can_still_be_cut_back() {
        let mut s = Session::new("system".into(), 2048, 1024);
        s.push(Message::user("do the thing"));
        for i in 0..40 {
            s.push(Message::assistant(format!("step {i}")));
            s.push(Message::tool("x".repeat(400), format!("c{i}")));
        }
        let before = s.size_chars();
        assert!(
            before > s.budget_chars(),
            "the test has to start over budget"
        );

        let dropped = s.compact();
        assert!(dropped > 0, "nothing was dropped");
        assert!(
            s.size_chars() <= s.budget_chars(),
            "still over: {} > {}",
            s.size_chars(),
            s.budget_chars()
        );
        // The system message and the last steps survive, and the cut is said once.
        assert_eq!(s.history.first().map(|m| m.role.as_str()), Some("system"));
        assert_eq!(s.history.get(2).map(|m| m.content.as_str()), Some(ELISION));
        assert!(s.history.len() >= KEEP_RECENT);
    }

    /// A tool result whose call has been dropped is a reply to nothing.
    /// Some backends refuse it outright.
    #[test]
    fn a_cut_never_leaves_a_reply_to_nothing() {
        let mut s = Session::new("system".into(), 2048, 1024);
        s.push(Message::user("do the thing"));
        for i in 0..40 {
            s.push(Message::assistant(format!("step {i}")));
            s.push(Message::tool("x".repeat(400), format!("c{i}")));
        }
        s.compact();
        // What follows the marker must not be a reply to a call that has gone.
        assert_ne!(s.history.get(3).map(|m| m.role.as_str()), Some("tool"));
    }

    /// Asking again means asking from where it was asked.
    /// It does not ask a model that has already read its own failed attempt.
    #[test]
    fn rewinding_gives_back_the_question_and_drops_what_followed() {
        let mut s = Session::new("system".into(), 8192, 1024);
        s.push(Message::user("do the thing"));
        s.push(Message::assistant("I will"));
        s.push(Message::user("actually do this instead"));
        s.push(Message::assistant("trying"));
        s.push(Message::tool("it failed", "c1".to_string()));

        assert_eq!(
            s.rewind_to_last_ask().as_deref(),
            Some("actually do this instead")
        );
        assert_eq!(s.history.last().map(|m| m.content.as_str()), Some("I will"));
        // The one before it is still there to ask again after that.
        assert_eq!(s.rewind_to_last_ask().as_deref(), Some("do the thing"));
        assert!(s.rewind_to_last_ask().is_none());
    }

    fn msg(role: &str, n: usize) -> Message {
        let body = "x".repeat(n);
        match role {
            "user" => Message::user(body),
            "assistant" => Message::assistant(body),
            _ => Message::tool(body, "id"),
        }
    }

    /// One result must never fill the window on its own.
    /// Otherwise compaction has nothing left to drop and the server truncates the preamble.
    #[test]
    fn a_single_tool_result_cannot_fill_the_budget() {
        let (ctx_tok, reserve) = (8192, 4096);
        let s = Session::new("sys".into(), ctx_tok, reserve);
        assert!(tool_output_chars(ctx_tok, reserve) * 2 < s.budget_chars());
    }

    /// The ratio is corrected towards what the server counted, not replaced by it.
    /// One request's ratio depends on what is in the history.
    #[test]
    fn the_budget_follows_the_server_count() {
        let mut s = Session::new("sys".into(), 8192, 4096);
        let started_at = s.budget_chars();

        // A tokenizer that gets ~2 chars to the token: dense code.
        // The constant is wrong there, in the direction that loses the preamble.
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

    /// The estimate decides whether to send, and the correction arrives after sending.
    /// It has to sit under the window, not on it.
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
        assert!(matches!(s.observe(40_000, 10_000), Observed::Quiet));
        // A tokenizer half as generous moves the budget a long way.
        assert!(matches!(s.observe(20_000, 10_000), Observed::Budget(_)));
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

    /// The system message carries the tool manifest and the response contract.
    /// It survives any budget.
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

    /// A tool result whose call was dropped reports an action with no antecedent.
    /// A step therefore leaves whole or not at all.
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

    /// Trimming past the line rather than up to it.
    /// Compacting invalidates the server's prefix cache.
    /// What matters is how often it happens, not how little it removes each time.
    #[test]
    fn compacting_is_rare_rather_than_minimal() {
        // A conversation that keeps growing past a fixed budget.
        fn compactions(budget_tokens: usize) -> usize {
            let mut s = Session::new("sys".into(), budget_tokens, 0);
            let mut events = 0;
            for _ in 0..200 {
                s.push(msg("user", 100));
                s.push(msg("assistant", 100));
                if s.compact() > 0 {
                    events += 1;
                }
            }
            events
        }

        const STEPS: usize = 200;
        let events = compactions(500);
        assert!(
            events > 0,
            "the conversation never overflowed; the test proves nothing"
        );
        // Trimming to exactly the budget makes the next step cross it again.
        // It then compacts on roughly every step once it is full.
        // Several steps between compactions is what this asserts.
        assert!(
            events * 3 < STEPS,
            "compacted {events} times in {STEPS} steps - fewer than three steps between, so \
             the server's prefix cache is being thrown away almost every step"
        );
    }

    /// Trimming further must not eat the exchange the model needs most.
    #[test]
    fn trimming_past_the_line_still_keeps_the_latest_turn() {
        let mut s = Session::new("sys".into(), 400, 0);
        for i in 0..20 {
            s.push(msg("user", 100));
            s.push(Message::assistant(format!("answer {i}")));
        }
        s.compact();
        assert!(
            s.history.iter().any(|m| m.content == "answer 19"),
            "the most recent answer was trimmed away"
        );
        assert_eq!(s.history[0].role, "system");
    }

    /// The last exchange is what the model needs most.
    /// Compaction must not eat into it even when one message alone exceeds the budget.
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

#[cfg(test)]
mod context_tests {
    use super::*;

    fn filled() -> Session {
        let mut s = Session::new("CONTRACT".into(), 8192, 4096);
        s.push(Message::user("a question".repeat(20)));
        s.push(Message::tool("a result".repeat(80), "c1"));
        s
    }

    /// The figure is against the budget, because compaction defends the budget.
    /// 100% is the step at which the session starts forgetting.
    #[test]
    fn fullness_is_measured_against_the_budget() {
        let s = filled();
        let expected = (s.size_chars() * 100 / s.budget_chars()) as u8;
        assert_eq!(s.fullness_percent(), expected);
        assert!(s.fullness_percent() < 100);
    }

    #[test]
    fn occupancy_names_what_holds_the_window_largest_first() {
        let rows = filled().occupancy();
        assert_eq!(rows[0].0, "tool results");
        assert!(rows.iter().any(|(k, _)| k == "what you said"));
        assert!(rows
            .iter()
            .any(|(k, _)| k.contains("standing instructions")));
        assert!(rows.windows(2).all(|w| w[0].1 >= w[1].1));
    }

    /// Compaction cuts into a turn only when dropping the turns before it is not enough.
    #[test]
    fn a_turn_is_cut_only_when_earlier_turns_do_not_make_room() {
        // 1000 tokens less 200 reserved: a budget of about 2900 characters.
        let mut s = Session::new("sys".into(), 1000, 200);
        s.push(Message::user("earlier"));
        s.push(Message::tool("old".repeat(800), "c0"));
        let from = s.history.len();
        s.push(Message::user("now"));
        s.push(Message::tool("new".repeat(200), "c1"));
        assert!(s.size_chars() > s.budget_chars());
        // Dropping the earlier turn makes room.
        assert!(!s.would_cut_from(from));
        s.push(Message::tool("new".repeat(500), "c2"));
        // The turn alone is over what compaction keeps.
        assert!(s.would_cut_from(from));
        // Within the budget, nothing is cut.
        let small = Session::new("sys".into(), 1000, 200);
        assert!(!small.would_cut_from(1));
    }

    /// Asked for early, compaction does what a full window would have made it do.
    /// It leaves the budget as it found it.
    #[test]
    fn compacting_early_elides_and_restores_the_budget() {
        let mut s = filled();
        for i in 0..8 {
            s.push(Message::user(format!("step {i}")));
            s.push(Message::tool("bulk".repeat(200), format!("c{i}")));
        }
        let budget = s.budget_chars();
        let before = s.size_chars();
        let dropped = s.compact_now();
        assert!(dropped > 0, "nothing was elided");
        assert!(s.size_chars() < before);
        assert_eq!(s.budget_chars(), budget);
        // The contract is never what goes.
        assert_eq!(s.history[0].role, "system");
    }

    /// The note left where steps were elided carries what it is given.
    #[test]
    fn the_elision_note_carries_the_task_list() {
        let mut s = filled();
        for i in 0..8 {
            s.push(Message::user(format!("step {i}")));
            s.push(Message::tool("bulk".repeat(200), format!("c{i}")));
        }
        s.set_carry(Some("The task list as it stood:\n[~] write b".into()));
        assert!(s.compact_now() > 0);
        let notes: Vec<_> = s
            .history
            .iter()
            .filter(|m| m.content.starts_with(ELISION))
            .collect();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].content.contains("[~] write b"),
            "{}",
            notes[0].content
        );
    }
}

use uuid::Uuid;

use crate::agent::transcript::{Record, Transcript};
use crate::backend::wire::Message;

/// Roughly how many characters one token covers on English prose and code.
///
/// The agent has no tokenizer, and acquiring one per model would tie it to a backend it
/// deliberately does not depend on. The figure only decides *when* to compact, so being
/// off by a fifth moves the trigger point and never corrupts the conversation.
pub(crate) const CHARS_PER_TOKEN: usize = 4;

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

/// How far below the budget a compaction trims.
///
/// The cost of compacting is not the copying - it is that the server's prefix cache is
/// invalidated and the whole prompt has to be processed again. Trimming to just under the
/// line means the next step crosses it and pays that cost again immediately; trimming well
/// under it means several steps fit before the next one.
const COMPACT_TO: f64 = 0.6;

/// How much of a new observation to believe.
///
/// Each request's ratio varies with what happens to be in the history, so corrections are
/// smoothed rather than taken whole.
const SMOOTHING: f64 = 0.3;

/// Note left in place of what was dropped, so the model can tell that the conversation
/// it can see is not the whole of it.
/// Messages at the end of the history that are never dropped.
///
/// A turn that cannot see the step it just took cannot carry on from it: it re-reads what
/// it read, or answers about something it can no longer see. Four is the last call, its
/// result, and the exchange before them.
const KEEP_RECENT: usize = 4;

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

/// The most of the window the answer may be promised, as a fraction of it.
///
/// Half. The reserve is subtracted from the window before anything else is budgeted, so a
/// reserve equal to the window leaves nothing: no history, and every tool result clamped
/// to its own truncation notice. That is not a small window, it is a client that cannot
/// read, and the model answers from what it can invent instead. Asking for a shorter
/// answer than configured is the recoverable half of that trade.
const RESERVE_OF_WINDOW: usize = 2;

/// What the answer may be promised, given the window it has to be promised out of.
///
/// The configured figure, unless the window cannot afford it.
pub fn answer_reserve(context_tokens: usize, asked: usize) -> usize {
    asked.min(context_tokens / RESERVE_OF_WINDOW)
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

/// What a reply's token count said about the request that produced it.
///
/// The count is the only thing a backend says about how it read the prompt, and two very
/// different facts come out of it: the tokenizer's ratio, which sizes the history, and
/// whether the prompt arrived whole.
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
/// An eighth of the budget, and half of what one tool result may take, because this text
/// is in the system message: compaction never drops it, so it is paid on every request of
/// the session rather than once.
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

    /// Take the window of another model, and forget what the last one taught.
    ///
    /// The chars-per-token ratio was measured against one tokenizer, so carrying it to
    /// another model is carrying a measurement of something else. It goes back to the
    /// constant and is corrected again from the first reply the new model sends.
    pub fn set_window(&mut self, context_tokens: usize, reserve_tokens: usize) {
        self.usable_tokens = context_tokens.saturating_sub(reserve_tokens);
        self.chars_per_token = CHARS_PER_TOKEN as f64;
        self.budget_chars = (self.usable_tokens as f64 * self.chars_per_token * SAFETY) as usize;
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

    /// How full the window is, as a percentage of the budget.
    ///
    /// Against the budget rather than the raw window, because the budget is what
    /// compaction defends: at a hundred per cent the next step starts dropping messages,
    /// which is the number worth watching.
    pub fn fullness_percent(&self) -> u8 {
        if self.budget_chars == 0 {
            return 0;
        }
        ((self.size_chars() * 100 / self.budget_chars) as u64).min(255) as u8
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

    /// Compact now, whatever the budget says, down to the same target a full window
    /// would be taken to. Returns how many messages went.
    /// Take the conversation back to just before the last thing the user asked, and hand
    /// that back.
    ///
    /// What the agent did after it goes: the point of asking again is to ask from where it
    /// was asked the first time, not to ask a model that has already read its own failed
    /// attempt. The transcript keeps both, because what was tried is part of what
    /// happened.
    pub fn rewind_to_last_ask(&mut self) -> Option<String> {
        let at = self
            .history
            .iter()
            .rposition(|m| m.role == "user" && !m.content.is_empty())?;
        let asked = self.history[at].content.clone();
        self.history.truncate(at);
        Some(asked)
    }

    pub fn compact_now(&mut self) -> usize {
        let budget = self.budget_chars;
        // Compaction is a comparison against the budget, so asking for it early is
        // saying the budget is smaller than it is, for one pass.
        self.budget_chars = (self.size_chars() as f64 * COMPACT_TO) as usize;
        let dropped = self.compact();
        self.budget_chars = budget;
        dropped
    }

    /// Correct the estimate from what the server actually counted.
    ///
    /// `sent_chars` is what this client measured before the request; `prompt_tokens` is
    /// what the server made of it. Their ratio is the one measurement of the model's
    /// tokenizer available without shipping one, and it is the whole reason the budget
    /// can be wrong in the dangerous direction without anyone noticing.
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
        // compactly, so the server did not read all of what was sent. It cuts from the
        // front, which is where the standing instructions are, and answers fluently about
        // a question it was only shown the end of. What it did read is the window it
        // serves, which is the one fact the caller needs to stop doing this.
        if measured > MAX_RATIO {
            return Observed::Cut(prompt_tokens as usize);
        }
        if measured < MIN_RATIO {
            return Observed::Quiet;
        }

        let before = self.budget_chars;
        self.chars_per_token = self.chars_per_token * (1.0 - SMOOTHING) + measured * SMOOTHING;
        self.budget_chars = (self.usable_tokens as f64 * self.chars_per_token * SAFETY) as usize;

        // Only worth saying when the change is large enough to alter behaviour; the ratio
        // drifts a little on every request and narrating that would be noise.
        let moved = before.abs_diff(self.budget_chars);
        match moved * 10 > before.max(1) {
            true => Observed::Budget(format!(
                "context budget now {} chars ({:.1} chars/token, measured against the server)",
                self.budget_chars, self.chars_per_token
            )),
            false => Observed::Quiet,
        }
    }

    /// Drop the oldest steps until the history is comfortably inside the budget, and
    /// report how many messages went.
    ///
    /// Two invariants make this safe to do behind the model's back. The system message
    /// stays: it carries the tool manifest and the response contract, and a server that
    /// has to truncate keeps the tail, so leaving the job to the server is precisely
    /// what removes the preamble. And a step is dropped whole - an assistant turn
    /// together with the tool results it produced - because a tool result whose call is
    /// gone describes an action the model cannot see itself having taken.
    ///
    /// It trims well past the line rather than to it, which is about the
    /// server's cache and not about memory here. The server keeps a prefix cache keyed on
    /// the conversation, and dropping anything from the front changes the prefix at the
    /// second message - so every compaction throws that cache away and the whole prompt is
    /// processed again. Trimming only just enough means the next step overflows again and
    /// pays it again. Trimming well past the line means it happens rarely, which is the
    /// only lever available from this side: an overflowing conversation cannot be made
    /// cheap, but it can be made infrequent.
    pub fn compact(&mut self) -> usize {
        if self.budget_chars == 0 || self.size_chars() <= self.budget_chars {
            return 0;
        }
        let target = (self.budget_chars as f64 * COMPACT_TO) as usize;
        let mut dropped = 0;
        // Whole turns first: the conversation the model reads still opens on somebody
        // asking for something.
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
        // Then the steps of the turn still running. One long turn has no second user
        // message to cut at, so this used to have nothing it was allowed to drop: the
        // history grew past the budget and stayed there, every request went out too big,
        // and the figure on the status bar climbed until it hit its own ceiling.
        if self.size_chars() > target {
            dropped += self.drop_oldest_steps();
        }
        dropped
    }

    /// Drop the oldest steps of the turn in one cut, keeping the ask and the last few.
    ///
    /// One cut rather than a loop: a turn that has blown the budget is not going to be
    /// saved by shedding a message at a time, and a loop that has to leave the history
    /// well formed after every pass is a loop that can fail to make progress.
    ///
    /// A tool result whose call has gone is a reply to nothing, which some backends refuse
    /// outright, so the cut lands where the next message is not one.
    fn drop_oldest_steps(&mut self) -> usize {
        // Index 0 is the contract and index 1 is what was asked; neither can go, and the
        // last few steps are what the turn is carrying on from.
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
        if self.history.get(KEEP_FIRST).map(|m| m.content.as_str()) != Some(ELISION) {
            self.history.insert(KEEP_FIRST, Message::user(ELISION));
        }
        dropped
    }

    /// Say once, where the cut was made, that something was cut.
    fn mark_elision(&mut self) {
        if self.history.get(1).map(|m| m.content.as_str()) != Some(ELISION) {
            self.history.insert(1, Message::user(ELISION));
        }
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
    /// A prompt the server only partly read is said, not swallowed.
    ///
    /// The client cannot always learn the window a backend serves - one that does not
    /// describe what it holds tells it nothing - so the only remaining sign is the token
    /// count coming back for a prompt that should have produced far more. The server cuts
    /// from the front, which is where the standing instructions are, so an answer to a
    /// truncated prompt is fluent and about the wrong thing.
    #[test]
    fn a_prompt_the_backend_only_partly_read_is_reported() {
        let mut s = Session::new("sys".into(), 32_768, 4096);
        // Ten characters a token: no text does that, so the prompt was cut - and what was
        // read is the window the backend serves.
        assert!(matches!(s.observe(100_000, 10_000), Observed::Cut(10_000)));
        // An ordinary ratio is measured, not read as damage.
        let ordinary = Session::new("sys".into(), 32_768, 4096).observe(40_000, 10_000);
        assert!(!matches!(ordinary, Observed::Cut(_)));
    }

    /// A reserve equal to the window leaves nothing to read with, which is how a client
    /// ends up answering from what it can invent: the window is subtracted to zero, every
    /// tool result clamps to its own truncation notice, and nothing says so.
    #[test]
    fn the_answer_never_takes_the_whole_window() {
        // What a backend offering 4096 tokens did with the default reserve.
        assert_eq!(answer_reserve(4096, 4096), 2048);
        assert!(tool_output_chars(4096, answer_reserve(4096, 4096)) > 0);
        // A window that can afford what was asked keeps it.
        assert_eq!(answer_reserve(32_768, 4096), 4096);
    }

    use super::*;

    /// One long turn has no second user message to cut at. It used to have nothing that
    /// could be dropped, so the history grew past the budget and stayed there.
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

    /// A tool result whose call has been dropped is a reply to nothing, which some
    /// backends refuse outright.
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

    /// Asking again means asking from where it was asked, not asking a model that has
    /// already read its own failed attempt.
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

    /// The point of trimming past the line rather than up to it. Compacting invalidates the
    /// server's prefix cache, so what matters is how often it happens, not how little it
    /// removes each time.
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
        // Trimming to exactly the budget makes the next step cross it again, so it compacts
        // on roughly every step once it is full. Several steps between compactions is the
        // whole of the improvement, and it is what this asserts.
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

#[cfg(test)]
mod context_tests {
    use super::*;

    fn filled() -> Session {
        let mut s = Session::new("CONTRACT".into(), 8192, 4096);
        s.push(Message::user("a question".repeat(20)));
        s.push(Message::tool("a result".repeat(80), "c1"));
        s
    }

    /// The figure is against the budget, because the budget is what compaction defends:
    /// a hundred per cent is the step at which the session starts forgetting.
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

    /// Asked for early, compaction does what a full window would have made it do, and
    /// leaves the budget as it found it.
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
}

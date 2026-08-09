use uuid::Uuid;

use crate::backend::wire::Message;

/// Roughly how many characters one token covers on English prose and code.
///
/// The agent has no tokenizer, and acquiring one per model would tie it to a backend it
/// deliberately does not depend on. The figure only decides *when* to compact, so being
/// off by a fifth moves the trigger point and never corrupts the conversation.
const CHARS_PER_TOKEN: usize = 4;

/// Note left in place of what was dropped, so the model can tell that the conversation
/// it can see is not the whole of it.
const ELISION: &str = "[earlier steps of this session were elided to fit the context window]";

/// Conversation state for a single TUI session.
pub struct Session {
    pub id: String,
    pub history: Vec<Message>,
    /// Characters the history may occupy before older steps are dropped.
    budget_chars: usize,
}

impl Session {
    /// `context_tokens` is the window the backend is expected to offer, and
    /// `reserve_tokens` what to leave free for the answer.
    pub fn new(system_prompt: String, context_tokens: usize, reserve_tokens: usize) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            history: vec![Message::system(system_prompt)],
            budget_chars: context_tokens.saturating_sub(reserve_tokens) * CHARS_PER_TOKEN,
        }
    }

    pub fn push(&mut self, m: Message) {
        self.history.push(m);
    }

    fn size_chars(&self) -> usize {
        self.history.iter().map(|m| m.content.len() + m.role.len()).sum()
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
            let Some(next) = self.next_user_boundary() else { break };
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
        let first = (1..self.history.len()).find(|&i| {
            self.history[i].role == "user" && self.history[i].content != ELISION
        })?;
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
        let first_real = s.history.iter().skip(1).find(|m| m.content != ELISION).unwrap();
        assert_eq!(first_real.role, "user", "history: {:?}",
            s.history.iter().map(|m| m.role.as_str()).collect::<Vec<_>>());
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
        assert!(dropped > 0, "nothing was dropped despite exceeding the budget");
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

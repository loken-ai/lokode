//! What each step of a turn asked for, and what it cost.
//!
//! Every field is either known before the request goes out, or measured after it.
//! The fields known before are what a decision can use.
//! The fields measured after are what judges the decision.

use serde::{Deserialize, Serialize};

/// What was knowable before the request went out.
///
/// No model is asked anything to fill this in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Before {
    /// Characters of history the request carried.
    pub context_chars: usize,
    /// How full the history budget was, as a percentage.
    pub context_percent: u8,
    /// Which step of the turn this was, from one.
    pub step: usize,
    /// The tools the previous step called, if any.
    pub previous_tools: Vec<String>,
    /// Whether the previous step's write came back with diagnostics.
    pub previous_failed_check: bool,
    /// Whether the model was asked to hold to a schema.
    pub grammar: bool,
    /// Planning or acting.
    pub mode: String,
}

/// What the step came to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct After {
    /// The tools it asked for, in the order it asked.
    pub tools: Vec<String>,
    /// Milliseconds between the request going out and the answer being complete.
    pub model_ms: u64,
    /// Milliseconds inside the tools.
    pub tool_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// How many repair round trips the reply needed.
    pub repairs: u32,
    /// Why the model stopped, as the server said it.
    pub finish_reason: Option<String>,
    /// `Some(false)` when a write in this step came back with diagnostics.
    /// `Some(true)` when it came back clean.
    /// `None` when nothing was written.
    pub check_passed: Option<bool>,
    /// Whether any tool of the step changed something.
    pub wrote: bool,
    /// What a loken server measured of the step's request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loken: Option<crate::backend::wire::LokenUsage>,
}

/// One step, both sides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub turn: u32,
    pub model: String,
    pub before: Before,
    pub after: After,
}

impl StepRecord {
    /// The one line a person reads.
    pub fn describe(&self) -> String {
        let tools = if self.after.tools.is_empty() {
            "answered".to_string()
        } else {
            self.after.tools.join(", ")
        };
        let check = match self.after.check_passed {
            Some(true) => "  check ok",
            Some(false) => "  check failed",
            None => "",
        };
        format!(
            "turn {} step {} on {}: {tools}; {:.1}s model, {:.1}s tools, {} in {} out{}{}",
            self.turn,
            self.before.step,
            self.model,
            self.after.model_ms as f64 / 1000.0,
            self.after.tool_ms as f64 / 1000.0,
            self.after.prompt_tokens,
            self.after.completion_tokens,
            if self.after.repairs > 0 {
                format!("  {} repair(s)", self.after.repairs)
            } else {
                String::new()
            },
            check
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_reads_as_one_line() {
        let rec = StepRecord {
            turn: 2,
            model: "qwen3:8b".into(),
            before: Before {
                step: 3,
                ..Default::default()
            },
            after: After {
                tools: vec!["read_file".into(), "grep".into()],
                model_ms: 8400,
                tool_ms: 120,
                prompt_tokens: 3200,
                completion_tokens: 90,
                repairs: 1,
                check_passed: Some(false),
                ..Default::default()
            },
        };
        let said = rec.describe();
        assert!(said.contains("turn 2 step 3 on qwen3:8b"), "{said}");
        assert!(said.contains("read_file, grep"), "{said}");
        assert!(said.contains("8.4s model"), "{said}");
        assert!(said.contains("1 repair(s)"), "{said}");
        assert!(said.contains("check failed"), "{said}");
    }

    #[test]
    fn a_step_that_only_answered_says_so() {
        let rec = StepRecord {
            turn: 1,
            model: "m".into(),
            before: Before::default(),
            after: After::default(),
        };
        assert!(rec.describe().contains("answered"));
    }
}

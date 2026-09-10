//! Work done while nobody is waiting.
//!
//! A session spends its first minutes finding out what it is working on, and spends them
//! again tomorrow. The finding out is cheap to do and expensive to do at the wrong moment:
//! it costs the window and it costs the user's time, and it happens exactly when they are
//! waiting for an answer.
//!
//! So it is moved. While the user reads, types or thinks, one question about the project
//! is taken off a queue, answered by a read-only session of its own, and what it found is
//! written into the digest. The queue and the digest both outlive the session, so the work
//! is never repeated and never has to finish in one sitting.
//!
//! Three rules hold it in place. It runs only while nothing else does, it asks the server
//! at a priority that yields, and it changes nothing: it reads, and it writes one sentence
//! into a file of sentences.

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::digest::{Digest, Kind};

/// What the background worker needs to answer one question.
pub struct Discovery {
    pub backend: Arc<dyn crate::backend::provider::Provider>,
    pub registry: Arc<crate::tools::registry::Registry>,
    pub ctx: crate::tools::registry::ToolCtx,
    pub model: String,
    pub protocol: crate::agent::protocol::ToolProtocol,
    pub context_tokens: usize,
    pub max_answer_tokens: usize,
    /// Shared with the session, because what is learned has to be there for the next turn
    /// rather than for the next process.
    pub digest: Arc<Mutex<Digest>>,
}

/// How many steps one question may take.
///
/// Small deliberately. A question that needs a dozen tool calls is not a question, it is a
/// task, and a task belongs to the person whose turn it is.
const STEPS_PER_QUESTION: usize = 4;

/// What the first questions are, for a project nothing is known about.
///
/// Written down rather than asked of a model: these are the things every project has an
/// answer to, and spending a round trip to be told what to ask would be the wrong first
/// use of a background turn.
pub fn opening_questions() -> Vec<String> {
    vec![
        "What is this project, in one sentence: what does it build and what language is \
         it written in?"
            .to_string(),
        "What is the command that checks a change here - the test command, and the \
         formatter or linter if there is one?"
            .to_string(),
        "Where does the code live? Name the top directories and what each is for, in one \
         line each."
            .to_string(),
        "What conventions would someone changing this code be expected to follow? Look at \
         a few files and say what is consistent about them."
            .to_string(),
    ]
}

/// Which part of the digest an answer belongs in, guessed from the question.
///
/// Guessed rather than asked, for the same reason: a round trip to classify a sentence
/// costs more than the occasional wrong shelf.
fn kind_for(question: &str) -> Kind {
    let q = question.to_lowercase();
    if q.contains("command") || q.contains("test") || q.contains("build") {
        Kind::Build
    } else if q.contains("convention") || q.contains("style") {
        Kind::Convention
    } else if q.contains("went wrong") || q.contains("fail") {
        Kind::Pitfall
    } else {
        Kind::Layout
    }
}

impl Discovery {
    /// Answer one question, if there is one waiting. Returns what was learned.
    ///
    /// Everything here is best effort: a background turn that failed is a background turn
    /// that will be tried again tomorrow, and nothing in the foreground depends on it.
    pub async fn answer_one(&self, cancel: CancellationToken) -> Option<String> {
        let question = self.digest.lock().await.next_question()?;
        let known = self
            .digest
            .lock()
            .await
            .preamble(crate::agent::digest::MAX_PREAMBLE_CHARS);

        let mut system = crate::agent::agent_loop::Agent::build_system_prompt(
            &self.registry,
            self.protocol,
            // Planning, which is what makes every tool that changes anything refuse
            // before it runs. A background turn nobody is watching must not be able to
            // write, run a command or reach the network.
            crate::agent::agent_loop::Mode::Plan,
            Some(
                "You are looking into a project so that later work does not have to. Use \
                 the read-only tools to find out, then answer in one or two sentences, \
                 stating the fact rather than describing your search. If you cannot find \
                 out, say exactly that in one sentence.",
            ),
        );
        if let Some(known) = known {
            system.push_str("\n\nAlready known, do not repeat:\n");
            system.push_str(&known);
        }

        let mut agent = crate::agent::agent_loop::Agent {
            before_plan: None,
            history: Default::default(),
            escalated_from: None,
            clean_steps: 0,
            warming: Default::default(),
            spend: Default::default(),
            steps: Vec::new(),
            lsp: None,
            steer: tokio::sync::mpsc::channel(1).1,
            backend: self.backend.clone(),
            registry: self.registry.clone(),
            session: crate::agent::session::Session::new(
                system,
                self.context_tokens,
                self.max_answer_tokens,
            ),
            cfg: crate::agent::agent_loop::AgentConfig {
                model: self.model.clone(),
                mode: crate::agent::agent_loop::Mode::Plan,
                preload: false,
                roles: Default::default(),
                hooks: Default::default(),
                instructions: None,
                remember_model_in: None,
                context_tokens: self.context_tokens,
                models: Default::default(),
                grammar_enabled: false,
                max_steps: STEPS_PER_QUESTION,
                max_answer_tokens: self.max_answer_tokens,
                protocol: self.protocol,
                allow_fallback: false,
            },
            ctx: self.ctx.clone(),
            checkpoints: None,
            turn: 0,
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let outcome = agent
            .run_user_turn(question.clone(), cancel.clone(), tx)
            .await;
        if outcome.is_err() || cancel.is_cancelled() {
            // Put it back: an interrupted question is not an answered one, and the whole
            // point is that the work survives being interrupted.
            self.digest.lock().await.ask(&question);
            return None;
        }
        let mut answer = String::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::agent::agent_loop::AgentEvent::AssistantText(s) = event {
                if !s.trim().is_empty() {
                    answer = s;
                }
            }
        }
        let answer = answer.trim().to_string();
        if answer.is_empty() || answer.len() > 400 {
            // Nothing usable. Not asked again: a question this model cannot answer in two
            // sentences will not be answered by asking it a second time.
            tracing::info!(%question, "background question produced nothing usable");
            return None;
        }

        let mut digest = self.digest.lock().await;
        digest.learn(
            kind_for(&question),
            &answer,
            "looked into in the background",
        );
        // What was worked out here is this machine's. A statement worth keeping belongs
        // in the repository, where the rest of the team gets it too - but that is the
        // user's call, so it is suggested rather than written.
        if digest.questions.is_empty() {
            digest.suggest(
                "the project has been looked into: /digest shows what was found, and what                  is worth keeping belongs in .verve.md where everyone gets it",
            );
        }
        digest.save();
        tracing::info!(%question, %answer, "learned in the background");
        Some(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_lands_on_the_shelf_it_belongs_to() {
        assert_eq!(kind_for("What is the test command here?"), Kind::Build);
        assert_eq!(
            kind_for("What conventions would someone follow?"),
            Kind::Convention
        );
        assert_eq!(kind_for("Where does the code live?"), Kind::Layout);
    }

    /// The opening questions are the ones every project answers, so a first session has
    /// something to do without asking a model what to ask.
    #[test]
    fn a_project_nothing_is_known_about_has_somewhere_to_start() {
        let qs = opening_questions();
        assert!(qs.len() >= 3);
        assert!(qs.iter().any(|q| q.to_lowercase().contains("command")));
        assert!(qs.iter().all(|q| q.ends_with('?') || q.ends_with('.')));
    }
}

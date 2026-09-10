//! The task list the model keeps while it works.
//!
//! A step counter says how far a turn has gone and nothing about what it is doing. On a
//! task of a dozen steps, the person watching wants to know what is done, what is being
//! worked on and what is left - and a small model, whose window drops the beginning of
//! the conversation first, needs the same list to remember the goal it was given.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// Where one task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Todo,
    Doing,
    Done,
}

impl State {
    /// The mark shown beside the task.
    pub fn mark(self) -> &'static str {
        match self {
            State::Todo => "[ ]",
            State::Doing => "[~]",
            State::Done => "[x]",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub what: String,
    pub state: State,
}

/// The list, shared between the tool that writes it and the interface that draws it.
#[derive(Default)]
pub struct TaskList {
    tasks: Mutex<Vec<Task>>,
}

impl TaskList {
    pub async fn set(&self, tasks: Vec<Task>) {
        *self.tasks.lock().await = tasks;
    }

    pub async fn get(&self) -> Vec<Task> {
        self.tasks.lock().await.clone()
    }

    /// The list as the model is told it back, so it reads the same thing the user sees.
    pub fn render(tasks: &[Task]) -> String {
        if tasks.is_empty() {
            return "the task list is empty".to_string();
        }
        tasks
            .iter()
            .map(|t| format!("{} {}", t.state.mark(), t.what))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub struct UpdatePlan {
    pub tasks: Arc<TaskList>,
}

#[derive(Deserialize)]
struct Args {
    tasks: Vec<Task>,
}

#[async_trait]
impl Tool for UpdatePlan {
    fn name(&self) -> &str {
        "update_plan"
    }

    /// Writes nothing outside this process, so it needs no approval and no checkpoint.
    fn mutates(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Record the task list for work of several steps, and update it as each is started \
         and finished. Send the whole list every time."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "what": {"type": "string"},
                            "state": {"type": "string", "enum": ["todo", "doing", "done"]}
                        },
                        "required": ["what", "state"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["tasks"],
            "additionalProperties": false
        })
    }

    async fn run(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: Args = serde_json::from_value(args)?;
        // At most a dozen: a list longer than the screen is one nobody reads, and it is
        // paid for in the window on every later step.
        if a.tasks.len() > 12 {
            return Ok(ToolOutput::err(
                "keep the list to twelve tasks or fewer; group the rest".to_string(),
            ));
        }
        let text = TaskList::render(&a.tasks);
        self.tasks.set(a.tasks).await;
        Ok(ToolOutput::ok(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};

    fn args(rows: &[(&str, &str)]) -> Value {
        json!({
            "tasks": rows
                .iter()
                .map(|(what, state)| json!({"what": what, "state": state}))
                .collect::<Vec<_>>()
        })
    }

    #[tokio::test]
    async fn the_list_is_kept_and_read_back_as_it_is_shown() {
        let h = Harness::new(Answer::Yes);
        let tasks = Arc::new(TaskList::default());
        let tool = UpdatePlan {
            tasks: tasks.clone(),
        };
        let out = tool
            .run(
                &h.ctx,
                args(&[("read the loader", "done"), ("fix the budget", "doing")]),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, "[x] read the loader\n[~] fix the budget");
        assert_eq!(tasks.get().await.len(), 2);

        // The whole list is sent each time, so the second call replaces the first.
        tool.run(&h.ctx, args(&[("fix the budget", "done")]))
            .await
            .unwrap();
        assert_eq!(tasks.get().await.len(), 1);
        assert_eq!(tasks.get().await[0].state, State::Done);
    }

    /// A list longer than the screen is one nobody reads, and it is paid for in the
    /// window on every later step.
    #[tokio::test]
    async fn an_overlong_list_is_refused() {
        let h = Harness::new(Answer::Yes);
        let rows: Vec<(String, &str)> = (0..13).map(|i| (format!("task {i}"), "todo")).collect();
        let pairs: Vec<(&str, &str)> = rows.iter().map(|(w, s)| (w.as_str(), *s)).collect();
        let tool = UpdatePlan {
            tasks: Arc::new(TaskList::default()),
        };
        let out = tool.run(&h.ctx, args(&pairs)).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("twelve"), "{out:?}");
    }
}

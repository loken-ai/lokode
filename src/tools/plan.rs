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

/// Put the plan to the user at the end of planning. Approved, it becomes the task list and
/// the session acts on it.
pub struct SubmitPlan {
    pub tasks: Arc<TaskList>,
}

#[derive(Deserialize)]
struct Plan {
    /// What the plan does, in a few lines.
    summary: String,
    steps: Vec<String>,
}

/// What the model is told when its plan is approved, before the plan itself.
pub const APPROVED: &str = "The user approved the plan.";

#[async_trait]
impl Tool for SubmitPlan {
    fn name(&self) -> &str {
        "submit_plan"
    }

    /// It asks the user and changes nothing, so it can be called while planning.
    fn mutates(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "While planning, put the finished plan to the user: a short summary and the steps, \
         each naming what it would run or change. Approved, planning ends, the steps become \
         the task list and the work is carried out. Refused, the user says what to change."
    }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string"},
                "steps": {"type": "array", "items": {"type": "string"}, "minItems": 1}
            },
            "required": ["summary", "steps"],
            "additionalProperties": false
        })
    }

    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        use crate::sandbox::approval::{Approval, ApprovalRequest, ApprovalScope};
        let p: Plan = serde_json::from_value(args)?;
        if !ctx.planning {
            return Ok(ToolOutput::err(
                "the session is not planning: carry the work out, and keep the task list with \
                 update_plan",
            ));
        }
        if p.steps.len() > 12 {
            return Ok(ToolOutput::err(
                "keep the plan to twelve steps or fewer; group the rest".to_string(),
            ));
        }
        let shown = format!(
            "{}\n\n{}",
            p.summary.trim(),
            p.steps
                .iter()
                .enumerate()
                .map(|(i, s)| format!("{}. {}", i + 1, s.trim()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        match ctx
            .approve(ApprovalRequest {
                tool: self.name().into(),
                summary: "act on this plan".into(),
                diff_preview: Some(shown.clone()),
                scope: ApprovalScope::Tool(self.name().into()),
            })
            .await
        {
            Approval::Proceed => {}
            Approval::Denied(why) => {
                return Ok(ToolOutput::err(format!(
                    "{}. Keep planning, and submit the plan again once it is changed.",
                    crate::sandbox::approval::denial(why)
                )))
            }
            Approval::Interrupted => return Ok(ToolOutput::err("interrupted by user")),
        }
        let tasks: Vec<Task> = p
            .steps
            .into_iter()
            .map(|what| Task {
                what,
                state: State::Todo,
            })
            .collect();
        let list = TaskList::render(&tasks);
        self.tasks.set(tasks).await;
        Ok(ToolOutput::ok(format!(
            "{APPROVED} Planning is over: carry it out now, step by step, and keep the task \
             list with update_plan.\n{list}"
        )))
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

//! Commands started in the background: their output is kept as it comes, read later, and the
//! process group is stopped on demand or when the session ends.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tools::bash::Group;
use crate::tools::registry::{Tool, ToolCtx, ToolOutput};

/// The output of a background command, kept within the size of one tool result.
struct Output {
    text: String,
    /// The bytes of `text` already read.
    read: usize,
    /// The lines dropped from the start to stay within `limit`.
    dropped: usize,
    limit: usize,
}

impl Output {
    fn push(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
        while self.text.len() > self.limit {
            let Some(end) = self.text.find('\n') else {
                break;
            };
            self.text.drain(..=end);
            self.read = self.read.saturating_sub(end + 1);
            self.dropped += 1;
        }
    }

    /// What was added since the last read, with the lines dropped before it.
    fn unread(&mut self) -> String {
        let mut out = String::new();
        if self.dropped > 0 {
            out.push_str(&format!("… ({} earlier lines dropped)\n", self.dropped));
            self.dropped = 0;
        }
        out.push_str(&self.text[self.read..]);
        self.read = self.text.len();
        out
    }
}

struct Proc {
    cmd: String,
    output: Arc<Mutex<Output>>,
    child: tokio::process::Child,
    /// Dropping the process kills its whole group.
    _group: Group,
}

fn procs() -> &'static Mutex<HashMap<u64, Proc>> {
    static PROCS: OnceLock<Mutex<HashMap<u64, Proc>>> = OnceLock::new();
    PROCS.get_or_init(Default::default)
}

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Keep a spawned command running, its output read as it comes; returns its id.
///
/// `limit` is the most characters of output kept.
pub(crate) fn start(
    cmd: String,
    mut child: tokio::process::Child,
    group: Group,
    limit: usize,
) -> u64 {
    let output = Arc::new(Mutex::new(Output {
        text: String::new(),
        read: 0,
        dropped: 0,
        limit: limit.max(1),
    }));
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(collect(stdout, output.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(collect(stderr, output.clone()));
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let proc = Proc {
        cmd,
        output,
        child,
        _group: group,
    };
    if let Ok(mut all) = procs().lock() {
        all.insert(id, proc);
    }
    id
}

async fn collect(stream: impl tokio::io::AsyncRead + Unpin, output: Arc<Mutex<Output>>) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(mut out) = output.lock() {
            out.push(&line);
        }
    }
}

/// Stop every background command of the session.
pub fn stop_all() {
    if let Ok(mut all) = procs().lock() {
        all.clear();
    }
}

fn unknown(id: u64) -> ToolOutput {
    let mut ids: Vec<u64> = procs()
        .lock()
        .map(|all| all.keys().copied().collect())
        .unwrap_or_default();
    ids.sort_unstable();
    ToolOutput::err(match ids.is_empty() {
        true => format!("no background process {id}: none is running"),
        false => format!("no background process {id}; the ones started are {ids:?}"),
    })
}

fn status(child: &mut tokio::process::Child) -> String {
    match child.try_wait() {
        Ok(None) => "running".into(),
        Ok(Some(s)) => match s.code() {
            Some(code) => format!("exited {code}"),
            None => "killed".into(),
        },
        Err(e) => format!("unknown ({e})"),
    }
}

#[derive(Deserialize)]
struct IdArgs {
    id: u64,
}

fn id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"id": {"type": "integer", "minimum": 1}},
        "required": ["id"],
        "additionalProperties": false
    })
}

pub struct ProcessOutput;

#[async_trait]
impl Tool for ProcessOutput {
    fn name(&self) -> &str {
        "process_output"
    }
    fn description(&self) -> &str {
        "Read what a command started with bash's background has printed since the last read, \
         and whether it still runs."
    }
    fn args_schema(&self) -> Value {
        id_schema()
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: IdArgs = serde_json::from_value(args)?;
        let Ok(mut all) = procs().lock() else {
            return Ok(unknown(a.id));
        };
        let Some(proc) = all.get_mut(&a.id) else {
            drop(all);
            return Ok(unknown(a.id));
        };
        let state = status(&mut proc.child);
        let new = proc
            .output
            .lock()
            .map(|mut o| o.unread())
            .unwrap_or_default();
        let new = match new.is_empty() {
            true => "(no new output)".to_string(),
            false => new,
        };
        Ok(ToolOutput::ok(
            ctx.clamp(format!("{state}  ({})\n{new}", proc.cmd)),
        ))
    }
}

pub struct ProcessStop;

#[async_trait]
impl Tool for ProcessStop {
    fn name(&self) -> &str {
        "process_stop"
    }
    fn mutates(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        "Stop a command started with bash's background, and everything it started, and give \
         its final status and unread output."
    }
    fn args_schema(&self) -> Value {
        id_schema()
    }
    async fn run(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput> {
        let a: IdArgs = serde_json::from_value(args)?;
        let proc = procs().lock().ok().and_then(|mut all| all.remove(&a.id));
        let Some(Proc {
            cmd,
            output,
            mut child,
            _group,
        }) = proc
        else {
            return Ok(unknown(a.id));
        };
        // Dropping the group kills it.
        drop(_group);
        let state = match child.wait().await {
            Ok(s) => match s.code() {
                Some(code) => format!("exited {code}"),
                None => "killed".into(),
            },
            Err(e) => format!("unknown ({e})"),
        };
        let rest = output.lock().map(|mut o| o.unread()).unwrap_or_default();
        Ok(ToolOutput::ok(
            ctx.clamp(format!("{state}  ({cmd})\n{rest}")),
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tools::harness::{Answer, Harness};

    async fn start_in_background(h: &Harness, cmd: &str) -> u64 {
        let out = h
            .run(
                &crate::tools::bash::Bash,
                json!({"cmd": cmd, "background": true}),
            )
            .await;
        assert!(!out.is_error, "{out:?}");
        out.content
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("started process "))
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no process id in {:?}", out.content))
    }

    /// A background command answers at once, and its output is read once, after it ends.
    #[tokio::test]
    async fn a_background_command_is_read_later() {
        let h = Harness::new(Answer::Yes);
        let id = start_in_background(&h, "sleep 0.2; echo hi").await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let out = h.run(&ProcessOutput, json!({"id": id})).await;
        assert!(out.content.starts_with("exited 0"), "{out:?}");
        assert!(out.content.contains("hi"), "{out:?}");
        let again = h.run(&ProcessOutput, json!({"id": id})).await;
        assert!(again.content.contains("(no new output)"), "{again:?}");
    }

    /// Stopping a background command stops what it started.
    #[tokio::test]
    async fn a_stopped_background_command_takes_its_children() {
        let h = Harness::new(Answer::Yes);
        let id = start_in_background(&h, "sleep 30 & echo $! > child.pid; wait").await;
        let pid_file = h.root.join("child.pid");
        for _ in 0..50 {
            if std::fs::read_to_string(&pid_file).is_ok_and(|s| !s.trim().is_empty()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .expect("the child wrote its pid");
        let out = h.run(&ProcessStop, json!({"id": id})).await;
        assert!(!out.is_error, "{out:?}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // SAFETY: signal 0 only asks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the child {pid} outlived its stopped command");
    }

    #[tokio::test]
    async fn an_unknown_process_is_an_error() {
        let h = Harness::new(Answer::Yes);
        let out = h.run(&ProcessOutput, json!({"id": 999_999})).await;
        assert!(out.is_error, "{out:?}");
    }

    /// The output keeps within its limit, saying how many lines it dropped.
    #[test]
    fn output_keeps_within_its_limit() {
        let mut out = Output {
            text: String::new(),
            read: 0,
            dropped: 0,
            limit: 8,
        };
        for line in ["aaa", "bbb", "ccc"] {
            out.push(line);
        }
        assert_eq!(out.unread(), "… (1 earlier lines dropped)\nbbb\nccc\n");
        assert_eq!(out.unread(), "");
    }
}

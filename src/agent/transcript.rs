//! A durable record of a session, one JSON object per line.
//!
//! It has 2 jobs.
//!
//! The first is inspection.
//! The history lives in memory and compaction discards the parts that fell out of the window.
//! The transcript keeps what happened, so a bad run can be examined afterwards.
//!
//! The second is resuming.
//! What is written is the whole conversation, including the steps compaction later elided.
//! The file is a log of what happened, not a snapshot of what the model could still see.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::backend::wire::Message;

/// One line of a transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// Written once, first. Carries what a later `--continue` has to match on.
    Meta {
        id: String,
        root: PathBuf,
        model: String,
        /// Seconds since the epoch, for ordering sessions without parsing dates.
        started_at: u64,
    },
    Message(Message),
    /// The model changed mid-session.
    /// A resume comes back on the model the session was last using.
    Model {
        name: String,
    },
    /// The session started or stopped planning.
    Mode {
        planning: bool,
    },
    /// The task list as it stands after the model changed it.
    Tasks {
        tasks: Vec<crate::tools::plan::Task>,
    },
    /// One step of a turn, with what was knowable before it and what it cost.
    ///
    /// Kept in the same file as the conversation, so a session can be read back and measured.
    Step(crate::agent::steps::StepRecord),
    /// Something the user was told that is not part of the conversation.
    Notice {
        text: String,
    },
    /// What a turn's requests drew, by what they were for, as the backend measured it.
    Energy(Vec<(crate::backend::meter::Purpose, crate::backend::meter::Spent)>),
    /// The conversation was started again from nothing.
    ///
    /// The record stays in the file.
    /// A resume does not bring back the messages before it.
    Cleared,
}

/// Appends records to a session file without holding up the turn.
///
/// Writing happens on its own task, so no `push` waits on the disk.
#[derive(Clone)]
pub struct Transcript {
    tx: mpsc::UnboundedSender<Record>,
    path: PathBuf,
}

impl Transcript {
    /// Open a transcript, returning it and the task that does the writing.
    ///
    /// The caller has to wait for the task before the process exits.
    /// Anything still sitting in the channel at that moment would never reach the disk.
    pub fn create(path: PathBuf) -> Result<(Self, tokio::task::JoinHandle<()>)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Record>();
        let mut file = tokio::fs::File::from_std(file);
        let writer = tokio::spawn(async move {
            while let Some(rec) = rx.recv().await {
                let Ok(mut line) = serde_json::to_vec(&rec) else {
                    continue;
                };
                line.push(b'\n');
                if file.write_all(&line).await.is_err() {
                    // Nowhere left to report this: the transcript is the reporting channel.
                    // Stop rather than spin on a broken file.
                    return;
                }
                // Flushed per record, so a killed session leaves its records on disk.
                let _ = file.flush().await;
            }
            let _ = file.flush().await;
        });

        Ok((Self { tx, path }, writer))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record something.
    /// Failure is silent: a session does not end because its log could not be written.
    pub fn append(&self, rec: Record) {
        let _ = self.tx.send(rec);
    }
}

/// A session read back off disk.
pub struct Replay {
    pub id: String,
    pub root: PathBuf,
    pub model: String,
    /// Whether the session was planning when it was last recorded.
    pub planning: bool,
    /// The task list as it was last recorded.
    pub tasks: Vec<crate::tools::plan::Task>,
    /// Every message recorded, in order, excluding the system message.
    ///
    /// The preamble is rebuilt from the current tool registry rather than replayed.
    /// The tools available now are the ones the model must be told about.
    pub messages: Vec<Message>,
}

/// Read a transcript back.
///
/// A line that will not parse is skipped rather than fatal.
/// The file is appended to by a process that can be killed mid-write.
/// So a truncated last line is expected, and it does not stop the resume.
pub fn replay(path: &Path) -> Result<Replay> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let mut id = None;
    let mut root = None;
    let mut model = None;
    let mut messages = Vec::new();
    let mut planning = false;
    let mut tasks = Vec::new();

    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        match rec {
            Record::Meta {
                id: i,
                root: r,
                model: m,
                ..
            } => {
                id = Some(i);
                root = Some(r);
                model = Some(m);
            }
            // A later switch wins over the one the session opened with.
            Record::Model { name } => model = Some(name),
            Record::Mode { planning: p } => planning = p,
            Record::Tasks { tasks: t } => tasks = t,
            // Everything before it was dropped, so it does not come back.
            Record::Cleared => {
                messages.clear();
                tasks.clear();
            }
            Record::Message(m) if m.role != "system" => messages.push(m),
            _ => {}
        }
    }

    Ok(Replay {
        id: id.unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        root: root.unwrap_or_default(),
        model: model.unwrap_or_default(),
        planning,
        tasks,
        messages,
    })
}

/// A session on disk, as much of it as is needed to recognise it.
pub struct Recorded {
    pub id: String,
    pub path: PathBuf,
    /// Seconds since the epoch, from when the file last changed.
    pub when: u64,
    /// The first thing asked in it, on one line.
    pub opening: String,
    /// How much was said in it.
    pub messages: usize,
}

/// Every session recorded against `root`, most recent first.
///
/// Matched on the root, because a session belonging to another project is not one to
/// come back to.
pub fn recorded_for_root(dir: &Path, root: &Path) -> Vec<Recorded> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Recorded> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        // The whole file is read.
        let Ok(replayed) = replay(&path) else {
            continue;
        };
        if replayed.root != root {
            continue;
        }
        let Ok(when) = entry.metadata().and_then(|m| m.modified()).and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .map_err(|e| std::io::Error::other(e.to_string()))
        }) else {
            continue;
        };
        out.push(Recorded {
            id: replayed.id,
            path,
            when,
            opening: replayed
                .messages
                .iter()
                .find(|m| m.role == "user")
                .and_then(|m| m.content.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("nothing was asked")
                .chars()
                .take(90)
                .collect(),
            messages: replayed.messages.len(),
        });
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.when));
    out
}

/// The most recent session recorded against `root`.
pub fn latest_for_root(dir: &Path, root: &Path) -> Option<PathBuf> {
    recorded_for_root(dir, root)
        .into_iter()
        .next()
        .map(|r| r.path)
}

/// Seconds since the epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn write_session(dir: &Path, id: &str, root: &Path, msgs: &[Message]) -> PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        let (t, writer) = Transcript::create(path.clone()).unwrap();
        t.append(Record::Meta {
            id: id.into(),
            root: root.to_path_buf(),
            model: "test-model".into(),
            started_at: 0,
        });
        for m in msgs {
            t.append(Record::Message(m.clone()));
        }
        // Dropping the last sender tells the writer there is nothing more coming.
        // Joining it guarantees the bytes are there to be read back.
        drop(t);
        let _ = writer.await;
        path
    }

    #[tokio::test]
    async fn a_session_reads_back_as_it_was_written() {
        let dir = tempdir().unwrap();
        let msgs = vec![
            Message::user("do the thing"),
            Message::assistant("{\"assistant_text\":\"done\"}"),
        ];
        let path = write_session(dir.path(), "abc", Path::new("/p"), &msgs).await;

        let back = replay(&path).unwrap();
        assert_eq!(back.id, "abc");
        assert_eq!(back.root, Path::new("/p"));
        assert_eq!(back.model, "test-model");
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.messages[0].content, "do the thing");
    }

    /// The mode and the task list come back as last recorded, and a clear drops the list.
    #[tokio::test]
    async fn the_mode_and_the_task_list_come_back() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("m.jsonl");
        let (t, writer) = Transcript::create(path.clone()).unwrap();
        let task = crate::tools::plan::Task {
            what: "write b".into(),
            state: crate::tools::plan::State::Doing,
        };
        t.append(Record::Mode { planning: true });
        t.append(Record::Tasks {
            tasks: vec![task.clone()],
        });
        drop(t);
        let _ = writer.await;
        let back = replay(&path).unwrap();
        assert!(back.planning);
        assert_eq!(back.tasks, [task]);

        let (t, writer) = Transcript::create(path.clone()).unwrap();
        t.append(Record::Cleared);
        drop(t);
        let _ = writer.await;
        assert!(replay(&path).unwrap().tasks.is_empty());
    }

    /// The preamble describes the tools available now, not the ones available when the
    /// session was recorded.
    #[tokio::test]
    async fn the_system_message_is_not_replayed() {
        let dir = tempdir().unwrap();
        let msgs = vec![Message::system("an old contract"), Message::user("hello")];
        let path = write_session(dir.path(), "abc", Path::new("/p"), &msgs).await;

        let back = replay(&path).unwrap();
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.messages[0].role, "user");
    }

    /// Killed mid-write is the ordinary way for one of these files to end.
    #[test]
    fn a_truncated_final_line_does_not_lose_the_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            "{\"kind\":\"meta\",\"id\":\"x\",\"root\":\"/p\",\"model\":\"m\",\"started_at\":0}\n\
             {\"kind\":\"message\",\"role\":\"user\",\"content\":\"kept\"}\n\
             {\"kind\":\"mess",
        )
        .unwrap();

        let back = replay(&path).unwrap();
        assert_eq!(back.id, "x");
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.messages[0].content, "kept");
    }

    /// Continuing in one project must not pick up a session from another.
    #[tokio::test]
    async fn the_latest_session_is_scoped_to_its_project() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "other", Path::new("/elsewhere"), &[]).await;
        let mine = write_session(dir.path(), "mine", Path::new("/here"), &[]).await;

        assert_eq!(latest_for_root(dir.path(), Path::new("/here")), Some(mine));
        assert_eq!(
            latest_for_root(dir.path(), Path::new("/nothing-here")),
            None
        );
    }
}

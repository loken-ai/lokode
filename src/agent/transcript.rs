//! A durable record of a session, one JSON object per line.
//!
//! Two jobs, and the first is the one that matters more often: when a session goes wrong
//! there is currently nothing left to look at afterwards. The history lives in memory,
//! compaction discards the parts that fell out of the window, and the process takes the
//! rest with it. A transcript is what makes a bad run answerable a day later.
//!
//! The second is resuming. What is written is the whole conversation, including the steps
//! compaction later elided - the file is a log of what happened, not a snapshot of what
//! the model could still see.

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
    /// The model changed mid-session. Recorded so a resume comes back on the model the
    /// session was last using rather than the one it opened with.
    Model {
        name: String,
    },
    /// One step of a turn, with what was knowable before it and what it cost.
    ///
    /// Kept in the same file as the conversation so a session can be read back and
    /// measured, which is what any later claim about routing will have to rest on.
    Step(crate::agent::steps::StepRecord),
    /// Something the user was told that is not part of the conversation.
    Notice {
        text: String,
    },
}

/// Appends records to a session file without holding up the turn.
///
/// Writing happens on its own task: a transcript that made every `push` wait on the disk
/// would put filesystem latency inside the agent loop, and losing the last line of a log
/// is a smaller problem than that.
#[derive(Clone)]
pub struct Transcript {
    tx: mpsc::UnboundedSender<Record>,
    path: PathBuf,
}

impl Transcript {
    /// Open a transcript, returning it and the task that does the writing.
    ///
    /// The handle comes back rather than being hidden inside, because the caller has to
    /// be able to wait for it: the process exits as soon as the UI returns, and anything
    /// still sitting in the channel at that moment would never reach the disk. The last
    /// thing a session records is usually the most interesting part of it.
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
                    // Nowhere left to report this: the transcript is the reporting
                    // channel. Stop rather than spin on a broken file.
                    return;
                }
                // Flushed per record rather than at the end: a session that is killed is
                // exactly the session someone will want to read afterwards.
                let _ = file.flush().await;
            }
            let _ = file.flush().await;
        });

        Ok((Self { tx, path }, writer))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record something. Failure is deliberately silent: a session must not end because
    /// its log could not be written.
    pub fn append(&self, rec: Record) {
        let _ = self.tx.send(rec);
    }
}

/// A session read back off disk.
pub struct Replay {
    pub id: String,
    pub root: PathBuf,
    pub model: String,
    /// Every message recorded, in order, excluding the system message.
    ///
    /// The preamble is rebuilt from the current tool registry rather than replayed: the
    /// tools available now are the ones the model must be told about, and they are not
    /// necessarily the ones it was told about then.
    pub messages: Vec<Message>,
}

/// Read a transcript back.
///
/// A line that will not parse is skipped rather than fatal. The file is appended to by a
/// process that can be killed mid-write, so a truncated last line is an ordinary thing to
/// find, and refusing to resume because of it would throw away the whole session over its
/// final fragment.
pub fn replay(path: &Path) -> Result<Replay> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let mut id = None;
    let mut root = None;
    let mut model = None;
    let mut messages = Vec::new();

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
        messages,
    })
}

/// The most recent session recorded against `root`.
///
/// Matched on the root rather than on time alone, because `--continue` in one project
/// should not pick up a session belonging to another.
pub fn latest_for_root(dir: &Path, root: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        // Only the first line is needed, but these files are small enough that being
        // clever about it would cost more in complexity than it saves in reads.
        let Ok(replayed) = replay(&path) else {
            continue;
        };
        if replayed.root != root {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()).and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .map_err(|e| std::io::Error::other(e.to_string()))
        }) else {
            continue;
        };
        if best.as_ref().is_none_or(|(t, _)| modified > *t) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, p)| p)
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
        // Dropping the last sender is what tells the writer there is nothing more coming;
        // joining it is what guarantees the bytes are there to be read back.
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

    /// The preamble describes the tools available now, which are not necessarily the ones
    /// available when the session was recorded.
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

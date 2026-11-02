use std::collections::HashSet;
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, oneshot, Mutex};

/// What an `Always` answer covers for the rest of the session.
///
/// Granted against what the user was shown, not against the tool that showed it.
/// Agreeing to one `ls` does not hand over a free shell.
/// Agreeing to one file being written does not hand over every file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalScope {
    /// One shell command, matched in full.
    ///
    /// The whole string is matched, not the program at the front of it.
    /// `ls` and `ls; rm -rf ~` share a first word, so the second does not inherit an
    /// agreement given to the first.
    /// The same command run again is covered.
    Command(String),
    /// One shell command, matched in full, run outside the sandbox.
    Unconfined(String),
    /// Writes by one tool anywhere under a directory.
    Path { tool: String, prefix: PathBuf },
    /// Network egress to one host, or to one port of it when written `host:port`.
    ///
    /// An address with colons is written in brackets before its port: `[::1]:443`.
    Host(String),
    /// Every call of a tool whose whole surface is the single action just approved.
    Tool(String),
    /// What a session elsewhere said `Always` covers, in its own words.
    Told(String),
}

impl ApprovalScope {
    /// How to describe this scope to the user, so `Always` is never wider than it reads.
    pub fn describe(&self) -> String {
        self.describe_under(std::path::Path::new(""))
    }

    /// The same, with paths written as the project sees them.
    ///
    /// A prompt is read in a narrow pane.
    /// The part of a path that says what is about to change is the part below the project root.
    pub fn describe_under(&self, root: &std::path::Path) -> String {
        match self {
            ApprovalScope::Command(cmd) => format!("this exact command ({cmd})"),
            ApprovalScope::Unconfined(cmd) => {
                format!("this exact command outside the sandbox ({cmd})")
            }
            ApprovalScope::Path { tool, prefix } => {
                let shown = prefix.strip_prefix(root).unwrap_or(prefix).display();
                let shown = shown.to_string();
                if shown.is_empty() {
                    format!("{tool} anywhere in the project")
                } else {
                    format!("{tool} anywhere under {shown}")
                }
            }
            ApprovalScope::Host(host) => format!("requests to {host}"),
            ApprovalScope::Tool(tool) => format!("every {tool} call"),
            ApprovalScope::Told(said) => said.clone(),
        }
    }

    /// Whether an agreement already given covers this request.
    ///
    /// Only `Path` is not an equality test.
    /// An agreement given while looking at a file in a directory covers that directory's
    /// contents.
    fn covered_by(&self, granted: &ApprovalScope) -> bool {
        match (granted, self) {
            (
                ApprovalScope::Path {
                    tool: gt,
                    prefix: gp,
                },
                ApprovalScope::Path {
                    tool: st,
                    prefix: sp,
                },
            ) => gt == st && sp.starts_with(gp),
            (ApprovalScope::Host(g), ApprovalScope::Host(h)) => {
                let (gh, gp) = split_authority(g);
                let (hh, hp) = split_authority(h);
                gh == hh && (gp.is_none() || gp == hp)
            }
            _ => granted == self,
        }
    }
}

/// A host and a port written as one name: `host:port`, with brackets around an address that
/// has colons.
pub fn authority(host: &str, port: u16) -> String {
    match host.contains(':') {
        true => format!("[{host}]:{port}"),
        false => format!("{host}:{port}"),
    }
}

/// The host, and the port when one is written, of a name made by `authority` or of a rule.
///
/// A rule may hold a wildcard in its host part, and a name with no port stands for every port.
pub fn split_authority(name: &str) -> (&str, Option<u16>) {
    if let Some(rest) = name.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((host, "")) => (host, None),
            Some((host, tail)) => match tail.strip_prefix(':').and_then(|p| p.parse().ok()) {
                Some(port) => (host, Some(port)),
                None => (name, None),
            },
            None => (name, None),
        };
    }
    match name.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => match port.parse() {
            Ok(port) => (host, Some(port)),
            Err(_) => (name, None),
        },
        _ => (name, None),
    }
}

/// Directory an `Always` answer about `target` should cover.
///
/// The file's parent.
pub fn write_scope(tool: &str, target: &Path) -> ApprovalScope {
    ApprovalScope::Path {
        tool: tool.to_string(),
        prefix: target.parent().unwrap_or(target).to_path_buf(),
    }
}

/// What kind of approval is being requested.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub summary: String,
    /// Optional unified-diff preview (write/edit/patch tools).
    pub diff_preview: Option<String>,
    /// What answering `Always` would agree to from here on.
    pub scope: ApprovalScope,
}

/// Outcome of a single approval prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Once,
    Always,
    /// Refused, with what the person said about it when they said anything.
    ///
    /// The words let the model know the call was the wrong call.
    Deny(Option<String>),
}

/// What a tool should do once the question has been settled, one way or another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    Proceed,
    Denied(Denial),
    /// The turn was abandoned while the prompt was on screen.
    /// The user refused nothing, so it is not `Denied`.
    Interrupted,
}

/// Who refused a call, and what they said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// The person, with what they said when they said anything.
    User(Option<String>),
    /// A rule of the configuration, named.
    Rule(String),
}

/// What a tool hands back when its call was refused.
///
/// Carries who refused it and why, so the model has something to act on.
pub fn denial(why: Denial) -> String {
    match why {
        Denial::User(None) => "user denied".to_string(),
        Denial::User(Some(why)) => format!("user denied: {why}"),
        Denial::Rule(why) => format!("{why}, without asking the user"),
    }
}

#[derive(Debug)]
pub struct ApprovalPrompt {
    pub req: ApprovalRequest,
    pub reply: oneshot::Sender<ApprovalDecision>,
    /// The tool call asking, when a call asks.
    pub call: Option<String>,
}

/// Sent from the agent loop to the UI to ask for approval.
pub type ApprovalSender = mpsc::Sender<ApprovalPrompt>;
/// Received by the UI to display the modal.
pub type ApprovalReceiver = mpsc::Receiver<ApprovalPrompt>;

pub fn channel() -> (ApprovalSender, ApprovalReceiver) {
    mpsc::channel(8)
}

/// Tracks the `Always` agreements given during one session, and those kept for its
/// project when it keeps them.
#[derive(Default, Debug)]
pub struct SessionApprovals {
    always: Mutex<HashSet<ApprovalScope>>,
    /// The project the agreements are kept for between sessions, when they are.
    kept: Option<PathBuf>,
    /// Whether writes go ahead without being asked about, for as long as it is set.
    ///
    /// Writes and nothing else.
    /// A shell command and a request leaving the machine are still asked about.
    writes: AtomicBool,
    /// The hosts an audit let through, which are listed for `/net save` and agreed to by nothing else.
    audited: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// What the commands' network has seen, which the questions about it are answered from.
    journal: std::sync::Arc<crate::sandbox::egress::Journal>,
}

impl SessionApprovals {
    pub fn new() -> Self {
        Self::default()
    }

    /// The agreements kept for the project at `root`, which later ones are added to.
    pub fn kept_for(root: &Path) -> Self {
        let always = read_kept()
            .remove(&root.display().to_string())
            .unwrap_or_default()
            .into_iter()
            .collect();
        Self {
            always: Mutex::new(always),
            kept: Some(root.to_path_buf()),
            journal: std::sync::Arc::new(crate::sandbox::egress::Journal::kept_for(root)),
            ..Self::default()
        }
    }

    /// Write the agreements held into the store of the project they are kept for.
    fn keep(&self, held: &HashSet<ApprovalScope>) {
        let (Some(root), Some(path)) = (&self.kept, kept_path()) else {
            return;
        };
        let mut all = read_kept();
        let key = root.display().to_string();
        if held.is_empty() {
            all.remove(&key);
        } else {
            all.insert(key, held.iter().cloned().collect());
        }
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| {
                std::fs::write(
                    &path,
                    serde_json::to_string_pretty(&all).unwrap_or_default(),
                )
            });
        if let Err(e) = written {
            tracing::warn!(path = %path.display(), error = %e, "agreements not kept");
        }
    }

    pub async fn is_always(&self, scope: &ApprovalScope) -> bool {
        self.always.lock().await.iter().any(|g| scope.covered_by(g))
    }

    pub async fn mark_always(&self, scope: ApprovalScope) {
        let mut held = self.always.lock().await;
        held.insert(scope);
        self.keep(&held);
    }

    /// What the commands' network has seen.
    pub fn journal(&self) -> std::sync::Arc<crate::sandbox::egress::Journal> {
        self.journal.clone()
    }

    /// Note a host that an audit let through.
    ///
    /// It is listed with the agreed hosts, but it stays asked about in a later session.
    pub fn audited(&self, host: &str) {
        self.audited.lock().unwrap().insert(host.to_string());
    }

    /// The hosts agreed to with `Always`, and those an audit let through, by name.
    pub async fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .always
            .lock()
            .await
            .iter()
            .filter_map(|s| match s {
                ApprovalScope::Host(h) => Some(h.clone()),
                _ => None,
            })
            .collect();
        hosts.extend(self.audited.lock().unwrap().iter().cloned());
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// Whether agreements outlive the session.
    pub fn keeps(&self) -> bool {
        self.kept.is_some()
    }

    /// What has been agreed to for the rest of this session, in the words the prompt
    /// used when it was agreed to.
    pub async fn standing(&self, root: &std::path::Path) -> Vec<String> {
        let mut out: Vec<String> = self
            .always
            .lock()
            .await
            .iter()
            .map(|s| s.describe_under(root))
            .collect();
        out.sort();
        out
    }

    /// Take back every standing agreement, so the next call asks again.
    pub async fn forget_always(&self) -> usize {
        let mut held = self.always.lock().await;
        let had = held.len();
        held.clear();
        self.keep(&held);
        had
    }

    /// Let writes go ahead unasked, or stop doing so.
    pub fn accept_writes(&self, on: bool) {
        self.writes.store(on, Ordering::Relaxed);
    }

    /// Whether this scope is covered by that standing decision.
    pub fn writes_accepted(&self, scope: &ApprovalScope) -> bool {
        matches!(scope, ApprovalScope::Path { .. }) && self.writes.load(Ordering::Relaxed)
    }
}

/// Where the agreements kept for each project are written.
fn kept_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("agreements.json"))
}

fn read_kept() -> std::collections::BTreeMap<String, Vec<ApprovalScope>> {
    kept_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Convenience: ask the UI for approval and wait for a decision. Returns
/// `Deny` if the UI channel is closed (e.g. user quit the app).
pub async fn ask(sender: &ApprovalSender, req: ApprovalRequest) -> ApprovalDecision {
    ask_for(sender, req, None).await
}

/// The same, for the tool call `call`.
pub async fn ask_for(
    sender: &ApprovalSender,
    req: ApprovalRequest,
    call: Option<String>,
) -> ApprovalDecision {
    let (tx, rx) = oneshot::channel();
    if sender
        .send(ApprovalPrompt {
            req,
            reply: tx,
            call,
        })
        .await
        .is_err()
    {
        return ApprovalDecision::Deny(None);
    }
    rx.await.unwrap_or(ApprovalDecision::Deny(None))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_name_is_split_into_host_and_port() {
        assert_eq!(split_authority("docs.rs"), ("docs.rs", None));
        assert_eq!(split_authority("docs.rs:443"), ("docs.rs", Some(443)));
        assert_eq!(split_authority("*.rs:22"), ("*.rs", Some(22)));
        assert_eq!(split_authority("[::1]:22"), ("::1", Some(22)));
        assert_eq!(split_authority("::1"), ("::1", None));
        assert_eq!(authority("::1", 22), "[::1]:22");
    }

    #[test]
    fn an_agreement_with_a_port_covers_that_port_only() {
        let host = |h: &str| ApprovalScope::Host(h.into());
        assert!(host("a.test:443").covered_by(&host("a.test:443")));
        assert!(!host("a.test:22").covered_by(&host("a.test:443")));
        assert!(host("a.test:22").covered_by(&host("a.test")));
        assert!(!host("a.test").covered_by(&host("a.test:443")));
    }

    use super::*;

    fn cmd(s: &str) -> ApprovalScope {
        ApprovalScope::Command(s.into())
    }

    /// Agreeing to one harmless command does not hand over the shell for the rest of the
    /// session.
    #[tokio::test]
    async fn agreeing_to_one_command_does_not_cover_another() {
        let a = SessionApprovals::new();
        a.mark_always(cmd("ls")).await;
        assert!(a.is_always(&cmd("ls")).await);
        assert!(!a.is_always(&cmd("rm -rf /")).await);
    }

    /// A command that merely starts with an approved one is a different command.
    #[tokio::test]
    async fn a_prefix_is_not_the_command_that_was_approved() {
        let a = SessionApprovals::new();
        a.mark_always(cmd("ls")).await;
        assert!(!a.is_always(&cmd("ls; rm -rf ~")).await);
        assert!(!a.is_always(&cmd("ls /etc")).await);
    }

    /// Agreeing while looking at a file covers the directory it lives in, and nothing above.
    #[tokio::test]
    async fn a_write_agreement_covers_its_directory_and_no_further_up() {
        let a = SessionApprovals::new();
        a.mark_always(write_scope("write_file", Path::new("/p/src/main.rs")))
            .await;

        assert!(
            a.is_always(&write_scope("write_file", Path::new("/p/src/other.rs")))
                .await
        );
        assert!(
            a.is_always(&write_scope("write_file", Path::new("/p/src/ui/a.rs")))
                .await
        );
        assert!(
            !a.is_always(&write_scope("write_file", Path::new("/p/Cargo.toml")))
                .await
        );
    }

    /// Agreeing to writes by one tool says nothing about another tool's writes.
    #[tokio::test]
    async fn a_write_agreement_is_per_tool() {
        let a = SessionApprovals::new();
        a.mark_always(write_scope("write_file", Path::new("/p/src/main.rs")))
            .await;
        assert!(
            !a.is_always(&write_scope("apply_patch", Path::new("/p/src/main.rs")))
                .await
        );
    }

    /// Reaching one host is not reaching every host.
    #[tokio::test]
    async fn a_host_agreement_covers_one_host() {
        let a = SessionApprovals::new();
        a.mark_always(ApprovalScope::Host("docs.rs".into())).await;
        assert!(a.is_always(&ApprovalScope::Host("docs.rs".into())).await);
        assert!(
            !a.is_always(&ApprovalScope::Host("169.254.169.254".into()))
                .await
        );
    }

    /// An agreement kept for a project is there in its next session, for that project
    /// only, until it is taken back.
    #[test]
    fn an_agreement_is_kept_for_its_project() {
        let data = tempfile::tempdir().unwrap();
        crate::paths::with_env(&[("XDG_DATA_HOME", data.path().to_str())], || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let root = Path::new("/somewhere/project");
                SessionApprovals::kept_for(root)
                    .mark_always(cmd("cargo test"))
                    .await;
                let next = SessionApprovals::kept_for(root);
                assert!(next.is_always(&cmd("cargo test")).await);
                let elsewhere = SessionApprovals::kept_for(Path::new("/elsewhere"));
                assert!(!elsewhere.is_always(&cmd("cargo test")).await);
                assert_eq!(next.forget_always().await, 1);
                let after = SessionApprovals::kept_for(root);
                assert!(!after.is_always(&cmd("cargo test")).await);
                assert!(!SessionApprovals::new().keeps());
            });
        });
    }

    /// Nothing is agreed to before the user has agreed to anything.
    #[tokio::test]
    async fn nothing_is_covered_by_default() {
        let a = SessionApprovals::new();
        assert!(!a.is_always(&cmd("ls")).await);
        assert!(!a.is_always(&ApprovalScope::Tool("run_tests".into())).await);
    }
}

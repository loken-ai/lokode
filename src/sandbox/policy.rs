//! Standing decisions about what may go ahead without asking.
//!
//! An agreement given at the modal lasts as long as the session and no longer, so a user
//! who works the same way every day answers the same questions every day. A policy in the
//! configuration states them once, in a file that can be read in review, and a refusal
//! stated there always wins over an agreement: the point of writing down what must never
//! happen is that nothing later can widen it.

use std::path::Path;

use serde::Deserialize;

use crate::sandbox::approval::ApprovalScope;

/// What a policy says about one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Stated as allowed, and within the cap on how much may go unasked.
    Allowed,
    /// Stated as refused. The reason names the rule so it can be found and changed.
    Refused(String),
    /// Nothing is stated: put it to the user.
    Ask,
}

/// One side of the policy: what it names, by kind.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Rules {
    /// Shell commands, matched in full against the pattern.
    #[serde(default)]
    pub bash: Vec<String>,
    /// Paths a file-changing tool may touch, relative to the project or absolute.
    #[serde(default)]
    pub write: Vec<String>,
    /// Hosts the fetch tool may reach.
    #[serde(default)]
    pub egress: Vec<String>,
    /// Whole tools, by name, for the ones whose surface is a single action.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// What the configuration says may happen without a question.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub allow: Rules,
    #[serde(default)]
    pub deny: Rules,
    /// How many requests one turn may have granted for it before every later request is
    /// put to the user again.
    ///
    /// A standing agreement is given for the work a person expects, and a loop that calls
    /// a tool a thousand times is not that work. The cap is what keeps a policy from
    /// turning into an unattended session nobody asked for.
    #[serde(default = "default_max_auto")]
    pub max_auto: usize,
}

fn default_max_auto() -> usize {
    50
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            allow: Rules::default(),
            deny: Rules::default(),
            max_auto: default_max_auto(),
        }
    }
}

impl Policy {
    /// Whether anything is stated at all, which is what decides if it is worth showing.
    pub fn is_empty(&self) -> bool {
        self.allow == Rules::default() && self.deny == Rules::default()
    }

    /// What this policy says about one request, with `used` already granted this turn.
    ///
    /// Deny is read first and answers on its own: a rule that says something must never
    /// happen cannot be outvoted by one that says it may.
    pub fn verdict(&self, scope: &ApprovalScope, root: &Path, used: usize) -> Verdict {
        if let Some(rule) = matching(&self.deny, scope, root) {
            return Verdict::Refused(format!("refused by the policy rule {rule}"));
        }
        match matching(&self.allow, scope, root) {
            None => Verdict::Ask,
            Some(_) if used >= self.max_auto => Verdict::Ask,
            Some(_) => Verdict::Allowed,
        }
    }

    /// The policy as the user can read it back.
    pub fn describe(&self) -> String {
        let mut out = Vec::new();
        let mut side = |name: &str, rules: &Rules| {
            for (kind, list) in [
                ("bash", &rules.bash),
                ("write", &rules.write),
                ("egress", &rules.egress),
                ("tools", &rules.tools),
            ] {
                if !list.is_empty() {
                    out.push(format!("{name} {kind}: {}", list.join(", ")));
                }
            }
        };
        side("allow", &self.allow);
        side("deny", &self.deny);
        if out.is_empty() {
            return "nothing is settled in advance; every request is asked".to_string();
        }
        out.push(format!("at most {} granted per turn", self.max_auto));
        out.join("\n")
    }
}

/// The pattern of `rules` that covers `scope`, if any.
fn matching(rules: &Rules, scope: &ApprovalScope, root: &Path) -> Option<String> {
    let (patterns, subject) = match scope {
        ApprovalScope::Command(cmd) => (&rules.bash, cmd.clone()),
        ApprovalScope::Host(host) => (&rules.egress, host.clone()),
        ApprovalScope::Tool(tool) => (&rules.tools, tool.clone()),
        ApprovalScope::Path { tool, prefix } => {
            if rules.tools.iter().any(|t| t == tool) {
                return Some(format!("tools = {tool}"));
            }
            // Stated relative to the project when it is inside it, which is how a shared
            // rule stays true on another machine.
            let shown = prefix
                .strip_prefix(root)
                .unwrap_or(prefix)
                .to_string_lossy()
                .to_string();
            (&rules.write, shown)
        }
    };
    patterns
        .iter()
        .find(|p| glob_matches(p, &subject))
        .map(|p| format!("\"{p}\""))
}

/// Whether `pattern` covers `subject`.
///
/// One wildcard, `*`, standing for any run of characters, separators included. A rule
/// language with more in it invites a rule nobody can read, and this is a file whose whole
/// value is that somebody can read it in review and see what it permits.
pub fn glob_matches(pattern: &str, subject: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return pattern == subject;
    };
    if !subject.starts_with(first) {
        return false;
    }
    let mut rest = &subject[first.len()..];
    let mut last: Option<&str> = None;
    for part in parts {
        last = Some(part);
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    // A pattern that does not end in a wildcard has to reach the end of the subject.
    match last {
        Some(part) if !part.is_empty() => rest.is_empty(),
        Some(_) => true,
        None => rest.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn policy(allow: Rules, deny: Rules) -> Policy {
        Policy {
            allow,
            deny,
            max_auto: 50,
        }
    }

    fn bash(cmd: &str) -> ApprovalScope {
        ApprovalScope::Command(cmd.into())
    }

    #[test]
    fn a_wildcard_covers_a_run_of_characters_and_an_exact_pattern_does_not() {
        assert!(glob_matches("cargo test*", "cargo test --release"));
        assert!(glob_matches("cargo test*", "cargo test"));
        assert!(!glob_matches("cargo test*", "cargo build"));
        assert!(glob_matches("git status", "git status"));
        assert!(!glob_matches("git status", "git status --short"));
        assert!(glob_matches("src/*", "src/a/b.rs"));
        assert!(!glob_matches("src/*", "tests/a.rs"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("*.rs", "src/main.rs"));
    }

    /// The rule the whole file exists for: what must never happen cannot be outvoted by
    /// what may.
    #[test]
    fn a_refusal_wins_over_an_agreement() {
        let p = policy(
            Rules {
                bash: vec!["git *".into()],
                ..Default::default()
            },
            Rules {
                bash: vec!["git push*".into()],
                ..Default::default()
            },
        );
        let root = PathBuf::from("/p");
        assert_eq!(p.verdict(&bash("git status"), &root, 0), Verdict::Allowed);
        match p.verdict(&bash("git push origin main"), &root, 0) {
            Verdict::Refused(why) => assert!(why.contains("git push*"), "{why}"),
            other => panic!("a denied command was not refused: {other:?}"),
        }
    }

    #[test]
    fn what_is_not_stated_is_asked() {
        let p = Policy::default();
        assert_eq!(p.verdict(&bash("ls"), Path::new("/p"), 0), Verdict::Ask);
    }

    /// A standing agreement is given for the work a person expects, not for a loop.
    #[test]
    fn past_the_cap_everything_is_asked_again() {
        let p = Policy {
            allow: Rules {
                bash: vec!["ls*".into()],
                ..Default::default()
            },
            deny: Rules::default(),
            max_auto: 2,
        };
        let root = Path::new("/p");
        assert_eq!(p.verdict(&bash("ls"), root, 1), Verdict::Allowed);
        assert_eq!(p.verdict(&bash("ls"), root, 2), Verdict::Ask);
    }

    /// A write rule is written against the project, so the same file works on another
    /// machine with the project checked out somewhere else.
    #[test]
    fn a_write_rule_is_read_relative_to_the_project() {
        let p = policy(
            Rules {
                write: vec!["src/*".into()],
                ..Default::default()
            },
            Rules::default(),
        );
        let root = PathBuf::from("/home/someone/project");
        let inside = ApprovalScope::Path {
            tool: "write_file".into(),
            prefix: root.join("src").join("ui"),
        };
        let outside = ApprovalScope::Path {
            tool: "write_file".into(),
            prefix: root.join("docs"),
        };
        assert_eq!(p.verdict(&inside, &root, 0), Verdict::Allowed);
        assert_eq!(p.verdict(&outside, &root, 0), Verdict::Ask);
    }
}

#[cfg(test)]
mod through_the_context {
    use std::sync::Arc;

    use super::*;
    use crate::sandbox::approval::{Approval, ApprovalRequest, ApprovalScope};
    use crate::tools::harness::{Answer, Harness};

    fn request(cmd: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool: "bash".into(),
            summary: cmd.into(),
            diff_preview: None,
            scope: ApprovalScope::Command(cmd.into()),
        }
    }

    /// What the policy settles never reaches the modal, and what it refuses never reaches
    /// the tool. The harness answers yes to everything, so a denial here can only have
    /// come from the policy.
    #[tokio::test]
    async fn the_policy_answers_before_anyone_is_asked() {
        let h = Harness::new(Answer::Yes);
        let mut ctx = h.ctx.clone();
        ctx.policy = Arc::new(Policy {
            allow: Rules {
                bash: vec!["cargo test*".into()],
                ..Default::default()
            },
            deny: Rules {
                bash: vec!["*rm -rf*".into()],
                ..Default::default()
            },
            max_auto: 2,
        });

        assert_eq!(
            ctx.approve(request("cargo test --release")).await,
            Approval::Proceed
        );
        assert_eq!(ctx.approve(request("rm -rf /")).await, Approval::Denied);
        // Nothing was put to the user for either.
        assert_eq!(h.asked().len(), 0);

        // Past the cap the allowed command is asked about again, and the harness says yes.
        assert_eq!(ctx.approve(request("cargo test")).await, Approval::Proceed);
        assert_eq!(ctx.approve(request("cargo test")).await, Approval::Proceed);
        assert_eq!(h.asked().len(), 1);
    }
}

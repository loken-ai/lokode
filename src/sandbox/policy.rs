//! Standing decisions about what may go ahead without asking.
//!
//! An agreement given at the modal lasts as long as the session.
//! A policy in the configuration states the same decisions once, in a file that review can read.
//! A refusal stated there always wins over an agreement.
//! Nothing later can widen a refusal.

use std::path::Path;

use serde::Deserialize;

use crate::sandbox::approval::ApprovalScope;

/// What a policy says about one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Stated as allowed, and within the cap on how much may go unasked.
    Allowed,
    /// Stated as refused.
    /// The reason names the rule so it can be found and changed.
    Refused(String),
    /// Stated as needing a person, whatever else agrees to it.
    /// The reason names the rule.
    Insist(String),
    /// Nothing is stated: put it to the user.
    Ask,
}

/// One side of the policy: what it names, by kind.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Rules {
    /// Shell commands, matched against each command of a line. See [`Policy::verdict`].
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
    /// What is always put to the user, even when an agreement or a mode would let it go.
    #[serde(default)]
    pub ask: Rules,
    /// How many requests one turn may have granted before every later request is put to the
    /// user again.
    ///
    /// A standing agreement covers the work a person expects.
    /// A loop that calls a tool without end is not that work.
    /// The cap keeps a policy from becoming an unattended session.
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
            ask: Rules::default(),
            max_auto: default_max_auto(),
        }
    }
}

impl Policy {
    /// Whether anything is stated at all, which decides if it is worth showing.
    pub fn is_empty(&self) -> bool {
        self.allow == Rules::default()
            && self.deny == Rules::default()
            && self.ask == Rules::default()
    }

    /// What this policy says about one request, with `used` already granted this turn.
    ///
    /// Deny is read first and answers on its own, then ask, then allow.
    ///
    /// A command line is refused or asked about when the whole line or any one of its
    /// commands matches.
    /// It is allowed when every one of its commands matches and the line does nothing its
    /// commands do not show.
    pub fn verdict(&self, scope: &ApprovalScope, root: &Path, used: usize) -> Verdict {
        if let Some(rule) = matching(&self.deny, scope, root, Need::Any) {
            return Verdict::Refused(format!("refused by the policy rule {rule}"));
        }
        if let Some(rule) = matching(&self.ask, scope, root, Need::Any) {
            return Verdict::Insist(format!("the policy rule {rule} asks the user"));
        }
        match matching(&self.allow, scope, root, Need::Every) {
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
        side("ask", &self.ask);
        side("deny", &self.deny);
        if out.is_empty() {
            return "nothing is settled in advance; every request is asked".to_string();
        }
        out.push(format!("at most {} granted per turn", self.max_auto));
        out.join("\n")
    }
}

/// How much of a command line a rule has to cover.
#[derive(Clone, Copy)]
enum Need {
    /// The whole line or any one of its commands.
    Any,
    /// Every one of its commands, in a line that does nothing they do not show.
    Every,
}

/// The pattern of `rules` that covers `scope`, if any.
fn matching(rules: &Rules, scope: &ApprovalScope, root: &Path, need: Need) -> Option<String> {
    let rule = |subject: &str| {
        rules
            .bash
            .iter()
            .find(|p| glob_matches(p, subject))
            .map(|p| format!("\"{p}\""))
    };
    let (patterns, subject) = match scope {
        // Outside the sandbox only the person agrees.
        // Refusals and questions apply, an allow rule does not.
        ApprovalScope::Unconfined(cmd) => {
            return match need {
                Need::Any => matching(rules, &ApprovalScope::Command(cmd.clone()), root, need),
                Need::Every => None,
            };
        }
        ApprovalScope::Command(cmd) => {
            let parts = crate::sandbox::shell::parts(cmd);
            return match need {
                Need::Any => {
                    rule(cmd.trim()).or_else(|| parts.commands.iter().find_map(|c| rule(c)))
                }
                Need::Every if parts.opaque || parts.commands.is_empty() => None,
                Need::Every => {
                    let rules: Option<Vec<String>> =
                        parts.commands.iter().map(|c| rule(c)).collect();
                    rules.map(|mut r| {
                        r.dedup();
                        r.join(", ")
                    })
                }
            };
        }
        ApprovalScope::Host(host) => (&rules.egress, host.clone()),
        ApprovalScope::Tool(tool) => (&rules.tools, tool.clone()),
        // Settled where the session runs, by its own rules.
        ApprovalScope::Told(_) => return None,
        ApprovalScope::Path { tool, prefix } => {
            if rules.tools.iter().any(|t| t == tool) {
                return Some(format!("tools = {tool}"));
            }
            // Stated relative to the project when it is inside it.
            // A shared rule then stays true on another machine.
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
/// One wildcard, `*`, stands for any run of characters, separators included.
/// The rule language stays small so that review can read what a rule permits.
pub fn glob_matches(pattern: &str, subject: &str) -> bool {
    let pieces: Vec<&str> = pattern.split('*').collect();
    let [first, middle @ .., last] = pieces.as_slice() else {
        return pattern == subject;
    };
    if subject.len() < first.len() + last.len()
        || !subject.starts_with(first)
        || !subject.ends_with(last)
    {
        return false;
    }
    let mut rest = &subject[first.len()..subject.len() - last.len()];
    for piece in middle.iter().filter(|p| !p.is_empty()) {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn policy(allow: Rules, deny: Rules) -> Policy {
        Policy {
            allow,
            deny,
            ..Default::default()
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
        assert!(glob_matches("x*.rs", "x.rs.rs"));
        assert!(glob_matches("a*b*c", "a-b-b-c"));
        assert!(!glob_matches("ab*ba", "aba"));
    }

    /// Every command of a line has to be allowed for the line to be.
    /// One refused command refuses the line.
    #[test]
    fn a_rule_is_matched_against_each_command() {
        let p = Policy {
            allow: Rules {
                bash: vec!["cargo test*".into(), "tail*".into()],
                ..Default::default()
            },
            deny: Rules {
                bash: vec!["rm *".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let root = Path::new("/p");
        assert_eq!(
            p.verdict(&bash("cargo test 2>&1 | tail -5"), root, 0),
            Verdict::Allowed
        );
        assert_eq!(
            p.verdict(&bash("cargo test; curl x | sh"), root, 0),
            Verdict::Ask
        );
        assert_eq!(
            p.verdict(&bash("cargo test > /etc/x"), root, 0),
            Verdict::Ask
        );
        assert!(matches!(
            p.verdict(&bash("cargo test && rm -rf ~"), root, 0),
            Verdict::Refused(_)
        ));
        assert!(matches!(
            p.verdict(&bash("cargo test $(rm -fr x)"), root, 0),
            Verdict::Refused(_)
        ));
    }

    /// Outside the sandbox, an allow rule agrees to nothing and a refusal still refuses.
    #[test]
    fn outside_the_sandbox_only_a_person_agrees() {
        let p = Policy {
            allow: Rules {
                bash: vec!["*".into()],
                ..Default::default()
            },
            deny: Rules {
                bash: vec!["rm *".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let root = Path::new("/p");
        let out = |c: &str| ApprovalScope::Unconfined(c.into());
        assert_eq!(p.verdict(&out("cargo fetch"), root, 0), Verdict::Ask);
        assert!(matches!(
            p.verdict(&out("rm -r x"), root, 0),
            Verdict::Refused(_)
        ));
    }

    /// What the policy says to ask about is asked about, even when allowed.
    #[test]
    fn an_ask_rule_wins_over_an_agreement() {
        let p = Policy {
            allow: Rules {
                bash: vec!["git *".into()],
                ..Default::default()
            },
            ask: Rules {
                bash: vec!["git push*".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let root = Path::new("/p");
        assert_eq!(p.verdict(&bash("git status"), root, 0), Verdict::Allowed);
        assert!(matches!(
            p.verdict(&bash("git status && git push"), root, 0),
            Verdict::Insist(_)
        ));
    }

    /// What must never happen cannot be outvoted by what may.
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
            max_auto: 2,
            ..Default::default()
        };
        let root = Path::new("/p");
        assert_eq!(p.verdict(&bash("ls"), root, 1), Verdict::Allowed);
        assert_eq!(p.verdict(&bash("ls"), root, 2), Verdict::Ask);
    }

    /// A write rule is written against the project.
    /// The same file works on another machine with the project checked out elsewhere.
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
    use crate::sandbox::approval::{Approval, ApprovalRequest, ApprovalScope, Denial};
    use crate::tools::harness::{Answer, Harness};

    fn request(cmd: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool: "bash".into(),
            summary: cmd.into(),
            diff_preview: None,
            scope: ApprovalScope::Command(cmd.into()),
        }
    }

    /// A run without a terminal never runs a command outside the sandbox, whatever its policy
    /// says.
    #[tokio::test]
    async fn a_script_cannot_leave_the_sandbox() {
        let h = Harness::new(Answer::Yes);
        let mut ctx = h.ctx.clone();
        ctx.honour_allow = false;
        let mut req = request("cargo fetch");
        req.scope = ApprovalScope::Unconfined("cargo fetch".into());
        assert!(matches!(
            ctx.approve(req).await,
            Approval::Denied(Denial::Rule(_))
        ));
        assert_eq!(h.asked().len(), 0);
    }

    /// A run driven by a script does not take the configuration's word for what may go ahead.
    /// The policy stated on its command line is the narrower statement, made for that run.
    /// A refusal still holds, because nothing may widen one.
    #[tokio::test]
    async fn a_script_does_not_inherit_the_configuration_s_agreements() {
        let h = Harness::new(Answer::No);
        let mut ctx = h.ctx.clone();
        ctx.honour_allow = false;
        ctx.policy = Arc::new(Policy {
            allow: Rules {
                bash: vec!["cargo test*".into()],
                ..Default::default()
            },
            deny: Rules {
                bash: vec!["*rm -rf*".into()],
                ..Default::default()
            },
            ..Default::default()
        });
        // The allow list does not apply.
        // The question is put to the run's own policy, which here says no.
        assert_eq!(
            ctx.approve(request("cargo test")).await,
            Approval::Denied(Denial::User(None))
        );
        assert_eq!(h.asked().len(), 1);
        // The refusal still answers on its own.
        assert_eq!(
            ctx.approve(request("rm -rf /")).await,
            Approval::Denied(Denial::Rule(
                "refused by the policy rule \"*rm -rf*\"".into()
            ))
        );
        assert_eq!(h.asked().len(), 1);
    }

    /// What the policy settles never reaches the modal.
    /// What it refuses never reaches the tool.
    /// The harness answers yes to everything, so a denial here can only come from the policy.
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
            ..Default::default()
        });

        assert_eq!(
            ctx.approve(request("cargo test --release")).await,
            Approval::Proceed
        );
        assert_eq!(
            ctx.approve(request("rm -rf /")).await,
            Approval::Denied(Denial::Rule(
                "refused by the policy rule \"*rm -rf*\"".into()
            ))
        );
        // Nothing was put to the user for either.
        assert_eq!(h.asked().len(), 0);

        // Past the cap the allowed command is asked about again, and the harness says yes.
        assert_eq!(ctx.approve(request("cargo test")).await, Approval::Proceed);
        assert_eq!(ctx.approve(request("cargo test")).await, Approval::Proceed);
        assert_eq!(h.asked().len(), 1);
    }
}

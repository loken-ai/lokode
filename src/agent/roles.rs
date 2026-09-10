//! Which model does which kind of work.
//!
//! One model for everything is one compromise for every step. A turn asks for very
//! different things: a plan, a patch, a search through files, a small JSON object to
//! repair a malformed reply. The work that needs reasoning wants the model that reasons
//! best; the work that needs to be over quickly wants the one already resident.
//!
//! Stated as roles rather than as names at each call site, so a project says once what it
//! wants and the loop never has a model name written into it.

use std::collections::BTreeMap;

use serde::Deserialize;

/// A kind of work a model is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Reading, searching, deciding what to do: the whole conversation, by default.
    Chat,
    /// Working out what the steps would be, without doing any of them.
    Plan,
    /// Writing the change.
    Edit,
    /// Finding out where something is. Answered by a delegated session.
    Search,
    /// Turning a malformed reply into the shape that was asked for. A small, mechanical
    /// job with nothing to reason about.
    Repair,
}

impl Role {
    pub fn key(self) -> &'static str {
        match self {
            Role::Chat => "chat",
            Role::Plan => "plan",
            Role::Edit => "edit",
            Role::Search => "search",
            Role::Repair => "repair",
        }
    }

    pub const ALL: [Role; 5] = [
        Role::Chat,
        Role::Plan,
        Role::Edit,
        Role::Search,
        Role::Repair,
    ];
}

/// What the configuration says each role should use.
///
/// Every field is optional and falls back to the session's model, so a project can name
/// one role and leave the rest alone. Nothing here changes behaviour on its own: it says
/// what is available to whoever is choosing.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Roles {
    pub chat: Option<String>,
    pub plan: Option<String>,
    pub edit: Option<String>,
    pub search: Option<String>,
    pub repair: Option<String>,
}

impl Roles {
    pub fn get(&self, role: Role) -> Option<&str> {
        match role {
            Role::Chat => self.chat.as_deref(),
            Role::Plan => self.plan.as_deref(),
            Role::Edit => self.edit.as_deref(),
            Role::Search => self.search.as_deref(),
            Role::Repair => self.repair.as_deref(),
        }
    }

    /// Later statements win, field by field, as everywhere else.
    pub fn merge(base: Self, over: Self) -> Self {
        Self {
            chat: over.chat.or(base.chat),
            plan: over.plan.or(base.plan),
            edit: over.edit.or(base.edit),
            search: over.search.or(base.search),
            repair: over.repair.or(base.repair),
        }
    }

    /// The roles that name a model, for saying what is in force.
    pub fn named(&self) -> BTreeMap<&'static str, &str> {
        Role::ALL
            .into_iter()
            .filter_map(|r| self.get(r).map(|m| (r.key(), m)))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_falls_back_to_nothing_rather_than_to_another_role() {
        let roles = Roles {
            plan: Some("big".into()),
            ..Default::default()
        };
        assert_eq!(roles.get(Role::Plan), Some("big"));
        assert_eq!(roles.get(Role::Edit), None);
        assert_eq!(roles.named().len(), 1);
    }

    #[test]
    fn the_narrower_statement_wins_field_by_field() {
        let user = Roles {
            plan: Some("mine".into()),
            repair: Some("tiny".into()),
            ..Default::default()
        };
        let project = Roles {
            plan: Some("theirs".into()),
            ..Default::default()
        };
        let merged = Roles::merge(user, project);
        assert_eq!(merged.plan.as_deref(), Some("theirs"));
        assert_eq!(merged.repair.as_deref(), Some("tiny"));
    }
}

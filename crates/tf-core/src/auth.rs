//! Accounts, tokens, variables and the one function that answers
//! "may this principal do this?".

use crate::Id;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Id,
    pub email: String,
    pub display_name: String,
    pub email_verified: bool,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Org {
    pub id: Id,
    pub name: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: Id,
    /// "user" acts as its user; "org" manages the organization but cannot plan or apply.
    pub kind: String,
    pub user_id: Option<Id>,
    pub org_id: Id,
    pub description: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variable {
    pub id: Id,
    pub workspace_id: Id,
    pub key: String,
    /// "terraform" or "env"
    pub category: String,
    pub hcl: bool,
    pub sensitive: bool,
    /// `None` for sensitive variables whenever a value is shown to a user.
    pub value: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct NewVariable {
    pub key: String,
    pub category: String,
    pub hcl: bool,
    pub sensitive: bool,
    pub value: String,
    pub description: String,
}

pub fn valid_variable_key(k: &str) -> bool {
    let mut c = k.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && k.len() <= 128
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

/// Who is making the request, resolved from a session cookie or a bearer token.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: Option<Id>,
    pub org_id: Id,
    /// "owner", "admin" or "member"
    pub role: String,
    /// `Some("user")` / `Some("org")` when authenticated by API token.
    pub token_kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    QueueRun,
    ConfirmRun,
    Lock,
    Unlock,
    CreateWorkspace,
    /// Rename a workspace or change its settings.
    UpdateWorkspace,
    /// Delete a workspace with its runs and state.
    DeleteWorkspace,
    ManageVariables,
    ForceUnlock,
    ManageTokens,
    /// Read the raw state document (it can contain secrets).
    ReadState,
    /// Push state and take the state lock from the CLI.
    WriteState,
    /// Stop a run that is working, possibly leaving resources unrecorded.
    ForceCancel,
}

/// Roles: owner and admin may do everything; member may read, queue, confirm,
/// lock and unlock. An organization token cannot plan or apply, whatever its role.
pub fn can(role: &str, token_kind: Option<&str>, action: Action) -> bool {
    use Action::*;
    if token_kind == Some("org") && matches!(action, QueueRun | ConfirmRun | ReadState | WriteState | ForceCancel) {
        return false;
    }
    match role {
        "owner" | "admin" => true,
        "member" => matches!(action, Read | QueueRun | ConfirmRun | Lock | Unlock | ReadState | WriteState),
        _ => false,
    }
}

impl Principal {
    pub fn can(&self, action: Action) -> bool {
        can(&self.role, self.token_kind.as_deref(), action)
    }
}

pub fn valid_email(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else { return false };
    e.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !e.chars().any(|c| c.is_whitespace() || c.is_control())
        && e.matches('@').count() == 1
}

pub const MIN_PASSWORD: usize = 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_matrix() {
        let all = [
            Action::Read,
            Action::QueueRun,
            Action::ConfirmRun,
            Action::Lock,
            Action::Unlock,
            Action::CreateWorkspace,
            Action::UpdateWorkspace,
            Action::DeleteWorkspace,
            Action::ManageVariables,
            Action::ForceUnlock,
            Action::ManageTokens,
            Action::ReadState,
            Action::WriteState,
            Action::ForceCancel,
        ];
        for a in all {
            assert!(can("owner", None, a) && can("admin", None, a), "{a:?}");
            assert!(!can("stranger", None, a) && !can("", None, a), "{a:?}");
        }
        for a in [Action::Read, Action::QueueRun, Action::ConfirmRun, Action::Lock, Action::Unlock, Action::ReadState, Action::WriteState] {
            assert!(can("member", None, a), "{a:?}");
        }
        for a in [Action::CreateWorkspace, Action::UpdateWorkspace, Action::DeleteWorkspace, Action::ManageVariables, Action::ForceUnlock, Action::ManageTokens, Action::ForceCancel] {
            assert!(!can("member", None, a), "{a:?}");
        }
    }

    #[test]
    fn org_tokens_cannot_plan_or_apply() {
        assert!(!can("owner", Some("org"), Action::QueueRun));
        assert!(!can("owner", Some("org"), Action::ConfirmRun));
        assert!(!can("owner", Some("org"), Action::ReadState), "state can hold secrets");
        assert!(!can("owner", Some("org"), Action::WriteState));
        assert!(!can("owner", Some("org"), Action::ForceCancel), "an organization token cannot stop runs");
        assert!(can("owner", Some("org"), Action::CreateWorkspace));
        assert!(can("owner", Some("user"), Action::QueueRun));
    }

    #[test]
    fn emails_and_keys() {
        assert!(valid_email("a@b.co") && !valid_email("a@b") && !valid_email("@b.co") && !valid_email("a b@c.de"));
        assert!(!valid_email("a@@b.co") && !valid_email("a@.co") && !valid_email("a@b."));
        assert!(valid_variable_key("AWS_REGION") && valid_variable_key("_x1"));
        assert!(!valid_variable_key("1x") && !valid_variable_key("a-b") && !valid_variable_key(""));
    }
}

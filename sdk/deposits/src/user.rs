use crate::{BoxFuture, CommandPrincipal, DepositError, UserId};

/// PS-owned opaque user reference. It intentionally contains no customer PII.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    /// Authenticated exchange principal that owns this user reference.
    pub owner: CommandPrincipal,
    /// First time PS durably associated this opaque identifier with a command.
    pub first_seen_at: u64,
}

/// Durable PS ownership of opaque users. Authentication and customer profiles
/// remain outside this boundary.
pub trait UserStore: Send + Sync {
    /// Creates the user when absent and otherwise returns the original record.
    /// The owner and first-seen timestamp are immutable once persisted. Reuse
    /// of the same opaque user ID by another principal is a conflict.
    fn ensure_user<'a>(&'a self, command: User) -> BoxFuture<'a, Result<User, DepositError>>;

    fn user<'a>(&'a self, id: &'a UserId) -> BoxFuture<'a, Result<Option<User>, DepositError>>;
}

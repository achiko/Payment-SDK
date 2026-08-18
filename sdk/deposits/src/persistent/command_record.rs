use super::*;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct ResolutionIdentity {
    pub(super) version: u16,
    pub(super) command: ReconciliationIdentity,
    pub(super) case_id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct IdRecord {
    pub(super) version: u16,
    pub(super) id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct AccountingIdentity {
    pub(super) principal: String,
    pub(super) operation: u8,
    pub(super) client_key: String,
    pub(super) request_hash: [u8; 32],
}

impl From<&CommandIdentity> for AccountingIdentity {
    fn from(value: &CommandIdentity) -> Self {
        Self {
            principal: value.principal.0.clone(),
            operation: 0,
            client_key: value.client_key.0.clone(),
            request_hash: value.request_hash.0,
        }
    }
}

impl TryFrom<AccountingIdentity> for CommandIdentity {
    type Error = DepositError;

    fn try_from(value: AccountingIdentity) -> Result<Self, Self::Error> {
        if value.operation != 0 {
            return Err(storage_error(
                "accounting idempotency record has an unknown operation",
            ));
        }
        Ok(Self {
            principal: CommandPrincipal(value.principal),
            operation: CommandOperation::Accounting,
            client_key: IdempotencyKey(value.client_key),
            request_hash: RequestHash(value.request_hash),
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct AccountingReplay {
    pub(super) version: u16,
    pub(super) command: AccountingIdentity,
    pub(super) ledger_entry_id: String,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct CursorRecord {
    pub(super) version: u16,
    pub(super) cursor: Option<u64>,
}

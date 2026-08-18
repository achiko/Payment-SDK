use super::*;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum KeyIdRecord {
    Identifier(String),
    DerivationPath(Vec<(u32, bool)>),
}

impl From<&KeyId> for KeyIdRecord {
    fn from(value: &KeyId) -> Self {
        match value {
            KeyId::Identifier(value) => Self::Identifier(value.clone()),
            KeyId::DerivationPath(path) => Self::DerivationPath(
                path.0
                    .iter()
                    .map(|child| (child.index, child.hardened))
                    .collect(),
            ),
        }
    }
}

impl From<KeyIdRecord> for KeyId {
    fn from(value: KeyIdRecord) -> Self {
        match value {
            KeyIdRecord::Identifier(value) => Self::Identifier(value),
            KeyIdRecord::DerivationPath(path) => Self::DerivationPath(DerivationPath(
                path.into_iter()
                    .map(|(index, hardened)| ChildIndex { index, hardened })
                    .collect(),
            )),
        }
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) enum DepositStateRecord {
    AwaitingWatch,
    Active(String),
    Expired(String),
    Closed,
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct DepositRecord {
    pub(super) version: u16,
    pub(super) id: String,
    pub(super) idempotency_key: String,
    pub(super) user_id: String,
    pub(super) asset_chain: String,
    pub(super) asset: String,
    pub(super) address: AddressRecord,
    pub(super) key: KeyIdRecord,
    pub(super) key_purpose: String,
    pub(super) expected: [u8; 32],
    pub(super) birthday: u64,
    pub(super) expires_at: u64,
    pub(super) state: DepositStateRecord,
    pub(super) created_at: u64,
}

impl From<&Deposit> for DepositRecord {
    fn from(value: &Deposit) -> Self {
        Self {
            version: DEPOSIT_RECORD_VERSION,
            id: value.id.0.clone(),
            idempotency_key: value.idempotency_key.0.clone(),
            user_id: value.user_id.0.clone(),
            asset_chain: value.asset.chain.0.clone(),
            asset: value.asset.asset.clone(),
            address: (&value.address).into(),
            key: (&value.key).into(),
            key_purpose: value.key_purpose.clone(),
            expected: amount::record_bytes(&value.expected),
            birthday: value.birthday.0,
            expires_at: value.expires_at,
            state: match &value.state {
                DepositState::AwaitingWatch => DepositStateRecord::AwaitingWatch,
                DepositState::Active { watch_id } => DepositStateRecord::Active(watch_id.0.clone()),
                DepositState::Expired { watch_id } => {
                    DepositStateRecord::Expired(watch_id.0.clone())
                }
                DepositState::Closed => DepositStateRecord::Closed,
            },
            created_at: value.created_at,
        }
    }
}

impl TryFrom<DepositRecord> for Deposit {
    type Error = DepositError;

    fn try_from(value: DepositRecord) -> Result<Self, Self::Error> {
        if value.version != DEPOSIT_RECORD_VERSION {
            return Err(storage_error(format!(
                "unsupported PS deposit record version {}",
                value.version
            )));
        }
        if value.key_purpose.trim().is_empty()
            || value.key_purpose.len() > MAX_KEY_PURPOSE_BYTES
            || value
                .key_purpose
                .bytes()
                .any(|byte| byte.is_ascii_control())
        {
            return Err(storage_error("persisted deposit key purpose is invalid"));
        }
        Ok(Self {
            id: DepositId(value.id),
            idempotency_key: IdempotencyKey(value.idempotency_key),
            user_id: UserId(value.user_id),
            asset: AssetId {
                chain: ChainId(value.asset_chain),
                asset: value.asset,
            },
            address: value.address.into(),
            key: value.key.into(),
            key_purpose: value.key_purpose,
            expected: amount::from_bytes(value.expected),
            birthday: BlockHeight(value.birthday),
            expires_at: value.expires_at,
            state: match value.state {
                DepositStateRecord::AwaitingWatch => DepositState::AwaitingWatch,
                DepositStateRecord::Active(watch_id) => DepositState::Active {
                    watch_id: WatchId(watch_id),
                },
                DepositStateRecord::Expired(watch_id) => DepositState::Expired {
                    watch_id: WatchId(watch_id),
                },
                DepositStateRecord::Closed => DepositState::Closed,
            },
            created_at: value.created_at,
        })
    }
}

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub(super) struct BalancesRecord {
    pub(super) received: [u8; 32],
    pub(super) confirmed: [u8; 32],
    pub(super) balance: [u8; 32],
    pub(super) collected: [u8; 32],
    pub(super) accounted: [u8; 32],
}

impl From<DepositBalances> for BalancesRecord {
    fn from(value: DepositBalances) -> Self {
        Self {
            received: amount::record_bytes(&value.received),
            confirmed: amount::record_bytes(&value.confirmed),
            balance: amount::record_bytes(&value.balance),
            collected: amount::record_bytes(&value.collected),
            accounted: amount::record_bytes(&value.accounted),
        }
    }
}

impl From<BalancesRecord> for DepositBalances {
    fn from(value: BalancesRecord) -> Self {
        Self {
            received: amount::from_bytes(value.received),
            confirmed: amount::from_bytes(value.confirmed),
            balance: amount::from_bytes(value.balance),
            collected: amount::from_bytes(value.collected),
            accounted: amount::from_bytes(value.accounted),
        }
    }
}

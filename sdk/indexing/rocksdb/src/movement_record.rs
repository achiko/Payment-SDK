use indexing::{
    AssetId, ChainId, IndexError, IndexErrorKind, MovementId, MovementKind, ValueMovement,
};

use crate::record::{ChainValue, MovementKindRecord, MovementRecord, ScopedValue};

fn kind_to_record(value: MovementKind) -> MovementKindRecord {
    match value {
        MovementKind::Transfer => MovementKindRecord::Transfer,
        MovementKind::Input => MovementKindRecord::Input,
        MovementKind::Output => MovementKindRecord::Output,
        MovementKind::InternalTransfer => MovementKindRecord::InternalTransfer,
        MovementKind::Mint => MovementKindRecord::Mint,
        MovementKind::Burn => MovementKindRecord::Burn,
    }
}

fn kind_from_record(value: MovementKindRecord) -> MovementKind {
    match value {
        MovementKindRecord::Transfer => MovementKind::Transfer,
        MovementKindRecord::Input => MovementKind::Input,
        MovementKindRecord::Output => MovementKind::Output,
        MovementKindRecord::InternalTransfer => MovementKind::InternalTransfer,
        MovementKindRecord::Mint => MovementKind::Mint,
        MovementKindRecord::Burn => MovementKind::Burn,
    }
}

pub(super) fn to_record(value: &ValueMovement) -> MovementRecord {
    MovementRecord {
        id: value.id().0.clone(),
        asset: ChainValue {
            chain: value.asset().chain.0.clone(),
            value: value.asset().asset.clone(),
        },
        amount: crate::amount_record::encode(value.amount()),
        from: value.from().map(ScopedValue::from_address),
        to: value.to().map(ScopedValue::from_address),
        kind: kind_to_record(value.kind()),
    }
}

pub(super) fn from_record(value: MovementRecord) -> Result<ValueMovement, IndexError> {
    let id = MovementId(value.id);
    let asset = AssetId {
        chain: ChainId(value.asset.chain),
        asset: value.asset.value,
    };
    let amount = crate::amount_record::decode(&value.amount)?;
    let from = value.from.map(ScopedValue::into_address);
    let to = value.to.map(ScopedValue::into_address);
    let invalid = || {
        IndexError::new(
            IndexErrorKind::Store,
            "stored movement has endpoints incompatible with its kind",
            false,
        )
    };
    match kind_from_record(value.kind) {
        MovementKind::Transfer => Ok(ValueMovement::Transfer {
            id,
            asset,
            amount,
            from: from.ok_or_else(&invalid)?,
            to: to.ok_or_else(&invalid)?,
        }),
        MovementKind::Input => Ok(ValueMovement::Input {
            id,
            asset,
            amount,
            owner: from,
        }),
        MovementKind::Output => Ok(ValueMovement::Output {
            id,
            asset,
            amount,
            owner: to,
        }),
        MovementKind::InternalTransfer => Ok(ValueMovement::InternalTransfer {
            id,
            asset,
            amount,
            from: from.ok_or_else(&invalid)?,
            to: to.ok_or_else(&invalid)?,
        }),
        MovementKind::Mint => Ok(ValueMovement::Mint {
            id,
            asset,
            amount,
            to: to.ok_or_else(&invalid)?,
        }),
        MovementKind::Burn => Ok(ValueMovement::Burn {
            id,
            asset,
            amount,
            from: from.ok_or_else(&invalid)?,
        }),
    }
}

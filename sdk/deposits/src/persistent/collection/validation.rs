use super::*;
use crate::amount;

pub(super) fn validate_non_empty(value: &str, field: &str) -> Result<(), DepositError> {
    if value.is_empty() {
        Err(invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

impl CollectionError {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        let error = self;
        validate_non_empty(&error.code, "safe collection error code")?;
        validate_non_empty(&error.message, "safe collection error message")?;
        if error.code.len() > MAX_SAFE_ERROR_CODE_BYTES {
            return Err(invalid("safe collection error code is too long"));
        }
        if error.message.len() > MAX_SAFE_ERROR_MESSAGE_BYTES {
            return Err(invalid("safe collection error message is too long"));
        }
        if !error
            .code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(invalid(
                "safe collection error code must use lowercase ASCII, digits, or underscore",
            ));
        }
        Ok(())
    }
}

pub(super) fn validate_asset(asset: &AssetId, field: &str) -> Result<(), DepositError> {
    validate_non_empty(&asset.chain.0, &format!("{field} chain"))?;
    validate_non_empty(&asset.asset, field)
}

pub(super) fn validate_transaction_for_collection(
    collection: &Collection,
    transaction_id: &TransactionRef,
) -> Result<(), DepositError> {
    validate_non_empty(&transaction_id.value, "transaction ID")?;
    if transaction_id.scope.chain != collection.asset.chain
        || transaction_id.scope.network != collection.destination.scope.network
    {
        return Err(invalid(
            "collection transaction chain does not match asset chain",
        ));
    }
    Ok(())
}

pub(super) fn validate_leg_shape(
    mode: CollectionMode,
    legs: &[(LegId, CollectionLegKind)],
) -> Result<(), DepositError> {
    if legs.is_empty() {
        return Err(invalid("collection must contain at least one leg"));
    }
    if legs.len() > usize::from(u16::MAX) + 1 {
        return Err(invalid("collection contains too many ordered legs"));
    }
    let mut ids = BTreeSet::new();
    for (id, _) in legs {
        validate_non_empty(&id.0, "collection leg ID")?;
        if !ids.insert(&id.0) {
            return Err(invalid("collection leg IDs must be unique"));
        }
    }
    let kinds = legs.iter().map(|(_, kind)| *kind).collect::<Vec<_>>();
    let valid = match mode {
        CollectionMode::AccountTransfer | CollectionMode::UtxoBatch => {
            kinds == [CollectionLegKind::Sweep]
        }
        CollectionMode::TokenWithGas => {
            kinds == [CollectionLegKind::Sweep]
                || kinds == [CollectionLegKind::GasFunding, CollectionLegKind::Sweep]
        }
    };
    if !valid {
        return Err(invalid(
            "collection legs must be one sweep or ordered gas-funding then sweep",
        ));
    }
    Ok(())
}

impl CollectionPlan {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        let command = self;
        validate_non_empty(&command.id.0, "collection ID")?;
        validate_non_empty(&command.job_id.0, "collection job ID")?;
        validate_non_empty(&command.user_id.0, "collection user ID")?;
        validate_non_empty(&command.deposit_id.0, "collection deposit ID")?;
        validate_asset(&command.asset, "collection asset")?;
        validate_non_empty(&command.destination.value, "collection destination")?;
        if command.destination.scope.chain != command.asset.chain
            || command.destination.scope.network.is_empty()
        {
            return Err(invalid(
                "collection destination chain does not match asset chain",
            ));
        }
        validate_non_empty(&command.policy.version, "collection policy version")?;
        if command.reservation_amount.is_zero() {
            return Err(invalid("collection reservation amount must be positive"));
        }
        if command.mode == CollectionMode::UtxoBatch {
            return Err(invalid(
                "UTXO-batch collections require explicit participant and outpoint reservations",
            ));
        }
        validate_leg_shape(
            command.mode,
            &command
                .legs
                .iter()
                .map(|leg| (leg.id.clone(), leg.kind))
                .collect::<Vec<_>>(),
        )?;
        for leg in &command.legs {
            match (leg.kind, leg.planned_amount.as_ref()) {
                (CollectionLegKind::GasFunding, Some(amount)) if !amount.is_zero() => {}
                (CollectionLegKind::GasFunding, _) => {
                    return Err(invalid(
                        "gas-funding collection leg requires a positive planned amount",
                    ));
                }
                (CollectionLegKind::Sweep, None) => {}
                (CollectionLegKind::Sweep, Some(_)) => {
                    return Err(invalid(
                        "sweep collection leg must use the aggregate reservation amount",
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(super) fn validate_spend_resources(
    asset: &AssetId,
    reservation_amount: Decimal,
    resources: &[SpendResource],
) -> Result<(), DepositError> {
    if resources.is_empty() {
        return Err(invalid(
            "UTXO-batch participant must reserve at least one exact spend resource",
        ));
    }
    let mut previous = None;
    let mut total = Decimal::zero();
    for resource in resources {
        validate_non_empty(
            &resource.id.transaction_id.value,
            "spend-resource transaction ID",
        )?;
        if resource.id.transaction_id.scope.chain != asset.chain
            || resource.id.transaction_id.scope.network.is_empty()
        {
            return Err(invalid(
                "spend-resource transaction chain does not match collection asset",
            ));
        }
        if resource.amount.is_zero() {
            return Err(invalid("spend-resource amount must be positive"));
        }
        if resource.evidence.as_bytes().is_empty() {
            return Err(invalid("spend-resource evidence must not be empty"));
        }
        if previous.as_ref().is_some_and(|id| id >= &resource.id) {
            return Err(invalid(
                "spend resources must be strictly ordered by transaction ID and output index",
            ));
        }
        previous = Some(resource.id.clone());
        total = amount::checked_add(&total, &resource.amount)
            .map_err(|_| invalid("spend-resource amount sum overflows"))?;
    }
    if total != reservation_amount {
        return Err(invalid(
            "participant reservation must equal the exact spend-resource amount sum",
        ));
    }
    Ok(())
}

impl CreateBatch {
    pub(super) fn validate(&self) -> Result<(), DepositError> {
        let command = self;
        validate_non_empty(&command.id.0, "collection ID")?;
        validate_non_empty(&command.job_id.0, "collection job ID")?;
        validate_asset(&command.asset, "collection asset")?;
        validate_non_empty(&command.destination.value, "collection destination")?;
        if command.destination.scope.chain != command.asset.chain
            || command.destination.scope.network.is_empty()
        {
            return Err(invalid(
                "collection destination chain does not match asset chain",
            ));
        }
        validate_non_empty(&command.policy.version, "collection policy version")?;
        validate_leg_shape(
            CollectionMode::UtxoBatch,
            &[(command.leg.id.clone(), command.leg.kind)],
        )?;
        if command.leg.planned_amount.is_some() {
            return Err(invalid(
                "UTXO-batch sweep leg must use participant reservations",
            ));
        }
        if command.participants.is_empty() {
            return Err(invalid(
                "UTXO-batch collection must contain at least one participant",
            ));
        }
        if command.participants.len() > MAX_COLLECTION_PARTICIPANTS {
            return Err(invalid("UTXO-batch collection has too many participants"));
        }
        let mut previous_deposit = None;
        let mut all_resources = BTreeSet::new();
        let mut evidence_bytes = 0_usize;
        for participant in &command.participants {
            validate_non_empty(&participant.user_id.0, "collection participant user ID")?;
            validate_non_empty(
                &participant.deposit_id.0,
                "collection participant deposit ID",
            )?;
            validate_non_empty(
                &participant.expected_ledger_head.0,
                "collection participant expected ledger head",
            )?;
            if participant.reservation_amount.is_zero() {
                return Err(invalid(
                    "collection participant reservation amount must be positive",
                ));
            }
            if previous_deposit
                .as_ref()
                .is_some_and(|deposit_id| deposit_id >= &participant.deposit_id)
            {
                return Err(invalid(
                    "UTXO-batch participants must be strictly ordered by deposit ID",
                ));
            }
            previous_deposit = Some(participant.deposit_id.clone());
            validate_spend_resources(
                &command.asset,
                participant.reservation_amount.clone(),
                &participant.spend_resources,
            )?;
            for resource in &participant.spend_resources {
                if !all_resources.insert(resource.id.clone()) {
                    return Err(invalid(
                        "UTXO-batch contains a duplicate exact spend resource",
                    ));
                }
                evidence_bytes = evidence_bytes
                    .checked_add(resource.evidence.as_bytes().len())
                    .ok_or_else(|| invalid("UTXO-batch evidence size overflows"))?;
            }
        }
        if all_resources.len() > MAX_COLLECTION_SPEND_RESOURCES {
            return Err(invalid(
                "UTXO-batch collection has too many spend resources",
            ));
        }
        if evidence_bytes > MAX_TOTAL_SPEND_RESOURCE_EVIDENCE_BYTES {
            return Err(invalid(
                "UTXO-batch total spend-resource evidence exceeds the persistence limit",
            ));
        }
        Ok(())
    }
}

pub(super) fn validate_allocation(
    collection: &Collection,
    allocation: &CollectionAllocation,
) -> Result<(), DepositError> {
    let participant = collection
        .participant(&allocation.deposit_id)
        .ok_or_else(|| invalid("collection allocation belongs to a non-participant deposit"))?;
    if allocation.asset != collection.asset {
        return Err(invalid(
            "collection allocation asset does not match the reservation",
        ));
    }
    validate_asset(&allocation.allocated_fee_asset, "allocated fee asset")?;
    if allocation.allocated_fee_asset.chain != collection.asset.chain {
        return Err(invalid(
            "allocated fee chain does not match the collection chain",
        ));
    }
    if allocation.gross_debit.is_zero() {
        return Err(invalid("confirmed collection gross debit must be positive"));
    }
    participant
        .reservation
        .amount
        .checked_sub(&allocation.gross_debit)
        .map_err(|_| invalid("confirmed gross debit exceeds the durable reservation"))?;

    if allocation.allocated_fee_asset == collection.asset {
        let attributed = allocation
            .master_credit
            .checked_add(&allocation.allocated_fee)
            .map_err(|_| invalid("confirmed collection attribution overflows"))?;
        if attributed != allocation.gross_debit {
            return Err(invalid(
                "same-asset master credit plus allocated fee must equal gross debit",
            ));
        }
    } else if allocation.master_credit != allocation.gross_debit {
        return Err(invalid(
            "cross-asset fee attribution must preserve gross collection asset credit",
        ));
    }
    Ok(())
}

pub(super) fn validate_allocations(
    collection: &Collection,
    allocations: &[CollectionAllocation],
) -> Result<(), DepositError> {
    let expected_len = if collection.mode == CollectionMode::UtxoBatch {
        collection.participants.len()
    } else {
        1
    };
    if allocations.len() != expected_len {
        return Err(invalid(
            "sweep attribution must contain exactly one allocation per participant",
        ));
    }
    for (participant, allocation) in collection.participants.iter().zip(allocations) {
        if allocation.deposit_id != participant.reservation.deposit_id {
            return Err(invalid(
                "collection allocations must follow canonical participant order",
            ));
        }
        if collection.mode == CollectionMode::UtxoBatch
            && allocation.allocated_fee_asset != collection.asset
        {
            return Err(invalid(
                "UTXO-batch allocation fee must use the collected native asset",
            ));
        }
        validate_allocation(collection, allocation)?;
    }
    Ok(())
}

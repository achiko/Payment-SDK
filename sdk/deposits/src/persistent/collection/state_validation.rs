use super::*;

impl Collection {
    pub(super) fn validate_persisted(&self) -> Result<(), DepositError> {
        let collection = self;
        validate_non_empty(&collection.id.0, "persisted collection ID")?;
        validate_non_empty(&collection.job_id.0, "persisted collection job ID")?;
        validate_asset(&collection.asset, "persisted collection asset")?;
        if collection.destination.scope.chain != collection.asset.chain
            || collection.destination.scope.network.is_empty()
        {
            return Err(storage_error(
                "persisted collection destination chain does not match asset chain",
            ));
        }
        if collection.participants.is_empty() {
            return Err(storage_error("persisted collection has no participants"));
        }
        let mut previous_deposit = None;
        let mut all_resources = BTreeSet::new();
        let mut evidence_bytes = 0_usize;
        if collection.participants.len() > MAX_COLLECTION_PARTICIPANTS {
            return Err(storage_error(
                "persisted collection has too many participants",
            ));
        }
        for participant in &collection.participants {
            if participant.user_id.0.is_empty()
                || participant.reservation.deposit_id.0.is_empty()
                || participant.reservation.asset != collection.asset
                || participant.reservation.amount.is_zero()
            {
                return Err(storage_error(
                    "persisted collection participant identity or reservation is invalid",
                ));
            }
            if previous_deposit
                .as_ref()
                .is_some_and(|deposit_id| deposit_id >= &participant.reservation.deposit_id)
            {
                return Err(storage_error(
                    "persisted collection participants are not canonically ordered",
                ));
            }
            previous_deposit = Some(participant.reservation.deposit_id.clone());
            if collection.mode == CollectionMode::UtxoBatch
                && !participant.spend_resources.is_empty()
            {
                validate_spend_resources(
                    &collection.asset,
                    participant.reservation.amount.clone(),
                    &participant.spend_resources,
                )
                .map_err(|error| storage_error(error.message))?;
            } else if collection.mode != CollectionMode::UtxoBatch
                && !participant.spend_resources.is_empty()
            {
                return Err(storage_error(
                    "persisted account-model participant contains UTXO spend resources",
                ));
            }
            for resource in &participant.spend_resources {
                if !all_resources.insert(resource.id.clone()) {
                    return Err(storage_error(
                        "persisted collection contains a duplicate spend resource",
                    ));
                }
                evidence_bytes = evidence_bytes
                    .checked_add(resource.evidence.as_bytes().len())
                    .ok_or_else(|| storage_error("persisted collection evidence size overflows"))?;
            }
        }
        if all_resources.len() > MAX_COLLECTION_SPEND_RESOURCES
            || evidence_bytes > MAX_TOTAL_SPEND_RESOURCE_EVIDENCE_BYTES
        {
            return Err(storage_error(
                "persisted collection spend-resource bounds are exceeded",
            ));
        }
        if collection.mode != CollectionMode::UtxoBatch && collection.participants.len() != 1 {
            return Err(storage_error(
                "persisted account-model collection must contain one participant",
            ));
        }
        if collection.updated_at < collection.created_at {
            return Err(storage_error(
                "persisted collection update predates its creation",
            ));
        }
        validate_leg_shape(
            collection.mode,
            &collection
                .legs
                .iter()
                .map(|leg| (leg.id.clone(), leg.kind))
                .collect::<Vec<_>>(),
        )
        .map_err(|error| storage_error(error.message))?;
        for (position, leg) in collection.legs.iter().enumerate() {
            if usize::from(leg.position) != position {
                return Err(storage_error(
                    "persisted collection leg positions are not contiguous",
                ));
            }
            if leg.updated_at < collection.created_at {
                return Err(storage_error(
                    "persisted collection leg update predates aggregate creation",
                ));
            }
            if let Some(transaction_id) = leg.state.transaction_id() {
                validate_transaction_for_collection(collection, transaction_id)
                    .map_err(|error| storage_error(error.message))?;
            }
            if let Some(error) = &leg.last_error {
                error
                    .validate()
                    .map_err(|error| storage_error(error.message))?;
            }
            let compatibility_allocation =
                (leg.allocations.len() == 1).then(|| leg.allocations[0].clone());
            if leg.allocation != compatibility_allocation {
                return Err(storage_error(
                    "persisted collection singular allocation mirror is inconsistent",
                ));
            }
            if !leg.allocations.is_empty() {
                validate_allocations(collection, &leg.allocations)
                    .map_err(|error| storage_error(error.message))?;
            }
            match leg.kind {
                CollectionLegKind::GasFunding
                    if leg
                        .planned_amount
                        .as_ref()
                        .is_none_or(|amount| amount.is_zero()) =>
                {
                    return Err(storage_error(
                        "persisted gas-funding leg is missing its positive planned amount",
                    ));
                }
                CollectionLegKind::Sweep
                    if collection.mode == CollectionMode::UtxoBatch
                        && leg.planned_amount.is_some() =>
                {
                    return Err(storage_error(
                        "persisted UTXO sweep leg must not contain a fee limit",
                    ));
                }
                CollectionLegKind::Sweep
                    if collection.mode != CollectionMode::UtxoBatch
                        && !matches!(leg.state, CollectionLegState::Required)
                        && leg.planned_amount.as_ref().is_none_or(Decimal::is_zero) =>
                {
                    return Err(storage_error(
                        "persisted account-model sweep is missing its positive fee limit",
                    ));
                }
                CollectionLegKind::GasFunding if !leg.allocations.is_empty() => {
                    return Err(storage_error(
                        "persisted gas-funding leg must not contain sweep attribution",
                    ));
                }
                CollectionLegKind::Sweep
                    if matches!(leg.state, CollectionLegState::Confirmed { .. })
                        && leg.allocations.is_empty() =>
                {
                    return Err(storage_error(
                        "persisted confirmed sweep is missing attribution",
                    ));
                }
                _ => {}
            }
            if leg.watch_id.is_some()
                && (matches!(leg.state, CollectionLegState::Required)
                    || (matches!(leg.state, CollectionLegState::Signed { .. })
                        && collection.mode != CollectionMode::UtxoBatch))
            {
                return Err(storage_error(
                    "persisted pre-broadcast collection leg contains an IX watch",
                ));
            }
            let attribution_allowed = matches!(
                leg.state,
                CollectionLegState::Confirmed { .. } | CollectionLegState::Reorged { .. }
            ) || (collection.mode == CollectionMode::UtxoBatch
                && matches!(
                    leg.state,
                    CollectionLegState::Signed { .. }
                        | CollectionLegState::Broadcast { .. }
                        | CollectionLegState::Failed { .. }
                ));
            if !leg.allocations.is_empty() && !attribution_allowed {
                return Err(storage_error(
                    "persisted collection attribution is attached to a non-confirmed leg",
                ));
            }
            if leg.last_error.is_some()
                != matches!(
                    leg.state,
                    CollectionLegState::Failed { .. } | CollectionLegState::Reorged { .. }
                )
            {
                return Err(storage_error(
                    "persisted collection leg safe error does not match its terminal state",
                ));
            }
        }
        if let Some(error) = &collection.last_error {
            error
                .validate()
                .map_err(|error| storage_error(error.message))?;
        }
        let summed_attempts = collection
            .legs
            .iter()
            .try_fold(0_u32, |sum, leg| sum.checked_add(leg.attempt_count))
            .ok_or_else(|| storage_error("persisted collection attempt total overflows"))?;
        if summed_attempts != collection.attempt_count {
            return Err(storage_error(
                "persisted collection attempt total does not match its legs",
            ));
        }
        match collection.state {
            CollectionState::Required => {
                if collection.participants.iter().any(|participant| {
                    !matches!(
                        participant.reservation.state,
                        CollectionReservationState::Active
                    )
                }) || collection.last_error.is_some()
                {
                    return Err(storage_error(
                        "required collection must have an active reservation and no error",
                    ));
                }
            }
            CollectionState::InProgress => {
                if collection.participants.iter().any(|participant| {
                    !matches!(
                        participant.reservation.state,
                        CollectionReservationState::Active
                    )
                }) || collection.last_error.is_some()
                {
                    return Err(storage_error(
                        "in-progress collection must have an active reservation and no error",
                    ));
                }
            }
            CollectionState::Completed => {
                if !collection.all_legs_confirmed()
                    || collection.participants.iter().any(|participant| {
                        !matches!(
                            participant.reservation.state,
                            CollectionReservationState::Consumed { .. }
                        )
                    })
                    || collection.last_error.is_some()
                {
                    return Err(storage_error(
                        "completed collection must have confirmed legs and a consumed reservation",
                    ));
                }
            }
            CollectionState::Failed => {
                if !collection
                    .legs
                    .iter()
                    .any(|leg| matches!(leg.state, CollectionLegState::Failed { .. }))
                    || collection.last_error.is_none()
                    || collection.participants.iter().any(|participant| {
                        matches!(
                            participant.reservation.state,
                            CollectionReservationState::Consumed { .. }
                        )
                    })
                {
                    return Err(storage_error(
                        "failed collection has inconsistent leg, error, or reservation state",
                    ));
                }
            }
            CollectionState::Reorged => {
                if !collection
                    .legs
                    .iter()
                    .any(|leg| matches!(leg.state, CollectionLegState::Reorged { .. }))
                    || collection.last_error.is_none()
                    || collection.participants.iter().any(|participant| {
                        matches!(
                            participant.reservation.state,
                            CollectionReservationState::Consumed { .. }
                        )
                    })
                {
                    return Err(storage_error(
                        "reorged collection has inconsistent leg, error, or reservation state",
                    ));
                }
            }
        }
        Ok(())
    }
}

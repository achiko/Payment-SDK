use crate::{
    IndexError, IndexErrorKind, RegisterWatch, WatchContext, WatchDecision, WatchId, WatchPlan,
    WatchReceipt, WatchTarget,
};

pub fn watch<T: Clone + PartialEq>(
    command: &RegisterWatch<T>,
    context: &WatchContext<T>,
) -> Result<WatchDecision<T>, IndexError> {
    let request = &command.request;
    if request.idempotency_key.trim().is_empty() {
        return Err(IndexError::new(
            IndexErrorKind::InvalidWatch,
            "watch idempotency key must not be empty",
            false,
        ));
    }
    if request.selector.scope != request.scope {
        return Err(IndexError::new(
            IndexErrorKind::ScopeMismatch,
            "watch address belongs to another scope",
            false,
        ));
    }
    if let Some(existing) = &context.existing {
        if existing.scope != request.scope
            || existing.selector != request.selector
            || existing.start_height != request.start_height
            || existing.target != command.target
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "watch idempotency key was reused with a different payload",
                false,
            ));
        }
        return Ok(WatchDecision {
            receipt: receipt(existing),
            plan: None,
        });
    }
    if context.checkpoint != command.registered_at {
        return Err(IndexError::new(
            IndexErrorKind::Conflict,
            "watch registration checkpoint changed before durable acknowledgement",
            true,
        ));
    }
    let next_id = context.next_id.checked_add(1).ok_or_else(|| {
        IndexError::new(
            IndexErrorKind::Store,
            "watch ID counter is exhausted",
            false,
        )
    })?;
    context.version.0.checked_add(1).ok_or_else(|| {
        IndexError::new(IndexErrorKind::Store, "watch version is exhausted", false)
    })?;
    let watch = WatchTarget {
        id: WatchId(format!("watch-{next_id:020}")),
        scope: request.scope.clone(),
        selector: request.selector.clone(),
        target: command.target.clone(),
        idempotency_key: request.idempotency_key.clone(),
        start_height: request.start_height,
        registered_at: command.registered_at.clone(),
    };
    Ok(WatchDecision {
        receipt: receipt(&watch),
        plan: Some(WatchPlan {
            watch,
            expected_checkpoint: context.checkpoint.clone(),
            expected_version: context.version,
        }),
    })
}

fn receipt<T>(watch: &WatchTarget<T>) -> WatchReceipt {
    WatchReceipt {
        id: watch.id.clone(),
        scope: watch.scope.clone(),
        selector: watch.selector.clone(),
        start_height: watch.start_height,
        registered_at: watch.registered_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockHeight, CanonicalAddress, ChainId, IndexScope, WatchRequest, WatchVersion};

    fn command(key: &str) -> RegisterWatch<CanonicalAddress> {
        let scope = IndexScope {
            chain: ChainId("test".into()),
            network: "local".into(),
        };
        let selector = CanonicalAddress {
            scope: scope.clone(),
            value: "address".into(),
        };
        RegisterWatch {
            request: WatchRequest {
                scope,
                selector: selector.clone(),
                start_height: BlockHeight(7),
                idempotency_key: key.into(),
            },
            target: selector,
            registered_at: None,
        }
    }

    #[test]
    fn creates_a_deterministic_plan() {
        let command = command("wallet");
        let decision = watch(
            &command,
            &WatchContext {
                checkpoint: None,
                version: WatchVersion(3),
                next_id: 8,
                existing: None,
            },
        )
        .expect("watch plans");
        let plan = decision.plan.expect("new watch has a plan");
        assert_eq!(plan.watch.id, WatchId("watch-00000000000000000009".into()));
        assert_eq!(plan.expected_version, WatchVersion(3));
    }

    #[test]
    fn returns_existing_only_for_the_same_payload() {
        let command = command("wallet");
        let context = WatchContext {
            checkpoint: None,
            version: WatchVersion(1),
            next_id: 1,
            existing: Some(WatchTarget {
                id: WatchId("watch-1".into()),
                scope: command.request.scope.clone(),
                selector: command.request.selector.clone(),
                target: command.target.clone(),
                idempotency_key: command.request.idempotency_key.clone(),
                start_height: command.request.start_height,
                registered_at: None,
            }),
        };
        let decision = watch(&command, &context).expect("same payload is idempotent");
        assert!(decision.plan.is_none());
    }
}

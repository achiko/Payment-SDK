use std::{collections::BTreeSet, time::Instant};

use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::{self, Duration};

use crate::{
    cli::ScenarioSelection,
    error::{HarnessError, OptionContext, Result},
    harness::{CollectionHandle, DepositHandle, Fixture, FixtureConfig},
    report::{CaseResult, CaseStatus},
};

const SCENARIO_TIMEOUT: Duration = Duration::from_secs(180);

pub async fn execute(config: FixtureConfig) -> CaseResult {
    let started = Instant::now();
    let scenario = config.scenario;
    let profile = config.profile;
    let mut fixture = match Fixture::new(config) {
        Ok(fixture) => fixture,
        Err(error) => {
            return CaseResult {
                scenario,
                authentication_mode: profile,
                risk: scenario.risk(),
                status: CaseStatus::Failed,
                duration_millis: started.elapsed().as_millis(),
                assertions: Vec::new(),
                evidence: Default::default(),
                error: Some(error.to_string()),
                retained_workdir: None,
            };
        }
    };

    let scenario_result = async {
        fixture.start().await?;
        match scenario {
            ScenarioSelection::Signing => signing(&mut fixture).await,
            ScenarioSelection::RestartReplay => restart_replay(&mut fixture).await,
            ScenarioSelection::Reorg => reorg(&mut fixture).await,
            ScenarioSelection::Reservation => reservation(&mut fixture).await,
            ScenarioSelection::All => Err(HarnessError::new(
                "aggregate scenario cannot execute inside one fixture",
            )),
        }
    }
    .await;

    let assertions = fixture.take_assertions();
    let evidence = fixture.take_evidence();
    let finish_result = fixture.finish().await;
    let (status, error, retained_workdir) = match (scenario_result, finish_result) {
        (Ok(()), Ok(retained)) => (CaseStatus::Passed, None, retained),
        (Err(error), Ok(retained)) => (CaseStatus::Failed, Some(error.to_string()), retained),
        (Ok(()), Err(error)) => (CaseStatus::Failed, Some(error.to_string()), None),
        (Err(scenario_error), Err(cleanup_error)) => (
            CaseStatus::Failed,
            Some(format!(
                "{scenario_error}; fixture cleanup also failed: {cleanup_error}"
            )),
            None,
        ),
    };
    CaseResult {
        scenario,
        authentication_mode: profile,
        risk: scenario.risk(),
        status,
        duration_millis: started.elapsed().as_millis(),
        assertions,
        evidence,
        error,
        retained_workdir,
    }
}

async fn signing(fixture: &mut Fixture) -> Result<()> {
    for (kind, witness_name) in [("p2wpkh", "SegWit v0"), ("p2tr", "Taproot key path")] {
        let source = fixture
            .generate_wallet_address(kind, &format!("signing-{kind}"))
            .await?;
        fixture
            .register_watch(
                "address",
                &source.address,
                &format!("signing-{kind}-address"),
            )
            .await?;
        let funding_txid = fixture.fund_address(&source.address, 500_000).await?;
        fixture
            .register_watch(
                "transaction",
                &funding_txid,
                &format!("signing-{kind}-funding"),
            )
            .await?;
        fixture.mine_blocks(1).await?;
        let included = fixture
            .wait_ix_transaction(&funding_txid, "included")
            .await?;
        fixture.assert(
            format!("IX publishes {witness_name} funding as Included at depth one"),
            included
                .get("status")
                .and_then(|status| status.get("confirmations"))
                .and_then(Value::as_str)
                == Some("1"),
        )?;
        fixture.mine_blocks(1).await?;
        fixture
            .wait_ix_transaction(&funding_txid, "confirmed")
            .await?;
        let outputs = fixture.wait_ix_utxo_total(&source.address, 500_000).await?;
        fixture.assert(
            format!("IX exposes one exact {witness_name} funding outpoint"),
            outputs.len() == 1 && outputs[0].confirmations.parse::<u64>().unwrap_or_default() >= 2,
        )?;

        let recipient = fixture
            .core_wallet_address(&format!("signing-{kind}-recipient"))
            .await?;
        let signed = fixture
            .sign_transfer(&source, &outputs[0], &recipient, 200_000)
            .await?;
        fixture.assert(
            format!("{witness_name} signing returns a positive fee and virtual size"),
            signed.fee_satoshis.parse::<u64>().unwrap_or_default() > 0
                && signed.virtual_size.parse::<u64>().unwrap_or_default() > 0,
        )?;
        fixture.assert(
            format!("{witness_name} signing does not broadcast"),
            !fixture
                .core_knows_mempool_transaction(&signed.transaction_id)
                .await?,
        )?;
        let before_receipt = fixture.receipt(&signed.transaction_id).await?;
        fixture.assert(
            format!("{witness_name} has no Core receipt before broadcast"),
            before_receipt.get("receipt") == Some(&Value::Null),
        )?;
        let acceptance = fixture.test_mempool_accept(&signed.raw_transaction).await?;
        fixture.assert(
            format!("Core testmempoolaccept accepts the signed {witness_name} transaction"),
            acceptance
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item.get("allowed"))
                .and_then(Value::as_bool)
                == Some(true),
        )?;
        fixture
            .register_watch(
                "transaction",
                &signed.transaction_id,
                &format!("signing-{kind}-spend"),
            )
            .await?;
        let returned_txid = fixture.broadcast(&signed).await?;
        fixture.assert(
            format!("WS broadcasts the exact {witness_name} transaction ID"),
            returned_txid == signed.transaction_id
                && fixture
                    .core_knows_mempool_transaction(&signed.transaction_id)
                    .await?,
        )?;
        let mempool_receipt = fixture.receipt(&signed.transaction_id).await?;
        fixture.assert(
            format!("{witness_name} receipt reports zero-confirmation mempool state"),
            mempool_receipt
                .get("receipt")
                .and_then(|receipt| receipt.get("confirmations"))
                .and_then(Value::as_u64)
                == Some(0)
                && mempool_receipt
                    .get("receipt")
                    .and_then(|receipt| receipt.get("included_in"))
                    == Some(&Value::Null),
        )?;
        fixture.mine_blocks(1).await?;
        fixture
            .wait_ix_transaction(&signed.transaction_id, "included")
            .await?;
        fixture.mine_blocks(1).await?;
        fixture
            .wait_ix_transaction(&signed.transaction_id, "confirmed")
            .await?;
        fixture.evidence(format!("{kind}_transaction_id"), signed.transaction_id);
    }
    Ok(())
}

async fn restart_replay(fixture: &mut Fixture) -> Result<()> {
    let deposit = confirmed_deposit(fixture, "restart", "restart-user", 250_000).await?;
    let create_job = fixture.wait_job_terminal(&deposit.job_id).await?;
    fixture.assert(
        "deposit creation job succeeds before restart",
        create_job.get("state").and_then(Value::as_str) == Some("succeeded"),
    )?;

    let ix_status_before = fixture.ix_status().await?;
    let ix_events_before = fixture.ix_events().await?;
    let ix_utxos_before = fixture.ix_utxos(&deposit.address).await?;
    let balances_before = fixture.ps_balances(&deposit.deposit_id).await?;
    let ledger_before = fixture.ps_ledger(&deposit.deposit_id).await?;
    let job_before = fixture.ps_job(&deposit.job_id).await?;
    let admin_before = fixture.ps_admin_status().await?;

    fixture.stop_application_services_for_restart().await?;
    fixture.restart_application_services().await?;

    let ix_status_after = fixture.ix_status().await?;
    let ix_events_after = fixture.ix_events().await?;
    let ix_utxos_after = fixture.ix_utxos(&deposit.address).await?;
    let balances_after = fixture.ps_balances(&deposit.deposit_id).await?;
    let ledger_after = fixture.ps_ledger(&deposit.deposit_id).await?;
    let job_after = fixture.ps_job(&deposit.job_id).await?;
    let admin_after = fixture.ps_admin_status().await?;

    fixture.assert(
        "IX restores the identical canonical checkpoint after restart",
        ix_status_before.get("checkpoint") == ix_status_after.get("checkpoint"),
    )?;
    fixture.assert(
        "IX replay preserves the exact event prefix without duplicates",
        ix_events_before == ix_events_after,
    )?;
    fixture.assert(
        "IX restores the exact watched UTXO projection after restart",
        comparable_utxos(&ix_utxos_before) == comparable_utxos(&ix_utxos_after),
    )?;
    fixture.assert(
        "PS restores balances, ledger, and durable job without duplication",
        balances_before == balances_after
            && ledger_before == ledger_after
            && job_before == job_after,
    )?;
    fixture.assert(
        "PS ingestion and projection cursors survive restart",
        admin_before.get("ingestion_cursor") == admin_after.get("ingestion_cursor")
            && admin_before.get("projection_cursor") == admin_after.get("projection_cursor"),
    )?;

    let collection = fixture
        .create_collection(std::slice::from_ref(&deposit.deposit_id), "restart-after")
        .await?;
    let broadcast = fixture
        .wait_collection_transaction(&collection.collection_id)
        .await?;
    let transaction_id = collection_transaction_id(&broadcast)?;
    fixture.assert(
        "stateless WS signs and broadcasts after restart while custody remains alive",
        fixture
            .core_knows_mempool_transaction(&transaction_id)
            .await?,
    )?;
    fixture.mine_blocks(2).await?;
    fixture
        .wait_collection_state(&collection.collection_id, &["completed"])
        .await?;
    fixture
        .wait_ps_balance(&deposit.deposit_id, "collected", "250000")
        .await?;
    fixture.evidence("deposit_id", deposit.deposit_id);
    fixture.evidence("collection_transaction_id", transaction_id);
    Ok(())
}

async fn reorg(fixture: &mut Fixture) -> Result<()> {
    let deposit = confirmed_deposit(fixture, "reorg", "reorg-user", 250_000).await?;
    let collection = fixture
        .create_collection(std::slice::from_ref(&deposit.deposit_id), "reorg")
        .await?;
    let broadcast = fixture
        .wait_collection_transaction(&collection.collection_id)
        .await?;
    let transaction_id = collection_transaction_id(&broadcast)?;
    fixture.mine_blocks(1).await?;
    fixture
        .wait_ix_transaction(&transaction_id, "included")
        .await?;
    fixture.mine_blocks(1).await?;
    fixture
        .wait_ix_transaction(&transaction_id, "confirmed")
        .await?;
    fixture
        .wait_collection_state(&collection.collection_id, &["completed"])
        .await?;
    let transaction = fixture.core_verbose_transaction(&transaction_id).await?;
    let old_block_hash = required_string(&transaction, "blockhash")?;
    let original_signed_bytes = required_string(&transaction, "hex")?;
    let old_tip_height = fixture.block_count().await?;

    fixture.stop_payment_service().await?;
    fixture.invalidate_block(&old_block_hash).await?;
    let mut replacement_hashes = Vec::new();
    while fixture.block_count().await? < old_tip_height {
        replacement_hashes.push(fixture.mine_empty_block().await?);
    }
    fixture.assert(
        "controlled fork mines an explicit empty replacement branch to the old tip height",
        !replacement_hashes.is_empty()
            && replacement_hashes
                .iter()
                .all(|hash| hash != &old_block_hash),
    )?;
    let reorged = fixture
        .wait_ix_transaction(&transaction_id, "reorged")
        .await?;
    let reorged_revision = parse_u64_field(&reorged, "revision")?;
    fixture
        .wait_ix_utxo_total(&deposit.address, 250_000)
        .await?;
    fixture.assert(
        "IX appends Reorged and restores every collection input UTXO",
        reorged_revision > 0,
    )?;

    fixture.stop_indexer_and_wallet().await?;
    fixture.restart_core().await?;
    fixture.assert(
        "controlled Core restart clears the disposable mempool before recovery",
        !fixture
            .core_knows_mempool_transaction(&transaction_id)
            .await?,
    )?;
    fixture.start_indexer_and_wallet().await?;
    fixture.start_payment_service().await?;
    fixture
        .wait_collection_state(&collection.collection_id, &["reorged"])
        .await?;
    let corrected = fixture
        .wait_ps_balance(&deposit.deposit_id, "collected", "0")
        .await?;
    fixture.assert(
        "PS appends a corrected absolute balance after collection rollback",
        corrected.get("balance").and_then(Value::as_str) == Some("250000")
            && corrected.get("confirmed").and_then(Value::as_str) == Some("250000"),
    )?;
    fixture
        .retry_collection(&collection.collection_id, "reorg-same-bytes")
        .await?;
    fixture
        .wait_collection_state(&collection.collection_id, &["in_progress"])
        .await?;
    wait_for_mempool_transaction(fixture, &transaction_id).await?;
    let retried = fixture.success(
        fixture.collection(&collection.collection_id).await?,
        "reading retried Bitcoin collection",
    )?;
    fixture.assert(
        "explicit recovery rebroadcasts the persisted transaction ID without re-signing",
        collection_transaction_id(&retried)? == transaction_id,
    )?;
    fixture.mine_blocks(1).await?;
    let reincluded = fixture
        .wait_ix_transaction(&transaction_id, "included")
        .await?;
    let reincluded_revision = parse_u64_field(&reincluded, "revision")?;
    fixture.assert(
        "same transaction receives a later Included revision",
        reincluded_revision > reorged_revision,
    )?;
    fixture.mine_blocks(1).await?;
    fixture
        .wait_ix_transaction(&transaction_id, "confirmed")
        .await?;
    fixture
        .wait_collection_state(&collection.collection_id, &["completed"])
        .await?;
    fixture
        .wait_ps_balance(&deposit.deposit_id, "collected", "250000")
        .await?;
    let re_included_transaction = fixture.core_verbose_transaction(&transaction_id).await?;
    let new_block_hash = required_string(&re_included_transaction, "blockhash")?;
    fixture.assert(
        "recovery rebroadcasts the exact persisted signed consensus bytes",
        required_string(&re_included_transaction, "hex")? == original_signed_bytes,
    )?;
    fixture.assert(
        "same transaction ID is confirmed in a different canonical block",
        new_block_hash != old_block_hash,
    )?;
    fixture.evidence("collection_transaction_id", transaction_id);
    fixture.evidence("orphaned_block_hash", old_block_hash);
    fixture.evidence("reinclusion_block_hash", new_block_hash);
    Ok(())
}

async fn reservation(fixture: &mut Fixture) -> Result<()> {
    let deposit = confirmed_deposit(fixture, "reservation", "reservation-user", 250_000).await?;
    let expected_resources = fixture
        .ix_utxos(&deposit.address)
        .await?
        .into_iter()
        .map(|output| format!("{}:{}", output.transaction_id, output.output_index))
        .collect::<BTreeSet<_>>();
    fixture.assert(
        "reservation fixture exposes at least one exact spendable outpoint",
        !expected_resources.is_empty(),
    )?;

    let first =
        fixture.create_collection(std::slice::from_ref(&deposit.deposit_id), "reservation-a");
    let second =
        fixture.create_collection(std::slice::from_ref(&deposit.deposit_id), "reservation-b");
    let (first, second) = tokio::join!(first, second);
    let first = first?;
    let second = second?;
    let (winner, loser, loser_job) = wait_competing_collections(fixture, &first, &second).await?;
    let winner_body = fixture.success(
        fixture.collection(&winner.collection_id).await?,
        "reading winning collection",
    )?;
    let winner_resources = collection_spend_resources(&winner_body)?;
    fixture.assert(
        "winning collection reserves the complete exact IX outpoint set",
        winner_resources == expected_resources,
    )?;
    fixture.assert(
        "competing collection fails without creating a second aggregate",
        loser_job.get("state").and_then(Value::as_str) == Some("failed")
            && fixture.collection(&loser.collection_id).await?.status == StatusCode::NOT_FOUND,
    )?;
    let loser_message = loser_job
        .get("last_error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let loser_code = loser_job
        .get("last_error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let loser_retryable = loser_job
        .get("last_error")
        .and_then(|error| error.get("retryable"))
        .and_then(Value::as_bool);
    fixture.evidence("losing_job_error_code", loser_code);
    fixture.assert(
        "losing collection reports a non-retryable sanitized invalid-job result",
        loser_code == "invalid_job"
            && loser_message == "durable job input is invalid"
            && loser_retryable == Some(false),
    )?;
    let transaction_id = collection_transaction_id(&winner_body)?;
    fixture.assert(
        "only the winning reserved aggregate broadcasts a transaction",
        fixture
            .core_knows_mempool_transaction(&transaction_id)
            .await?,
    )?;
    assert_collection_allocation_conservation(fixture, &winner_body, 250_000)?;
    fixture.mine_blocks(2).await?;
    fixture
        .wait_collection_state(&winner.collection_id, &["completed"])
        .await?;
    fixture.evidence("winning_collection_id", winner.collection_id);
    fixture.evidence("losing_collection_id", loser.collection_id);
    fixture.evidence("collection_transaction_id", transaction_id);
    Ok(())
}

async fn confirmed_deposit(
    fixture: &mut Fixture,
    suffix: &str,
    user_id: &str,
    satoshis: u64,
) -> Result<DepositHandle> {
    let deposit = fixture.create_deposit(user_id, satoshis, suffix).await?;
    let funding_txid = fixture.fund_address(&deposit.address, satoshis).await?;
    fixture.mine_blocks(1).await?;
    fixture
        .wait_ps_balance(&deposit.deposit_id, "received", &satoshis.to_string())
        .await?;
    fixture.mine_blocks(1).await?;
    let balances = fixture
        .wait_ps_balance(&deposit.deposit_id, "confirmed", &satoshis.to_string())
        .await?;
    fixture.assert(
        format!("PS confirms the complete {suffix} deposit without changing accounted"),
        balances.get("balance").and_then(Value::as_str) == Some(satoshis.to_string().as_str())
            && balances.get("accounted").and_then(Value::as_str) == Some("0"),
    )?;
    fixture.evidence(format!("{suffix}_funding_transaction_id"), funding_txid);
    Ok(deposit)
}

fn comparable_utxos(outputs: &[crate::harness::IndexedUtxo]) -> BTreeSet<String> {
    outputs
        .iter()
        .map(|output| {
            format!(
                "{}:{}:{}:{}:{}",
                output.transaction_id,
                output.output_index,
                output.value_sats,
                output.script_pubkey,
                output.address
            )
        })
        .collect()
}

fn collection_transaction_id(collection: &Value) -> Result<String> {
    collection
        .get("legs")
        .and_then(Value::as_array)
        .and_then(|legs| legs.first())
        .and_then(|leg| leg.get("transaction_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context(|| "collection has no persisted transaction ID".to_owned())
}

fn collection_spend_resources(collection: &Value) -> Result<BTreeSet<String>> {
    let participants = collection
        .get("participants")
        .and_then(Value::as_array)
        .context(|| "collection participants are missing".to_owned())?;
    let mut resources = BTreeSet::new();
    for participant in participants {
        let items = participant
            .get("spend_resources")
            .and_then(Value::as_array)
            .context(|| "collection spend resources are missing".to_owned())?;
        for item in items {
            resources.insert(format!(
                "{}:{}",
                required_string(item, "transaction_id")?,
                required_string_or_number(item, "output_index")?
            ));
        }
    }
    Ok(resources)
}

async fn wait_competing_collections(
    fixture: &Fixture,
    first: &CollectionHandle,
    second: &CollectionHandle,
) -> Result<(CollectionHandle, CollectionHandle, Value)> {
    let started = Instant::now();
    loop {
        let first_collection = fixture.collection(&first.collection_id).await?;
        let second_collection = fixture.collection(&second.collection_id).await?;
        let first_job = fixture.ps_job(&first.job_id).await?;
        let second_job = fixture.ps_job(&second.job_id).await?;
        let first_has_tx = first_collection.status == StatusCode::OK
            && collection_transaction_id(&first_collection.body).is_ok();
        let second_has_tx = second_collection.status == StatusCode::OK
            && collection_transaction_id(&second_collection.body).is_ok();
        if first_has_tx && second_job.get("state").and_then(Value::as_str) == Some("failed") {
            return Ok((clone_handle(first), clone_handle(second), second_job));
        }
        if second_has_tx && first_job.get("state").and_then(Value::as_str) == Some("failed") {
            return Ok((clone_handle(second), clone_handle(first), first_job));
        }
        if started.elapsed() >= SCENARIO_TIMEOUT {
            return Err(HarnessError::new(
                "competing collections did not converge to one winner and one failed loser",
            ));
        }
        time::sleep(Duration::from_millis(250)).await;
    }
}

fn clone_handle(handle: &CollectionHandle) -> CollectionHandle {
    CollectionHandle {
        collection_id: handle.collection_id.clone(),
        job_id: handle.job_id.clone(),
    }
}

fn assert_collection_allocation_conservation(
    fixture: &mut Fixture,
    collection: &Value,
    expected_gross: u64,
) -> Result<()> {
    let allocations = collection
        .get("legs")
        .and_then(Value::as_array)
        .and_then(|legs| legs.first())
        .and_then(|leg| leg.get("allocations"))
        .and_then(Value::as_array)
        .context(|| "collection allocations are missing".to_owned())?;
    let mut gross = 0_u64;
    let mut credit = 0_u64;
    let mut fee = 0_u64;
    for allocation in allocations {
        gross = gross
            .checked_add(parse_u64_field(allocation, "gross_debit")?)
            .context(|| "collection gross allocation overflow".to_owned())?;
        credit = credit
            .checked_add(parse_u64_field(allocation, "master_credit")?)
            .context(|| "collection credit allocation overflow".to_owned())?;
        fee = fee
            .checked_add(parse_u64_field(allocation, "allocated_fee")?)
            .context(|| "collection fee allocation overflow".to_owned())?;
    }
    fixture.assert(
        "collection allocation conserves gross input as master credit plus fee",
        gross == expected_gross && credit.checked_add(fee) == Some(gross),
    )
}

fn parse_u64_field(value: &Value, field: &str) -> Result<u64> {
    required_string(value, field)?
        .parse::<u64>()
        .map_err(|error| HarnessError::new(format!("invalid unsigned {field}: {error}")))
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context(|| format!("JSON field {field} is missing or not a string"))
}

fn required_string_or_number(value: &Value, field: &str) -> Result<String> {
    let field = value
        .get(field)
        .context(|| format!("JSON field {field} is missing"))?;
    if let Some(value) = field.as_str() {
        Ok(value.to_owned())
    } else if let Some(value) = field.as_u64() {
        Ok(value.to_string())
    } else {
        Err(HarnessError::new("JSON field is not a string or number"))
    }
}

async fn wait_for_mempool_transaction(fixture: &Fixture, transaction_id: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        if fixture
            .core_knows_mempool_transaction(transaction_id)
            .await?
        {
            return Ok(());
        }
        if started.elapsed() >= SCENARIO_TIMEOUT {
            return Err(HarnessError::new(format!(
                "transaction {transaction_id} did not return to the Core mempool after retry"
            )));
        }
        time::sleep(Duration::from_millis(250)).await;
    }
}

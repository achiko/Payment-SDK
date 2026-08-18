use std::str::FromStr;

use base::Decimal;
use indexing::{
    AssetId, BlockHeight, CanonicalAddress, ChainId, ConfirmationPolicy, ConfirmationProof,
    EventCursor, EventId, EventPage, IndexError, IndexErrorKind, IndexScope, MovementId,
    NetworkFee, ObservationEvent, ObservationRevision, ObservedTransaction, TransactionPage,
    TransactionRef, TransactionStatus, UnwatchOutcome, ValueMovement, WatchId, WatchReceipt,
    WatchSelector,
};
use serde::{Deserialize, Serialize};

use crate::checkpoint::BlockDto;

#[derive(Serialize)]
pub(crate) struct WatchBody<'a> {
    pub selector: SelectorBody<'a>,
    pub start_height: String,
    pub idempotency_key: &'a str,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum SelectorBody<'a> {
    Address(&'a str),
    Transaction(&'a str),
}

#[derive(Deserialize)]
pub(crate) struct WatchDto {
    id: String,
    scope: ScopeDto,
    selector: SelectorDto,
    start_height: String,
    registered_at: Option<BlockDto>,
    inactive_from: Option<String>,
    confirmation_depth: String,
    require_chain_finality: bool,
}

impl WatchDto {
    pub(crate) fn convert(self) -> Result<WatchReceipt, IndexError> {
        let scope = self.scope.convert();
        Ok(WatchReceipt {
            id: WatchId(self.id),
            selector: self.selector.convert(&scope),
            start_height: BlockHeight(parse_u64(&self.start_height, "watch start height")?),
            registered_at: self.registered_at.map(BlockDto::convert).transpose()?,
            inactive_from: self
                .inactive_from
                .map(|value| parse_u64(&value, "watch inactive height").map(BlockHeight))
                .transpose()?,
            confirmation_policy: ConfirmationPolicy {
                minimum_confirmations: parse_u64(
                    &self.confirmation_depth,
                    "watch confirmation depth",
                )?,
                require_chain_finality: self.require_chain_finality,
            },
            scope,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct UnwatchDto {
    outcome: String,
}

impl UnwatchDto {
    pub(crate) fn convert(self) -> Result<UnwatchOutcome, IndexError> {
        match self.outcome.as_str() {
            "deactivated" => Ok(UnwatchOutcome::Deactivated),
            "already_inactive" => Ok(UnwatchOutcome::AlreadyInactive),
            _ => Err(invalid_response("unwatch outcome is unknown")),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct TransactionsDto {
    transactions: Vec<TransactionDto>,
    next: Option<String>,
}

impl TransactionsDto {
    pub(crate) fn convert(self, scope: &IndexScope) -> Result<TransactionPage, IndexError> {
        let transactions = self
            .transactions
            .into_iter()
            .map(TransactionDto::convert)
            .collect::<Result<Vec<_>, _>>()?;
        let next = self.next.map(|value| TransactionRef {
            scope: scope.clone(),
            value,
        });
        Ok(TransactionPage { transactions, next })
    }
}

#[derive(Deserialize)]
pub(crate) struct EventsDto {
    events: Vec<EventDto>,
    next_cursor: Option<String>,
}

impl EventsDto {
    pub(crate) fn convert(self) -> Result<EventPage, IndexError> {
        Ok(EventPage {
            events: self
                .events
                .into_iter()
                .map(EventDto::convert)
                .collect::<Result<Vec<_>, _>>()?,
            next: self
                .next_cursor
                .map(|value| parse_u64(&value, "event cursor").map(EventCursor))
                .transpose()?,
        })
    }
}

#[derive(Deserialize)]
struct ScopeDto {
    chain: String,
    network: String,
}

impl ScopeDto {
    fn convert(self) -> IndexScope {
        IndexScope {
            chain: ChainId(self.chain),
            network: self.network,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum SelectorDto {
    Address(String),
    Transaction(String),
}

impl SelectorDto {
    fn convert(self, scope: &IndexScope) -> WatchSelector {
        match self {
            Self::Address(value) => WatchSelector::Address(CanonicalAddress {
                scope: scope.clone(),
                value,
            }),
            Self::Transaction(value) => WatchSelector::Transaction(TransactionRef {
                scope: scope.clone(),
                value,
            }),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct TransactionDto {
    scope: ScopeDto,
    transaction_id: String,
    revision: String,
    status: StatusDto,
    movements: Vec<MovementDto>,
    fee: Option<FeeDto>,
    first_seen_at: String,
    observed_at: String,
}

impl TransactionDto {
    pub(crate) fn convert(self) -> Result<ObservedTransaction, IndexError> {
        let scope = self.scope.convert();
        Ok(ObservedTransaction {
            transaction_id: TransactionRef {
                scope: scope.clone(),
                value: self.transaction_id,
            },
            revision: ObservationRevision(parse_u64(&self.revision, "transaction revision")?),
            status: self.status.convert(&scope)?,
            movements: self
                .movements
                .into_iter()
                .map(|movement| movement.convert(&scope))
                .collect::<Result<Vec<_>, _>>()?,
            fee: self.fee.map(|fee| fee.convert(&scope)).transpose()?,
            first_seen_at: parse_u64(&self.first_seen_at, "first-seen time")?,
            observed_at: parse_u64(&self.observed_at, "observation time")?,
            scope,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StatusDto {
    Pending,
    Included {
        block: BlockDto,
        confirmations: String,
    },
    Confirmed {
        block: BlockDto,
        proof: ProofDto,
    },
    Failed {
        block: Option<BlockDto>,
        reason: Option<String>,
    },
    Replaced {
        by: String,
    },
    Dropped,
    Reorged {
        previous_block: BlockDto,
    },
}

impl StatusDto {
    fn convert(self, scope: &IndexScope) -> Result<TransactionStatus, IndexError> {
        Ok(match self {
            Self::Pending => TransactionStatus::Pending,
            Self::Included {
                block,
                confirmations,
            } => TransactionStatus::Included {
                block: block.convert()?,
                confirmations: parse_u64(&confirmations, "confirmation count")?,
            },
            Self::Confirmed { block, proof } => TransactionStatus::Confirmed {
                block: block.convert()?,
                proof: proof.convert()?,
            },
            Self::Failed { block, reason } => TransactionStatus::Failed {
                block: block.map(BlockDto::convert).transpose()?,
                reason,
            },
            Self::Replaced { by } => TransactionStatus::Replaced {
                by: TransactionRef {
                    scope: scope.clone(),
                    value: by,
                },
            },
            Self::Dropped => TransactionStatus::Dropped,
            Self::Reorged { previous_block } => TransactionStatus::Reorged {
                previous_block: previous_block.convert()?,
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProofDto {
    Depth { required: String, observed: String },
    ChainFinalized,
    DepthAndChainFinalized { required: String, observed: String },
}

impl ProofDto {
    fn convert(self) -> Result<ConfirmationProof, IndexError> {
        Ok(match self {
            Self::Depth { required, observed } => ConfirmationProof::Depth {
                required: parse_u64(&required, "required confirmation depth")?,
                observed: parse_u64(&observed, "observed confirmation depth")?,
            },
            Self::ChainFinalized => ConfirmationProof::ChainFinalized,
            Self::DepthAndChainFinalized { required, observed } => {
                ConfirmationProof::DepthAndChainFinalized {
                    required: parse_u64(&required, "required confirmation depth")?,
                    observed: parse_u64(&observed, "observed confirmation depth")?,
                }
            }
        })
    }
}

#[derive(Deserialize)]
struct MovementDto {
    id: String,
    asset: String,
    amount: String,
    from: Option<String>,
    to: Option<String>,
    kind: MovementKindDto,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum MovementKindDto {
    Transfer,
    Input,
    Output,
    InternalTransfer,
    Mint,
    Burn,
}

impl MovementDto {
    fn convert(self, scope: &IndexScope) -> Result<ValueMovement, IndexError> {
        let chain = &scope.chain;
        let id = MovementId(self.id);
        let asset = AssetId {
            chain: chain.clone(),
            asset: self.asset,
        };
        let amount = parse_decimal(&self.amount)?;
        let from = self.from.map(|value| CanonicalAddress {
            scope: scope.clone(),
            value,
        });
        let to = self.to.map(|value| CanonicalAddress {
            scope: scope.clone(),
            value,
        });
        match self.kind {
            MovementKindDto::Transfer => Ok(ValueMovement::Transfer {
                id,
                asset,
                amount,
                from: required(from, "transfer source")?,
                to: required(to, "transfer destination")?,
            }),
            MovementKindDto::Input => Ok(ValueMovement::Input {
                id,
                asset,
                amount,
                owner: from,
            }),
            MovementKindDto::Output => Ok(ValueMovement::Output {
                id,
                asset,
                amount,
                owner: to,
            }),
            MovementKindDto::InternalTransfer => Ok(ValueMovement::InternalTransfer {
                id,
                asset,
                amount,
                from: required(from, "internal transfer source")?,
                to: required(to, "internal transfer destination")?,
            }),
            MovementKindDto::Mint => Ok(ValueMovement::Mint {
                id,
                asset,
                amount,
                to: required(to, "mint destination")?,
            }),
            MovementKindDto::Burn => Ok(ValueMovement::Burn {
                id,
                asset,
                amount,
                from: required(from, "burn source")?,
            }),
        }
    }
}

#[derive(Deserialize)]
struct FeeDto {
    asset: String,
    amount: String,
    payer: Option<String>,
}

impl FeeDto {
    fn convert(self, scope: &IndexScope) -> Result<NetworkFee, IndexError> {
        let chain = &scope.chain;
        Ok(NetworkFee {
            asset: AssetId {
                chain: chain.clone(),
                asset: self.asset,
            },
            amount: parse_decimal(&self.amount)?,
            payer: self.payer.map(|value| CanonicalAddress {
                scope: scope.clone(),
                value,
            }),
        })
    }
}

#[derive(Deserialize)]
struct EventDto {
    id: String,
    cursor: String,
    watch_ids: Vec<String>,
    previous_status: Option<StatusDto>,
    transaction: TransactionDto,
}

impl EventDto {
    fn convert(self) -> Result<ObservationEvent, IndexError> {
        let transaction = self.transaction.convert()?;
        let scope = &transaction.scope;
        Ok(ObservationEvent {
            id: EventId(self.id),
            cursor: EventCursor(parse_u64(&self.cursor, "event cursor")?),
            watch_ids: self.watch_ids.into_iter().map(WatchId).collect(),
            previous_status: self
                .previous_status
                .map(|status| status.convert(scope))
                .transpose()?,
            transaction,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct ErrorDto {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

pub(crate) fn parse_u64(value: &str, field: &str) -> Result<u64, IndexError> {
    value
        .parse()
        .map_err(|_| invalid_response(format!("{field} is not an unsigned decimal string")))
}

pub(crate) fn parse_decimal(value: &str) -> Result<Decimal, IndexError> {
    Decimal::from_str(value).map_err(|_| invalid_response("amount is not a canonical decimal"))
}

pub(crate) fn parse_hex(value: &str, field: &str) -> Result<Vec<u8>, IndexError> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if digits.is_empty() || digits.len() % 2 != 0 {
        return Err(invalid_response(format!(
            "{field} is not a whole hexadecimal byte sequence"
        )));
    }
    (0..digits.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&digits[index..index + 2], 16)
                .map_err(|_| invalid_response(format!("{field} contains invalid hexadecimal")))
        })
        .collect()
}

pub(crate) fn encode_hex(value: &[u8]) -> String {
    let mut output = String::with_capacity(2 + value.len() * 2);
    output.push_str("0x");
    for byte in value {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn required<T>(value: Option<T>, field: &str) -> Result<T, IndexError> {
    value.ok_or_else(|| invalid_response(format!("{field} is missing")))
}

pub(crate) fn invalid_response(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Other, message, false)
}

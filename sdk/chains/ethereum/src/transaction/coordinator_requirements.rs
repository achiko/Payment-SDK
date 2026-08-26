use std::collections::BTreeMap;

use super::{Draft, Preparation, PreparationError, chain_error};
use crate::{Address, ChainErrorKind, Wei};

type Contributions = (Wei, Vec<(usize, Wei)>);
type RequirementsByAsset = BTreeMap<(Address, RequiredAsset), Contributions>;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RequiredAsset {
    Native,
    Erc20(Address),
}

pub(super) struct Requirement {
    pub(super) source: Address,
    pub(super) asset: RequiredAsset,
    pub(super) amount: Wei,
    contributions: Vec<(usize, Wei)>,
}

impl Requirement {
    pub(super) fn first_index(&self) -> usize {
        self.contributions.first().map_or(0, |(index, _)| *index)
    }

    pub(super) fn failure_index(&self, balance: &Wei) -> usize {
        let mut cumulative = Wei::ZERO;
        for (index, amount) in &self.contributions {
            let Some(next) = cumulative.checked_add(amount) else {
                return *index;
            };
            cumulative = next;
            if &cumulative > balance {
                return *index;
            }
        }
        self.first_index()
    }
}

pub(super) struct Requirements {
    pub(super) values: Vec<Requirement>,
}

impl Requirements {
    pub(super) fn from_drafts(drafts: &[Draft<'_>]) -> Result<Self, PreparationError> {
        let mut values = RequirementsByAsset::new();
        for (index, draft) in drafts.iter().enumerate() {
            let request = &draft.preparation.request;
            let fee = draft
                .context
                .max_fee_per_gas
                .checked_mul_u64(draft.context.gas_limit)
                .ok_or_else(|| {
                    PreparationError::new(
                        index,
                        chain_error(
                            ChainErrorKind::FeeUnavailable,
                            "Ethereum aggregate maximum fee overflows U256",
                        ),
                    )
                })?;
            let native = if request.erc20_transfer().is_none() {
                fee.checked_add(&request.value()).ok_or_else(|| {
                    PreparationError::new(
                        index,
                        chain_error(
                            ChainErrorKind::InsufficientFunds,
                            "Ethereum aggregate native requirement overflows U256",
                        ),
                    )
                })?
            } else {
                fee
            };
            add(
                &mut values,
                request.from().clone(),
                RequiredAsset::Native,
                native,
                index,
            )?;
            if let Some((token, amount)) = request.erc20_transfer() {
                add(
                    &mut values,
                    request.from().clone(),
                    RequiredAsset::Erc20(token.clone()),
                    amount.clone(),
                    index,
                )?;
            }
        }
        let mut values = values
            .into_iter()
            .map(|((source, asset), (amount, contributions))| Requirement {
                source,
                asset,
                amount,
                contributions,
            })
            .collect::<Vec<_>>();
        values.sort_by_key(Requirement::first_index);
        Ok(Self { values })
    }
}

fn add(
    values: &mut RequirementsByAsset,
    source: Address,
    asset: RequiredAsset,
    amount: Wei,
    index: usize,
) -> Result<(), PreparationError> {
    let entry = values
        .entry((source, asset))
        .or_insert_with(|| (Wei::ZERO, Vec::new()));
    entry.0 = entry.0.checked_add(&amount).ok_or_else(|| {
        PreparationError::new(
            index,
            chain_error(
                ChainErrorKind::InsufficientFunds,
                "Ethereum aggregate asset requirement overflows U256",
            ),
        )
    })?;
    entry.1.push((index, amount));
    Ok(())
}

pub(super) fn senders(preparations: &[Preparation<'_>]) -> Vec<(Address, usize)> {
    let mut values = BTreeMap::new();
    for (index, preparation) in preparations.iter().enumerate() {
        values
            .entry(preparation.request.from().clone())
            .or_insert(index);
    }
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_by_key(|(_, index)| *index);
    values
}

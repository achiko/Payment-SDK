use alloy_primitives::{Address as AlloyAddress, U256};
// design-lint: allow owned-vocabulary -- Alloy's standard Solidity ABI macro import belongs to this Ethereum adapter
use alloy_sol_types::{SolCall, sol};

use crate::{Address, TransferIntent, TransferRequest, Wei};

// design-lint: allow owned-vocabulary -- Alloy's standard Solidity ABI declaration macro belongs to this Ethereum adapter
sol! {
    interface Erc20 {
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
        function transfer(address recipient, uint256 amount) external returns (bool);
    }
}

// design-lint: allow unclassified-free-function -- shared ERC-20 balanceOf ABI encoding adapts SDK addresses through Alloy without adding token-call behavior to Address
pub(crate) fn balance_of(address: &Address) -> Vec<u8> {
    Erc20::balanceOfCall {
        account: AlloyAddress::from(address.0),
    }
    .abi_encode()
}

pub(crate) fn decimals() -> Vec<u8> {
    Erc20::decimalsCall {}.abi_encode()
}

impl TransferRequest {
    pub(crate) fn input(&self) -> Vec<u8> {
        match self.intent() {
            TransferIntent::Native { .. } => Vec::new(),
            TransferIntent::Erc20 {
                recipient, amount, ..
            } => Erc20::transferCall {
                recipient: AlloyAddress::from(recipient.0),
                amount: U256::from_be_bytes(amount.0),
            }
            .abi_encode(),
        }
    }
}

// design-lint: allow unclassified-free-function -- strict ERC-20 balanceOf return decoding bridges Alloy ABI words and Wei for balance reads and token admission without adding token-call policy to amounts
pub(crate) fn decode_balance(word: &[u8]) -> Result<Wei, &'static str> {
    let value = Erc20::balanceOfCall::abi_decode_returns_validate(word)
        .map_err(|_| "invalid balanceOf ABI word")?;
    if Erc20::balanceOfCall::abi_encode_returns(&value) != word {
        return Err("non-canonical balanceOf ABI word");
    }
    Ok(Wei(value.to_be_bytes()))
}

// design-lint: allow unclassified-free-function -- strict ERC-20 decimals return decoding owns canonical uint8 ABI validation separately from configured-token admission policy
pub(crate) fn decode_decimals(word: &[u8]) -> Result<u8, &'static str> {
    let value = Erc20::decimalsCall::abi_decode_returns_validate(word)
        .map_err(|_| "invalid decimals ABI word")?;
    if Erc20::decimalsCall::abi_encode_returns(&value) != word {
        return Err("non-canonical decimals ABI word");
    }
    Ok(value)
}

// design-lint: allow unclassified-free-function -- strict ERC-20 transfer return decoding validates the canonical ABI boolean separately from preflight policy requiring true
pub(crate) fn decode_transfer(word: &[u8]) -> Result<bool, &'static str> {
    let value = Erc20::transferCall::abi_decode_returns_validate(word)
        .map_err(|_| "invalid transfer ABI word")?;
    if Erc20::transferCall::abi_encode_returns(&value) != word {
        return Err("non-canonical transfer ABI word");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_calls_match_canonical_erc20_abi() {
        assert_eq!(
            balance_of(&Address([0x11; 20])),
            [hex("70a08231"), vec![0; 12], vec![0x11; 20]].concat()
        );
        assert_eq!(decimals(), hex("313ce567"));
        assert_eq!(
            TransferRequest::erc20(
                Address([1; 20]),
                Address([2; 20]),
                Address([0x22; 20]),
                Wei::from_u128(7)
            )
            .input(),
            [
                hex("a9059cbb"),
                vec![0; 12],
                vec![0x22; 20],
                vec![0; 31],
                vec![7],
            ]
            .concat()
        );
    }

    #[test]
    fn transfer_input_preserves_all_amount_bits_and_native_input_is_empty() {
        for amount in [Wei::ZERO, Wei([255; 32])] {
            let request = TransferRequest::erc20(
                Address([1; 20]),
                Address([2; 20]),
                Address([0xab; 20]),
                amount.clone(),
            );
            assert_eq!(
                request.input(),
                [
                    hex("a9059cbb"),
                    vec![0; 12],
                    vec![0xab; 20],
                    amount.0.to_vec()
                ]
                .concat()
            );
        }
        let native =
            TransferRequest::native_atomic(Address([1; 20]), Address([2; 20]), Wei([255; 32]));
        assert!(native.input().is_empty());
    }

    #[test]
    fn typed_results_require_canonical_words() {
        let mut amount = [0_u8; 32];
        amount[31] = 7;
        assert_eq!(decode_balance(&amount), Ok(Wei::from_u128(7)));

        let mut decimals = [0_u8; 32];
        decimals[31] = 6;
        assert_eq!(decode_decimals(&decimals), Ok(6));

        let mut success = [0_u8; 32];
        success[31] = 1;
        assert_eq!(decode_transfer(&success), Ok(true));
        assert!(decode_transfer(&[]).is_err());
        assert!(decode_transfer(&[0; 31]).is_err());
        assert!(decode_transfer(&[2; 32]).is_err());
    }

    #[test]
    fn return_decoders_reject_truncation_and_trailing_words() {
        for length in [0, 31, 33, 64] {
            let bytes = vec![0; length];
            assert!(decode_balance(&bytes).is_err());
            assert!(decode_decimals(&bytes).is_err());
            assert!(decode_transfer(&bytes).is_err());
        }
        assert_eq!(decode_balance(&[255; 32]), Ok(Wei([255; 32])));
        assert_eq!(decode_transfer(&[0; 32]), Ok(false));
        let mut word = [0; 32];
        word[31] = 255;
        assert_eq!(decode_decimals(&word), Ok(255));
        assert!(decode_transfer(&word).is_err());
        word[0] = 1;
        assert!(decode_decimals(&word).is_err());
        assert!(decode_transfer(&word).is_err());
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).expect("test ABI hex must be UTF-8");
                u8::from_str_radix(pair, 16).expect("test ABI hex must be valid")
            })
            .collect()
    }
}

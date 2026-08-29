use solana_address::Address as NativeAddress;
use solana_instruction::Instruction;
use solana_message::Message as WireMessage;

use crate::{Address, BlockhashLifetime, Error, ErrorKind, Lamport};

use super::Memo;

/// One exact legacy System-transfer-plus-Memo message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message(WireMessage);

impl Message {
    pub fn native_transfer(
        source: &Address,
        destination: &Address,
        amount: Lamport,
        memo: Memo,
        lifetime: &BlockhashLifetime,
    ) -> Result<Self, Error> {
        if amount == Lamport::ZERO {
            return Err(Error::new(
                ErrorKind::InvalidBatch,
                "Solana transfer amount must be positive",
            ));
        }
        let source = NativeAddress::from(*source.as_bytes());
        let destination = NativeAddress::from(*destination.as_bytes());
        let transfer =
            solana_system_interface::instruction::transfer(&source, &destination, amount.atomic());
        let memo = Instruction {
            program_id: spl_memo_interface::v3::ID,
            accounts: Vec::new(),
            data: memo.to_string().into_bytes(),
        };
        Ok(Self(WireMessage::new_with_blockhash(
            &[transfer, memo],
            Some(&source),
            lifetime.blockhash(),
        )))
    }

    pub fn wire_bytes(&self) -> Result<Vec<u8>, Error> {
        bincode::serialize(&self.0)
            .map_err(|_| Error::new(ErrorKind::Signing, "Solana message encoding failed"))
    }

    #[must_use]
    pub const fn native(&self) -> &WireMessage {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use solana_hash::Hash;
    use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_ID};

    use super::*;

    #[test]
    fn builds_exact_system_then_memo_legacy_message() {
        let source = Address::from_bytes([7; 32]);
        let destination = Address::from_bytes([8; 32]);
        let lifetime = BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44);
        let memo = Memo::from_bytes([3; Memo::LENGTH]);
        let message = Message::native_transfer(
            &source,
            &destination,
            Lamport::from_atomic(17),
            memo,
            &lifetime,
        )
        .expect("native transfer message");
        let native = message.native();

        assert_eq!(native.header.num_required_signatures, 1);
        assert_eq!(native.account_keys[0].to_bytes(), *source.as_bytes());
        assert_eq!(native.instructions.len(), 2);
        let transfer = &native.instructions[0];
        assert_eq!(
            native.account_keys[usize::from(transfer.program_id_index)],
            SYSTEM_ID
        );
        assert_eq!(transfer.accounts, [0, 1]);
        assert_eq!(
            bincode::deserialize::<SystemInstruction>(&transfer.data)
                .expect("maintained System instruction"),
            SystemInstruction::Transfer { lamports: 17 }
        );
        let memo_instruction = &native.instructions[1];
        assert_eq!(
            native.account_keys[usize::from(memo_instruction.program_id_index)],
            spl_memo_interface::v3::ID
        );
        assert!(memo_instruction.accounts.is_empty());
        assert_eq!(memo_instruction.data, memo.to_string().as_bytes());
        assert_ne!(SYSTEM_ID, spl_memo_interface::v3::ID);
    }

    #[test]
    fn rejects_zero_before_message_construction() {
        let error = Message::native_transfer(
            &Address::from_bytes([7; 32]),
            &Address::from_bytes([8; 32]),
            Lamport::ZERO,
            Memo::from_bytes([3; Memo::LENGTH]),
            &BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44),
        )
        .expect_err("zero transfer");
        assert_eq!(error.kind(), ErrorKind::InvalidBatch);
    }
}

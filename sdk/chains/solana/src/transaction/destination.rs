use solana_address::Address as NativeAddress;

use crate::{Address, Error, ErrorKind};

/// Canonical Solana address admitted for an initial native SOL payment.
///
/// Construction is deliberately separate from general address parsing:
/// off-curve addresses remain valid protocol identities, but this payment
/// product cannot establish their spending mechanism.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeDestination(Address);

impl NativeDestination {
    #[must_use]
    pub fn address(&self) -> &Address {
        &self.0
    }
}

impl TryFrom<Address> for NativeDestination {
    type Error = Error;

    fn try_from(address: Address) -> Result<Self, Self::Error> {
        if !NativeAddress::from(*address.as_bytes()).is_on_curve() {
            return Err(Error::new(
                ErrorKind::UnsupportedDestination,
                "off-curve Solana addresses are unsupported native SOL destinations",
            ));
        }
        Ok(Self(address))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use solana_keypair::{Keypair, Signer};

    use super::*;

    fn keypair_address() -> Address {
        let keypair = Keypair::new_from_array([7; Keypair::SECRET_KEY_LENGTH]);
        Address::from_bytes(*keypair.pubkey().as_array())
    }

    fn program_derived_address() -> Address {
        "2fnQrngrQT4SeLcdToJAD96phoEjNL2man2kfRLCASVk"
            .parse()
            .expect("the maintained-library PDA fixture must parse")
    }

    #[test]
    fn admits_a_known_keypair_public_key() {
        let address = keypair_address();
        let destination = NativeDestination::try_from(address.clone())
            .expect("a keypair public key must be on-curve");

        assert_eq!(destination.address(), &address);
    }

    #[test]
    fn rejects_a_known_program_derived_address_without_reclassifying_its_syntax() {
        let address = program_derived_address();
        let canonical = address.to_string();

        assert_eq!(
            canonical
                .parse::<Address>()
                .expect("the off-curve address remains valid protocol syntax"),
            address
        );
        assert_eq!(
            NativeDestination::try_from(address)
                .expect_err("a program-derived address must fail the native-send gate")
                .kind(),
            ErrorKind::UnsupportedDestination
        );
    }

    #[test]
    fn rejects_off_curve_before_the_next_external_effect() {
        let rpc_calls = Cell::new(0_u32);
        let signer_calls = Cell::new(0_u32);

        let result = NativeDestination::try_from(program_derived_address()).map(|_| {
            rpc_calls.set(rpc_calls.get() + 1);
            signer_calls.set(signer_calls.get() + 1);
        });

        assert_eq!(
            result.expect_err("the local curve gate must stop preparation"),
            Error::new(
                ErrorKind::UnsupportedDestination,
                "off-curve Solana addresses are unsupported native SOL destinations",
            )
        );
        assert_eq!(rpc_calls.get(), 0);
        assert_eq!(signer_calls.get(), 0);
    }

    #[test]
    fn both_curve_classes_keep_canonical_address_round_trips() {
        for address in [keypair_address(), program_derived_address()] {
            let rendered = address.to_string();
            assert_eq!(
                rendered
                    .parse::<Address>()
                    .expect("curve class must not alter address parsing"),
                address
            );
        }
    }
}

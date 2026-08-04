//! Mock Bitcoin signing credential adapter.

use network::{IntoWallet, TxSigner};
use network_bitcoin::{
    Bitcoin, BitcoinNetwork, BitcoinTxEnvelope, BitcoinUnsignedTx, BitcoinWallet,
};
use primitives::{Address, Signature};
use signer::{Digest, PublicKey, Signer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinSigner<S> {
    credential: S,
    address: Address<Bitcoin>,
    network: BitcoinNetwork,
}

impl<S> BitcoinSigner<S>
where
    S: Signer,
{
    #[must_use]
    pub fn new(credential: S, network: BitcoinNetwork) -> Self {
        let prefix = match network {
            BitcoinNetwork::Mainnet => "bc1q",
            BitcoinNetwork::Testnet => "tb1q",
            BitcoinNetwork::Regtest => "bcrt1q",
        };
        let address = Address::new(format!(
            "{prefix}mock-bitcoin-{}",
            credential.public_key().as_str()
        ));

        Self {
            credential,
            address,
            network,
        }
    }

    #[must_use]
    pub const fn network(&self) -> BitcoinNetwork {
        self.network
    }

    #[must_use]
    pub const fn credential(&self) -> &S {
        &self.credential
    }
}

impl<S> Signer for BitcoinSigner<S>
where
    S: Signer,
{
    type Signature = Signature<Bitcoin>;
    type Error = S::Error;

    fn public_key(&self) -> &PublicKey {
        self.credential.public_key()
    }

    fn sign_digest(&self, digest: &Digest) -> Result<Self::Signature, Self::Error> {
        let _credential_signature = self.credential.sign_digest(digest)?;
        Ok(Signature::new("mock bitcoin hash signature"))
    }
}

impl<S> TxSigner<Bitcoin> for BitcoinSigner<S>
where
    S: Signer,
{
    fn address(&self) -> &Address<Bitcoin> {
        &self.address
    }

    fn sign(&self, transaction: BitcoinUnsignedTx) -> BitcoinTxEnvelope {
        BitcoinTxEnvelope::new(
            transaction,
            Signature::new("mock bitcoin transaction signature"),
        )
    }
}

impl<S> IntoWallet<Bitcoin> for BitcoinSigner<S>
where
    S: Signer + Send + Sync + 'static,
{
    type Wallet = BitcoinWallet;

    fn into_wallet(self) -> Self::Wallet {
        BitcoinWallet::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use network::FullSigner;
    use signer::SignerError;
    use signer_local::LocalSigner;

    fn assert_full_signer<S: FullSigner<Bitcoin>>(_signer: &S) {}

    #[test]
    fn adapts_a_local_credential_for_bitcoin() -> Result<(), SignerError> {
        let signer = BitcoinSigner::new(LocalSigner::generate()?, BitcoinNetwork::Regtest);

        assert_full_signer(&signer);
        assert_eq!(
            signer.sign_message("hello")?.as_str(),
            "mock bitcoin hash signature"
        );
        assert_eq!(signer.network(), BitcoinNetwork::Regtest);
        assert!(
            TxSigner::<Bitcoin>::address(&signer)
                .as_str()
                .starts_with("bcrt1qmock-bitcoin-")
        );
        Ok(())
    }
}

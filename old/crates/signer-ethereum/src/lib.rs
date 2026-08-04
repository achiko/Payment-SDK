//! Mock Ethereum signing credential adapter.

use network::{IntoWallet, TxSigner};
use network_ethereum::{Ethereum, EthereumTxEnvelope, EthereumUnsignedTx, EthereumWallet};
use primitives::{Address, Signature};
use signer::{Digest, PublicKey, Signer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthereumSigner<S> {
    credential: S,
    address: Address<Ethereum>,
    chain_id: Option<u64>,
}

impl<S> EthereumSigner<S>
where
    S: Signer,
{
    #[must_use]
    pub fn new(credential: S) -> Self {
        let address = Address::new(format!(
            "0xmock-ethereum-{}",
            credential.public_key().as_str()
        ));

        Self {
            credential,
            address,
            chain_id: None,
        }
    }

    #[must_use]
    pub fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    #[must_use]
    pub const fn chain_id(&self) -> Option<u64> {
        self.chain_id
    }

    #[must_use]
    pub const fn credential(&self) -> &S {
        &self.credential
    }
}

impl<S> Signer for EthereumSigner<S>
where
    S: Signer,
{
    type Signature = Signature<Ethereum>;
    type Error = S::Error;

    fn public_key(&self) -> &PublicKey {
        self.credential.public_key()
    }

    fn sign_digest(&self, digest: &Digest) -> Result<Self::Signature, Self::Error> {
        let _credential_signature = self.credential.sign_digest(digest)?;
        Ok(Signature::new("mock ethereum hash signature"))
    }
}

impl<S> TxSigner<Ethereum> for EthereumSigner<S>
where
    S: Signer,
{
    fn address(&self) -> &Address<Ethereum> {
        &self.address
    }

    fn sign(&self, transaction: EthereumUnsignedTx) -> EthereumTxEnvelope {
        EthereumTxEnvelope::new(
            transaction,
            Signature::new("mock ethereum transaction signature"),
        )
    }
}

impl<S> IntoWallet<Ethereum> for EthereumSigner<S>
where
    S: Signer + Send + Sync + 'static,
{
    type Wallet = EthereumWallet;

    fn into_wallet(self) -> Self::Wallet {
        EthereumWallet::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use network::FullSigner;
    use signer::SignerError;
    use signer_local::LocalSigner;

    fn assert_full_signer<S: FullSigner<Ethereum>>(_signer: &S) {}

    #[test]
    fn adapts_a_local_credential_for_ethereum() -> Result<(), SignerError> {
        let signer = EthereumSigner::new(LocalSigner::generate()?).with_chain_id(1);

        assert_full_signer(&signer);
        assert_eq!(
            signer.sign_message("hello")?.as_str(),
            "mock ethereum hash signature"
        );
        assert_eq!(signer.chain_id(), Some(1));
        assert!(
            TxSigner::<Ethereum>::address(&signer)
                .as_str()
                .starts_with("0xmock-ethereum-")
        );
        Ok(())
    }
}

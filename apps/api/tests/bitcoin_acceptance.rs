use super::*;

impl BitcoinNode {
    #[must_use]
    pub fn submitted_count(&self) -> usize {
        self.state
            .lock()
            .expect("RPC fixture lock must be healthy")
            .submitted
            .len()
    }

    #[must_use]
    pub fn submitted_owners(&self) -> Vec<String> {
        let state = self.state.lock().expect("RPC fixture lock must be healthy");
        let transaction = state
            .submitted
            .last()
            .expect("a submitted transaction must exist");
        transaction
            .input
            .iter()
            .map(|input| {
                let public = input
                    .witness
                    .iter()
                    .last()
                    .and_then(|bytes| PublicKey::from_slice(bytes).ok())
                    .and_then(|public| CompressedPublicKey::try_from(public).ok())
                    .expect("P2WPKH input must carry a compressed owner key");
                bitcoin::Address::p2wpkh(&public, Network::Regtest).to_string()
            })
            .collect()
    }
}

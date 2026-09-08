use crate::{Address, Lamport};

/// Complete native account facts observed at one contextual read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountSnapshot {
    owner: Address,
    lamports: Lamport,
    executable: bool,
    data: Vec<u8>,
}

impl AccountSnapshot {
    #[must_use]
    pub fn new(owner: Address, lamports: Lamport, executable: bool, data: Vec<u8>) -> Self {
        Self {
            owner,
            lamports,
            executable,
            data,
        }
    }

    #[must_use]
    pub fn owner(&self) -> &Address {
        &self.owner
    }

    #[must_use]
    pub const fn lamports(&self) -> Lamport {
        self.lamports
    }

    #[must_use]
    pub fn executable(&self) -> bool {
        self.executable
    }

    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_complete_structural_account_facts() {
        let owner = Address::from_bytes([23; 32]);
        let snapshot = AccountSnapshot::new(
            owner.clone(),
            Lamport::from_atomic(u64::MAX),
            true,
            vec![1; 17],
        );
        assert_eq!(snapshot.owner(), &owner);
        assert_eq!(snapshot.lamports().atomic(), u64::MAX);
        assert!(snapshot.executable());
        assert_eq!(snapshot.data().len(), 17);
    }
}

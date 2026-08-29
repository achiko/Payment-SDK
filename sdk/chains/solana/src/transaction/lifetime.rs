use solana_hash::Hash;

/// One recent blockhash and its exact last valid block height.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lifetime {
    blockhash: Hash,
    last_valid_block_height: u64,
}

impl Lifetime {
    #[must_use]
    pub fn new(blockhash: Hash, last_valid_block_height: u64) -> Self {
        Self {
            blockhash,
            last_valid_block_height,
        }
    }

    #[must_use]
    pub fn blockhash(&self) -> &Hash {
        &self.blockhash
    }

    #[must_use]
    pub fn last_valid_block_height(&self) -> u64 {
        self.last_valid_block_height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_blockhash_and_height_as_one_value() {
        let hash = Hash::new_from_array([11; 32]);
        let lifetime = Lifetime::new(hash.clone(), u64::MAX);
        assert_eq!(lifetime.blockhash(), &hash);
        assert_eq!(lifetime.last_valid_block_height(), u64::MAX);
    }
}

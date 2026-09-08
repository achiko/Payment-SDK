use crate::{Error, ErrorKind};

/// Remaining produced-block allowance for one bounded source read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    remaining: usize,
}

impl Budget {
    pub fn new(limit: usize) -> Result<Self, Error> {
        if limit == 0 {
            return Err(Error::new(
                ErrorKind::InvalidBudget,
                "Solana source budget must be greater than zero",
            ));
        }
        Ok(Self { remaining: limit })
    }

    #[must_use]
    pub fn remaining(self) -> usize {
        self.remaining
    }

    /// Claims no more than the remaining produced-block allowance.
    pub fn claim(&mut self, available: usize) -> usize {
        let claimed = available.min(self.remaining);
        self.remaining -= claimed;
        claimed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_claims_by_returned_blocks() {
        assert_eq!(Budget::new(0).unwrap_err().kind(), ErrorKind::InvalidBudget);
        let mut budget = Budget::new(3).unwrap();
        assert_eq!(budget.claim(2), 2);
        assert_eq!(budget.remaining(), 1);
        assert_eq!(budget.claim(4), 1);
        assert_eq!(budget.remaining(), 0);
    }
}

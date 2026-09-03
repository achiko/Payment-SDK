use wallets::MAX_TRANSFERS;

use crate::{Error, ErrorKind};

/// One validated, non-empty ordered Solana submission collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch<T> {
    items: Vec<T>,
}

impl<T> Batch<T> {
    pub fn new(items: Vec<T>) -> Result<Self, Error> {
        if items.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidBatch,
                "at least one transfer is required",
            ));
        }
        if items.len() > MAX_TRANSFERS {
            return Err(Error::new(
                ErrorKind::InvalidBatch,
                "at most 50 transfers are allowed",
            ));
        }
        Ok(Self { items })
    }

    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_only_the_shared_public_bounds() {
        assert_eq!(
            Batch::new(Vec::<u8>::new()).unwrap_err().kind(),
            ErrorKind::InvalidBatch
        );
        assert_eq!(Batch::new(vec![0; 1]).unwrap().as_slice().len(), 1);
        assert_eq!(
            Batch::new(vec![0; MAX_TRANSFERS]).unwrap().as_slice().len(),
            MAX_TRANSFERS
        );
        assert_eq!(
            Batch::new(vec![0; MAX_TRANSFERS + 1]).unwrap_err().kind(),
            ErrorKind::InvalidBatch
        );
    }
}

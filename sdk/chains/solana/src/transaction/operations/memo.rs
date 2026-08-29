/// Opaque 256-bit token carried by one Memo-v3 operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Memo([u8; Self::LENGTH]);

impl Memo {
    pub const LENGTH: usize = 32;

    #[must_use]
    pub fn from_bytes(bytes: [u8; Self::LENGTH]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Self::LENGTH] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_opaque_token_bytes() {
        let bytes = [19; Memo::LENGTH];
        assert_eq!(Memo::from_bytes(bytes).as_bytes(), &bytes);
    }
}

//! Small chain-tagged values shared below signer and network capabilities.

use std::{fmt, hash::Hash, marker::PhantomData};

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Address<C> {
    value: String,
    chain: PhantomData<fn() -> C>,
}

impl<C> Address<C> {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            chain: PhantomData,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl<C> fmt::Debug for Address<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Address").field(&self.value).finish()
    }
}

impl<C> fmt::Display for Address<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(formatter)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TxHash<C> {
    value: String,
    chain: PhantomData<fn() -> C>,
}

impl<C> TxHash<C> {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            chain: PhantomData,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl<C> fmt::Debug for TxHash<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("TxHash").field(&self.value).finish()
    }
}

impl<C> fmt::Display for TxHash<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(formatter)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Signature<C> {
    value: String,
    chain: PhantomData<fn() -> C>,
}

impl<C> Signature<C> {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            chain: PhantomData,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl<C> fmt::Debug for Signature<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Signature")
            .field(&self.value)
            .finish()
    }
}

impl<C> fmt::Display for Signature<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(formatter)
    }
}

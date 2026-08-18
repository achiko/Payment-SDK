use std::{collections::BTreeMap, error, fmt};

#[derive(Debug)]
pub struct CompositionError {
    message: String,
}

impl CompositionError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(super) fn configuration(error: impl fmt::Display) -> Self {
        Self::invalid(format!("invalid Payment Service configuration: {error}"))
    }

    pub(crate) fn adapter(name: &str, error: impl fmt::Display) -> Self {
        Self::invalid(format!("{name} initialization failed: {error}"))
    }
}

impl fmt::Display for CompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for CompositionError {}

/// Secret values captured by the executable before domain composition.
/// Debug output exposes only the number of injected values.
#[derive(Default)]
pub struct Secrets(BTreeMap<String, String>);

impl Secrets {
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.0.insert(name.into(), value.into());
    }

    pub(super) fn read(&self, name: &str) -> Result<String, CompositionError> {
        self.0.get(name).cloned().ok_or_else(|| {
            CompositionError::invalid(format!("required secret {name} was not provided"))
        })
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Secrets")
            .field("value_count", &self.0.len())
            .finish()
    }
}

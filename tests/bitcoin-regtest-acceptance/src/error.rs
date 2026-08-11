use std::{error::Error, fmt};

pub type Result<T> = std::result::Result<T, HarnessError>;

#[derive(Debug)]
pub struct HarnessError {
    message: String,
}

impl HarnessError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HarnessError {}

pub trait ResultContext<T> {
    fn context(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E> ResultContext<T> for std::result::Result<T, E>
where
    E: fmt::Display,
{
    fn context(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|error| HarnessError::new(format!("{}: {error}", context())))
    }
}

pub trait OptionContext<T> {
    fn context(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> OptionContext<T> for Option<T> {
    fn context(self, context: impl FnOnce() -> String) -> Result<T> {
        self.ok_or_else(|| HarnessError::new(context()))
    }
}

#[cfg(test)]
mod tests {
    use super::{OptionContext, ResultContext};

    #[test]
    fn result_context_preserves_stage_and_source_message() {
        let result: std::result::Result<(), &str> = Err("source failure");
        let error = result
            .context(|| "starting fixture".to_owned())
            .expect_err("contextualized result must fail");
        assert_eq!(error.to_string(), "starting fixture: source failure");
    }

    #[test]
    fn option_context_names_the_missing_value() {
        let error = None::<u8>
            .context(|| "missing field".to_owned())
            .expect_err("missing option must fail");
        assert_eq!(error.to_string(), "missing field");
    }
}

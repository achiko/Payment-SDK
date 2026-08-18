use std::error::Error;

pub(super) type AppError = Box<dyn Error + Send + Sync>;
pub(super) type AppResult<T> = Result<T, AppError>;

pub(super) fn failure(message: impl Into<String>) -> AppError {
    Box::new(Failure(message.into()))
}

#[derive(Debug)]
struct Failure(String);

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for Failure {}

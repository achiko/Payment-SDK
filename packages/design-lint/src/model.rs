use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Error,
}

impl Severity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        "error"
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub source: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    pub subject: String,
    pub message: String,
    pub help: String,
    pub location: Location,
}

impl Finding {
    pub fn error(rule: &'static str, subject: impl Into<String>, location: Location) -> Self {
        Self {
            rule,
            severity: Severity::Error,
            subject: subject.into(),
            message: String::new(),
            help: String::new(),
            location,
        }
    }

    #[must_use]
    pub const fn is_violation(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    pub rule: &'static str,
    pub severity: Severity,
    pub findings: usize,
}

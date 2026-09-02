pub(crate) mod adopted;
mod registry;
pub use registry::Registry;
pub(crate) mod production;
pub(crate) mod references;
mod repository;
mod rust;
pub(crate) mod syntax;

use crate::{Finding, Location, Policy, Result, Severity, source::Workspace};

type CheckFn = fn(&Workspace, &Policy) -> Result<Vec<Finding>>;

pub trait Rule {
    fn id(&self) -> &'static str;
    fn severity(&self) -> Severity;
    fn check(&self, workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>>;
}

struct Check {
    id: &'static str,
    severity: Severity,
    run: CheckFn,
}

impl Rule for Check {
    fn id(&self) -> &'static str {
        self.id
    }
    fn severity(&self) -> Severity {
        self.severity
    }
    fn check(&self, workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>> {
        (self.run)(workspace, policy)
    }
}

fn original() -> Vec<Check> {
    repository::checks()
        .into_iter()
        .chain(rust::checks())
        .map(|(id, run)| Check {
            id,
            severity: Severity::Error,
            run,
        })
        .collect()
}

fn finding(
    rule: &'static str,
    subject: impl Into<String>,
    location: Location,
    message: impl Into<String>,
    help: impl Into<String>,
) -> Finding {
    let mut finding = Finding::error(rule, subject, location);
    finding.message = message.into();
    finding.help = help.into();
    finding
}

#[cfg(test)]
#[path = "test.rs"]
mod tests;

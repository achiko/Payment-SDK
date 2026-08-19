mod production;
mod repository;
mod rust;

use crate::{Finding, Location, Policy, Result, Severity, source::Workspace};

type CheckFn = fn(&Workspace, &Policy) -> Result<Vec<Finding>>;

pub trait Rule {
    fn id(&self) -> &'static str;
    fn severity(&self) -> Severity;
    fn check(&self, workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>>;
}

struct Check {
    id: &'static str,
    run: CheckFn,
}

impl Rule for Check {
    fn id(&self) -> &'static str {
        self.id
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn check(&self, workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>> {
        (self.run)(workspace, policy)
    }
}

pub fn standard(_policy: &Policy) -> Vec<Box<dyn Rule>> {
    repository::checks()
        .into_iter()
        .chain(rust::checks())
        .map(|(id, run)| Box::new(Check { id, run }) as Box<dyn Rule>)
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

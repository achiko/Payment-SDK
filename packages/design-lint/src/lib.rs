//! Small, policy-driven checks for the architectural rules this repository relies on.

mod error;
mod model;
mod policy;
mod report;
mod rule;
mod source;

pub use error::{LintError, Result};
pub use model::{Finding, Location, Severity, Summary};
pub use policy::Policy;
pub use report::{Cases, Diagnostic, Markdown, Reporter};
use source::Workspace;
use std::path::PathBuf;

/// Runs the focused repository and Rust API rules.
pub struct Linter {
    policy: Policy,
}

impl Linter {
    #[must_use]
    pub fn standard_with_policy(policy: Policy) -> Self {
        Self { policy }
    }

    pub fn run(&self, paths: Vec<PathBuf>, reporter: &mut dyn Reporter) -> Result<Vec<Summary>> {
        let workspace = Workspace::load(paths, &self.policy.source)?;
        reporter.begin(&workspace)?;
        let rules = rule::standard(&self.policy);
        let mut summaries = Vec::with_capacity(rules.len());
        for rule in rules {
            let findings = rule.check(&workspace, &self.policy)?;
            let count = findings
                .iter()
                .filter(|finding| finding.is_violation())
                .count();
            for finding in &findings {
                reporter.finding(finding)?;
            }
            summaries.push(Summary {
                rule: rule.id(),
                severity: rule.severity(),
                findings: count,
            });
        }
        reporter.finish(&summaries)?;
        Ok(summaries)
    }
}

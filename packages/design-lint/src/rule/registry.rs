use super::{Rule, adopted, original};
use crate::{LintError, Policy, Result};
use std::collections::BTreeSet;

/// Ordered collection of checks with unique stable identifiers.
pub struct Registry {
    rules: Vec<Box<dyn Rule>>,
}
impl Registry {
    pub fn new(rules: Vec<Box<dyn Rule>>) -> Result<Self> {
        let mut ids = BTreeSet::new();
        for rule in &rules {
            if rule.id().is_empty() || !ids.insert(rule.id()) {
                return Err(LintError::configuration(format!(
                    "empty or duplicate rule ID `{}`",
                    rule.id()
                )));
            }
        }
        Ok(Self { rules })
    }

    /// Original SDK checks plus explicitly enabled adopted checks.
    pub fn standard(policy: &Policy) -> Result<Self> {
        policy.validate()?;
        Self::new(
            original()
                .into_iter()
                .chain(
                    adopted::checks()
                        .into_iter()
                        .filter(|check| policy.rules.enabled.iter().any(|id| id == check.id)),
                )
                .map(|check| Box::new(check) as Box<dyn Rule>)
                .collect(),
        )
    }

    /// Full review selection, retaining the original severity of every rule.
    pub fn all() -> Result<Self> {
        Self::new(
            original()
                .into_iter()
                .chain(adopted::checks())
                .map(|check| Box::new(check) as Box<dyn Rule>)
                .collect(),
        )
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Rule> {
        self.rules.iter().map(Box::as_ref)
    }
}

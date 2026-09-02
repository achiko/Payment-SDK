use crate::{Result, model::Finding, policy::Policy, source::Workspace};

mod marker;
mod namespace;
mod wrapper;

#[cfg(test)]
mod tests;

/// Reviews structure that adds navigation without a contract, boundary, or invariant.
pub(crate) const ID: &str = "ceremonial-structure";

pub(crate) fn check(workspace: &Workspace, policy: &Policy) -> Result<Vec<Finding>> {
    let mut findings = namespace::findings(workspace, policy);
    findings.extend(marker::findings(workspace));
    findings.extend(wrapper::findings(workspace));
    findings.sort_by(|left, right| {
        left.location
            .path
            .cmp(&right.location.path)
            .then(left.location.line.cmp(&right.location.line))
            .then(left.subject.cmp(&right.subject))
    });
    Ok(findings)
}

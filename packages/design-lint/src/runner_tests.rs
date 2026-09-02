use crate::{
    Cases, Finding, LintError, Linter, Policy, Registry, Reporter, Result, Review, Rule, Severity,
    Summary, source::Workspace, test_support::Fixture,
};
use std::fs;

struct Probe {
    id: &'static str,
    severity: Severity,
    fail: bool,
}
impl Rule for Probe {
    fn id(&self) -> &'static str {
        self.id
    }
    fn severity(&self) -> Severity {
        self.severity
    }
    fn check(&self, workspace: &Workspace, _: &Policy) -> Result<Vec<Finding>> {
        if self.fail {
            return Err(LintError::configuration("injected rule failure"));
        }
        let mut finding = Finding::error(
            self.id,
            "subject",
            workspace.sources()[0].location(proc_macro2::Span::call_site()),
        );
        finding.severity = self.severity;
        finding.review = Some(Review::default());
        Ok(vec![finding])
    }
}
#[derive(Default)]
struct Collected {
    findings: Vec<Finding>,
    began: bool,
}
impl Reporter for Collected {
    fn begin(&mut self, _: &Workspace) -> Result<()> {
        self.began = true;
        Ok(())
    }
    fn finding(&mut self, finding: &Finding) -> Result<()> {
        self.findings.push(finding.clone());
        Ok(())
    }
    fn finish(&mut self, _: &[Summary]) -> Result<()> {
        Ok(())
    }
}
#[test]
fn registry_rejects_duplicates_and_keeps_sdk_rules() {
    let rules: Vec<Box<dyn Rule>> = (0..2)
        .map(|_| {
            Box::new(Probe {
                id: "same",
                severity: Severity::Error,
                fail: false,
            }) as Box<dyn Rule>
        })
        .collect();
    assert!(Registry::new(rules).is_err());
    let policy = Policy::default();
    assert_eq!(Registry::standard(&policy).unwrap().iter().count(), 11);
    assert_eq!(Registry::all().unwrap().iter().count(), 28);
    let mut policy = policy;
    policy.rules.enabled = vec!["missing".into()];
    assert!(Registry::standard(&policy).is_err());
    policy.rules.enabled = vec!["single-use-free-function".into()];
    let registry = Registry::standard(&policy).unwrap();
    assert_eq!(registry.iter().count(), 12);
    assert_eq!(
        registry
            .iter()
            .filter(|rule| rule.severity() == Severity::Error)
            .count(),
        11
    );
}
#[test]
fn review_evidence_never_hides_errors_or_warnings() {
    let fixture = Fixture::new(&[("src/lib.rs", "const VALUE: u8 = 1;")]);
    for severity in [Severity::Error, Severity::Warning] {
        let registry = Registry::new(vec![Box::new(Probe {
            id: "probe",
            severity,
            fail: false,
        })])
        .unwrap();
        let mut reporter = Collected::default();
        let summary = Linter::new(Policy::default(), registry)
            .run(vec![fixture.path().to_owned()], &mut reporter)
            .unwrap();
        assert!(reporter.began);
        assert_eq!(summary[0].findings, 1);
        assert_eq!(summary[0].severity, severity);
        assert!(reporter.findings[0].is_violation());
    }
}
#[test]
fn failed_analysis_leaves_existing_cases_untouched() {
    let fixture = Fixture::new(&[
        ("src/lib.rs", "const VALUE: u8 = 1;"),
        ("lint/errors/prior.md", "prior evidence"),
        ("lint/check/notes.md", "user note"),
        ("lint/errors/.gitkeep", ""),
    ]);
    let registry = Registry::new(vec![Box::new(Probe {
        id: "fail",
        severity: Severity::Error,
        fail: true,
    })])
    .unwrap();
    let mut cases = Cases::with_output(fixture.path().join("lint"), Vec::new());
    assert!(
        Linter::new(Policy::default(), registry)
            .run(vec![fixture.path().join("src")], &mut cases)
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(fixture.path().join("lint/errors/prior.md")).unwrap(),
        "prior evidence"
    );
    assert_eq!(
        fs::read_to_string(fixture.path().join("lint/check/notes.md")).unwrap(),
        "user note"
    );
    assert!(fixture.path().join("lint/errors/.gitkeep").exists());
}
#[test]
fn only_reasoned_comments_suppress_adopted_findings() {
    for (prefix, count) in [
        (
            "// design-lint: allow struct-noun-naming -- intentional command vocabulary",
            0,
        ),
        ("// design-lint: allow struct-noun-naming -- ", 1),
        (
            "const NOTE: &str = \"// design-lint: allow struct-noun-naming -- literal\";",
            1,
        ),
    ] {
        let text = format!("{prefix}\nstruct Selected {{ id: u8 }}");
        let fixture = Fixture::new(&[("src/lib.rs", &text)]);
        let mut policy = Policy::default();
        policy.rules.enabled.push("struct-noun-naming".into());
        let mut reporter = Collected::default();
        Linter::standard_with_policy(policy)
            .unwrap()
            .run(vec![fixture.path().to_owned()], &mut reporter)
            .unwrap();
        assert_eq!(
            reporter
                .findings
                .iter()
                .filter(|finding| finding.rule == "struct-noun-naming")
                .count(),
            count,
            "{prefix}"
        );
    }
}
#[test]
fn boundary_selectors_have_stable_scope_and_reject_invalid_policy() {
    let fixture = Fixture::new(&[
        ("Cargo.toml", "[workspace]\nmembers=['apps/api']"),
        (
            "apps/api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'",
        ),
        (
            "apps/api/src/config.rs",
            "fn load() { std::env::var(\"TOKEN\"); }",
        ),
    ]);
    let selector = crate::SourceSelector {
        package: Some("api".into()),
        path: Some("apps/api/src/config.rs".into()),
    };
    for path in [
        fixture.path().to_owned(),
        fixture.path().join("apps/api/src/config.rs"),
    ] {
        let workspace = Workspace::load(vec![path], &Policy::default().source).unwrap();
        assert!(selector.matches(&workspace.sources()[0], &workspace));
    }
    for text in [
        "[[boundaries.environment]]",
        "[[boundaries.process]]\npath='../escape'",
        "[[dependency.layers]]\nname='a'\nmay_depend_on=['unknown']",
        "[rules]\nenabled=['missing']",
    ] {
        let policy: Policy = toml::from_str(text).unwrap();
        assert!(policy.validate().is_err(), "{text}");
    }
}

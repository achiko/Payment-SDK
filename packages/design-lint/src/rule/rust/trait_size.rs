use syn::{ItemTrait, TraitItem, visit::Visit};

use crate::{
    Result,
    model::{Finding, Severity},
    rule::Rule,
    source::{Source, Workspace, requires_test},
};

const MAXIMUM_METHODS: usize = 3;

/// Rejects traits that expose more than three functions.
pub struct TraitMethodCount;

impl Rule for TraitMethodCount {
    fn id(&self) -> &'static str {
        "trait-method-count"
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, workspace: &Workspace) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        for source in workspace.production() {
            Collector {
                source,
                findings: &mut findings,
            }
            .visit_file(&source.syntax);
        }
        Ok(findings)
    }
}

struct Collector<'a> {
    source: &'a Source,
    findings: &'a mut Vec<Finding>,
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        if !requires_test(&item.attrs) {
            let methods = item
                .items
                .iter()
                .filter(|trait_item| matches!(trait_item, TraitItem::Fn(_)))
                .count();
            if methods > MAXIMUM_METHODS {
                let mut finding = Finding::error(
                    "trait-method-count",
                    item.ident.to_string(),
                    self.source.location(item.ident.span()),
                );
                finding.message = format!(
                    "trait `{}` declares {methods} functions; the maximum is {MAXIMUM_METHODS}",
                    item.ident
                );
                finding.help = "split the trait into one-to-three-function capabilities owned by their callers"
                    .to_owned();
                self.findings.push(finding);
            }
        }
        syn::visit::visit_item_trait(self, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    fn findings(source: &str) -> Vec<Finding> {
        let root = std::env::temp_dir().join(format!(
            "design-lint-trait-size-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='trait-size-fixture'\nversion='0.0.0'\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), source).unwrap();
        let workspace = Workspace::load([PathBuf::from(&root)]).unwrap();
        let findings = TraitMethodCount.check(&workspace).unwrap();
        fs::remove_dir_all(root).unwrap();
        findings
    }

    #[test]
    fn accepts_three_functions() {
        assert!(findings("trait Port { fn a(&self); fn b(&self); fn c(&self); }").is_empty());
    }

    #[test]
    fn rejects_four_functions() {
        let findings =
            findings("trait Port { fn a(&self); fn b(&self); fn c(&self); fn d(&self); }");
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("declares 4 functions"));
    }
}

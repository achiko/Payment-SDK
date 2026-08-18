use syn::{ItemStruct, visit::Visit};

use crate::{
    Result,
    model::{Finding, Severity},
    rule::Rule,
    source::{Source, Workspace, requires_test},
};

/// Rejects structs that carry no state.
pub struct EmptyStruct {
    _private: (),
}

impl EmptyStruct {
    /// Creates the empty-struct rule.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for EmptyStruct {
    fn default() -> Self {
        Self::new()
    }
}

impl Rule for EmptyStruct {
    fn id(&self) -> &'static str {
        "empty-struct"
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
    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        if item.fields.is_empty() && !requires_test(&item.attrs) {
            let mut finding = Finding::error(
                "empty-struct",
                item.ident.to_string(),
                self.source.location(item.ident.span()),
            );
            finding.message = format!(
                "struct `{}` has no fields and therefore carries no state",
                item.ident
            );
            finding.help = "use a module or free function for namespacing, a trait for behavior, an enum for identity, or give the type meaningful state"
                .to_owned();
            self.findings.push(finding);
        }
        syn::visit::visit_item_struct(self, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    fn findings(source: &str) -> Vec<Finding> {
        let root = std::env::temp_dir().join(format!(
            "design-lint-empty-struct-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("fixture source directory is creatable");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='empty-struct-fixture'\nversion='0.0.0'\n",
        )
        .expect("fixture manifest is writable");
        fs::write(root.join("src/lib.rs"), source).expect("fixture source is writable");
        let workspace = Workspace::load([root.join("src/lib.rs")]).expect("fixture parses");
        let result = EmptyStruct::new().check(&workspace).expect("rule runs");
        fs::remove_dir_all(root).expect("fixture is removable");
        result
    }

    #[test]
    fn rejects_unit_and_braced_empty_structs() {
        let result =
            findings("pub struct Unit; pub struct Braced {} struct Stateful { value: u8 }");
        assert_eq!(
            result
                .iter()
                .map(|finding| finding.subject.as_str())
                .collect::<Vec<_>>(),
            ["Unit", "Braced"]
        );
    }

    #[test]
    fn ignores_test_only_structs() {
        assert!(findings("#[cfg(test)] struct Fixture;").is_empty());
    }
}

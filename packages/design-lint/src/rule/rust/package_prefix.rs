use syn::{ItemEnum, ItemStruct, ItemTrait, ItemType, visit::Visit};

use crate::{
    Result,
    model::{Finding, Severity},
    rule::Rule,
    source::{Source, Workspace, requires_test, snake_case},
};

/// Rejects type declarations that repeat their Cargo package namespace.
pub struct PackagePrefix;

impl Rule for PackagePrefix {
    fn id(&self) -> &'static str {
        "package-name-prefix"
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

impl Collector<'_> {
    fn declaration(&mut self, identifier: &syn::Ident, attributes: &[syn::Attribute]) {
        if requires_test(attributes) {
            return;
        }
        let package_words = snake_case(&self.source.package.replace('-', "_"));
        let declared_words = snake_case(&identifier.to_string());
        let repeats_package = package_words
            .split('_')
            .filter(|word| !matches!(*word, "chain" | "crate" | "sdk"))
            .any(|word| declared_words.split('_').next() == Some(word));
        if !repeats_package {
            return;
        }
        let mut finding = Finding::error(
            "package-name-prefix",
            identifier.to_string(),
            self.source.location(identifier.span()),
        );
        finding.message = format!(
            "type `{identifier}` repeats its owning package `{}`",
            self.source.package
        );
        finding.help = "remove the package prefix from the internal declaration; retain a prefixed public re-export alias only when compatibility requires it"
            .to_owned();
        self.findings.push(finding);
    }
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        self.declaration(&item.ident, &item.attrs);
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
        self.declaration(&item.ident, &item.attrs);
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        self.declaration(&item.ident, &item.attrs);
        syn::visit::visit_item_trait(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast ItemType) {
        self.declaration(&item.ident, &item.attrs);
        syn::visit::visit_item_type(self, item);
    }
}

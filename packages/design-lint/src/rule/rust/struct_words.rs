use syn::{ItemStruct, visit::Visit};

use crate::{
    Result,
    model::{Finding, Severity},
    rule::Rule,
    source::{Source, Workspace, requires_test},
};

const MAXIMUM_WORDS: usize = 2;

/// Rejects struct names containing more than two semantic words.
pub struct StructWordCount {
    _private: (),
}

impl StructWordCount {
    /// Creates the struct-word-count rule.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for StructWordCount {
    fn default() -> Self {
        Self::new()
    }
}

impl Rule for StructWordCount {
    fn id(&self) -> &'static str {
        "struct-word-count"
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
        if !requires_test(&item.attrs) {
            let words = words(&item.ident.to_string());
            if words.len() > MAXIMUM_WORDS {
                let mut finding = Finding::error(
                    "struct-word-count",
                    item.ident.to_string(),
                    self.source.location(item.ident.span()),
                );
                finding.message = format!(
                    "struct `{}` contains {} words; the maximum is {MAXIMUM_WORDS}",
                    item.ident,
                    words.len()
                );
                finding.help =
                    "remove redundant package or module context and choose a one- or two-word noun"
                        .to_owned();
                self.findings.push(finding);
            }
        }
        syn::visit::visit_item_struct(self, item);
    }
}

fn words(name: &str) -> Vec<String> {
    let characters = name.trim_start_matches("r#").chars().collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut start = 0;
    for index in 0..characters.len() {
        let character = characters[index];
        if !character.is_ascii_alphanumeric() {
            push_word(&characters[start..index], &mut result);
            start = index + 1;
            continue;
        }
        if index == start {
            continue;
        }
        let previous = characters[index - 1];
        let next = characters.get(index + 1).copied();
        let boundary = character.is_ascii_uppercase()
            && (previous.is_ascii_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_ascii_uppercase()
                    && next.is_some_and(|next| next.is_ascii_lowercase())));
        if boundary {
            push_word(&characters[start..index], &mut result);
            start = index;
        }
    }
    push_word(&characters[start..], &mut result);
    if result.last().is_some_and(|word| {
        word.chars().all(|character| character.is_ascii_digit())
            || word.strip_prefix('v').is_some_and(|version| {
                !version.is_empty() && version.chars().all(|character| character.is_ascii_digit())
            })
    }) {
        result.pop();
    }
    result
}

fn push_word(characters: &[char], output: &mut Vec<String>) {
    let word = characters
        .iter()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if !word.is_empty() {
        output.push(word);
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
            "design-lint-struct-words-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).expect("fixture source directory is creatable");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='struct-words-fixture'\nversion='0.0.0'\n",
        )
        .expect("fixture manifest is writable");
        fs::write(root.join("src/lib.rs"), source).expect("fixture source is writable");
        let workspace = Workspace::load([root.join("src/lib.rs")]).expect("fixture parses");
        let result = StructWordCount::new().check(&workspace).expect("rule runs");
        fs::remove_dir_all(root).expect("fixture is removable");
        result
    }

    #[test]
    fn rejects_three_words_and_accepts_two() {
        let result = findings(
            "struct BlockInterpreter { value: u8 } struct BitcoinBlockInterpreter { value: u8 }",
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].subject, "BitcoinBlockInterpreter");
    }

    #[test]
    fn treats_acronyms_and_digits_as_words() {
        assert_eq!(words("HTTPServerV2"), ["http", "server"]);
        assert_eq!(words("RecordV1"), ["record"]);
        assert_eq!(words("Version2Record"), ["version2", "record"]);
    }

    #[test]
    fn ignores_only_a_trailing_version_suffix() {
        let result = findings(
            "struct HTTPServerV2 { value: u8 } \
             struct BitcoinRPCClientV1 { value: u8 } \
             struct ParserV1State { value: u8 }",
        );
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].subject, "BitcoinRPCClientV1");
        assert_eq!(result[1].subject, "ParserV1State");
    }
}

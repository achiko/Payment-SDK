use std::collections::BTreeSet;

use syn::spanned::Spanned;

use crate::{
    Result,
    model::{Finding, Severity},
    policy::VocabularyPolicy,
    rule::Rule,
    source::Workspace,
};

/// Keeps implementation-specific vocabulary inside its owning paths.
pub struct OwnedVocabulary {
    policy: VocabularyPolicy,
}

impl OwnedVocabulary {
    /// Creates the rule from repository-owned terms and path exceptions.
    #[must_use]
    pub fn new(policy: VocabularyPolicy) -> Self {
        Self { policy }
    }
}

impl Rule for OwnedVocabulary {
    fn id(&self) -> &'static str {
        "owned-vocabulary"
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn check(&self, workspace: &Workspace) -> Result<Vec<Finding>> {
        let forbidden = self
            .policy
            .forbidden
            .iter()
            .map(|word| word.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        let mut findings = Vec::new();
        for source in workspace.sources() {
            let path = source.path.to_string_lossy().replace('\\', "/");
            if self
                .policy
                .allowed_paths
                .iter()
                .any(|allowed| path.contains(allowed))
            {
                continue;
            }
            let present = semantic_words(&source.text)
                .into_iter()
                .filter(|word| forbidden.contains(word))
                .collect::<BTreeSet<_>>();
            for word in present {
                let mut finding = Finding::error(
                    self.id(),
                    word.clone(),
                    source.location(source.syntax.span()),
                );
                finding.message = format!(
                    "chain-specific vocabulary `{word}` appears outside its owning chain or application"
                );
                finding.help = "move the chain-specific type or behavior into its concrete chain crate and inject it from an application"
                    .to_owned();
                findings.push(finding);
            }
        }
        Ok(findings)
    }
}

fn semantic_words(text: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(text.len());
    let mut previous_lowercase = false;
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && previous_lowercase {
                normalized.push(' ');
            }
            normalized.push(character.to_ascii_lowercase());
            previous_lowercase = character.is_ascii_lowercase();
        } else {
            normalized.push(' ');
            previous_lowercase = false;
        }
    }
    normalized.split_whitespace().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use crate::{Rule, VocabularyPolicy, source::Workspace};

    use super::{OwnedVocabulary, semantic_words};

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "design-lint-owned-vocabulary-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        for directory in ["tests/system", "sdk/indexing/src"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        root
    }

    #[test]
    fn separates_identifiers_strings_and_abbreviations_without_substring_matches() {
        let words = semantic_words("BitcoinIndex btc ethereum ETH method");
        assert!(words.iter().any(|word| word == "bitcoin"));
        assert!(words.iter().any(|word| word == "btc"));
        assert!(words.iter().any(|word| word == "ethereum"));
        assert!(words.iter().any(|word| word == "eth"));
        assert!(!words.iter().any(|word| word == "method" && word == "eth"));
    }

    #[test]
    fn acceptance_path_does_not_exempt_generic_sdk_source() {
        let root = fixture();
        fs::write(
            root.join("tests/system/chain.rs"),
            "struct BitcoinFixture;\n",
        )
        .unwrap();
        fs::write(
            root.join("sdk/indexing/src/lib.rs"),
            "struct BitcoinFixture;\n",
        )
        .unwrap();
        let policy = VocabularyPolicy {
            forbidden: vec!["bitcoin".into()],
            allowed_paths: vec!["tests/".into()],
        };

        let findings = OwnedVocabulary::new(policy)
            .check(&Workspace::load([root.clone()]).unwrap())
            .unwrap();

        assert_eq!(findings.len(), 1);
        assert!(
            findings[0]
                .location
                .path
                .ends_with("sdk/indexing/src/lib.rs")
        );
        fs::remove_dir_all(root).unwrap();
    }
}

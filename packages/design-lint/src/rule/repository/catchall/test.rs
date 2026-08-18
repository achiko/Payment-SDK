use super::SourcePath;
use crate::{Rule, source::Workspace};
use std::{fs, path::PathBuf};

fn fixture(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "design-lint-catch-all-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    root
}

#[test]
fn rejects_generic_rust_files_and_directories_below_any_source_root() {
    let root = fixture("rust");
    for path in [
        "common.rs",
        "portable/helpers/clock.rs",
        "portable/network.rs",
    ] {
        let path = root.join("src").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "const VALUE: usize = 1;\n").unwrap();
    }

    let workspace = Workspace::load([root.clone()]).unwrap();
    let findings = SourcePath.check(&workspace).unwrap();
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding
                .location
                .path
                .strip_prefix(root.join("src"))
                .unwrap()
                .to_owned())
            .collect::<Vec<_>>(),
        [
            PathBuf::from("common.rs"),
            PathBuf::from("portable/helpers")
        ]
    );
    assert!(
        findings
            .iter()
            .all(|finding| finding.rule == "catch-all-source-path")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_every_reserved_name_without_project_paths() {
    let root = fixture("names");
    for name in [
        "common", "core", "helper", "helpers", "misc", "shared", "util", "utils",
    ] {
        let source = root.join("src").join(format!("{name}.rs"));
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(source, "const VALUE: usize = 1;\n").unwrap();
    }
    let findings = SourcePath
        .check(&Workspace::load([root.clone()]).unwrap())
        .unwrap();
    assert_eq!(findings.len(), 8);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn accepts_shared_file_when_capability_and_sibling_supply_context() {
    let root = fixture("contextual-shared");
    for path in ["platform/memory/shared.rs", "platform/memory/mapping.rs"] {
        let path = root.join("src").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "const VALUE: usize = 1;\n").unwrap();
    }

    let findings = SourcePath
        .check(&Workspace::load([root.clone()]).unwrap())
        .unwrap();
    assert!(findings.is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_shared_file_without_a_scoped_capability_or_precise_sibling() {
    let root = fixture("ambiguous-shared");
    for path in [
        "shared.rs",
        "memory/shared.rs",
        "platform/storage/shared.rs",
    ] {
        let path = root.join("src").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "const VALUE: usize = 1;\n").unwrap();
    }

    let findings = SourcePath
        .check(&Workspace::load([root.clone()]).unwrap())
        .unwrap();
    assert_eq!(findings.len(), 3);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn contextual_exception_does_not_apply_to_other_catch_all_names() {
    let root = fixture("contextual-common");
    for path in ["platform/memory/common.rs", "platform/memory/mapping.rs"] {
        let path = root.join("src").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "const VALUE: usize = 1;\n").unwrap();
    }

    let findings = SourcePath
        .check(&Workspace::load([root.clone()]).unwrap())
        .unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].subject, "common");
    fs::remove_dir_all(root).unwrap();
}

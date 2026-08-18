use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use proc_macro2::Span;
use serde::Deserialize;

use crate::{LintError, Location, Result, policy::Source as SourcePolicy};

pub struct SourceFile {
    pub path: PathBuf,
    pub text: String,
    pub syntax: syn::File,
}

pub struct Package {
    pub name: String,
    pub root: PathBuf,
    pub dependencies: Vec<String>,
}

pub struct Workspace {
    pub roots: Vec<PathBuf>,
    pub sources: Vec<SourceFile>,
    pub packages: Vec<Package>,
    pub directories: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Manifest {
    package: Option<ManifestPackage>,
    #[serde(default)]
    dependencies: toml::Table,
    #[serde(rename = "dev-dependencies", default)]
    dev_dependencies: toml::Table,
    #[serde(rename = "build-dependencies", default)]
    build_dependencies: toml::Table,
}

#[derive(Deserialize)]
struct ManifestPackage {
    name: String,
}

impl Workspace {
    pub fn load(paths: Vec<PathBuf>, policy: &SourcePolicy) -> Result<Self> {
        let mut files = BTreeSet::new();
        let mut directories = BTreeSet::new();
        let mut roots = Vec::new();
        for path in paths {
            let canonical =
                fs::canonicalize(&path).map_err(|error| LintError::io("open", &path, error))?;
            let root = if canonical.is_file() {
                canonical.parent().unwrap_or(&canonical).to_owned()
            } else {
                canonical.clone()
            };
            roots.push(root);
            collect(&canonical, policy, &mut files, &mut directories)?;
        }
        let mut sources = Vec::new();
        let mut packages = Vec::new();
        for path in files {
            match path.file_name().and_then(|name| name.to_str()) {
                Some("Cargo.toml") => {
                    if let Some(package) = package(&path)? {
                        packages.push(package);
                    }
                }
                _ if path.extension().and_then(|value| value.to_str()) == Some("rs") => {
                    let text = fs::read_to_string(&path)
                        .map_err(|error| LintError::io("read", &path, error))?;
                    let syntax = syn::parse_file(&text).map_err(|source| LintError::Parse {
                        path: path.clone(),
                        source,
                    })?;
                    sources.push(SourceFile { path, text, syntax });
                }
                _ => {}
            }
        }
        let ignored_roots = packages
            .iter()
            .filter(|package| policy.self_packages.contains(&package.name))
            .map(|package| package.root.clone())
            .collect::<Vec<_>>();
        sources.retain(|source| {
            !ignored_roots
                .iter()
                .any(|root| source.path.starts_with(root))
                && !test_path(&source.path)
        });
        Ok(Self {
            roots,
            sources,
            packages,
            directories: directories.into_iter().collect(),
        })
    }
}

impl SourceFile {
    pub fn location(&self, span: Span) -> Location {
        let start = span.start();
        let source = self
            .text
            .lines()
            .nth(start.line.saturating_sub(1))
            .unwrap_or_default()
            .trim()
            .to_owned();
        Location {
            path: self.path.clone(),
            line: start.line.max(1),
            column: start.column + 1,
            source,
        }
    }

    pub fn suppressed(&self, rule: &str, line: usize) -> bool {
        let start = line.saturating_sub(3);
        self.text
            .lines()
            .skip(start)
            .take(line - start)
            .any(|text| {
                let marker = format!("design-lint: allow {rule} -- ");
                text.contains(&marker)
                    && !text
                        .split_once(&marker)
                        .is_none_or(|(_, reason)| reason.trim().is_empty())
            })
    }
}

fn collect(
    path: &Path,
    policy: &SourcePolicy,
    files: &mut BTreeSet<PathBuf>,
    directories: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if path.is_file() {
        files.insert(path.to_owned());
        return Ok(());
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if policy
        .ignored_directories
        .iter()
        .any(|ignored| ignored == name)
    {
        return Ok(());
    }
    directories.insert(path.to_owned());
    let entries = fs::read_dir(path).map_err(|error| LintError::io("read", path, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| LintError::io("read", path, error))?;
        collect(&entry.path(), policy, files, directories)?;
    }
    Ok(())
}

fn package(path: &Path) -> Result<Option<Package>> {
    let text = fs::read_to_string(path).map_err(|error| LintError::io("read", path, error))?;
    let manifest: Manifest = toml::from_str(&text).map_err(|error| {
        LintError::configuration(format!("failed to parse {}: {error}", path.display()))
    })?;
    let Some(package) = manifest.package else {
        return Ok(None);
    };
    let dependencies = manifest
        .dependencies
        .keys()
        .chain(manifest.dev_dependencies.keys())
        .chain(manifest.build_dependencies.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Some(Package {
        name: package.name,
        root: path.parent().unwrap_or(Path::new(".")).to_owned(),
        dependencies,
    }))
}

pub fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn test_path(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == "tests")
        || path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "test" || name.ends_with("_test"))
}

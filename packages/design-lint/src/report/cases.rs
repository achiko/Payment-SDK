use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

use crate::{Finding, LintError, Result, Summary, report::Reporter};

/// Refreshes persistent Markdown findings under `errors` and `check`.
pub struct Cases<Output = io::Stderr> {
    root: PathBuf,
    count: usize,
    output: Output,
}

impl Cases<io::Stderr> {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self::with_output(root, io::stderr())
    }
}

impl<Output> Cases<Output> {
    pub fn with_output(root: PathBuf, output: Output) -> Self {
        Self {
            root,
            count: 0,
            output,
        }
    }
    pub fn into_inner(self) -> Output {
        self.output
    }
}

impl<Output: Write> Reporter for Cases<Output> {
    fn begin(&mut self, _workspace: &crate::source::Workspace) -> Result<()> {
        self.count = 0;
        for queue in [self.root.join("errors"), self.root.join("check")] {
            fs::create_dir_all(&queue).map_err(|error| LintError::io("create", &queue, error))?;
            for entry in
                fs::read_dir(&queue).map_err(|error| LintError::io("read", &queue, error))?
            {
                let path = entry
                    .map_err(|error| LintError::io("read", &queue, error))?
                    .path();
                if path.file_name().and_then(|name| name.to_str()) != Some(".gitkeep")
                    && path.is_file()
                {
                    fs::remove_file(&path).map_err(|error| LintError::io("clear", &path, error))?;
                }
            }
        }
        Ok(())
    }

    fn finding(&mut self, finding: &Finding) -> Result<()> {
        if !finding.is_violation() {
            return Ok(());
        }
        let name = safe(&format!(
            "{}_{}_{}",
            finding.rule, finding.subject, self.count
        ));
        let path = self.root.join("errors").join(format!("{name}.md"));
        let text = format!(
            "# `{}`\n\n- [ ] Resolved\n- Rule: `{}`\n- Source: `{}:{}:{}`\n\n{}\n\nHelp: {}\n\n```rust\n{}\n```\n",
            finding.subject,
            finding.rule,
            finding.location.path.display(),
            finding.location.line,
            finding.location.column,
            finding.message,
            finding.help,
            finding.location.source
        );
        fs::write(&path, text).map_err(|error| LintError::io("write", &path, error))?;
        self.count += 1;
        Ok(())
    }

    fn finish(&mut self, _summaries: &[Summary]) -> Result<()> {
        writeln!(
            self.output,
            "wrote {} case(s) to {}",
            self.count,
            self.root.display()
        )
        .map_err(|error| LintError::report("case summary", error))
    }
}

fn safe(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
}

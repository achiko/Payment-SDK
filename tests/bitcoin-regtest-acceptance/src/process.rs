use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::{process::Command, time};

use crate::error::{HarnessError, Result, ResultContext};

const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub log_path: PathBuf,
}

impl ProcessSpec {
    #[must_use]
    pub fn sanitized_command(&self) -> String {
        let mut rendered = self.program.display().to_string();
        for argument in &self.args {
            rendered.push(' ');
            rendered.push_str(&argument.to_string_lossy());
        }
        rendered
    }
}

#[derive(Default, Debug)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn register(&mut self, secret: impl Into<String>) {
        let secret = secret.into();
        if secret.len() >= 6 && !self.secrets.iter().any(|existing| existing == &secret) {
            self.secrets.push(secret);
            self.secrets
                .sort_by_key(|value| std::cmp::Reverse(value.len()));
        }
    }

    #[must_use]
    pub fn sanitize(&self, value: &str) -> String {
        self.secrets
            .iter()
            .fold(value.to_owned(), |output, secret| {
                output.replace(secret, "[REDACTED]")
            })
    }
}

struct ManagedProcess {
    name: String,
    child: tokio::process::Child,
    log_path: PathBuf,
}

impl ManagedProcess {
    async fn interrupt(&mut self) -> Result<()> {
        if self
            .child
            .try_wait()
            .context(|| format!("polling {} process", self.name))?
            .is_some()
        {
            return Ok(());
        }
        let process_id = self
            .child
            .id()
            .ok_or_else(|| HarnessError::new(format!("{} process has no identifier", self.name)))?;
        let status = Command::new("/bin/kill")
            .arg("-INT")
            .arg(process_id.to_string())
            .status()
            .await
            .context(|| format!("sending interrupt to {} process", self.name))?;
        if !status.success() {
            return Err(HarnessError::new(format!(
                "interrupt command for {} exited with {status}",
                self.name
            )));
        }
        Ok(())
    }

    async fn wait_or_kill(&mut self) -> Result<()> {
        self.wait_or_kill_with_timeout(PROCESS_STOP_TIMEOUT).await
    }

    async fn wait_or_kill_with_timeout(&mut self, timeout: Duration) -> Result<()> {
        match time::timeout(timeout, self.child.wait()).await {
            Ok(result) => {
                result.context(|| format!("waiting for {} process", self.name))?;
                Ok(())
            }
            Err(_) => {
                self.child
                    .kill()
                    .await
                    .context(|| format!("force-stopping {} after timeout", self.name))?;
                Ok(())
            }
        }
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[derive(Default)]
pub struct ProcessSupervisor {
    active: Vec<ManagedProcess>,
    logs: Vec<(String, PathBuf)>,
}

impl ProcessSupervisor {
    pub async fn start(&mut self, spec: ProcessSpec) -> Result<()> {
        if self.active.iter().any(|process| process.name == spec.name) {
            return Err(HarnessError::new(format!(
                "process {} is already active",
                spec.name
            )));
        }
        if let Some(parent) = spec.log_path.parent() {
            fs::create_dir_all(parent)
                .context(|| format!("creating process-log directory {}", parent.display()))?;
        }
        let stdout = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&spec.log_path)
            .context(|| format!("opening {} log", spec.name))?;
        let stderr = stdout
            .try_clone()
            .context(|| format!("cloning {} log handle", spec.name))?;
        let sanitized_command = spec.sanitized_command();
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .env_clear()
            .envs(spec.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
        let child = command
            .spawn()
            .context(|| format!("starting {} with {sanitized_command}", spec.name))?;
        self.active.push(ManagedProcess {
            name: spec.name,
            child,
            log_path: spec.log_path,
        });
        Ok(())
    }

    pub fn ensure_running(&mut self, name: &str) -> Result<()> {
        let process = self
            .active
            .iter_mut()
            .find(|process| process.name == name)
            .ok_or_else(|| HarnessError::new(format!("process {name} is not managed")))?;
        if let Some(status) = process
            .child
            .try_wait()
            .context(|| format!("polling {name} process"))?
        {
            return Err(HarnessError::new(format!(
                "process {name} exited unexpectedly with {status}"
            )));
        }
        Ok(())
    }

    pub async fn stop(&mut self, name: &str) -> Result<()> {
        let position = self
            .active
            .iter()
            .position(|process| process.name == name)
            .ok_or_else(|| HarnessError::new(format!("process {name} is not managed")))?;
        let mut process = self.active.remove(position);
        let interrupt = process.interrupt().await;
        let wait = process.wait_or_kill().await;
        self.logs
            .push((process.name.clone(), process.log_path.clone()));
        interrupt.and(wait)
    }

    pub async fn wait_after_external_stop(&mut self, name: &str) -> Result<()> {
        let position = self
            .active
            .iter()
            .position(|process| process.name == name)
            .ok_or_else(|| HarnessError::new(format!("process {name} is not managed")))?;
        let mut process = self.active.remove(position);
        process.wait_or_kill().await?;
        self.logs
            .push((process.name.clone(), process.log_path.clone()));
        Ok(())
    }

    pub async fn stop_all(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        while let Some(mut process) = self.active.pop() {
            if let Err(error) = process.interrupt().await {
                failures.push(error.to_string());
            }
            if let Err(error) = process.wait_or_kill().await {
                failures.push(error.to_string());
            }
            self.logs
                .push((process.name.clone(), process.log_path.clone()));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(HarnessError::new(format!(
                "one or more fixture processes did not stop cleanly: {}",
                failures.join("; ")
            )))
        }
    }

    pub fn write_sanitized_logs(&self, directory: &Path, redactor: &Redactor) -> Result<()> {
        fs::create_dir_all(directory)
            .context(|| format!("creating sanitized-log directory {}", directory.display()))?;
        for (index, (name, source)) in self.logs.iter().enumerate() {
            let contents = fs::read_to_string(source)
                .context(|| format!("reading private {} process log", name))?;
            let destination = directory.join(format!("{index:02}-{name}.log"));
            fs::write(&destination, redactor.sanitize(&contents))
                .context(|| format!("writing sanitized log {}", destination.display()))?;
        }
        Ok(())
    }
}

impl Drop for ProcessSupervisor {
    fn drop(&mut self) {
        for process in &mut self.active {
            let _ = process.child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, path::PathBuf, process::Stdio, time::Duration};

    use tempfile::TempDir;
    use tokio::process::Command;

    use super::{ManagedProcess, ProcessSpec, ProcessSupervisor, Redactor};

    #[test]
    fn redactor_replaces_longest_registered_values_without_exposing_them() {
        let mut redactor = Redactor::default();
        redactor.register("short-secret");
        redactor.register("short-secret-with-suffix");
        let output = redactor.sanitize("short-secret-with-suffix and short-secret");
        assert_eq!(output, "[REDACTED] and [REDACTED]");
    }

    #[test]
    fn sanitized_command_never_includes_environment_values() {
        let spec = ProcessSpec {
            name: "service".to_owned(),
            program: PathBuf::from("/tmp/service"),
            args: vec![OsString::from("serve")],
            environment: vec![(
                OsString::from("BEARER_TOKEN"),
                OsString::from("private-value"),
            )],
            log_path: PathBuf::from("/tmp/service.log"),
        };
        let rendered = spec.sanitized_command();
        assert!(rendered.contains("serve"));
        assert!(!rendered.contains("private-value"));
        assert!(!rendered.contains("BEARER_TOKEN"));
    }

    #[tokio::test]
    async fn process_configuration_clears_parent_environment_and_keeps_declared_values()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let log_path = directory.path().join("environment.log");
        let mut supervisor = ProcessSupervisor::default();
        supervisor
            .start(ProcessSpec {
                name: "environment".to_owned(),
                program: PathBuf::from("/usr/bin/env"),
                args: Vec::new(),
                environment: vec![(
                    OsString::from("ACCEPTANCE_MARKER"),
                    OsString::from("declared"),
                )],
                log_path: log_path.clone(),
            })
            .await?;
        supervisor.wait_after_external_stop("environment").await?;

        assert_eq!(
            fs::read_to_string(log_path)?,
            "ACCEPTANCE_MARKER=declared\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn timeout_force_stops_a_process_without_waiting_for_its_natural_exit()
    -> Result<(), Box<dyn std::error::Error>> {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut process = ManagedProcess {
            name: "timeout".to_owned(),
            child,
            log_path: PathBuf::from("/dev/null"),
        };
        process
            .wait_or_kill_with_timeout(Duration::from_millis(10))
            .await?;

        assert!(process.child.try_wait()?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_stops_processes_in_reverse_start_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let mut supervisor = ProcessSupervisor::default();
        for name in ["core", "indexer"] {
            supervisor
                .start(ProcessSpec {
                    name: name.to_owned(),
                    program: PathBuf::from("/bin/sleep"),
                    args: vec![OsString::from("30")],
                    environment: Vec::new(),
                    log_path: directory.path().join(format!("{name}.log")),
                })
                .await?;
        }
        supervisor.stop_all().await?;

        assert_eq!(
            supervisor
                .logs
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["indexer", "core"]
        );
        assert!(supervisor.active.is_empty());
        Ok(())
    }
}

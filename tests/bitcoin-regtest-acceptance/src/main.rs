mod cli;
mod error;
mod harness;
mod process;
mod report;
mod scenarios;

use std::{fs, process::ExitCode};

use clap::Parser;
use uuid::Uuid;

use crate::{
    cli::Cli,
    error::{HarnessError, Result, ResultContext},
    harness::{FixtureConfig, unix_timestamp, verify_core_binary},
    report::{CaseStatus, RunSummary, write_reports},
};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bitcoin-regtest-acceptance failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let bitcoind_version = verify_core_binary(&cli.bitcoind, "bitcoind").await?;
    let bitcoin_cli_version = verify_core_binary(&cli.bitcoin_cli, "bitcoin-cli").await?;
    let run_id = format!("btc31-{}", Uuid::now_v7());
    let run_artifacts = cli.artifacts_dir.join(&run_id);
    fs::create_dir_all(&run_artifacts)
        .context(|| format!("creating run artifact path {}", run_artifacts.display()))?;
    let started_at_unix = unix_timestamp()?;
    let mut cases = Vec::new();

    for profile in cli.mode.concrete().into_iter().flatten() {
        for scenario in cli.scenario.concrete().into_iter().flatten() {
            let case_artifacts = run_artifacts
                .join(profile.canonical_name())
                .join(scenario.canonical_name());
            fs::create_dir_all(&case_artifacts)
                .context(|| format!("creating case artifact path {}", case_artifacts.display()))?;
            eprintln!(
                "running {} [{}] against isolated Bitcoin Core regtest",
                scenario.canonical_name(),
                profile.canonical_name()
            );
            let config = FixtureConfig {
                bitcoind: cli.bitcoind.clone(),
                bitcoin_cli: cli.bitcoin_cli.clone(),
                profile,
                scenario,
                case_artifacts,
                keep_workdir: cli.keep_workdir,
            };
            let case = tokio::select! {
                result = scenarios::execute(config) => result,
                signal = tokio::signal::ctrl_c() => {
                    signal.context(|| "waiting for interrupt signal".to_owned())?;
                    return Err(HarnessError::new(
                        "acceptance run interrupted; active fixture processes were terminated",
                    ));
                }
            };
            eprintln!(
                "{} [{}]: {}",
                scenario.canonical_name(),
                profile.canonical_name(),
                match case.status {
                    CaseStatus::Passed => "passed",
                    CaseStatus::Failed => "failed",
                }
            );
            cases.push(case);
        }
    }

    let summary = RunSummary {
        schema_version: 1,
        run_id,
        started_at_unix,
        finished_at_unix: unix_timestamp()?,
        bitcoind_version,
        bitcoin_cli_version,
        cases,
    };
    write_reports(&run_artifacts, &summary)?;
    eprintln!(
        "sanitized acceptance artifacts: {}",
        run_artifacts.display()
    );
    if summary.passed() {
        Ok(())
    } else {
        Err(HarnessError::new(
            "one or more Bitcoin regtest acceptance cases failed; inspect summary.json",
        ))
    }
}

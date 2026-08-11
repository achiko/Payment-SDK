use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    cli::{AuthenticationProfile, ScenarioSelection},
    error::{Result, ResultContext},
};

#[derive(Debug, Serialize)]
pub struct RunSummary {
    pub schema_version: u32,
    pub run_id: String,
    pub started_at_unix: u64,
    pub finished_at_unix: u64,
    pub bitcoind_version: String,
    pub bitcoin_cli_version: String,
    pub cases: Vec<CaseResult>,
}

impl RunSummary {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.cases
            .iter()
            .all(|case| case.status == CaseStatus::Passed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Passed,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct CaseResult {
    pub scenario: ScenarioSelection,
    pub authentication_mode: AuthenticationProfile,
    pub risk: &'static str,
    pub status: CaseStatus,
    pub duration_millis: u128,
    pub assertions: Vec<String>,
    pub evidence: BTreeMap<String, String>,
    pub error: Option<String>,
    pub retained_workdir: Option<PathBuf>,
}

pub fn write_reports(directory: &Path, summary: &RunSummary) -> Result<()> {
    fs::create_dir_all(directory)
        .context(|| format!("creating report directory {}", directory.display()))?;
    let json = serde_json::to_vec_pretty(summary)
        .context(|| "serializing acceptance summary".to_owned())?;
    fs::write(directory.join("summary.json"), json)
        .context(|| "writing summary.json".to_owned())?;
    fs::write(directory.join("junit.xml"), junit(summary))
        .context(|| "writing junit.xml".to_owned())?;
    Ok(())
}

fn junit(summary: &RunSummary) -> String {
    let failures = summary
        .cases
        .iter()
        .filter(|case| case.status == CaseStatus::Failed)
        .count();
    let duration = summary
        .cases
        .iter()
        .map(|case| case.duration_millis)
        .sum::<u128>() as f64
        / 1_000.0;
    let mut output = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"bitcoin-regtest-acceptance\" tests=\"{}\" failures=\"{failures}\" time=\"{duration:.3}\">\n",
        summary.cases.len()
    );
    for case in &summary.cases {
        let name = format!(
            "{}[{}]",
            case.scenario.canonical_name(),
            case.authentication_mode.canonical_name()
        );
        let seconds = case.duration_millis as f64 / 1_000.0;
        output.push_str(&format!(
            "  <testcase classname=\"bitcoin.regtest.{}\" name=\"{}\" time=\"{seconds:.3}\">",
            escape_xml(case.risk),
            escape_xml(&name)
        ));
        if let Some(error) = &case.error {
            output.push_str(&format!(
                "<failure message=\"{}\">{}</failure>",
                escape_xml(error),
                escape_xml(error)
            ));
        }
        output.push_str("</testcase>\n");
    }
    output.push_str("</testsuite>\n");
    output
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use tempfile::TempDir;

    use super::{CaseResult, CaseStatus, RunSummary, junit, write_reports};
    use crate::cli::{AuthenticationProfile, ScenarioSelection};

    #[test]
    fn junit_escapes_failure_text_and_counts_failures() {
        let summary = RunSummary {
            schema_version: 1,
            run_id: "run".to_owned(),
            started_at_unix: 1,
            finished_at_unix: 2,
            bitcoind_version: "31.1.0".to_owned(),
            bitcoin_cli_version: "31.1.0".to_owned(),
            cases: vec![CaseResult {
                scenario: ScenarioSelection::Signing,
                authentication_mode: AuthenticationProfile::Strict,
                risk: "P0",
                status: CaseStatus::Failed,
                duration_millis: 250,
                assertions: Vec::new(),
                evidence: BTreeMap::new(),
                error: Some("bad <value> & secret\"".to_owned()),
                retained_workdir: None,
            }],
        };
        let xml = junit(&summary);
        assert!(xml.contains("failures=\"1\""));
        assert!(xml.contains("bad &lt;value&gt; &amp; secret&quot;"));
        assert!(!xml.contains("bad <value>"));
    }

    #[test]
    fn report_writer_emits_parseable_json_and_junit() -> Result<(), Box<dyn std::error::Error>> {
        let directory = TempDir::new()?;
        let summary = RunSummary {
            schema_version: 1,
            run_id: "report-run".to_owned(),
            started_at_unix: 10,
            finished_at_unix: 12,
            bitcoind_version: "31.1.0".to_owned(),
            bitcoin_cli_version: "31.1.0".to_owned(),
            cases: vec![CaseResult {
                scenario: ScenarioSelection::Reservation,
                authentication_mode: AuthenticationProfile::GlobalTrusted,
                risk: "P0",
                status: CaseStatus::Passed,
                duration_millis: 1_250,
                assertions: vec!["one owner".to_owned()],
                evidence: BTreeMap::new(),
                error: None,
                retained_workdir: None,
            }],
        };
        write_reports(directory.path(), &summary)?;

        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("summary.json"))?)?;
        assert_eq!(
            json.get("run_id").and_then(serde_json::Value::as_str),
            Some("report-run")
        );
        assert_eq!(
            json.get("cases")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        let xml = fs::read_to_string(directory.path().join("junit.xml"))?;
        assert!(xml.contains("tests=\"1\" failures=\"0\""));
        Ok(())
    }
}

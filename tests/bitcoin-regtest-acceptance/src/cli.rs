use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationProfile {
    Strict,
    GlobalTrusted,
    All,
}

impl AuthenticationProfile {
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::GlobalTrusted => "global_trusted",
            Self::All => "all",
        }
    }

    #[must_use]
    pub const fn strict_value(self) -> &'static str {
        match self {
            Self::Strict => "true",
            Self::GlobalTrusted => "false",
            Self::All => unreachable_profile(),
        }
    }

    #[must_use]
    pub const fn concrete(self) -> [Option<Self>; 2] {
        match self {
            Self::Strict => [Some(Self::Strict), None],
            Self::GlobalTrusted => [Some(Self::GlobalTrusted), None],
            Self::All => [Some(Self::Strict), Some(Self::GlobalTrusted)],
        }
    }
}

const fn unreachable_profile() -> ! {
    panic!("aggregate authentication profile has no runtime boolean")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ScenarioSelection {
    Signing,
    RestartReplay,
    Reorg,
    Reservation,
    All,
}

impl ScenarioSelection {
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::Signing => "signing",
            Self::RestartReplay => "restart-replay",
            Self::Reorg => "reorg",
            Self::Reservation => "reservation",
            Self::All => "all",
        }
    }

    #[must_use]
    pub const fn risk(self) -> &'static str {
        match self {
            Self::RestartReplay => "P1",
            Self::Signing | Self::Reorg | Self::Reservation => "P0",
            Self::All => "aggregate",
        }
    }

    #[must_use]
    pub const fn concrete(self) -> [Option<Self>; 4] {
        match self {
            Self::Signing => [Some(Self::Signing), None, None, None],
            Self::RestartReplay => [Some(Self::RestartReplay), None, None, None],
            Self::Reorg => [Some(Self::Reorg), None, None, None],
            Self::Reservation => [Some(Self::Reservation), None, None, None],
            Self::All => [
                Some(Self::Signing),
                Some(Self::RestartReplay),
                Some(Self::Reorg),
                Some(Self::Reservation),
            ],
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "bitcoin-regtest-acceptance",
    version,
    about = "Opt-in black-box Bitcoin Core regtest acceptance runner"
)]
pub struct Cli {
    /// Explicit Bitcoin Core 31.1 daemon binary. The runner never downloads Core.
    #[arg(long)]
    pub bitcoind: PathBuf,

    /// Explicit Bitcoin Core 31.1 CLI binary paired with `bitcoind`.
    #[arg(long)]
    pub bitcoin_cli: PathBuf,

    /// Service-authentication profile. `all` runs every case in isolated strict and global-trusted fixtures.
    #[arg(long, value_enum, default_value_t = AuthenticationProfile::All)]
    pub mode: AuthenticationProfile,

    /// Acceptance scenario. `all` runs four isolated risk-weighted cases.
    #[arg(long, value_enum, default_value_t = ScenarioSelection::All)]
    pub scenario: ScenarioSelection,

    /// Sanitized JSON, JUnit, and process-log destination.
    #[arg(long, default_value = "target/bitcoin-regtest-acceptance")]
    pub artifacts_dir: PathBuf,

    /// Retain the private temporary fixture, including credentials and signed bytes, for local diagnosis.
    #[arg(long, default_value_t = false)]
    pub keep_workdir: bool,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{AuthenticationProfile, Cli, ScenarioSelection};

    #[test]
    fn cli_defaults_to_the_complete_matrix() {
        let cli = Cli::try_parse_from([
            "runner",
            "--bitcoind",
            "/tmp/bitcoind",
            "--bitcoin-cli",
            "/tmp/bitcoin-cli",
        ])
        .expect("valid required paths must parse");
        assert_eq!(cli.mode, AuthenticationProfile::All);
        assert_eq!(cli.scenario, ScenarioSelection::All);
        assert!(!cli.keep_workdir);
    }

    #[test]
    fn aggregate_selections_expand_deterministically() {
        assert_eq!(
            AuthenticationProfile::All.concrete(),
            [
                Some(AuthenticationProfile::Strict),
                Some(AuthenticationProfile::GlobalTrusted)
            ]
        );
        assert_eq!(
            ScenarioSelection::All
                .concrete()
                .into_iter()
                .flatten()
                .count(),
            4
        );
    }
}

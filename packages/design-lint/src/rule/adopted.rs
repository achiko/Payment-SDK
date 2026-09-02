mod accessor;
mod blocking;
mod boolean;
mod catchall;
mod ceremony;
mod command;
pub(super) mod contract;
mod duplicate;
mod environment;
mod function;
mod model;
mod naming;
mod nesting;
mod object;
mod receiver;
mod result;
mod single;
mod state;

use super::{Check, Severity};

pub(super) fn checks() -> Vec<Check> {
    vec![
        Check {
            id: receiver::ID,
            severity: Severity::Error,
            run: receiver::check,
        },
        Check {
            id: catchall::ID,
            severity: Severity::Error,
            run: catchall::check,
        },
        Check {
            id: naming::ID,
            severity: Severity::Error,
            run: naming::check,
        },
        Check {
            id: function::ID,
            severity: Severity::Error,
            run: function::check,
        },
        Check {
            id: single::ID,
            severity: Severity::Warning,
            run: single::check,
        },
        Check {
            id: nesting::ID,
            severity: Severity::Warning,
            run: nesting::check,
        },
        Check {
            id: environment::ID,
            severity: Severity::Error,
            run: environment::check,
        },
        Check {
            id: command::ID,
            severity: Severity::Error,
            run: command::check,
        },
        Check {
            id: result::ID,
            severity: Severity::Error,
            run: result::check,
        },
        Check {
            id: blocking::ID,
            severity: Severity::Error,
            run: blocking::check,
        },
        Check {
            id: boolean::ID,
            severity: Severity::Warning,
            run: boolean::check,
        },
        Check {
            id: state::ID,
            severity: Severity::Warning,
            run: state::check,
        },
        Check {
            id: object::ID,
            severity: Severity::Warning,
            run: object::check,
        },
        Check {
            id: accessor::ID,
            severity: Severity::Warning,
            run: accessor::check,
        },
        Check {
            id: duplicate::ID,
            severity: Severity::Error,
            run: duplicate::check,
        },
        Check {
            id: model::ID,
            severity: Severity::Warning,
            run: model::check,
        },
        Check {
            id: ceremony::ID,
            severity: Severity::Warning,
            run: ceremony::check,
        },
    ]
}

pub(crate) fn contains(id: &str) -> bool {
    checks().iter().any(|rule| rule.id == id)
}

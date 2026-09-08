//! Runtime shutdown ordering enforced at the application boundary.

use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Running,
    NotReady,
    AdmissionStopped,
    RegistrarClosed,
    HandlersDrained,
    SubmissionsDrained,
    IndexingStopped,
    Joined,
}

pub(crate) struct ShutdownOrder(Stage);

impl ShutdownOrder {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self(Stage::Running)
    }

    pub(crate) fn not_ready(&mut self) -> Result<(), io::Error> {
        self.advance(Stage::Running, Stage::NotReady)
    }

    pub(crate) fn stop_admission(&mut self) -> Result<(), io::Error> {
        self.advance(Stage::NotReady, Stage::AdmissionStopped)
    }

    pub(crate) fn close_registrar(&mut self) -> Result<(), io::Error> {
        self.advance(Stage::AdmissionStopped, Stage::RegistrarClosed)
    }

    pub(crate) fn drain_handlers(&mut self) -> Result<(), io::Error> {
        self.advance(Stage::RegistrarClosed, Stage::HandlersDrained)
    }

    pub(crate) fn drain_submissions(&mut self) -> Result<(), io::Error> {
        self.advance(Stage::HandlersDrained, Stage::SubmissionsDrained)
    }

    pub(crate) fn stop_indexing(&mut self) -> Result<(), io::Error> {
        self.advance(Stage::SubmissionsDrained, Stage::IndexingStopped)
    }

    pub(crate) fn join(&mut self) -> Result<(), io::Error> {
        self.advance(Stage::IndexingStopped, Stage::Joined)
    }

    fn advance(&mut self, expected: Stage, next: Stage) -> Result<(), io::Error> {
        if self.0 != expected {
            return Err(io::Error::other("application shutdown order was violated"));
        }
        self.0 = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_complete_evidence_preserving_order() {
        let mut order = ShutdownOrder::new();
        order.not_ready().expect("not ready");
        order.stop_admission().expect("admission stopped");
        order.close_registrar().expect("registrar closed");
        order.drain_handlers().expect("handlers drained");
        order.drain_submissions().expect("submissions drained");
        order.stop_indexing().expect("indexing stopped");
        order.join().expect("tasks joined");
        assert_eq!(order.0, Stage::Joined);
    }

    #[test]
    fn rejects_every_skipped_or_repeated_stage() {
        let mut skipped = ShutdownOrder::new();
        assert!(skipped.stop_admission().is_err());

        let mut repeated = ShutdownOrder::new();
        repeated.not_ready().expect("first transition");
        assert!(repeated.not_ready().is_err());
    }
}

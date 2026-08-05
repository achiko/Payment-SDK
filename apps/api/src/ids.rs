use uuid::Uuid;

/// Generates opaque, time-sortable identifiers for PS-owned durable records.
///
/// Idempotency is provided by the repository mapping, not by regenerating the
/// same UUID. The first accepted command stores the generated identifiers and
/// every replay returns those persisted values.
#[derive(Clone, Copy, Debug, Default)]
pub struct ServerIdGenerator;

impl ServerIdGenerator {
    #[must_use]
    pub fn job_id(self) -> String {
        prefixed("job")
    }

    #[must_use]
    pub fn deposit_id(self) -> String {
        prefixed("dep")
    }

    #[must_use]
    pub fn collection_id(self) -> String {
        prefixed("col")
    }

    #[cfg(test)]
    #[must_use]
    pub fn reconciliation_id(self) -> String {
        prefixed("rec")
    }
}

fn prefixed(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::now_v7())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_prefixed_version_seven_uuids() {
        let generator = ServerIdGenerator;
        for (prefix, value) in [
            ("job_", generator.job_id()),
            ("dep_", generator.deposit_id()),
            ("col_", generator.collection_id()),
            ("rec_", generator.reconciliation_id()),
        ] {
            let uuid = value
                .strip_prefix(prefix)
                .expect("generated ID must retain its type prefix")
                .parse::<Uuid>()
                .expect("generated ID must contain a UUID");
            assert_eq!(uuid.get_version_num(), 7);
        }
    }

    #[test]
    fn separate_commands_receive_different_ids() {
        let generator = ServerIdGenerator;
        assert_ne!(generator.job_id(), generator.job_id());
        assert_ne!(generator.deposit_id(), generator.deposit_id());
    }
}

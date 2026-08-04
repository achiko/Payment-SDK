use crate::{Key, Namespace, Value, Version};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    Missing {
        namespace: Namespace,
        key: Key,
    },
    Version {
        namespace: Namespace,
        key: Key,
        expected: Version,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Put {
        namespace: Namespace,
        key: Key,
        value: Value,
    },
    Delete {
        namespace: Namespace,
        key: Key,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteBatch {
    /// All conditions and operations must be evaluated atomically.
    pub conditions: Vec<Condition>,
    pub operations: Vec<Operation>,
}

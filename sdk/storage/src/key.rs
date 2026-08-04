#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Namespace(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value(pub Vec<u8>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredValue {
    pub value: Value,
    pub version: Version,
}

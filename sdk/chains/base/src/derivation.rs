#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct DerivationPath(pub Vec<ChildIndex>);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct ChildIndex {
    pub index: u32,
    pub hardened: bool,
}

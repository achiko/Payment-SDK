#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SighashType {
    All,
    None,
    Single,
    AllAnyoneCanPay,
    NoneAnyoneCanPay,
    SingleAnyoneCanPay,
    TaprootDefault,
}

use indexing::SourceError;

use crate::FeeRate;

use super::{
    Client,
    error::source_error,
    transport::Client as Transport,
    wire::{parse_btc_amount, parse_object},
};

impl<C> Client<C>
where
    C: Transport,
{
    pub async fn estimate_fee_rate(&self, target_blocks: u16) -> Result<FeeRate, SourceError> {
        if target_blocks == 0 {
            return Err(source_error(
                "Bitcoin fee-estimation target must be greater than zero",
                false,
            ));
        }
        let raw = self
            .request_result(
                "estimatesmartfee",
                serde_json::json!([target_blocks, "conservative"]),
            )
            .await?;
        let result = parse_object(&raw, "Bitcoin estimatesmartfee result")?;
        let fee_rate = result.get("feerate").ok_or_else(|| {
            source_error("Bitcoin Core cannot currently estimate a fee rate", true)
        })?;
        let satoshis = parse_btc_amount(fee_rate, "Bitcoin estimated BTC/kvB fee rate")?;
        if satoshis == 0 {
            return Err(source_error("Bitcoin Core estimated a zero fee rate", true));
        }
        Ok(FeeRate::new(satoshis))
    }
}

//! Operation DTOs generated from pinned official OpenAPI input schemas.
use fin_domain::{FinError, Result};
use fin_exchange::kraken::KrakenEndpoint;
fn deserialize_present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Value>, D::Error> {
    Ok(Some(
        <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?,
    ))
}
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetServerTimeInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetSystemStatusInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetMaintenanceScheduleInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetAssetInfoInput {
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(
        rename = "assetVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_version: Option<i64>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetTradableAssetPairsInput {
    #[serde(
        rename = "assetVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_version: Option<i64>,
    #[serde(rename = "pair", default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<String>,
    #[serde(
        rename = "aclass_base",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub aclass_base: Option<String>,
    #[serde(rename = "info", default, skip_serializing_if = "Option::is_none")]
    pub info: Option<String>,
    #[serde(
        rename = "country_code",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub country_code: Option<String>,
    #[serde(
        rename = "execution_venue",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub execution_venue: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetTickerInformationInput {
    #[serde(rename = "pair", default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<String>,
    #[serde(
        rename = "assetVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_version: Option<i64>,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetOhlcDataInput {
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(
        rename = "assetVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_version: Option<i64>,
    #[serde(rename = "interval", default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<i64>,
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetOrderBookInput {
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(
        rename = "assetVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_version: Option<i64>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetGroupedOrderBookInput {
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(rename = "depth", default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<i64>,
    #[serde(
        rename = "grouping",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub grouping: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetRecentTradesInput {
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(
        rename = "assetVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_version: Option<i64>,
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetRecentSpreadsInput {
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(
        rename = "assetVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_version: Option<i64>,
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetPreTradeInput {
    #[serde(rename = "symbol")]
    pub symbol: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetPostTradeInput {
    #[serde(rename = "symbol", default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(rename = "from_ts", default, skip_serializing_if = "Option::is_none")]
    pub from_ts: Option<String>,
    #[serde(rename = "to_ts", default, skip_serializing_if = "Option::is_none")]
    pub to_ts: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetLevel3OrderBookInput {
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(rename = "depth", default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<i64>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetAccountBalanceInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetExtendedBalanceInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetCreditLinesInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetTradeBalanceInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetOpenOrdersInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "trades", default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<bool>,
    #[serde(rename = "userref", default, skip_serializing_if = "Option::is_none")]
    pub userref: Option<i64>,
    #[serde(rename = "cl_ord_id", default, skip_serializing_if = "Option::is_none")]
    pub cl_ord_id: Option<String>,
    #[serde(
        rename = "consolidate_taker",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub consolidate_taker: Option<bool>,
    #[serde(
        rename = "with_cursor",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub with_cursor: Option<bool>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetClosedOrdersInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "trades", default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<bool>,
    #[serde(rename = "userref", default, skip_serializing_if = "Option::is_none")]
    pub userref: Option<i64>,
    #[serde(rename = "cl_ord_id", default, skip_serializing_if = "Option::is_none")]
    pub cl_ord_id: Option<String>,
    #[serde(rename = "start", default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(rename = "end", default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
    #[serde(rename = "ofs", default, skip_serializing_if = "Option::is_none")]
    pub ofs: Option<i64>,
    #[serde(
        rename = "with_cursor",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub with_cursor: Option<bool>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(rename = "closetime", default, skip_serializing_if = "Option::is_none")]
    pub closetime: Option<String>,
    #[serde(
        rename = "consolidate_taker",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub consolidate_taker: Option<bool>,
    #[serde(
        rename = "without_count",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub without_count: Option<bool>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetOrdersInfoInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "trades", default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<bool>,
    #[serde(rename = "userref", default, skip_serializing_if = "Option::is_none")]
    pub userref: Option<i64>,
    #[serde(rename = "txid")]
    pub txid: String,
    #[serde(rename = "cl_ord_id", default, skip_serializing_if = "Option::is_none")]
    pub cl_ord_id: Option<String>,
    #[serde(
        rename = "consolidate_taker",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub consolidate_taker: Option<bool>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetOrderAmendsInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "order_id")]
    pub order_id: String,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetTradeHistoryInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(rename = "trades", default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<bool>,
    #[serde(rename = "start", default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(rename = "end", default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
    #[serde(rename = "ofs", default, skip_serializing_if = "Option::is_none")]
    pub ofs: Option<i64>,
    #[serde(
        rename = "without_count",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub without_count: Option<bool>,
    #[serde(
        rename = "consolidate_taker",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub consolidate_taker: Option<bool>,
    #[serde(rename = "ledgers", default, skip_serializing_if = "Option::is_none")]
    pub ledgers: Option<bool>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "pair", default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<String>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(
        rename = "with_cursor",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub with_cursor: Option<bool>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetTradesInfoInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "txid")]
    pub txid: String,
    #[serde(rename = "trades", default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<bool>,
    #[serde(rename = "ledgers", default, skip_serializing_if = "Option::is_none")]
    pub ledgers: Option<bool>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetOpenPositionsInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "txid", default, skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    #[serde(rename = "docalcs", default, skip_serializing_if = "Option::is_none")]
    pub docalcs: Option<bool>,
    #[serde(
        rename = "consolidation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub consolidation: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetLedgersInputAssetChoice1Item {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "aclass")]
    pub aclass: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenGetLedgersInputAsset {
    Choice0(String),
    Choice1(Vec<KrakenGetLedgersInputAssetChoice1Item>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetLedgersInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<KrakenGetLedgersInputAsset>,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(rename = "start", default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(rename = "end", default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
    #[serde(rename = "ofs", default, skip_serializing_if = "Option::is_none")]
    pub ofs: Option<i64>,
    #[serde(
        rename = "with_cursor",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub with_cursor: Option<bool>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(
        rename = "without_count",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub without_count: Option<bool>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetLedgersInfoInput {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "trades", default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<bool>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetTradeVolumeInputPairChoice1Item {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "aclass")]
    pub aclass: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenGetTradeVolumeInputPair {
    Choice0(String),
    Choice1(Vec<KrakenGetTradeVolumeInputPairChoice1Item>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenGetTradeVolumeInputFeeInfo {
    Choice0(String),
    Choice1(bool),
    Choice2(String),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetTradeVolumeInput {
    #[serde(rename = "pair", default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<KrakenGetTradeVolumeInputPair>,
    #[serde(rename = "fee-info", default, skip_serializing_if = "Option::is_none")]
    pub fee_info: Option<KrakenGetTradeVolumeInputFeeInfo>,
    #[serde(
        rename = "fee_schedule",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub fee_schedule: Option<serde_json::Value>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAddExportInput {
    #[serde(rename = "report")]
    pub report: String,
    #[serde(rename = "format", default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(rename = "description")]
    pub description: String,
    #[serde(rename = "fields", default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<String>,
    #[serde(rename = "starttm", default, skip_serializing_if = "Option::is_none")]
    pub starttm: Option<i64>,
    #[serde(rename = "endtm", default, skip_serializing_if = "Option::is_none")]
    pub endtm: Option<i64>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenExportStatusInput {
    #[serde(rename = "report")]
    pub report: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenRetrieveExportInput {
    #[serde(rename = "id")]
    pub id: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenRemoveExportInput {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetApiKeyInfoInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListWalletAccountsInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAddOrderInput {
    #[serde(rename = "userref", default, skip_serializing_if = "Option::is_none")]
    pub userref: Option<i64>,
    #[serde(rename = "cl_ord_id", default, skip_serializing_if = "Option::is_none")]
    pub cl_ord_id: Option<String>,
    #[serde(rename = "ordertype")]
    pub ordertype: String,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(rename = "volume")]
    pub volume: String,
    #[serde(
        rename = "displayvol",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub displayvol: Option<String>,
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
    #[serde(rename = "price", default, skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(rename = "price2", default, skip_serializing_if = "Option::is_none")]
    pub price2: Option<String>,
    #[serde(rename = "trigger", default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(rename = "leverage", default, skip_serializing_if = "Option::is_none")]
    pub leverage: Option<String>,
    #[serde(
        rename = "reduce_only",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reduce_only: Option<bool>,
    #[serde(rename = "stptype", default, skip_serializing_if = "Option::is_none")]
    pub stptype: Option<String>,
    #[serde(rename = "oflags", default, skip_serializing_if = "Option::is_none")]
    pub oflags: Option<String>,
    #[serde(
        rename = "timeinforce",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub timeinforce: Option<String>,
    #[serde(rename = "starttm", default, skip_serializing_if = "Option::is_none")]
    pub starttm: Option<String>,
    #[serde(rename = "expiretm", default, skip_serializing_if = "Option::is_none")]
    pub expiretm: Option<String>,
    #[serde(
        rename = "close[ordertype]",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub close_ordertype: Option<String>,
    #[serde(
        rename = "close[price]",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub close_price: Option<String>,
    #[serde(
        rename = "close[price2]",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub close_price2: Option<String>,
    #[serde(rename = "deadline", default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(rename = "validate", default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<bool>,
    #[serde(
        rename = "broker",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub broker: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAmendOrderInput {
    #[serde(rename = "txid", default, skip_serializing_if = "Option::is_none")]
    pub txid: Option<String>,
    #[serde(rename = "cl_ord_id", default, skip_serializing_if = "Option::is_none")]
    pub cl_ord_id: Option<String>,
    #[serde(rename = "order_qty", default, skip_serializing_if = "Option::is_none")]
    pub order_qty: Option<String>,
    #[serde(
        rename = "display_qty",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_qty: Option<String>,
    #[serde(
        rename = "limit_price",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub limit_price: Option<String>,
    #[serde(
        rename = "trigger_price",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trigger_price: Option<String>,
    #[serde(rename = "pair", default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<String>,
    #[serde(rename = "post_only", default, skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    #[serde(rename = "deadline", default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenCancelOrderInputTxid {
    Choice0(String),
    Choice1(i64),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCancelOrderInput {
    #[serde(rename = "txid", default, skip_serializing_if = "Option::is_none")]
    pub txid: Option<KrakenCancelOrderInputTxid>,
    #[serde(rename = "cl_ord_id", default, skip_serializing_if = "Option::is_none")]
    pub cl_ord_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCancelAllOrdersInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCancelAllOrdersAfterInput {
    #[serde(rename = "timeout")]
    pub timeout: i64,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAddOrderBatchInputOrdersItemClose {
    #[serde(rename = "ordertype")]
    pub ordertype: String,
    #[serde(rename = "price", default, skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(rename = "price2", default, skip_serializing_if = "Option::is_none")]
    pub price2: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAddOrderBatchInputOrdersItem {
    #[serde(rename = "userref", default, skip_serializing_if = "Option::is_none")]
    pub userref: Option<i64>,
    #[serde(rename = "cl_ord_id", default, skip_serializing_if = "Option::is_none")]
    pub cl_ord_id: Option<String>,
    #[serde(rename = "ordertype")]
    pub ordertype: String,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(rename = "volume")]
    pub volume: String,
    #[serde(
        rename = "displayvol",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub displayvol: Option<String>,
    #[serde(rename = "price", default, skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(rename = "price2", default, skip_serializing_if = "Option::is_none")]
    pub price2: Option<String>,
    #[serde(rename = "trigger", default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(rename = "leverage", default, skip_serializing_if = "Option::is_none")]
    pub leverage: Option<String>,
    #[serde(
        rename = "reduce_only",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reduce_only: Option<bool>,
    #[serde(rename = "stptype", default, skip_serializing_if = "Option::is_none")]
    pub stptype: Option<String>,
    #[serde(rename = "oflags", default, skip_serializing_if = "Option::is_none")]
    pub oflags: Option<String>,
    #[serde(
        rename = "timeinforce",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub timeinforce: Option<String>,
    #[serde(rename = "starttm", default, skip_serializing_if = "Option::is_none")]
    pub starttm: Option<String>,
    #[serde(rename = "expiretm", default, skip_serializing_if = "Option::is_none")]
    pub expiretm: Option<String>,
    #[serde(rename = "close", default, skip_serializing_if = "Option::is_none")]
    pub close: Option<KrakenAddOrderBatchInputOrdersItemClose>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAddOrderBatchInput {
    #[serde(rename = "orders")]
    pub orders: Vec<KrakenAddOrderBatchInputOrdersItem>,
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
    #[serde(rename = "deadline", default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(rename = "validate", default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<bool>,
    #[serde(
        rename = "broker",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub broker: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenCancelOrderBatchInputOrdersItemTxid {
    Choice0(String),
    Choice1(i64),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCancelOrderBatchInputOrdersItem {
    #[serde(rename = "txid", default, skip_serializing_if = "Option::is_none")]
    pub txid: Option<KrakenCancelOrderBatchInputOrdersItemTxid>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCancelOrderBatchInputClOrdIdsItem {
    #[serde(rename = "cl_ord_id", default, skip_serializing_if = "Option::is_none")]
    pub cl_ord_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCancelOrderBatchInput {
    #[serde(rename = "orders", default, skip_serializing_if = "Option::is_none")]
    pub orders: Option<Vec<KrakenCancelOrderBatchInputOrdersItem>>,
    #[serde(
        rename = "cl_ord_ids",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cl_ord_ids: Option<Vec<KrakenCancelOrderBatchInputClOrdIdsItem>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenEditOrderInputTxid {
    Choice0(String),
    Choice1(i64),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenEditOrderInput {
    #[serde(rename = "userref", default, skip_serializing_if = "Option::is_none")]
    pub userref: Option<i64>,
    #[serde(rename = "txid")]
    pub txid: KrakenEditOrderInputTxid,
    #[serde(rename = "volume", default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    #[serde(
        rename = "displayvol",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub displayvol: Option<String>,
    #[serde(rename = "pair")]
    pub pair: String,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
    #[serde(rename = "price", default, skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(rename = "price2", default, skip_serializing_if = "Option::is_none")]
    pub price2: Option<String>,
    #[serde(rename = "oflags", default, skip_serializing_if = "Option::is_none")]
    pub oflags: Option<serde_json::Value>,
    #[serde(rename = "deadline", default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(
        rename = "cancel_response",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cancel_response: Option<bool>,
    #[serde(rename = "validate", default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetDepositMethodsInput {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenGetDepositAddressesInputAmount {
    Choice0(String),
    Choice1(i64),
    Choice2(String),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetDepositAddressesInput {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "method")]
    pub method: String,
    #[serde(rename = "amount", default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<KrakenGetDepositAddressesInputAmount>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenGetStatusRecentDepositsInputCursor {
    Choice0(bool),
    Choice1(String),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetStatusRecentDepositsInput {
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "method", default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(rename = "start", default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(rename = "end", default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<KrakenGetStatusRecentDepositsInputCursor>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetWithdrawalMethodsInput {
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "network", default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetWithdrawalAddressesInput {
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "method", default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(rename = "key", default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(rename = "verified", default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetWithdrawalInformationInput {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "key")]
    pub key: String,
    #[serde(rename = "amount")]
    pub amount: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenWithdrawFundsInput {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "key")]
    pub key: String,
    #[serde(rename = "address", default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(rename = "amount")]
    pub amount: String,
    #[serde(rename = "max_fee", default, skip_serializing_if = "Option::is_none")]
    pub max_fee: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenGetStatusRecentWithdrawalsInputCursor {
    Choice0(bool),
    Choice1(String),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetStatusRecentWithdrawalsInput {
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "method", default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(rename = "start", default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(rename = "end", default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<KrakenGetStatusRecentWithdrawalsInputCursor>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub rebase_multiplier: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCancelWithdrawalInput {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "refid")]
    pub refid: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenWalletTransferInput {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "from")]
    pub from: String,
    #[serde(rename = "to")]
    pub to: String,
    #[serde(rename = "amount")]
    pub amount: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateSubaccountInput {
    #[serde(rename = "username")]
    pub username: String,
    #[serde(rename = "email")]
    pub email: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAccountTransferInput {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
    #[serde(rename = "amount")]
    pub amount: String,
    #[serde(rename = "from")]
    pub from: String,
    #[serde(rename = "to")]
    pub to: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenAllocateStrategyInput {
    #[serde(rename = "amount")]
    pub amount: String,
    #[serde(rename = "strategy_id")]
    pub strategy_id: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenDeallocateStrategyInput {
    #[serde(rename = "amount")]
    pub amount: String,
    #[serde(rename = "strategy_id")]
    pub strategy_id: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetAllocateStrategyStatusInput {
    #[serde(rename = "strategy_id")]
    pub strategy_id: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenGetDeallocateStrategyStatusInput {
    #[serde(rename = "strategy_id")]
    pub strategy_id: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListStrategiesInput {
    #[serde(
        rename = "ascending",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub ascending: Option<serde_json::Value>,
    #[serde(
        rename = "asset",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub asset: Option<serde_json::Value>,
    #[serde(
        rename = "cursor",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub cursor: Option<serde_json::Value>,
    #[serde(
        rename = "limit",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub limit: Option<serde_json::Value>,
    #[serde(
        rename = "lock_type",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub lock_type: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListAllocationsInput {
    #[serde(
        rename = "ascending",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub ascending: Option<serde_json::Value>,
    #[serde(
        rename = "converted_asset",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub converted_asset: Option<serde_json::Value>,
    #[serde(
        rename = "hide_zero_allocations",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub hide_zero_allocations: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetFeeSchedulesV3Input {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetInstrumentsInput {
    #[serde(
        rename = "contractType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub contract_type: Option<Vec<String>>,
    #[serde(rename = "expired", default, skip_serializing_if = "Option::is_none")]
    pub expired: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesInstrumentsStatusInput {
    #[serde(
        rename = "contractType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub contract_type: Option<Vec<String>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesInstrumentStatusInput {
    #[serde(rename = "symbol")]
    pub symbol: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetOrderbookInput {
    #[serde(rename = "symbol")]
    pub symbol: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetTickersInput {
    #[serde(
        rename = "contractType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub contract_type: Option<Vec<String>>,
    #[serde(rename = "symbol", default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<Vec<String>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetTickerInput {
    #[serde(rename = "symbol")]
    pub symbol: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetHistoryInput {
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "lastTime", default, skip_serializing_if = "Option::is_none")]
    pub last_time: Option<String>,
    #[serde(
        rename = "includeMTFData",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub include_mtf_data: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesHistoricalFundingRatesInput {
    #[serde(rename = "symbol")]
    pub symbol: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetUserFeeScheduleVolumesV3Input {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetOpenPositionsInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetNotificationsInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetAccountsInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetOpenOrdersInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetRecentOrdersInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetFillsInput {
    #[serde(
        rename = "lastFillTime",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_fill_time: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetTransfersInput {
    #[serde(
        rename = "lastTransferTime",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_transfer_time: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetLeverageSettingInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetPnlCurrencyPreferenceInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetAssignmentProgramCurrentInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetAssignmentProgramHistoryInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetOrderStatusInput {
    #[serde(rename = "orderIds", default, skip_serializing_if = "Option::is_none")]
    pub order_ids: Option<Vec<String>>,
    #[serde(rename = "cliOrdIds", default, skip_serializing_if = "Option::is_none")]
    pub cli_ord_ids: Option<Vec<String>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetUnwindQueueInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetSelfTradeStrategyInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesListSubaccountsInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetSubaccountTradingCapabilityInput {
    #[serde(rename = "subaccountUid")]
    pub subaccount_uid: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetRfqAssignmentMaxLeverageInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSendOrderInput {
    #[serde(
        rename = "processBefore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub process_before: Option<String>,
    #[serde(rename = "orderType")]
    pub order_type: String,
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "side")]
    pub side: String,
    #[serde(rename = "size")]
    pub size: String,
    #[serde(
        rename = "limitPrice",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub limit_price: Option<String>,
    #[serde(rename = "stopPrice", default, skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
    #[serde(rename = "cliOrdId", default, skip_serializing_if = "Option::is_none")]
    pub cli_ord_id: Option<String>,
    #[serde(
        rename = "triggerSignal",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trigger_signal: Option<String>,
    #[serde(
        rename = "reduceOnly",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reduce_only: Option<bool>,
    #[serde(
        rename = "trailingStopMaxDeviation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trailing_stop_max_deviation: Option<String>,
    #[serde(
        rename = "trailingStopDeviationUnit",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trailing_stop_deviation_unit: Option<String>,
    #[serde(
        rename = "limitPriceOffsetValue",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub limit_price_offset_value: Option<String>,
    #[serde(
        rename = "limitPriceOffsetUnit",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub limit_price_offset_unit: Option<String>,
    #[serde(rename = "broker", default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesEditOrderSpringInput {
    #[serde(
        rename = "processBefore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub process_before: Option<String>,
    #[serde(rename = "orderId", default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(rename = "cliOrdId", default, skip_serializing_if = "Option::is_none")]
    pub cli_ord_id: Option<String>,
    #[serde(rename = "size", default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(
        rename = "limitPrice",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub limit_price: Option<String>,
    #[serde(rename = "stopPrice", default, skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
    #[serde(
        rename = "trailingStopMaxDeviation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trailing_stop_max_deviation: Option<String>,
    #[serde(
        rename = "trailingStopDeviationUnit",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trailing_stop_deviation_unit: Option<String>,
    #[serde(rename = "qtyMode", default, skip_serializing_if = "Option::is_none")]
    pub qty_mode: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesCancelOrderInput {
    #[serde(
        rename = "processBefore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub process_before: Option<String>,
    #[serde(rename = "order_id", default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(rename = "cliOrdId", default, skip_serializing_if = "Option::is_none")]
    pub cli_ord_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesTransferInput {
    #[serde(rename = "fromAccount")]
    pub from_account: String,
    #[serde(rename = "toAccount")]
    pub to_account: String,
    #[serde(rename = "unit")]
    pub unit: String,
    #[serde(rename = "amount")]
    pub amount: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSubAccountTransferInput {
    #[serde(rename = "fromUser")]
    pub from_user: String,
    #[serde(rename = "toUser")]
    pub to_user: String,
    #[serde(rename = "fromAccount")]
    pub from_account: String,
    #[serde(rename = "toAccount")]
    pub to_account: String,
    #[serde(rename = "unit")]
    pub unit: String,
    #[serde(rename = "amount")]
    pub amount: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSendBatchOrderInputBatchOrderItemChoice0 {
    #[serde(rename = "order")]
    pub order: String,
    #[serde(rename = "order_tag")]
    pub order_tag: String,
    #[serde(rename = "orderType")]
    pub order_type: String,
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "side")]
    pub side: String,
    #[serde(rename = "size")]
    pub size: String,
    #[serde(
        rename = "limitPrice",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub limit_price: Option<String>,
    #[serde(rename = "stopPrice", default, skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
    #[serde(rename = "cliOrdId", default, skip_serializing_if = "Option::is_none")]
    pub cli_ord_id: Option<String>,
    #[serde(
        rename = "triggerSignal",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trigger_signal: Option<String>,
    #[serde(
        rename = "reduceOnly",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reduce_only: Option<bool>,
    #[serde(
        rename = "trailingStopMaxDeviation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trailing_stop_max_deviation: Option<String>,
    #[serde(
        rename = "trailingStopDeviationUnit",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trailing_stop_deviation_unit: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSendBatchOrderInputBatchOrderItemChoice1 {
    #[serde(
        rename = "cliOrdId",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub cli_ord_id: Option<serde_json::Value>,
    #[serde(rename = "order_id", default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(rename = "order")]
    pub order: String,
    #[serde(rename = "size", default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(
        rename = "limitPrice",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub limit_price: Option<String>,
    #[serde(rename = "stopPrice", default, skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
    #[serde(
        rename = "trailingStopMaxDeviation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trailing_stop_max_deviation: Option<String>,
    #[serde(
        rename = "trailingStopDeviationUnit",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub trailing_stop_deviation_unit: Option<String>,
    #[serde(rename = "qtyMode", default, skip_serializing_if = "Option::is_none")]
    pub qty_mode: Option<String>,
    #[serde(rename = "order_tag")]
    pub order_tag: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSendBatchOrderInputBatchOrderItemChoice2 {
    #[serde(rename = "order")]
    pub order: String,
    #[serde(rename = "order_id", default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(rename = "cliOrdId", default, skip_serializing_if = "Option::is_none")]
    pub cli_ord_id: Option<String>,
    #[serde(rename = "order_tag")]
    pub order_tag: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenfuturesSendBatchOrderInputBatchOrderItem {
    Choice0(Box<KrakenfuturesSendBatchOrderInputBatchOrderItemChoice0>),
    Choice1(Box<KrakenfuturesSendBatchOrderInputBatchOrderItemChoice1>),
    Choice2(Box<KrakenfuturesSendBatchOrderInputBatchOrderItemChoice2>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSendBatchOrderInput {
    #[serde(
        rename = "processBefore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub process_before: Option<String>,
    #[serde(rename = "broker", default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<String>,
    #[serde(rename = "batchOrder")]
    pub batch_order: Vec<KrakenfuturesSendBatchOrderInputBatchOrderItem>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesCancelAllOrdersInput {
    #[serde(rename = "symbol", default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesCancelAllOrdersAfterInput {
    #[serde(rename = "timeout", default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesWithdrawalInput {
    #[serde(rename = "currency")]
    pub currency: String,
    #[serde(rename = "amount")]
    pub amount: String,
    #[serde(
        rename = "sourceWallet",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_wallet: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesDeleteAssignmentProgramInput {
    #[serde(rename = "id")]
    pub id: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSetLeverageSettingInput {
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(
        rename = "maxLeverage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_leverage: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSetPnlCurrencyPreferenceInput {
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "pnlPreference")]
    pub pnl_preference: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesSetSelfTradeStrategyInput {
    #[serde(rename = "strategy")]
    pub strategy: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesUpdateSubaccountTradingCapabilityInput {
    #[serde(rename = "subaccountUid")]
    pub subaccount_uid: String,
    #[serde(rename = "tradingEnabled")]
    pub trading_enabled: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesUpdateRfqAssignmentMaxLeverageInput {
    #[serde(rename = "maxLeverage")]
    pub max_leverage: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesDeleteRfqAssignmentMaxLeverageInput {}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesCandlesInput {
    #[serde(rename = "tick_type")]
    pub tick_type: String,
    #[serde(rename = "symbol")]
    pub symbol: String,
    #[serde(rename = "resolution")]
    pub resolution: String,
    #[serde(rename = "from", default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(rename = "to", default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesLiquidityPoolStatsInput {
    #[serde(rename = "since")]
    pub since: i64,
    #[serde(rename = "interval")]
    pub interval: i64,
    #[serde(rename = "to", default, skip_serializing_if = "Option::is_none")]
    pub to: Option<i64>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetOrderEventsInput {
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(rename = "before", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(rename = "sort", default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(
        rename = "continuation_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub continuation_token: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(rename = "tradeable", default, skip_serializing_if = "Option::is_none")]
    pub tradeable: Option<String>,
    #[serde(rename = "opened", default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<bool>,
    #[serde(rename = "closed", default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetExecutionEventsInput {
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(rename = "before", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(rename = "sort", default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(
        rename = "continuation_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub continuation_token: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(rename = "tradeable", default, skip_serializing_if = "Option::is_none")]
    pub tradeable: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetTriggerEventsInput {
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(rename = "before", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(rename = "sort", default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(
        rename = "continuation_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub continuation_token: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(rename = "tradeable", default, skip_serializing_if = "Option::is_none")]
    pub tradeable: Option<String>,
    #[serde(rename = "opened", default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<bool>,
    #[serde(rename = "closed", default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesAccountLogCsvInput {
    #[serde(
        rename = "conversion_details",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub conversion_details: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesAccountLogInput {
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(rename = "before", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(rename = "from", default, skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    #[serde(rename = "to", default, skip_serializing_if = "Option::is_none")]
    pub to: Option<i64>,
    #[serde(rename = "sort", default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(rename = "info", default, skip_serializing_if = "Option::is_none")]
    pub info: Option<Vec<String>>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
    #[serde(
        rename = "conversion_details",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub conversion_details: Option<bool>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetPublicOrderEventsInput {
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(rename = "before", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(rename = "sort", default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(
        rename = "continuation_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub continuation_token: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(rename = "tradeable")]
    pub tradeable: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetPublicExecutionEventsInput {
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(rename = "before", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(rename = "sort", default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(
        rename = "continuation_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub continuation_token: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(rename = "tradeable")]
    pub tradeable: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetPublicPriceEventsInput {
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(rename = "before", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(rename = "sort", default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(
        rename = "continuation_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub continuation_token: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(rename = "tradeable")]
    pub tradeable: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesGetPositionEventsInput {
    #[serde(rename = "since", default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(rename = "before", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(rename = "sort", default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(
        rename = "continuation_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub continuation_token: Option<String>,
    #[serde(rename = "count", default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(rename = "opened", default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<bool>,
    #[serde(rename = "closed", default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<bool>,
    #[serde(rename = "increased", default, skip_serializing_if = "Option::is_none")]
    pub increased: Option<bool>,
    #[serde(rename = "decreased", default, skip_serializing_if = "Option::is_none")]
    pub decreased: Option<bool>,
    #[serde(rename = "reversed", default, skip_serializing_if = "Option::is_none")]
    pub reversed: Option<bool>,
    #[serde(rename = "no_change", default, skip_serializing_if = "Option::is_none")]
    pub no_change: Option<bool>,
    #[serde(rename = "trades", default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<bool>,
    #[serde(
        rename = "funding_realization",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub funding_realization: Option<bool>,
    #[serde(
        rename = "settlement",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub settlement: Option<bool>,
    #[serde(rename = "tradeable", default, skip_serializing_if = "Option::is_none")]
    pub tradeable: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCalculateFundingFeesV1Input {
    #[serde(rename = "method_id")]
    pub method_id: String,
    #[serde(rename = "amount")]
    pub amount: String,
    #[serde(
        rename = "fee_included",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub fee_included: Option<bool>,
    #[serde(
        rename = "withdrawal_fee_token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub withdrawal_fee_token: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rebase_multiplier: Option<String>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenClaimFundingDepositAddressV1Input {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "method_id", default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingAddressesV1InputScopeChoice0 {
    #[serde(rename = "method_id", default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingAddressesV1InputScopeChoice1 {
    #[serde(
        rename = "network_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingAddressesV1InputScopeChoice2 {
    #[serde(
        rename = "network_group_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_group_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenListFundingAddressesV1InputScope {
    Choice0(Box<KrakenListFundingAddressesV1InputScopeChoice0>),
    Choice1(Box<KrakenListFundingAddressesV1InputScopeChoice1>),
    Choice2(Box<KrakenListFundingAddressesV1InputScopeChoice2>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingAddressesV1Input {
    #[serde(rename = "scope", default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<KrakenListFundingAddressesV1InputScope>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingAddressV1InputScopeChoice0 {
    #[serde(rename = "method_id", default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingAddressV1InputScopeChoice1 {
    #[serde(
        rename = "network_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingAddressV1InputScopeChoice2 {
    #[serde(
        rename = "network_group_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_group_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenCreateFundingAddressV1InputScope {
    Choice0(Box<KrakenCreateFundingAddressV1InputScopeChoice0>),
    Choice1(Box<KrakenCreateFundingAddressV1InputScopeChoice1>),
    Choice2(Box<KrakenCreateFundingAddressV1InputScopeChoice2>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingAddressV1InputAddressDetailsChoice0Crypto {
    #[serde(rename = "address", default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(rename = "tag", default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(rename = "memo", default, skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingAddressV1InputAddressDetailsChoice0 {
    #[serde(rename = "crypto", default, skip_serializing_if = "Option::is_none")]
    pub crypto: Option<KrakenCreateFundingAddressV1InputAddressDetailsChoice0Crypto>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingAddressV1Input {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "scope")]
    pub scope: KrakenCreateFundingAddressV1InputScope,
    #[serde(rename = "address_details")]
    pub address_details: KrakenCreateFundingAddressV1InputAddressDetailsChoice0,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(
        rename = "description",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingWithdrawalsV1InputAsset {
    #[serde(rename = "class")]
    pub class: String,
    #[serde(rename = "name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingWithdrawalsV1InputScopeChoice0 {
    #[serde(rename = "method_id", default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingWithdrawalsV1InputScopeChoice1 {
    #[serde(
        rename = "network_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingWithdrawalsV1InputScopeChoice2 {
    #[serde(
        rename = "network_group_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_group_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenListFundingWithdrawalsV1InputScope {
    Choice0(Box<KrakenListFundingWithdrawalsV1InputScopeChoice0>),
    Choice1(Box<KrakenListFundingWithdrawalsV1InputScopeChoice1>),
    Choice2(Box<KrakenListFundingWithdrawalsV1InputScopeChoice2>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingWithdrawalsV1Input {
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<KrakenListFundingWithdrawalsV1InputAsset>,
    #[serde(rename = "scope", default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<KrakenListFundingWithdrawalsV1InputScope>,
    #[serde(rename = "status", default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(
        rename = "start_time",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub start_time: Option<String>,
    #[serde(rename = "end_time", default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rebase_multiplier: Option<String>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputScopeChoice0 {
    #[serde(rename = "method_id", default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputScopeChoice1 {
    #[serde(
        rename = "network_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenCreateFundingWithdrawalV1InputScope {
    Choice0(Box<KrakenCreateFundingWithdrawalV1InputScopeChoice0>),
    Choice1(Box<KrakenCreateFundingWithdrawalV1InputScopeChoice1>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputAmountAssetAmountAsset {
    #[serde(rename = "class")]
    pub class: String,
    #[serde(rename = "name")]
    pub name: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputAmountAssetAmount {
    #[serde(rename = "asset")]
    pub asset: KrakenCreateFundingWithdrawalV1InputAmountAssetAmountAsset,
    #[serde(rename = "amount")]
    pub amount: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputAmount {
    #[serde(rename = "asset_amount")]
    pub asset_amount: KrakenCreateFundingWithdrawalV1InputAmountAssetAmount,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rebase_multiplier: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputFeeChoice0CurrentFeeMaxFeeAssetAmountAsset {
    #[serde(rename = "class")]
    pub class: String,
    #[serde(rename = "name")]
    pub name: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputFeeChoice0CurrentFeeMaxFeeAssetAmount {
    #[serde(rename = "asset")]
    pub asset: KrakenCreateFundingWithdrawalV1InputFeeChoice0CurrentFeeMaxFeeAssetAmountAsset,
    #[serde(rename = "amount")]
    pub amount: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputFeeChoice0CurrentFeeMaxFee {
    #[serde(rename = "asset_amount")]
    pub asset_amount: KrakenCreateFundingWithdrawalV1InputFeeChoice0CurrentFeeMaxFeeAssetAmount,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rebase_multiplier: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputFeeChoice0CurrentFee {
    #[serde(rename = "max_fee", default, skip_serializing_if = "Option::is_none")]
    pub max_fee: Option<KrakenCreateFundingWithdrawalV1InputFeeChoice0CurrentFeeMaxFee>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputFeeChoice0 {
    #[serde(
        rename = "current_fee",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub current_fee: Option<KrakenCreateFundingWithdrawalV1InputFeeChoice0CurrentFee>,
    #[serde(rename = "fee_included")]
    pub fee_included: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputFeeChoice1QuotedFee {
    #[serde(rename = "token")]
    pub token: String,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rebase_multiplier: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1InputFeeChoice1 {
    #[serde(
        rename = "quoted_fee",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub quoted_fee: Option<KrakenCreateFundingWithdrawalV1InputFeeChoice1QuotedFee>,
    #[serde(rename = "fee_included")]
    pub fee_included: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenCreateFundingWithdrawalV1InputFee {
    Choice0(Box<KrakenCreateFundingWithdrawalV1InputFeeChoice0>),
    Choice1(Box<KrakenCreateFundingWithdrawalV1InputFeeChoice1>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateFundingWithdrawalV1Input {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "scope")]
    pub scope: KrakenCreateFundingWithdrawalV1InputScope,
    #[serde(rename = "address_id")]
    pub address_id: String,
    #[serde(rename = "amount")]
    pub amount: KrakenCreateFundingWithdrawalV1InputAmount,
    #[serde(rename = "fee", default, skip_serializing_if = "Option::is_none")]
    pub fee: Option<KrakenCreateFundingWithdrawalV1InputFee>,
    #[serde(
        rename = "expected_address",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub expected_address: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenUpdateFundingAddressV1Input {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
    #[serde(rename = "name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        rename = "description",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenDeleteFundingAddressV1Input {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingAssetsV1Input {
    #[serde(rename = "direction")]
    pub direction: String,
    #[serde(
        rename = "asset_class",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub asset_class: Option<String>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingClaimedAddressesV2InputScopeChoice0 {
    #[serde(rename = "method_id", default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingClaimedAddressesV2InputScopeChoice1 {
    #[serde(
        rename = "network_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenListFundingClaimedAddressesV2InputScope {
    Choice0(Box<KrakenListFundingClaimedAddressesV2InputScopeChoice0>),
    Choice1(Box<KrakenListFundingClaimedAddressesV2InputScopeChoice1>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingClaimedAddressesV2InputAsset {
    #[serde(rename = "class")]
    pub class: String,
    #[serde(rename = "name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingClaimedAddressesV2Input {
    #[serde(rename = "scope", default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<KrakenListFundingClaimedAddressesV2InputScope>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<KrakenListFundingClaimedAddressesV2InputAsset>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositLimitsV1InputPreferredAsset {
    #[serde(rename = "class", default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(rename = "name")]
    pub name: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositLimitsV1Input {
    #[serde(rename = "asset_class")]
    pub asset_class: String,
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(
        rename = "preferred_asset",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub preferred_asset: Option<KrakenListFundingDepositLimitsV1InputPreferredAsset>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositsV1InputAsset {
    #[serde(rename = "class")]
    pub class: String,
    #[serde(rename = "name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositsV1InputScopeChoice0 {
    #[serde(rename = "method_id", default, skip_serializing_if = "Option::is_none")]
    pub method_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositsV1InputScopeChoice1 {
    #[serde(
        rename = "network_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositsV1InputScopeChoice2 {
    #[serde(
        rename = "network_group_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_group_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenListFundingDepositsV1InputScope {
    Choice0(Box<KrakenListFundingDepositsV1InputScopeChoice0>),
    Choice1(Box<KrakenListFundingDepositsV1InputScopeChoice1>),
    Choice2(Box<KrakenListFundingDepositsV1InputScopeChoice2>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositsV1InputStatusChoice0 {
    #[serde(rename = "list", default, skip_serializing_if = "Option::is_none")]
    pub list: Option<Vec<String>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositsV1InputStatusChoice1Range {
    #[serde(rename = "start", default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(rename = "end", default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositsV1InputStatusChoice1 {
    #[serde(rename = "range", default, skip_serializing_if = "Option::is_none")]
    pub range: Option<KrakenListFundingDepositsV1InputStatusChoice1Range>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenListFundingDepositsV1InputStatus {
    Choice0(Box<KrakenListFundingDepositsV1InputStatusChoice0>),
    Choice1(Box<KrakenListFundingDepositsV1InputStatusChoice1>),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingDepositsV1Input {
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<KrakenListFundingDepositsV1InputAsset>,
    #[serde(rename = "scope", default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<KrakenListFundingDepositsV1InputScope>,
    #[serde(rename = "status", default, skip_serializing_if = "Option::is_none")]
    pub status: Option<KrakenListFundingDepositsV1InputStatus>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(
        rename = "start_time",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub start_time: Option<String>,
    #[serde(rename = "end_time", default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rebase_multiplier: Option<String>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingMethodsV1InputAsset {
    #[serde(rename = "class")]
    pub class: String,
    #[serde(rename = "name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingMethodsV1Input {
    #[serde(rename = "direction")]
    pub direction: String,
    #[serde(rename = "asset", default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<KrakenListFundingMethodsV1InputAsset>,
    #[serde(
        rename = "rebase_multiplier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rebase_multiplier: Option<String>,
    #[serde(rename = "limit", default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(rename = "cursor", default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingNetworksV1Input {
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingWithdrawalLimitsV1InputPreferredAsset {
    #[serde(rename = "class", default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(rename = "name")]
    pub name: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenListFundingWithdrawalLimitsV1Input {
    #[serde(rename = "asset_class")]
    pub asset_class: String,
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(
        rename = "preferred_asset",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub preferred_asset: Option<KrakenListFundingWithdrawalLimitsV1InputPreferredAsset>,
    #[serde(
        rename = "account_id",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub account_id: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum KrakenCreateDepositAddressLegacyInputAmount {
    Choice0(String),
    Choice1(i64),
    Choice2(String),
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenCreateDepositAddressLegacyInput {
    #[serde(rename = "asset")]
    pub asset: String,
    #[serde(rename = "aclass", default, skip_serializing_if = "Option::is_none")]
    pub aclass: Option<String>,
    #[serde(rename = "method")]
    pub method: String,
    #[serde(rename = "amount", default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<KrakenCreateDepositAddressLegacyInputAmount>,
    #[serde(rename = "new")]
    pub new: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct KrakenfuturesCheckApiKeyV3Input {}

fn decode<T: serde::de::DeserializeOwned + serde::Serialize>(
    value: serde_json::Value,
) -> Result<serde_json::Value> {
    let input: T = serde_json::from_value(value)
        .map_err(|_| FinError::validation("Kraken operation fields are invalid."))?;
    serde_json::to_value(input)
        .map_err(|_| FinError::validation("Kraken input serialization failed."))
}
pub fn decode_wire(
    endpoint: KrakenEndpoint,
    value: serde_json::Value,
) -> Result<serde_json::Value> {
    match endpoint {
        KrakenEndpoint::KrakenPublicGetTime => decode::<KrakenGetServerTimeInput>(value),
        KrakenEndpoint::KrakenPublicGetSystemStatus => decode::<KrakenGetSystemStatusInput>(value),
        KrakenEndpoint::KrakenPublicGetMaintenanceSchedule => {
            decode::<KrakenGetMaintenanceScheduleInput>(value)
        }
        KrakenEndpoint::KrakenPublicGetAssets => decode::<KrakenGetAssetInfoInput>(value),
        KrakenEndpoint::KrakenPublicGetAssetPairs => {
            decode::<KrakenGetTradableAssetPairsInput>(value)
        }
        KrakenEndpoint::KrakenPublicGetTicker => decode::<KrakenGetTickerInformationInput>(value),
        KrakenEndpoint::KrakenPublicGetOhlc => decode::<KrakenGetOhlcDataInput>(value),
        KrakenEndpoint::KrakenPublicGetDepth => decode::<KrakenGetOrderBookInput>(value),
        KrakenEndpoint::KrakenPublicGetGroupedBook => {
            decode::<KrakenGetGroupedOrderBookInput>(value)
        }
        KrakenEndpoint::KrakenPublicGetTrades => decode::<KrakenGetRecentTradesInput>(value),
        KrakenEndpoint::KrakenPublicGetSpread => decode::<KrakenGetRecentSpreadsInput>(value),
        KrakenEndpoint::KrakenPublicGetPreTrade => decode::<KrakenGetPreTradeInput>(value),
        KrakenEndpoint::KrakenPublicGetPostTrade => decode::<KrakenGetPostTradeInput>(value),
        KrakenEndpoint::KrakenPrivatePostLevel3 => decode::<KrakenGetLevel3OrderBookInput>(value),
        KrakenEndpoint::KrakenPrivatePostBalance => decode::<KrakenGetAccountBalanceInput>(value),
        KrakenEndpoint::KrakenPrivatePostBalanceEx => {
            decode::<KrakenGetExtendedBalanceInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostCreditLines => decode::<KrakenGetCreditLinesInput>(value),
        KrakenEndpoint::KrakenPrivatePostTradeBalance => {
            decode::<KrakenGetTradeBalanceInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostOpenOrders => decode::<KrakenGetOpenOrdersInput>(value),
        KrakenEndpoint::KrakenPrivatePostClosedOrders => {
            decode::<KrakenGetClosedOrdersInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostQueryOrders => decode::<KrakenGetOrdersInfoInput>(value),
        KrakenEndpoint::KrakenPrivatePostOrderAmends => decode::<KrakenGetOrderAmendsInput>(value),
        KrakenEndpoint::KrakenPrivatePostTradesHistory => {
            decode::<KrakenGetTradeHistoryInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostQueryTrades => decode::<KrakenGetTradesInfoInput>(value),
        KrakenEndpoint::KrakenPrivatePostOpenPositions => {
            decode::<KrakenGetOpenPositionsInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostLedgers => decode::<KrakenGetLedgersInput>(value),
        KrakenEndpoint::KrakenPrivatePostQueryLedgers => decode::<KrakenGetLedgersInfoInput>(value),
        KrakenEndpoint::KrakenPrivatePostTradeVolume => decode::<KrakenGetTradeVolumeInput>(value),
        KrakenEndpoint::KrakenPrivatePostAddExport => decode::<KrakenAddExportInput>(value),
        KrakenEndpoint::KrakenPrivatePostExportStatus => decode::<KrakenExportStatusInput>(value),
        KrakenEndpoint::KrakenPrivatePostRetrieveExport => {
            decode::<KrakenRetrieveExportInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostRemoveExport => decode::<KrakenRemoveExportInput>(value),
        KrakenEndpoint::KrakenPrivatePostGetApiKeyInfo => decode::<KrakenGetApiKeyInfoInput>(value),
        KrakenEndpoint::KrakenPrivatePostListWalletAccounts => {
            decode::<KrakenListWalletAccountsInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostAddOrder => decode::<KrakenAddOrderInput>(value),
        KrakenEndpoint::KrakenPrivatePostAmendOrder => decode::<KrakenAmendOrderInput>(value),
        KrakenEndpoint::KrakenPrivatePostCancelOrder => decode::<KrakenCancelOrderInput>(value),
        KrakenEndpoint::KrakenPrivatePostCancelAll => decode::<KrakenCancelAllOrdersInput>(value),
        KrakenEndpoint::KrakenPrivatePostCancelAllOrdersAfter => {
            decode::<KrakenCancelAllOrdersAfterInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostAddOrderBatch => decode::<KrakenAddOrderBatchInput>(value),
        KrakenEndpoint::KrakenPrivatePostCancelOrderBatch => {
            decode::<KrakenCancelOrderBatchInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostEditOrder => decode::<KrakenEditOrderInput>(value),
        KrakenEndpoint::KrakenPrivatePostDepositMethods => {
            decode::<KrakenGetDepositMethodsInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostDepositAddresses => {
            decode::<KrakenGetDepositAddressesInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostDepositStatus => {
            decode::<KrakenGetStatusRecentDepositsInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostWithdrawMethods => {
            decode::<KrakenGetWithdrawalMethodsInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostWithdrawAddresses => {
            decode::<KrakenGetWithdrawalAddressesInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostWithdrawInfo => {
            decode::<KrakenGetWithdrawalInformationInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostWithdraw => decode::<KrakenWithdrawFundsInput>(value),
        KrakenEndpoint::KrakenPrivatePostWithdrawStatus => {
            decode::<KrakenGetStatusRecentWithdrawalsInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostWithdrawCancel => {
            decode::<KrakenCancelWithdrawalInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostWalletTransfer => {
            decode::<KrakenWalletTransferInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostCreateSubaccount => {
            decode::<KrakenCreateSubaccountInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostAccountTransfer => {
            decode::<KrakenAccountTransferInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostEarnAllocate => {
            decode::<KrakenAllocateStrategyInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostEarnDeallocate => {
            decode::<KrakenDeallocateStrategyInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostEarnAllocateStatus => {
            decode::<KrakenGetAllocateStrategyStatusInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostEarnDeallocateStatus => {
            decode::<KrakenGetDeallocateStrategyStatusInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostEarnStrategies => {
            decode::<KrakenListStrategiesInput>(value)
        }
        KrakenEndpoint::KrakenPrivatePostEarnAllocations => {
            decode::<KrakenListAllocationsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetFeeschedules => {
            decode::<KrakenfuturesGetFeeSchedulesV3Input>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetInstruments => {
            decode::<KrakenfuturesGetInstrumentsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetInstrumentsStatus => {
            decode::<KrakenfuturesInstrumentsStatusInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetInstrumentsSymbolStatus => {
            decode::<KrakenfuturesInstrumentStatusInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetOrderbook => {
            decode::<KrakenfuturesGetOrderbookInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetTickers => {
            decode::<KrakenfuturesGetTickersInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetTickersSymbol => {
            decode::<KrakenfuturesGetTickerInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetHistory => {
            decode::<KrakenfuturesGetHistoryInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPublicGetHistoricalfundingrates => {
            decode::<KrakenfuturesHistoricalFundingRatesInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetFeeschedulesVolumes => {
            decode::<KrakenfuturesGetUserFeeScheduleVolumesV3Input>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetOpenpositions => {
            decode::<KrakenfuturesGetOpenPositionsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetNotifications => {
            decode::<KrakenfuturesGetNotificationsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetAccounts => {
            decode::<KrakenfuturesGetAccountsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetOpenorders => {
            decode::<KrakenfuturesGetOpenOrdersInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetRecentorders => {
            decode::<KrakenfuturesGetRecentOrdersInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetFills => decode::<KrakenfuturesGetFillsInput>(value),
        KrakenEndpoint::KrakenfuturesPrivateGetTransfers => {
            decode::<KrakenfuturesGetTransfersInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetLeveragepreferences => {
            decode::<KrakenfuturesGetLeverageSettingInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetPnlpreferences => {
            decode::<KrakenfuturesGetPnlCurrencyPreferenceInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetAssignmentprogramCurrent => {
            decode::<KrakenfuturesGetAssignmentProgramCurrentInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetAssignmentprogramHistory => {
            decode::<KrakenfuturesGetAssignmentProgramHistoryInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetOrdersStatus => {
            decode::<KrakenfuturesGetOrderStatusInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetUnwindqueue => {
            decode::<KrakenfuturesGetUnwindQueueInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetSelfTradeStrategy => {
            decode::<KrakenfuturesGetSelfTradeStrategyInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetSubaccounts => {
            decode::<KrakenfuturesListSubaccountsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetSubaccountUidTradingEnabled => {
            decode::<KrakenfuturesGetSubaccountTradingCapabilityInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateGetRfqAssignmentMaxLeverage => {
            decode::<KrakenfuturesGetRfqAssignmentMaxLeverageInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostSendorder => {
            decode::<KrakenfuturesSendOrderInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostEditorder => {
            decode::<KrakenfuturesEditOrderSpringInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostCancelorder => {
            decode::<KrakenfuturesCancelOrderInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostTransfer => {
            decode::<KrakenfuturesTransferInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostTransferSubaccount => {
            decode::<KrakenfuturesSubAccountTransferInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostBatchorder => {
            decode::<KrakenfuturesSendBatchOrderInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostCancelallorders => {
            decode::<KrakenfuturesCancelAllOrdersInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostCancelallordersafter => {
            decode::<KrakenfuturesCancelAllOrdersAfterInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostWithdrawal => {
            decode::<KrakenfuturesWithdrawalInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePostAssignmentprogramDelete => {
            decode::<KrakenfuturesDeleteAssignmentProgramInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePutLeveragepreferences => {
            decode::<KrakenfuturesSetLeverageSettingInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePutPnlpreferences => {
            decode::<KrakenfuturesSetPnlCurrencyPreferenceInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePutSelfTradeStrategy => {
            decode::<KrakenfuturesSetSelfTradeStrategyInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePutSubaccountUidTradingEnabled => {
            decode::<KrakenfuturesUpdateSubaccountTradingCapabilityInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivatePutRfqAssignmentMaxLeverage => {
            decode::<KrakenfuturesUpdateRfqAssignmentMaxLeverageInput>(value)
        }
        KrakenEndpoint::KrakenfuturesPrivateDeleteRfqAssignmentMaxLeverage => {
            decode::<KrakenfuturesDeleteRfqAssignmentMaxLeverageInput>(value)
        }
        KrakenEndpoint::KrakenfuturesChartsGetPriceTypeSymbolInterval => {
            decode::<KrakenfuturesCandlesInput>(value)
        }
        KrakenEndpoint::KrakenfuturesChartsGetAnalyticsLiquidityPool => {
            decode::<KrakenfuturesLiquidityPoolStatsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetOrders => {
            decode::<KrakenfuturesGetOrderEventsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetExecutions => {
            decode::<KrakenfuturesGetExecutionEventsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetTriggers => {
            decode::<KrakenfuturesGetTriggerEventsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetAccountlogcsv => {
            decode::<KrakenfuturesAccountLogCsvInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetAccountLog => {
            decode::<KrakenfuturesAccountLogInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetMarketSymbolOrders => {
            decode::<KrakenfuturesGetPublicOrderEventsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetMarketSymbolExecutions => {
            decode::<KrakenfuturesGetPublicExecutionEventsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetMarketSymbolPrice => {
            decode::<KrakenfuturesGetPublicPriceEventsInput>(value)
        }
        KrakenEndpoint::KrakenfuturesHistoryGetPositions => {
            decode::<KrakenfuturesGetPositionEventsInput>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1FeesMethodId => {
            decode::<KrakenCalculateFundingFeesV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingPutFundingV1DepositAddress => {
            decode::<KrakenClaimFundingDepositAddressV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1Addresses => {
            decode::<KrakenListFundingAddressesV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingPostFundingV1Addresses => {
            decode::<KrakenCreateFundingAddressV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1Withdrawals => {
            decode::<KrakenListFundingWithdrawalsV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingPostFundingV1Withdrawals => {
            decode::<KrakenCreateFundingWithdrawalV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingPutFundingV1AddressesId => {
            decode::<KrakenUpdateFundingAddressV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingDeleteFundingV1AddressesId => {
            decode::<KrakenDeleteFundingAddressV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1AssetsDirection => {
            decode::<KrakenListFundingAssetsV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV2DepositAddresses => {
            decode::<KrakenListFundingClaimedAddressesV2Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1LimitsDepositAssetClassAsset => {
            decode::<KrakenListFundingDepositLimitsV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1Deposits => {
            decode::<KrakenListFundingDepositsV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1MethodsDirection => {
            decode::<KrakenListFundingMethodsV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1Networks => {
            decode::<KrakenListFundingNetworksV1Input>(value)
        }
        KrakenEndpoint::KrakenFundingGetFundingV1LimitsWithdrawalAssetClassAsset => {
            decode::<KrakenListFundingWithdrawalLimitsV1Input>(value)
        }
        KrakenEndpoint::KrakenLegacyCreateDepositAddress => {
            decode::<KrakenCreateDepositAddressLegacyInput>(value)
        }
        KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check => {
            decode::<KrakenfuturesCheckApiKeyV3Input>(value)
        }
    }
}

//! Local, validated provider-conversation accounting. Never account-block spend.
use crate::{DateMillis, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionUsageAvailability {
    Available,
    Unavailable,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionUsageScope {
    ConversationLifetime,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedSessionTokens {
    /// Uncached input; cache reads/writes are separate, disjoint categories.
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Included in output, not an additional billed category.
    pub reasoning_output: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedSessionContext {
    pub tokens: i64,
    /// Unknown window is not zero and does not discard reported occupancy.
    pub window: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsagePricing {
    /// Estimate for priced contributions only. None when no tokens are priced.
    pub estimated_cost_usd: Option<f64>,
    pub priced_tokens: i64,
    pub total_tokens: i64,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageResult {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub availability: SessionUsageAvailability,
    pub reason: Option<String>,
    pub provider: Option<String>,
    pub conversation_id: Option<String>,
    pub scope: SessionUsageScope,
    /// Includes provider history before an Ubra resume; no incarnation baseline.
    pub scope_note: String,
    pub tokens: Option<ReportedSessionTokens>,
    pub context: Option<ReportedSessionContext>,
    pub context_reason: Option<String>,
    pub pricing: Option<SessionUsagePricing>,
    /// Time the transcript was observed, not a provider billing timestamp.
    pub observed_at: DateMillis,
    /// Last transcript file modification observed; optional when unavailable.
    pub source_updated_at: Option<DateMillis>,
    pub stale: bool,
}

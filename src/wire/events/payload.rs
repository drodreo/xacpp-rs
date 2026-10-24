//! Event payload types.

use serde::{Deserialize, Serialize};

use super::content::ContentPart;
use super::upload::TokenUsage;

/// Content part event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentPartEvent {
    /// Unique identifier for one engine run.
    pub round: String,
    /// Unique identifier for one loop iteration.
    pub pair: String,
    /// Actual content part.
    pub payload: ContentPart,
}

/// Content delta event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentDeltaEvent {
    /// Unique identifier for one engine run.
    pub round: String,
    /// Unique identifier for one loop iteration.
    pub pair: String,
    /// Actual content delta.
    pub payload: ContentPart,
}

/// Traceable event payload (shared by Info/Warn/Error).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceableEvent {
    /// Main sentence for user/interface (required).
    pub title: String,
    /// Extended details (optional, defaults to empty string).
    #[serde(default)]
    pub content: String,
}

/// Authorization request Alert level.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    #[default]
    Info,
    Warn,
    Critical,
}

/// Activity-level authorization mode.
///
/// `Default` keeps the existing per-request authorization flow; `Auto` has the
/// serve side short-circuit action-class authorization requests for the whole
/// activity (semantics owned by the serve side, see x-agent
/// tool-runtime-permission.md). Questions are not affected.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityMode {
    #[default]
    Default,
    Auto,
}

/// Tool call started event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUseEvent {
    /// ToolRequest.id.
    pub request_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Tool call sequence number.
    pub index: u32,
    /// Human-readable summary of tool arguments (may be truncated, not guaranteed parseable).
    pub arguments: String,
    /// Intent description of the tool call.
    pub intent: String,
}

/// Tool call lifecycle status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    /// Terminal success.
    Completed,
    /// Still executing (process running in the background beyond this event).
    Running,
    /// Cancelled before completion (engine cancel path terminal state).
    Cancelled,
    /// Failed termination (tool exception or failed command).
    Error,
}

/// Tool call finished event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEvent {
    /// ToolRequest.id.
    pub request_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Tool call sequence number.
    pub index: u32,
    /// Tool execution result content parts (may be empty; must be present).
    pub parts: Vec<ContentPart>,
    /// True when this result was produced under the activity `auto`
    /// authorization mode (self-approved, no user interaction).
    pub auto: bool,
    /// Lifecycle status of this tool call; absent means unknown — never
    /// interpreted as a concrete terminal state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolResultStatus>,
    /// True when the tool keeps running in the background beyond this result
    /// (persistent process / timeout-to-background); absent means unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_background: Option<bool>,
    /// True when this result is an error result (tool exception; orthogonal
    /// to `status`).
    pub is_error: bool,
}

/// Security alert event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityAlertEvent {
    /// Event unique identifier.
    pub event_id: String,
    /// Associated tool name.
    pub tool_name: String,
    /// Alert level.
    pub alert_level: AlertLevel,
    /// Threat description.
    pub description: String,
    /// Threat type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threat_type: Option<String>,
    /// Matched attack pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_pattern: Option<String>,
    /// Context snippet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_snippet: Option<String>,
}

/// SubActivity task started event payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityStartEvent {
    /// Task goal summary.
    pub goal: String,
    /// SubActivity unique identifier.
    pub activity: String,
    /// Additional metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<std::collections::HashMap<String, String>>,
}

/// Activity metadata shared across commands, responses, and events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityInfo {
    pub activity: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Human-readable activity description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Last update time (RFC 3339 string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Working directory; fillers normalize the agent home prefix to the `$AGENT_HOME` placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Current activity authorization mode (in-memory state on the serve
    /// side; a restored activity is always `Default`).
    // COMPAT: additive optional field; old serve peers omit it (reads as
    // Default). Remove once all consumers are mode-aware.
    #[serde(default)]
    pub mode: ActivityMode,
}

/// Model tier selected for an activity run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// Standard tier.
    Default,
    /// Fast/light tier.
    Lite,
    /// Expert tier.
    Expert,
}

/// Pair completion event payload.
///
/// Emitted when one loop iteration's assistant output has settled. Carries
/// the loop identity (`pair`), the serving model's context window and token
/// usage, and the model tier/name selected for the run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairCompleteEvent {
    /// Unique identifier of the completed loop iteration.
    pub pair: String,
    /// Context window size (tokens) of the model that served this pair.
    pub context_window: u32,
    /// Token usage of this pair's LLM call.
    pub token_usage: TokenUsage,
    /// Model tier for this pair (invoke-time selection, fixed at run start);
    /// absent means unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<ModelTier>,
    /// Model display name for this pair; absent means unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Completion event payload.
///
/// Terminal signal of one invoke: carries the run identity, the final
/// assistant reply, and termination semantics (abort reason / cancellation).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteEvent {
    /// Unique identifier of one engine run (turn-level identity).
    pub round: String,
    /// Final assistant reply content parts.
    pub assistant_reply: Vec<ContentPart>,
    /// Abort reason when the run was aborted; absence MUST NOT be used to
    /// infer `canceled` — `canceled` is authoritative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<String>,
    /// True when the run was canceled by the user.
    pub canceled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::events::content::TextPart;

    #[test]
    fn tool_result_canonical_json() {
        // Full-shape canonical wire form: required fields present, optional
        // fields present as Some. Explicit false booleans are facts and MUST
        // appear; empty parts MUST appear.
        let event = ToolResultEvent {
            request_id: "req-1".into(),
            tool_name: "shell".into(),
            index: 0,
            parts: vec![],
            auto: false,
            status: Some(ToolResultStatus::Running),
            is_background: Some(true),
            is_error: false,
        };
        let s = serde_json::to_string(&event).unwrap();
        assert_eq!(
            s,
            r#"{"requestId":"req-1","toolName":"shell","index":0,"parts":[],"auto":false,"status":"running","isBackground":true,"isError":false}"#
        );
        let de: ToolResultEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(de.request_id, "req-1");
        assert_eq!(de.tool_name, "shell");
        assert_eq!(de.index, 0);
        assert!(de.parts.is_empty());
        assert!(!de.auto);
        assert_eq!(de.status, Some(ToolResultStatus::Running));
        assert_eq!(de.is_background, Some(true));
        assert!(!de.is_error);
    }

    #[test]
    fn tool_result_required_fields_missing_fails() {
        // `parts`, `auto`, and `is_error` are required under 1.2: absence is
        // a hard deserialization error, never a silently applied default.
        let missing_parts =
            r#"{"requestId":"req-1","toolName":"shell","index":0,"auto":false,"isError":false}"#;
        assert!(
            serde_json::from_str::<ToolResultEvent>(missing_parts).is_err(),
            "missing parts must fail"
        );

        let missing_auto =
            r#"{"requestId":"req-1","toolName":"shell","index":0,"parts":[],"isError":false}"#;
        assert!(
            serde_json::from_str::<ToolResultEvent>(missing_auto).is_err(),
            "missing auto must fail"
        );

        let missing_is_error =
            r#"{"requestId":"req-1","toolName":"shell","index":0,"parts":[],"auto":false}"#;
        assert!(
            serde_json::from_str::<ToolResultEvent>(missing_is_error).is_err(),
            "missing isError must fail"
        );
    }

    #[test]
    fn tool_result_optional_absent_reads_none_and_reserializes_absent() {
        // Optional fields absent on the wire read as None (= unknown) and
        // re-serialize as absent — never as a concrete status or fact.
        let de: ToolResultEvent = serde_json::from_str(
            r#"{"requestId":"req-1","toolName":"shell","index":0,"parts":[],"auto":true,"isError":false}"#,
        )
        .unwrap();
        assert_eq!(de.status, None);
        assert_eq!(de.is_background, None);
        let s = serde_json::to_string(&de).unwrap();
        assert_eq!(
            s,
            r#"{"requestId":"req-1","toolName":"shell","index":0,"parts":[],"auto":true,"isError":false}"#
        );
    }

    #[test]
    fn tool_result_status_roundtrip() {
        let spellings = [
            (ToolResultStatus::Completed, "completed"),
            (ToolResultStatus::Running, "running"),
            (ToolResultStatus::Cancelled, "cancelled"),
            (ToolResultStatus::Error, "error"),
        ];
        for (status, wire) in spellings {
            let s = serde_json::to_string(&status).unwrap();
            assert_eq!(s, format!(r#""{wire}""#));
            let de: ToolResultStatus = serde_json::from_str(&s).unwrap();
            assert_eq!(de, status);
        }
    }

    #[test]
    fn pair_complete_canonical_json() {
        let event = PairCompleteEvent {
            pair: "p-1".into(),
            context_window: 200_000,
            token_usage: TokenUsage {
                id: "u-1".into(),
                message: "m-1".into(),
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cache_hit_tokens: Some(8),
            },
            tier: Some(ModelTier::Lite),
            model: Some("doubao-seed-1-6".into()),
        };
        let s = serde_json::to_string(&event).unwrap();
        assert_eq!(
            s,
            r#"{"pair":"p-1","contextWindow":200000,"tokenUsage":{"id":"u-1","message":"m-1","promptTokens":10,"completionTokens":20,"totalTokens":30,"cacheHitTokens":8},"tier":"lite","model":"doubao-seed-1-6"}"#
        );
        let de: PairCompleteEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(de.pair, "p-1");
        assert_eq!(de.context_window, 200_000);
        assert_eq!(de.token_usage, event.token_usage);
        assert_eq!(de.tier, Some(ModelTier::Lite));
        assert_eq!(de.model.as_deref(), Some("doubao-seed-1-6"));
    }

    #[test]
    fn pair_complete_required_fields_missing_fails() {
        // `pair`, `contextWindow`, and `tokenUsage` are required under 1.2.
        let missing_pair = r#"{"contextWindow":128000,"tokenUsage":{"id":"u","message":"m","promptTokens":1,"completionTokens":2,"totalTokens":3}}"#;
        assert!(
            serde_json::from_str::<PairCompleteEvent>(missing_pair).is_err(),
            "missing pair must fail"
        );

        let missing_context = r#"{"pair":"p-1","tokenUsage":{"id":"u","message":"m","promptTokens":1,"completionTokens":2,"totalTokens":3}}"#;
        assert!(
            serde_json::from_str::<PairCompleteEvent>(missing_context).is_err(),
            "missing contextWindow must fail"
        );

        let missing_usage = r#"{"pair":"p-1","contextWindow":128000}"#;
        assert!(
            serde_json::from_str::<PairCompleteEvent>(missing_usage).is_err(),
            "missing tokenUsage must fail"
        );
    }

    #[test]
    fn pair_complete_unknown_tier_model_absent() {
        // tier/model absent = unknown; re-serialization keeps them absent
        // instead of inventing a concrete tier or empty name.
        let de: PairCompleteEvent = serde_json::from_str(
            r#"{"pair":"p-1","contextWindow":128000,"tokenUsage":{"id":"u","message":"m","promptTokens":1,"completionTokens":2,"totalTokens":3}}"#,
        )
        .unwrap();
        assert_eq!(de.tier, None);
        assert_eq!(de.model, None);
        let s = serde_json::to_string(&de).unwrap();
        assert_eq!(
            s,
            r#"{"pair":"p-1","contextWindow":128000,"tokenUsage":{"id":"u","message":"m","promptTokens":1,"completionTokens":2,"totalTokens":3}}"#
        );
    }

    #[test]
    fn model_tier_roundtrip() {
        let spellings = [
            (ModelTier::Default, "default"),
            (ModelTier::Lite, "lite"),
            (ModelTier::Expert, "expert"),
        ];
        for (tier, wire) in spellings {
            let s = serde_json::to_string(&tier).unwrap();
            assert_eq!(s, format!(r#""{wire}""#));
            let de: ModelTier = serde_json::from_str(&s).unwrap();
            assert_eq!(de, tier);
        }
    }

    #[test]
    fn complete_canonical_json() {
        let event = CompleteEvent {
            round: "r-1".into(),
            assistant_reply: vec![ContentPart::Text(TextPart {
                text: "done".into(),
                part_id: None,
                part_type: "default".into(),
            })],
            abort_reason: Some("user abort".into()),
            canceled: true,
        };
        let s = serde_json::to_string(&event).unwrap();
        assert_eq!(
            s,
            r#"{"round":"r-1","assistantReply":[{"type":"text","text":"done","partType":"default"}],"abortReason":"user abort","canceled":true}"#
        );
        let de: CompleteEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(de.round, "r-1");
        assert_eq!(de.assistant_reply.len(), 1);
        assert_eq!(de.abort_reason.as_deref(), Some("user abort"));
        assert!(de.canceled);
    }

    #[test]
    fn complete_required_fields_missing_fails() {
        // `round`, `assistantReply`, and `canceled` are required under 1.2.
        let missing_round = r#"{"assistantReply":[],"canceled":false}"#;
        assert!(
            serde_json::from_str::<CompleteEvent>(missing_round).is_err(),
            "missing round must fail"
        );

        let missing_reply = r#"{"round":"r-1","canceled":false}"#;
        assert!(
            serde_json::from_str::<CompleteEvent>(missing_reply).is_err(),
            "missing assistantReply must fail"
        );

        let missing_canceled = r#"{"round":"r-1","assistantReply":[]}"#;
        assert!(
            serde_json::from_str::<CompleteEvent>(missing_canceled).is_err(),
            "missing canceled must fail"
        );
    }

    #[test]
    fn complete_normal_form_canceled_false_preserved() {
        // Normal completion: abortReason absent (= not aborted, never a
        // cancellation fact) while canceled=false stays explicit on the wire.
        let event = CompleteEvent {
            round: "r-2".into(),
            assistant_reply: vec![],
            abort_reason: None,
            canceled: false,
        };
        let s = serde_json::to_string(&event).unwrap();
        assert_eq!(s, r#"{"round":"r-2","assistantReply":[],"canceled":false}"#);
        let de: CompleteEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(de.abort_reason, None);
        assert!(!de.canceled);
    }

    #[test]
    fn token_usage_cache_hit_tokens() {
        // Some(n) serializes as cacheHitTokens.
        let usage = TokenUsage {
            id: "u".into(),
            message: "m".into(),
            prompt_tokens: 100,
            completion_tokens: 10,
            total_tokens: 110,
            cache_hit_tokens: Some(90),
        };
        let s = serde_json::to_string(&usage).unwrap();
        assert_eq!(
            s,
            r#"{"id":"u","message":"m","promptTokens":100,"completionTokens":10,"totalTokens":110,"cacheHitTokens":90}"#
        );
        let de: TokenUsage = serde_json::from_str(&s).unwrap();
        assert_eq!(de.cache_hit_tokens, Some(90));

        // Some(0) is an explicit zero reported by the provider and MUST be
        // preserved on the wire — not collapsed into "absent".
        let zero = TokenUsage {
            id: "u".into(),
            message: "m".into(),
            prompt_tokens: 100,
            completion_tokens: 10,
            total_tokens: 110,
            cache_hit_tokens: Some(0),
        };
        let s = serde_json::to_string(&zero).unwrap();
        assert!(s.contains(r#""cacheHitTokens":0"#), "json: {s}");
        let de: TokenUsage = serde_json::from_str(&s).unwrap();
        assert_eq!(de.cache_hit_tokens, Some(0));

        // None: absent on the wire (= not reported); reading JSON without
        // the field yields None.
        let none = TokenUsage {
            id: "u".into(),
            message: "m".into(),
            prompt_tokens: 100,
            completion_tokens: 10,
            total_tokens: 110,
            cache_hit_tokens: None,
        };
        let s = serde_json::to_string(&none).unwrap();
        assert!(!s.contains("cacheHitTokens"), "json: {s}");
        assert_eq!(
            s,
            r#"{"id":"u","message":"m","promptTokens":100,"completionTokens":10,"totalTokens":110}"#
        );
        let de: TokenUsage = serde_json::from_str(&s).unwrap();
        assert_eq!(de.cache_hit_tokens, None);
    }
}

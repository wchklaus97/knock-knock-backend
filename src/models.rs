use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct UserPrincipal {
    pub user_id: String,
}

#[derive(Debug, Clone)]
pub struct AgentPrincipal {
    pub agent_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillAction {
    pub id: String,
    pub risk: String,
    #[serde(default)]
    pub confirm: bool,
    pub title: String,
    #[serde(default)]
    pub payload: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTtl {
    pub default_sec: i64,
    pub destructive_sec: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub skill_id: String,
    pub template: String,
    #[serde(default)]
    pub facts_schema: Vec<String>,
    #[serde(default)]
    pub actions: Vec<SkillAction>,
    pub ttl: SkillTtl,
    #[serde(default)]
    pub version: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserRow {
    pub id: String,
    pub email: String,
    pub password_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub user_id: String,
    pub label: String,
    pub host_label: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PairingRow {
    pub user_id: String,
    pub expires_at: String,
    pub claimed_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillRow {
    pub skill_id: String,
    pub template: String,
    pub facts_schema_json: String,
    pub actions_json: String,
    pub ttl_json: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub agent_id: String,
    pub user_id: String,
    pub skill_id: String,
    pub state: String,
    pub progress_status: Option<String>,
    pub progress_message: Option<String>,
    pub progress_percent: Option<f64>,
    pub title: Option<String>,
    pub chat_id: Option<String>,
    pub summary_text: Option<String>,
    pub voice_script: Option<String>,
    pub facts_json: String,
    pub available_actions_json: Option<String>,
    pub expires_at: String,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
    pub deleted_at: Option<String>,
    pub retention_expires_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionRow {
    pub id: String,
    pub session_id: String,
    pub agent_id: String,
    pub action_key: String,
    pub title: Option<String>,
    pub risk: String,
    pub confirm_required: i32,
    pub status: String,
    pub result_json: Option<String>,
    pub claimed_at: Option<String>,
    pub expires_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventRow {
    pub id: String,
    pub pushed: i32,
    pub summary_text: Option<String>,
    pub voice_script: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum CommandState {
    Pending,
    Validated,
    AwaitingConfirmation,
    Queued,
    Running,
    Succeeded,
    Failed,
    Expired,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum CommandRisk {
    Low,
    Medium,
    High,
    Destructive,
}

/// Canonical v1 command envelope. Model output is untrusted input; the
/// backend must validate every field before it changes state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub schema_version: i32,
    pub command_id: String,
    pub intent: String,
    pub args: Map<String, Value>,
    pub risk_level: String,
    pub needs_confirmation: bool,
    pub idempotency_key: String,
    pub confidence: f64,
    pub locale: String,
    pub timezone: String,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct CommandRow {
    pub id: String,
    pub user_id: String,
    pub device_id: Option<String>,
    pub session_id: Option<String>,
    pub schema_version: i32,
    pub intent: String,
    pub args_json: String,
    pub risk_level: String,
    pub needs_confirmation: i32,
    pub idempotency_key: String,
    pub confidence: Option<f64>,
    pub locale: String,
    pub timezone: String,
    pub state: String,
    pub command_hash: String,
    pub result_json: Option<String>,
    pub error_code: Option<String>,
    pub expires_at: Option<String>,
    pub model_version: Option<String>,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ConfirmationTokenRow {
    pub id: String,
    pub command_id: String,
    pub user_id: String,
    pub token_hash: String,
    pub command_hash: String,
    pub expires_at: String,
    pub used_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct SessionMessageRow {
    pub id: String,
    pub user_id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub metadata_json: String,
    pub command_id: Option<String>,
    pub sequence: i64,
    pub retention_expires_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RetrievalItemRow {
    pub id: String,
    pub user_id: String,
    pub session_id: String,
    pub message_id: Option<String>,
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    pub score: Option<f64>,
    pub content_hash: String,
    pub r2_key: Option<String>,
    pub retention_expires_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PhoneChangeRow {
    pub cursor: i64,
    pub user_id: String,
    pub entity_type: String,
    pub entity_id: String,
    pub session_id: Option<String>,
    pub version: i64,
    pub created_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct OutboxEventRow {
    pub id: String,
    pub user_id: Option<String>,
    pub topic: String,
    pub aggregate_id: String,
    pub payload_json: String,
    pub idempotency_key: String,
    pub state: String,
    pub attempts: i32,
    pub next_attempt_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ActionAttemptRow {
    pub id: String,
    pub user_id: Option<String>,
    pub command_id: Option<String>,
    pub action_id: Option<String>,
    pub provider: String,
    pub provider_idempotency_key: String,
    pub state: String,
    pub request_hash: String,
    pub response_json: Option<String>,
    pub attempts: i32,
    pub next_attempt_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Lightweight cursor row used by the phone SSE transport. The session table
/// is the source of truth, so every agent progress update and phone action
/// automatically becomes observable without duplicating business events.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct SessionStreamRow {
    pub id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuditRow {
    pub id: String,
    pub action: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub metadata_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PushRow {
    pub id: String,
    pub session_id: Option<String>,
    pub title: String,
    pub body: String,
    pub voice_script: Option<String>,
    pub created_at: String,
    pub read_at: Option<String>,
    pub dismissed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthCredentials {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub label: String,
    pub host_label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PairingCodeRequest {
    pub ttl_sec: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PairingClaimRequest {
    pub code: String,
    pub label: String,
    pub host_label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionRequest {
    pub skill_id: String,
    pub session_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub title: Option<String>,
    pub chat_id: Option<String>,
    #[serde(default)]
    pub facts: Option<Map<String, Value>>,
    #[serde(default)]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
pub struct ProgressRequest {
    pub status: String,
    pub message: Option<String>,
    pub percent: Option<f64>,
    #[serde(default)]
    pub facts: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InlineAction {
    pub id: String,
    pub risk: Option<String>,
    pub confirm: Option<bool>,
    pub title: Option<String>,
    pub payload: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ActionInput {
    Key(String),
    Definition(InlineAction),
}

#[derive(Debug, Deserialize)]
pub struct EventRequest {
    pub status: String,
    pub summary: Option<String>,
    #[serde(default)]
    pub facts: Option<Map<String, Value>>,
    #[serde(default)]
    pub actions: Option<Vec<ActionInput>>,
    pub idempotency_key: String,
    pub force_push: Option<bool>,
    #[serde(default)]
    pub retrievals: Option<Vec<RetrievalInput>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RetrievalInput {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    pub score: Option<f64>,
    pub content_hash: String,
    pub r2_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ActionResultRequest {
    pub ok: bool,
    pub message: Option<String>,
    #[serde(default)]
    pub output: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct DeviceRequest {
    pub platform: String,
    pub push_token: Option<String>,
    pub locale: Option<String>,
    pub timezone: Option<String>,
    pub device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PhoneReplyRequest {
    pub action_key: String,
    pub utterance: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PhoneConfirmRequest {
    pub action_id: String,
    pub confirm: bool,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PhoneSessionUpdateRequest {
    pub title: Option<String>,
    pub archived: Option<bool>,
}

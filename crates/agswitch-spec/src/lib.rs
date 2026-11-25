use std::collections::BTreeMap;

use agswitch_primitives::{AccountId, AccountTier, ModelFamily, RuleId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Every command supported by `Engine::execute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandId {
    GetDashboard,
    SwitchNext,
    SwitchToProfile,
    OpenAlongside,
    StopProfile,
    AddAccount,
    UpdateAccount,
    DeleteAccount,
    RestoreDefault,
    ToggleCooldown,
    SetSharedChats,
    ResetLogin,
    ListAudit,
    RefreshTelemetry,
}

impl CommandId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GetDashboard => "get_dashboard",
            Self::SwitchNext => "switch_next",
            Self::SwitchToProfile => "switch_to_profile",
            Self::OpenAlongside => "open_alongside",
            Self::StopProfile => "stop_profile",
            Self::AddAccount => "add_account",
            Self::UpdateAccount => "update_account",
            Self::DeleteAccount => "delete_account",
            Self::RestoreDefault => "restore_default",
            Self::ToggleCooldown => "toggle_cooldown",
            Self::SetSharedChats => "set_shared_chats",
            Self::ResetLogin => "reset_login",
            Self::ListAudit => "list_audit",
            Self::RefreshTelemetry => "refresh_telemetry",
        }
    }
}

/// Access policy checked by `CommandRegistry` before command execution (Rule R5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessPolicy {
    LocalUser,
    DestructiveLocalUser,
}

/// Caller metadata passed to `Engine::execute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerContext {
    pub local_trusted: bool,
    pub confirmed_destructive: bool,
}

impl CallerContext {
    pub const fn local_interactive() -> Self {
        Self {
            local_trusted: true,
            confirmed_destructive: false,
        }
    }

    pub const fn local_confirmed() -> Self {
        Self {
            local_trusted: true,
            confirmed_destructive: true,
        }
    }
}

/// Configured Antigravity account slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountProfile {
    pub id: AccountId,
    pub display_name: String,
    pub subtitle: String,
    pub accent_hex: String,
    pub is_default_profile: bool,
    pub root_path: String,
}

impl AccountProfile {
    pub fn create_default() -> Self {
        Self {
            id: AccountId::default_id(),
            display_name: "Primary Account".to_owned(),
            subtitle: "Default OS Profile".to_owned(),
            accent_hex: "#6FD8EE".to_owned(),
            is_default_profile: true,
            root_path: String::new(),
        }
    }

    pub fn create_isolated(
        id: AccountId,
        display_name: String,
        subtitle: String,
        accent_hex: String,
        root_path: String,
    ) -> Self {
        Self {
            id,
            display_name,
            subtitle,
            accent_hex,
            is_default_profile: false,
            root_path,
        }
    }

    pub fn initials(&self) -> String {
        let trimmed = self.display_name.trim();
        if trimmed.is_empty() {
            return "AG".to_owned();
        }
        let parts: Vec<&str> = trimmed
            .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
            .filter(|s| !s.is_empty())
            .collect();

        if let (Some(first), Some(second)) = (parts.first(), parts.get(1))
            && let (Some(c1), Some(c2)) = (first.chars().next(), second.chars().next())
        {
            return format!("{}{}", c1.to_ascii_uppercase(), c2.to_ascii_uppercase());
        }

        if let Some(word) = parts.first() {
            let chars: Vec<char> = word.chars().collect();
            if let Some(&first) = chars.first() {
                if chars.len() == 1 {
                    return first.to_ascii_uppercase().to_string();
                }
                if let Some(&last) = chars.last()
                    && last.is_ascii_digit()
                {
                    return format!("{}{}", first.to_ascii_uppercase(), last);
                }
                if let Some(&second) = chars.get(1) {
                    return format!(
                        "{}{}",
                        first.to_ascii_uppercase(),
                        second.to_ascii_uppercase()
                    );
                }
            }
        }
        "AG".to_owned()
    }
}

/// Persisted switcher configuration (`switcher-config.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SwitcherConfig {
    pub profiles_root_dir: String,
    pub antigravity_bin_path: String,
    pub last_active_profile_id: Option<String>,
    pub hide_default_profile: bool,
    pub ignored_profile_paths: Vec<String>,
    pub profiles: Vec<AccountProfile>,
}

/// Recorded active session interval for shared-chat token attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveSessionSpan {
    pub profile_id: String,
    pub start_utc: DateTime<Utc>,
    pub end_utc: DateTime<Utc>,
}

/// Persisted usage heartbeat & cooldown ledger (`usage-ledger.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UsageLedger {
    pub intervals: Vec<ActiveSessionSpan>,
    pub cooldown_until_utc: BTreeMap<String, DateTime<Utc>>,
    pub tier_overrides: BTreeMap<String, AccountTier>,
}

/// Running Antigravity main instance reported by `ProcessPort`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningInstance {
    pub pid: u32,
    pub user_data_dir: Option<String>,
    pub command_line: String,
}

/// OS-specific launch specification produced purely by `L2 domain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub exe_path: String,
    pub arguments: Vec<String>,
    pub env_overrides: BTreeMap<String, String>,
    pub env_removals: Vec<String>,
    pub dirs_to_ensure: Vec<String>,
}

/// Parsed single event from `transcript.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptStep {
    pub timestamp_utc: DateTime<Utc>,
    pub is_user_turn: bool,
    #[serde(default)]
    pub model: Option<String>,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
}

/// Exact per-model usage entry parsed from `PLANNER_RESPONSE.model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SingleModelUsage {
    pub model_name: String,
    pub family: ModelFamily,
    pub calls_5h: u32,
    pub tokens_5h: u64,
    pub calls_7d: u32,
    pub tokens_7d: u64,
}

/// Per-model family usage & 2-pool countdown quota (`100% -> 0%`) across 5h and Weekly (7d) windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsageBreakdown {
    pub gemini_calls_5h: u32,
    pub claude_calls_5h: u32,
    pub gpt_calls_5h: u32,
    pub gemini_calls_7d: u32,
    pub claude_calls_7d: u32,
    pub gpt_calls_7d: u32,
    pub gemini_tokens_7d: u64,
    pub claude_tokens_7d: u64,
    pub gpt_tokens_7d: u64,
    /// Remaining 5-hour quota % for Gemini models (counts DOWN from 100.0 to 0.0).
    pub gemini_remaining_5h_pct: f64,
    /// Remaining Weekly (7d) quota % for Gemini models (counts DOWN from 100.0 to 0.0).
    pub gemini_remaining_7d_pct: f64,
    /// Remaining 5-hour quota % for Claude & GPT models (counts DOWN from 100.0 to 0.0).
    pub claude_gpt_remaining_5h_pct: f64,
    /// Remaining Weekly (7d) quota % for Claude & GPT models (counts DOWN from 100.0 to 0.0).
    pub claude_gpt_remaining_7d_pct: f64,
    /// Exact seconds remaining in the 5-hour rolling window (or manual 5h cooldown) for Gemini pool.
    #[serde(default)]
    pub gemini_reset_5h_secs: Option<u64>,
    /// Exact seconds remaining in the 5-hour rolling window (or manual 5h cooldown) for Claude & GPT pool.
    #[serde(default)]
    pub claude_gpt_reset_5h_secs: Option<u64>,
    /// Estimated 5h window reset countdown for Gemini pool (e.g. `4h 12m`).
    pub gemini_reset_5h_label: String,
    /// Estimated 5h window reset countdown for Claude & GPT pool.
    pub claude_gpt_reset_5h_label: String,
    /// Exact model ID breakdown (e.g. `gemini-4-neon-high`, `claude-sonnet-4-5`, `gpt-5.2-codex`).
    pub by_model: BTreeMap<String, SingleModelUsage>,
}

impl Default for ModelUsageBreakdown {
    fn default() -> Self {
        Self {
            gemini_calls_5h: 0,
            claude_calls_5h: 0,
            gpt_calls_5h: 0,
            gemini_calls_7d: 0,
            claude_calls_7d: 0,
            gpt_calls_7d: 0,
            gemini_tokens_7d: 0,
            claude_tokens_7d: 0,
            gpt_tokens_7d: 0,
            gemini_remaining_5h_pct: 100.0,
            gemini_remaining_7d_pct: 100.0,
            claude_gpt_remaining_5h_pct: 100.0,
            claude_gpt_remaining_7d_pct: 100.0,
            gemini_reset_5h_secs: None,
            claude_gpt_reset_5h_secs: None,
            gemini_reset_5h_label: String::new(),
            claude_gpt_reset_5h_label: String::new(),
            by_model: BTreeMap::new(),
        }
    }
}

/// Transcript events loaded from one conversation log file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptFeed {
    pub fixed_owner_profile_id: Option<String>,
    pub last_write_utc: DateTime<Utc>,
    pub steps: Vec<TranscriptStep>,
}

/// Per-account computed usage metrics (5-hour, 24-hour, and Weekly 7-day).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountUsageMetrics {
    pub profile_id: String,
    pub tier: AccountTier,
    pub turns_5h: u32,
    pub steps_5h: u32,
    pub tokens_5h: u64,
    pub output_tokens_5h: u64,
    pub tokens_24h: u64,
    pub turns_7d: u32,
    pub steps_7d: u32,
    pub tokens_7d: u64,
    pub load_percent_5h: f64,
    pub load_percent_7d: f64,
    pub models: ModelUsageBreakdown,
    pub is_on_cooldown: bool,
    pub reset_in_secs: Option<u64>,
}

/// Aggregate local telemetry across all accounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub by_profile_id: BTreeMap<String, AccountUsageMetrics>,
    pub total_tokens_24h: u64,
    pub total_turns_24h: u32,
    pub total_tokens_7d: u64,
    pub total_turns_7d: u32,
    pub models_7d: ModelUsageBreakdown,
    pub active_context_tokens: u64,
    pub active_context_budget_percent: f64,
    pub active_cache_hit_percent: f64,
}

/// Single server-reported quota bucket from an optional local telemetry bridge (e.g. `ai-usage-tracker`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerQuotaBucket {
    pub remaining_percent: f64,
    pub used_percent: f64,
    #[serde(default)]
    pub reset_at: String,
}

/// Optional server-reported Antigravity quota overlay for one account (read-only from local telemetry feed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalAntigravityQuota {
    pub name: String,
    #[serde(default)]
    pub email: String,
    pub ok: bool,
    #[serde(default)]
    pub gemini_5h: Option<ServerQuotaBucket>,
    #[serde(default)]
    pub gemini_weekly: Option<ServerQuotaBucket>,
    #[serde(default)]
    pub claude_gpt_5h: Option<ServerQuotaBucket>,
    #[serde(default)]
    pub claude_gpt_weekly: Option<ServerQuotaBucket>,
}

/// Generic multi-provider summary row read from a local telemetry bridge (zero credentials).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalProviderSummary {
    pub id: String,
    pub name: String,
    pub ok: bool,
    pub headline: String,
    pub subline: String,
    #[serde(default)]
    pub remaining_percent: Option<f64>,
    pub status_badge: String,
}

/// Optional snapshot from a local telemetry bridge (`http://127.0.0.1:8289/api/usage.json` or JSON file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ExternalTelemetrySnapshot {
    pub connected: bool,
    #[serde(default)]
    pub source_label: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub burn_per_day_usd: Option<f64>,
    #[serde(default)]
    pub runway_days: Option<f64>,
    #[serde(default)]
    pub antigravity_quotas: Vec<ExternalAntigravityQuota>,
    #[serde(default)]
    pub providers: Vec<ExternalProviderSummary>,
}

/// Rich view-model for one account row (shared by CLI and GPUI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountView {
    pub id: String,
    pub display_name: String,
    pub subtitle: String,
    pub initials: String,
    pub accent_hex: String,
    pub root_path: String,
    pub is_default_profile: bool,
    pub is_running: bool,
    pub running_pids: Vec<u32>,
    pub is_sole_active: bool,
    pub is_authenticated: bool,
    pub is_shared_chats: bool,
    pub tier: AccountTier,
    pub tier_override: Option<AccountTier>,
    pub turns_5h: u32,
    pub steps_5h: u32,
    pub tokens_5h: u64,
    pub tokens_24h: u64,
    pub turns_7d: u32,
    pub steps_7d: u32,
    pub tokens_7d: u64,
    pub load_percent_5h: f64,
    pub load_percent_7d: f64,
    pub models: ModelUsageBreakdown,
    pub is_on_cooldown: bool,
    #[serde(default)]
    pub reset_in_secs: Option<u64>,
    pub reset_label: String,
    /// Optional server-side quota overlay matched from the local telemetry bridge.
    #[serde(default)]
    pub server_quota: Option<ExternalAntigravityQuota>,
}

/// Status of an engine-enforced rule surfaced in the UI Rules tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleStatus {
    pub rule: RuleId,
    pub code: String,
    pub title: String,
    pub description: String,
    pub enforced: bool,
}

/// Complete dashboard state returned by `CommandId::GetDashboard`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub antigravity_bin_path: String,
    pub antigravity_bin_exists: bool,
    pub profiles_root_dir: String,
    pub hide_default_profile: bool,
    pub active_names: Vec<String>,
    pub next_account_id: Option<String>,
    pub next_account_name: Option<String>,
    pub active_context_tokens: u64,
    pub context_budget_percent: f64,
    pub active_cache_hit_percent: f64,
    pub total_tokens_24h: u64,
    pub total_turns_24h: u32,
    pub total_tokens_7d: u64,
    pub total_turns_7d: u32,
    pub models_7d: ModelUsageBreakdown,
    pub accounts: Vec<AccountView>,
    pub rules: Vec<RuleStatus>,
    /// Optional multi-provider & server-quota telemetry read from local bridge.
    #[serde(default)]
    pub external_telemetry: Option<ExternalTelemetrySnapshot>,
}

/// Centralized audit outcome recorded by `Engine::execute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Ok,
    Blocked,
    Failed,
    Panicked,
}

/// Single centralized audit trail entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp_utc: DateTime<Utc>,
    pub command: CommandId,
    pub summary: String,
    pub outcome: AuditOutcome,
    pub rule: Option<RuleId>,
}

/// Input payload for creating a new isolated account vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddAccountInput {
    pub display_name: String,
    pub subtitle: String,
    pub accent_hex: Option<String>,
    pub tier_override: Option<AccountTier>,
    pub share_chats_and_skills: bool,
    pub open_and_sign_in: bool,
}

/// Input payload for updating an existing account slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateAccountInput {
    pub profile_id: String,
    pub display_name: String,
    pub subtitle: String,
    pub accent_hex: Option<String>,
    pub tier_override: Option<AccountTier>,
    pub share_chats_and_skills: Option<bool>,
}

/// Input payload for removing or hiding an account slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteAccountInput {
    pub profile_id: String,
    pub delete_disk_directory: bool,
}

/// Unified typed command request dispatched through `Engine::execute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandRequest {
    GetDashboard,
    SwitchNext,
    SwitchToProfile { profile_id: String },
    OpenAlongside { profile_id: String },
    StopProfile { profile_id: String },
    AddAccount(AddAccountInput),
    UpdateAccount(UpdateAccountInput),
    DeleteAccount(DeleteAccountInput),
    RestoreDefault,
    ToggleCooldown { profile_id: String },
    SetSharedChats { profile_id: String, enabled: bool },
    ResetLogin { profile_id: String, launch_alongside: bool },
    ListAudit { limit: usize },
    RefreshTelemetry,
}

impl CommandRequest {
    pub const fn command_id(&self) -> CommandId {
        match self {
            Self::GetDashboard => CommandId::GetDashboard,
            Self::SwitchNext => CommandId::SwitchNext,
            Self::SwitchToProfile { .. } => CommandId::SwitchToProfile,
            Self::OpenAlongside { .. } => CommandId::OpenAlongside,
            Self::StopProfile { .. } => CommandId::StopProfile,
            Self::AddAccount(_) => CommandId::AddAccount,
            Self::UpdateAccount(_) => CommandId::UpdateAccount,
            Self::DeleteAccount(_) => CommandId::DeleteAccount,
            Self::RestoreDefault => CommandId::RestoreDefault,
            Self::ToggleCooldown { .. } => CommandId::ToggleCooldown,
            Self::SetSharedChats { .. } => CommandId::SetSharedChats,
            Self::ResetLogin { .. } => CommandId::ResetLogin,
            Self::ListAudit { .. } => CommandId::ListAudit,
            Self::RefreshTelemetry => CommandId::RefreshTelemetry,
        }
    }
}

/// Unified typed command response returned by `Engine::execute`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum CommandResponse {
    Dashboard(DashboardSnapshot),
    Switched {
        target_id: String,
        target_name: String,
        closed_count: usize,
    },
    OpenedAlongside {
        target_id: String,
        target_name: String,
        focused_existing: bool,
    },
    Stopped {
        profile_id: String,
        closed_count: usize,
    },
    AccountSaved(AccountProfile),
    AccountRemoved {
        profile_id: String,
        hid_default: bool,
        deleted_disk: bool,
    },
    CooldownToggled {
        profile_id: String,
        is_on_cooldown: bool,
    },
    SharedChatsUpdated {
        profile_id: String,
        enabled: bool,
    },
    LoginReset {
        profile_id: String,
    },
    AuditLog(Vec<AuditEntry>),
}

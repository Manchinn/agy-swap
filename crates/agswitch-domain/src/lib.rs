use std::collections::{BTreeMap, BTreeSet};

use agswitch_primitives::{AccountTier, AppError, ModelFamily, RuleId, RuntimeOs};
use agswitch_spec::{
    AccountProfile, AccountUsageMetrics, ActiveSessionSpan, ExternalAntigravityQuota,
    ExternalProviderSummary, ExternalTelemetrySnapshot, LaunchSpec, ModelUsageBreakdown,
    RuleStatus, ServerQuotaBucket, SingleModelUsage, TranscriptFeed, TranscriptStep, UsageLedger,
    UsageSnapshot,
};
use chrono::{DateTime, Duration, Utc};

pub const MOCK_SSH_CONNECTION: &str = "127.0.0.1 1234 127.0.0.1 22";
pub const CONTEXT_SOFT_BUDGET: u64 = 1_000_000;
pub const COOLDOWN_HOURS: i64 = 5;
pub const ACCENT_PALETTE: [&str; 6] = [
    "#6FD8EE", "#9FC8FF", "#34D399", "#FFC48A", "#FF9F88", "#C4B5FD",
];

pub const SHARED_GEMINI_FOLDERS: [&str; 3] = ["antigravity", "config", "skills"];

/// Normalize a filesystem path for cross-platform case-aware/insensitive matching.
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() {
        return String::new();
    }
    let mut normalized = trimmed.replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        // Keep Windows drive root like `C:/` intact
        if normalized.len() == 3 && normalized.as_bytes().get(1) == Some(&b':') {
            break;
        }
        normalized.pop();
    }
    normalized
}

/// Join path segments using the target OS separator without touching host `std::fs`.
pub fn join_os_path(base: &str, segments: &[&str], os: RuntimeOs) -> String {
    let sep = match os {
        RuntimeOs::Windows => "\\",
        RuntimeOs::MacOs | RuntimeOs::Linux => "/",
    };
    let mut out = base
        .trim_end_matches(['/', '\\'])
        .replace(if os == RuntimeOs::Windows { '/' } else { '\\' }, sep);
    for seg in segments {
        let clean = seg.trim_matches(['/', '\\']);
        if clean.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(sep);
        }
        out.push_str(clean);
    }
    out
}

/// Compute `UserData` directory for an isolated profile.
pub fn user_data_dir(profile: &AccountProfile, os: RuntimeOs) -> Option<String> {
    if profile.is_default_profile || profile.root_path.trim().is_empty() {
        return None;
    }
    Some(join_os_path(&profile.root_path, &["UserData"], os))
}

/// Compute OAuth marker file path (for existence-only auth check under Rule R2).
pub fn oauth_marker_path(profile: &AccountProfile, primary_home: &str, os: RuntimeOs) -> String {
    let base = if profile.is_default_profile {
        primary_home
    } else {
        profile.root_path.as_str()
    };
    join_os_path(base, &[".gemini", "jetski-standalone-oauth-token"], os)
}

/// Compute installation marker path for the primary profile fallback check.
pub fn primary_install_marker_path(primary_home: &str, os: RuntimeOs) -> String {
    join_os_path(primary_home, &[".gemini", "antigravity", "installation_id"], os)
}

/// Determine whether a process command line belongs to a child/helper/sidecar rather than the main UI instance.
pub fn is_child_process(command_line: &str) -> bool {
    if command_line.trim().is_empty() {
        return false;
    }
    let lower = command_line.to_ascii_lowercase();
    lower.contains("--type=")
        || lower.contains("--import=")
        || lower.contains("sidecar_sdk")
        || lower.contains("sidecars/")
        || lower.contains("sidecars\\")
        || lower.contains(".cjs")
        || lower.contains(".mjs")
        || lower.contains("antigravity helper")
        || lower.contains("crashpad")
}

/// Extract `--user-data-dir=...` or `--user-data-dir ...` from a command line.
/// Handles `--user-data-dir="/path with spaces"`, `"--user-data-dir=/path with spaces"`,
/// `--user-data-dir "/path with spaces"`, and unquoted paths.
pub fn extract_user_data_dir(command_line: &str) -> Option<String> {
    let lower = command_line.to_ascii_lowercase();
    let flag = "--user-data-dir";
    let start = lower.find(flag)?;
    let enclosing_quote = if start > 0 {
        match command_line.as_bytes().get(start - 1) {
            Some(b'"') => Some('"'),
            Some(b'\'') => Some('\''),
            _ => None,
        }
    } else {
        None
    };

    let rest = command_line.get(start + flag.len()..)?;
    let rest = rest.trim_start();

    let after_sep = if let Some(eq_rest) = rest.strip_prefix('=') {
        eq_rest.trim_start()
    } else {
        rest
    };

    if after_sep.is_empty() {
        return None;
    }

    let raw_val = if let Some(quoted) = after_sep.strip_prefix('"') {
        quoted.split('"').next().unwrap_or_default()
    } else if let Some(quoted) = after_sep.strip_prefix('\'') {
        quoted.split('\'').next().unwrap_or_default()
    } else if let Some(q) = enclosing_quote {
        after_sep.split(q).next().unwrap_or_default()
    } else {
        after_sep.split_whitespace().next().unwrap_or_default()
    };

    let norm = normalize_path(raw_val);
    if norm.is_empty() { None } else { Some(norm) }
}

/// Match a running Antigravity main instance command line to a configured profile id.
pub fn match_profile_id(command_line: &str, profiles: &[AccountProfile], os: RuntimeOs) -> String {
    let Some(extracted) = extract_user_data_dir(command_line) else {
        return profiles
            .iter()
            .find(|p| p.is_default_profile)
            .map_or_else(|| "default".to_owned(), |p| p.id.as_str().to_owned());
    };

    let norm_extracted = normalize_path(&extracted);
    for profile in profiles.iter().filter(|p| !p.is_default_profile) {
        if let Some(ud) = user_data_dir(profile, os)
            && normalize_path(&ud).eq_ignore_ascii_case(&norm_extracted)
        {
            return profile.id.as_str().to_owned();
        }
    }

    format!("untracked:{norm_extracted}")
}

/// Build an OS-appropriate isolated `LaunchSpec` without touching the host OS.
pub fn build_launch_spec(
    profile: &AccountProfile,
    antigravity_bin_path: &str,
    os: RuntimeOs,
) -> LaunchSpec {
    if profile.is_default_profile {
        return LaunchSpec {
            exe_path: antigravity_bin_path.to_owned(),
            arguments: Vec::new(),
            env_overrides: BTreeMap::new(),
            env_removals: vec!["SSH_CONNECTION".to_owned()],
            dirs_to_ensure: Vec::new(),
        };
    }

    let root = join_os_path(&profile.root_path, &[], os);
    let user_data = join_os_path(&root, &["UserData"], os);
    let mut env = BTreeMap::new();
    env.insert("SSH_CONNECTION".to_owned(), MOCK_SSH_CONNECTION.to_owned());

    let dirs = match os {
        RuntimeOs::Windows => {
            let roaming = join_os_path(&root, &["AppData", "Roaming"], os);
            let local = join_os_path(&root, &["AppData", "Local"], os);
            env.insert("USERPROFILE".to_owned(), root.clone());
            env.insert("APPDATA".to_owned(), roaming.clone());
            env.insert("LOCALAPPDATA".to_owned(), local.clone());
            vec![root, roaming, local, user_data.clone()]
        }
        RuntimeOs::MacOs | RuntimeOs::Linux => {
            let xdg_config = join_os_path(&root, &[".config"], os);
            let xdg_data = join_os_path(&root, &[".local", "share"], os);
            let gemini_dir = join_os_path(&root, &[".gemini"], os);
            env.insert("HOME".to_owned(), root.clone());
            env.insert("XDG_CONFIG_HOME".to_owned(), xdg_config.clone());
            env.insert("XDG_DATA_HOME".to_owned(), xdg_data.clone());
            vec![root, xdg_config, xdg_data, gemini_dir, user_data.clone()]
        }
    };

    LaunchSpec {
        exe_path: antigravity_bin_path.to_owned(),
        arguments: vec![format!("--user-data-dir={user_data}")],
        env_overrides: env,
        env_removals: Vec::new(),
        dirs_to_ensure: dirs,
    }
}

/// Generate a unique URL/slug-safe profile ID.
pub fn slugify(raw: &str, existing_ids: &[String]) -> String {
    let existing_lower: BTreeSet<String> = existing_ids
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let mut base = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            base.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !base.is_empty() {
            base.push('-');
            prev_dash = true;
        }
    }
    let mut base = base.trim_matches('-').to_owned();
    if base.is_empty() || base.eq_ignore_ascii_case("default") {
        base = "account".to_owned();
    }

    let mut candidate = base.clone();
    let mut counter: u32 = 2;
    while existing_lower.contains(&candidate) {
        candidate = format!("{base}-{counter}");
        counter = counter.saturating_add(1);
    }
    candidate
}

/// Sanitize a display name into a cross-platform safe folder name.
pub fn sanitize_folder_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "Profile".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Choose the next ready profile in rotation, skipping profiles currently in 5h cooldown.
pub fn choose_next_profile<'a>(
    profiles: &'a [AccountProfile],
    running_ids: &BTreeSet<String>,
    last_active_id: Option<&str>,
    cooldown_ids: &BTreeSet<String>,
) -> Result<&'a AccountProfile, AppError> {
    if profiles.is_empty() {
        return Err(AppError::Validation("no profiles configured".to_owned()));
    }
    if profiles.len() == 1 {
        return profiles
            .first()
            .ok_or_else(|| AppError::Validation("no profiles configured".to_owned()));
    }

    let running_lower: BTreeSet<String> =
        running_ids.iter().map(|s| s.to_ascii_lowercase()).collect();
    let cooldown_lower: BTreeSet<String> = cooldown_ids
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();

    let mut anchor: Option<usize> = None;

    if let Some(last_id) = last_active_id
        && running_lower.contains(&last_id.to_ascii_lowercase())
    {
        anchor = profiles.iter().position(|p| p.id.eq_ignore_case(last_id));
    }

    if anchor.is_none() && !running_lower.is_empty() {
        anchor = profiles
            .iter()
            .position(|p| running_lower.contains(&p.id.as_str().to_ascii_lowercase()));
    }

    if anchor.is_none()
        && let Some(last_id) = last_active_id
    {
        anchor = profiles.iter().position(|p| p.id.eq_ignore_case(last_id));
    }

    let anchor_idx = anchor.unwrap_or(0);
    let len = profiles.len();

    if !cooldown_lower.is_empty() {
        for offset in 1..=len {
            let idx = (anchor_idx + offset) % len;
            if let Some(candidate) = profiles.get(idx)
                && !cooldown_lower.contains(&candidate.id.as_str().to_ascii_lowercase())
            {
                return Ok(candidate);
            }
        }
    }

    let next_idx = (anchor_idx + 1) % len;
    profiles
        .get(next_idx)
        .ok_or_else(|| AppError::Validation("failed to select next profile".to_owned()))
}

/// Record active profile heartbeat spans into the usage ledger.
pub fn record_active_heartbeat(
    ledger: &mut UsageLedger,
    active_ids: &[String],
    now: DateTime<Utc>,
) -> bool {
    let mut changed = false;
    let mut seen = BTreeSet::new();

    for id in active_ids {
        let lower = id.to_ascii_lowercase();
        if !seen.insert(lower) {
            continue;
        }
        let existing_idx = ledger
            .intervals
            .iter()
            .rposition(|span| span.profile_id.eq_ignore_ascii_case(id));

        if let Some(idx) = existing_idx
            && let Some(span) = ledger.intervals.get_mut(idx)
            && now >= span.start_utc
            && (now - span.end_utc) <= Duration::minutes(40)
        {
            if now > span.end_utc {
                span.end_utc = now;
                changed = true;
            }
            continue;
        }

        ledger.intervals.push(ActiveSessionSpan {
            profile_id: id.clone(),
            start_utc: now,
            end_utc: now,
        });
        changed = true;
    }

    let cutoff = now - Duration::days(8);
    let before = ledger.intervals.len();
    ledger.intervals.retain(|s| s.end_utc >= cutoff);
    if ledger.intervals.len() != before {
        changed = true;
    }

    changed
}

/// Active cooldown profile IDs at `now`.
pub fn active_cooldown_ids(ledger: &UsageLedger, now: DateTime<Utc>) -> BTreeSet<String> {
    ledger
        .cooldown_until_utc
        .iter()
        .filter(|(_, until)| **until > now)
        .map(|(k, _)| k.clone())
        .collect()
}

/// Toggle a 5-hour cooldown for `profile_id`. Returns `true` if now on cooldown.
pub fn toggle_cooldown_5h(ledger: &mut UsageLedger, profile_id: &str, now: DateTime<Utc>) -> bool {
    let is_active = ledger
        .cooldown_until_utc
        .get(profile_id)
        .is_some_and(|until| *until > now);

    if is_active {
        ledger.cooldown_until_utc.remove(profile_id);
        false
    } else {
        ledger
            .cooldown_until_utc
            .insert(profile_id.to_owned(), now + Duration::hours(COOLDOWN_HOURS));
        true
    }
}

/// Resolve tier badge from explicit override or display name heuristic.
pub fn resolve_tier(profile: &AccountProfile, ledger: &UsageLedger) -> AccountTier {
    if let Some(tier) = ledger.tier_overrides.get(profile.id.as_str()) {
        return *tier;
    }
    if profile.display_name.to_ascii_lowercase().contains("ultra") {
        AccountTier::Ultra
    } else {
        AccountTier::Pro
    }
}

/// Parse one line from `transcript.jsonl` safely, ignoring malformed or unrelated entries.
pub fn parse_transcript_line(line: &str, fallback_ts: DateTime<Utc>) -> Option<TranscriptStep> {
    if line.trim().is_empty() {
        return None;
    }
    if !line.contains("\"USER_INPUT\"") && !line.contains("\"PLANNER_RESPONSE\"") {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = val.get("type")?.as_str()?;
    let timestamp_utc = val
        .get("created_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map_or(fallback_ts, |dt| dt.with_timezone(&Utc));

    match event_type {
        "USER_INPUT" => Some(TranscriptStep {
            timestamp_utc,
            is_user_turn: true,
            model: None,
            input_tokens: 0,
            cache_read_tokens: 0,
            output_tokens: 0,
        }),
        "PLANNER_RESPONSE" => {
            let model = val
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            let input_tokens = val
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let cache_read_tokens = val
                .get("cache_read_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let output_tokens = val
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            Some(TranscriptStep {
                timestamp_utc,
                is_user_turn: false,
                model,
                input_tokens,
                cache_read_tokens,
                output_tokens,
            })
        }
        _ => None,
    }
}

/// Attribute a shared-transcript step timestamp to a profile via recorded heartbeat intervals.
pub fn attribute_timestamp<'a>(
    intervals: &'a [ActiveSessionSpan],
    ts: DateTime<Utc>,
    fallback: &'a str,
) -> &'a str {
    for span in intervals.iter().rev() {
        if ts >= span.start_utc && ts <= span.end_utc {
            return span.profile_id.as_str();
        }
    }
    let grace = Duration::minutes(2);
    for span in intervals.iter().rev() {
        if ts >= span.start_utc - grace && ts <= span.end_utc + grace {
            return span.profile_id.as_str();
        }
    }
    fallback
}

/// Compute complete usage telemetry (5h, 24h, Weekly 7d, and per-model breakdown) purely from loaded feeds.
pub fn compute_usage_snapshot(
    profiles: &[AccountProfile],
    ledger: &UsageLedger,
    feeds: &[TranscriptFeed],
    now: DateTime<Utc>,
) -> UsageSnapshot {
    #[derive(Default)]
    struct Acc {
        turns_5h: u32,
        steps_5h: u32,
        tokens_5h: u64,
        output_tokens_5h: u64,
        tokens_24h: u64,
        turns_7d: u32,
        steps_7d: u32,
        tokens_7d: u64,
        models: ModelUsageBreakdown,
        oldest_5h_utc: Option<DateTime<Utc>>,
        oldest_gemini_5h_utc: Option<DateTime<Utc>>,
        oldest_claude_gpt_5h_utc: Option<DateTime<Utc>>,
    }

    let cutoff_5h = now - Duration::hours(5);
    let cutoff_24h = now - Duration::hours(24);
    let cutoff_7d = now - Duration::days(7);
    let default_id = profiles
        .iter()
        .find(|p| p.is_default_profile)
        .or_else(|| profiles.first())
        .map_or("default", |p| p.id.as_str());

    let mut accs: BTreeMap<String, Acc> = profiles
        .iter()
        .map(|p| (p.id.as_str().to_owned(), Acc::default()))
        .collect();

    let mut total_tokens_24h: u64 = 0;
    let mut total_turns_24h: u32 = 0;
    let mut total_tokens_7d: u64 = 0;
    let mut total_turns_7d: u32 = 0;
    let mut global_models = ModelUsageBreakdown::default();
    let mut active_context_tokens: u64 = 0;
    let mut active_cache_hit_percent: f64 = 0.0;
    let mut newest_mtime: Option<DateTime<Utc>> = None;

    for feed in feeds {
        let is_newer = newest_mtime.is_none_or(|prev| feed.last_write_utc > prev);
        if is_newer && !feed.steps.is_empty() {
            let last_model = feed.steps.iter().rev().find(|s| {
                !s.is_user_turn && (s.input_tokens + s.cache_read_tokens + s.output_tokens) > 0
            });
            if let Some(lm) = last_model {
                newest_mtime = Some(feed.last_write_utc);
                active_context_tokens = lm
                    .input_tokens
                    .saturating_add(lm.cache_read_tokens)
                    .saturating_add(lm.output_tokens);
                let prompt_tok: u64 = feed
                    .steps
                    .iter()
                    .filter(|s| !s.is_user_turn)
                    .map(|s| s.input_tokens.saturating_add(s.cache_read_tokens))
                    .sum();
                let cache_tok: u64 = feed
                    .steps
                    .iter()
                    .filter(|s| !s.is_user_turn)
                    .map(|s| s.cache_read_tokens)
                    .sum();
                active_cache_hit_percent = ratio_percent_u64(cache_tok, prompt_tok);
            }
        }

        for step in &feed.steps {
            if step.timestamp_utc < cutoff_7d {
                continue;
            }
            let owner = feed
                .fixed_owner_profile_id
                .as_deref()
                .unwrap_or_else(|| {
                    attribute_timestamp(&ledger.intervals, step.timestamp_utc, default_id)
                });

            let acc = if accs.contains_key(owner) {
                accs.get_mut(owner)
            } else {
                accs.get_mut(default_id)
            };
            let Some(acc) = acc else {
                continue;
            };

            let step_total = step
                .input_tokens
                .saturating_add(step.cache_read_tokens)
                .saturating_add(step.output_tokens);

            if step.is_user_turn {
                total_turns_7d = total_turns_7d.saturating_add(1);
                acc.turns_7d = acc.turns_7d.saturating_add(1);
                if step.timestamp_utc >= cutoff_24h {
                    total_turns_24h = total_turns_24h.saturating_add(1);
                }
                if step.timestamp_utc >= cutoff_5h {
                    acc.turns_5h = acc.turns_5h.saturating_add(1);
                    update_oldest(&mut acc.oldest_5h_utc, step.timestamp_utc);
                }
            } else {
                let family = ModelFamily::classify(step.model.as_deref());
                let in_5h = step.timestamp_utc >= cutoff_5h;

                acc.steps_7d = acc.steps_7d.saturating_add(1);
                acc.tokens_7d = acc.tokens_7d.saturating_add(step_total);
                total_tokens_7d = total_tokens_7d.saturating_add(step_total);
                add_model_usage(
                    &mut acc.models,
                    step.model.as_deref(),
                    family,
                    in_5h,
                    step_total,
                );
                add_model_usage(
                    &mut global_models,
                    step.model.as_deref(),
                    family,
                    in_5h,
                    step_total,
                );

                if step.timestamp_utc >= cutoff_24h {
                    acc.tokens_24h = acc.tokens_24h.saturating_add(step_total);
                    total_tokens_24h = total_tokens_24h.saturating_add(step_total);
                }
                if in_5h {
                    acc.steps_5h = acc.steps_5h.saturating_add(1);
                    acc.tokens_5h = acc.tokens_5h.saturating_add(step_total);
                    acc.output_tokens_5h = acc.output_tokens_5h.saturating_add(step.output_tokens);
                    update_oldest(&mut acc.oldest_5h_utc, step.timestamp_utc);
                    match family {
                        ModelFamily::Gemini => {
                            update_oldest(&mut acc.oldest_gemini_5h_utc, step.timestamp_utc);
                        }
                        ModelFamily::Claude | ModelFamily::Gpt => {
                            update_oldest(&mut acc.oldest_claude_gpt_5h_utc, step.timestamp_utc);
                        }
                        ModelFamily::Other => {}
                    }
                }
            }
        }
    }

    let mut by_profile_id = BTreeMap::new();
    for profile in profiles {
        let id = profile.id.as_str();
        let acc = accs.get(id);
        let tier = resolve_tier(profile, ledger);
        let budget_5h = tier.steps_5h_budget();
        let budget_7d = tier.steps_7d_budget();

        let cooldown_until = ledger.cooldown_until_utc.get(id).copied();
        let is_on_cooldown = cooldown_until.is_some_and(|u| u > now);

        let reset_in_secs = if let Some(until) = cooldown_until
            && until > now
        {
            u64::try_from((until - now).num_seconds()).ok()
        } else if let Some(oldest) = acc.and_then(|a| a.oldest_5h_utc) {
            let window_end = oldest + Duration::hours(5);
            if window_end > now {
                u64::try_from((window_end - now).num_seconds()).ok()
            } else {
                None
            }
        } else {
            None
        };

        let steps_5h = acc.map_or(0, |a| a.steps_5h);
        let steps_7d = acc.map_or(0, |a| a.steps_7d);
        let load_percent_5h = if is_on_cooldown {
            100.0
        } else {
            let raw = (f64::from(steps_5h) / f64::from(budget_5h)) * 100.0;
            ((raw * 10.0).round() / 10.0).min(99.0)
        };
        let load_percent_7d = {
            let raw = (f64::from(steps_7d) / f64::from(budget_7d)) * 100.0;
            ((raw * 10.0).round() / 10.0).min(100.0)
        };

        let mut models = acc.map_or_else(ModelUsageBreakdown::default, |a| a.models.clone());
        let cg_budget_5h = tier.claude_gpt_5h_budget();
        let cg_budget_7d = tier.claude_gpt_7d_budget();
        let cg_5h = models.claude_calls_5h.saturating_add(models.gpt_calls_5h);
        let cg_7d = models.claude_calls_7d.saturating_add(models.gpt_calls_7d);

        models.gemini_remaining_5h_pct = if is_on_cooldown {
            0.0
        } else {
            remaining_percent_u32(models.gemini_calls_5h, budget_5h)
        };
        models.gemini_remaining_7d_pct = remaining_percent_u32(models.gemini_calls_7d, budget_7d);
        models.claude_gpt_remaining_5h_pct = if is_on_cooldown {
            0.0
        } else {
            remaining_percent_u32(cg_5h, cg_budget_5h)
        };
        models.claude_gpt_remaining_7d_pct = remaining_percent_u32(cg_7d, cg_budget_7d);
        let g_reset_secs = window_reset_secs(
            cooldown_until,
            acc.and_then(|a| a.oldest_gemini_5h_utc),
            now,
        );
        let cg_reset_secs = window_reset_secs(
            cooldown_until,
            acc.and_then(|a| a.oldest_claude_gpt_5h_utc),
            now,
        );
        models.gemini_reset_5h_secs = g_reset_secs;
        models.claude_gpt_reset_5h_secs = cg_reset_secs;
        models.gemini_reset_5h_label = format_reset_secs(g_reset_secs);
        models.claude_gpt_reset_5h_label = format_reset_secs(cg_reset_secs);

        by_profile_id.insert(
            id.to_owned(),
            AccountUsageMetrics {
                profile_id: id.to_owned(),
                tier,
                turns_5h: acc.map_or(0, |a| a.turns_5h),
                steps_5h,
                tokens_5h: acc.map_or(0, |a| a.tokens_5h),
                output_tokens_5h: acc.map_or(0, |a| a.output_tokens_5h),
                tokens_24h: acc.map_or(0, |a| a.tokens_24h),
                turns_7d: acc.map_or(0, |a| a.turns_7d),
                steps_7d,
                tokens_7d: acc.map_or(0, |a| a.tokens_7d),
                load_percent_5h,
                load_percent_7d,
                models,
                is_on_cooldown,
                reset_in_secs,
            },
        );
    }

    let active_context_budget_percent =
        ratio_percent_u64(active_context_tokens, CONTEXT_SOFT_BUDGET).min(100.0);

    UsageSnapshot {
        by_profile_id,
        total_tokens_24h,
        total_turns_24h,
        total_tokens_7d,
        total_turns_7d,
        models_7d: global_models,
        active_context_tokens,
        active_context_budget_percent,
        active_cache_hit_percent,
    }
}

fn add_model_usage(
    target: &mut ModelUsageBreakdown,
    raw_model: Option<&str>,
    family: ModelFamily,
    in_5h: bool,
    tokens: u64,
) {
    match family {
        ModelFamily::Gemini => {
            target.gemini_calls_7d = target.gemini_calls_7d.saturating_add(1);
            target.gemini_tokens_7d = target.gemini_tokens_7d.saturating_add(tokens);
            if in_5h {
                target.gemini_calls_5h = target.gemini_calls_5h.saturating_add(1);
            }
        }
        ModelFamily::Claude => {
            target.claude_calls_7d = target.claude_calls_7d.saturating_add(1);
            target.claude_tokens_7d = target.claude_tokens_7d.saturating_add(tokens);
            if in_5h {
                target.claude_calls_5h = target.claude_calls_5h.saturating_add(1);
            }
        }
        ModelFamily::Gpt => {
            target.gpt_calls_7d = target.gpt_calls_7d.saturating_add(1);
            target.gpt_tokens_7d = target.gpt_tokens_7d.saturating_add(tokens);
            if in_5h {
                target.gpt_calls_5h = target.gpt_calls_5h.saturating_add(1);
            }
        }
        ModelFamily::Other => {}
    }

    let model_name = raw_model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    let entry = target
        .by_model
        .entry(model_name.to_owned())
        .or_insert_with(|| SingleModelUsage {
            model_name: model_name.to_owned(),
            family,
            calls_5h: 0,
            tokens_5h: 0,
            calls_7d: 0,
            tokens_7d: 0,
        });
    entry.calls_7d = entry.calls_7d.saturating_add(1);
    entry.tokens_7d = entry.tokens_7d.saturating_add(tokens);
    if in_5h {
        entry.calls_5h = entry.calls_5h.saturating_add(1);
        entry.tokens_5h = entry.tokens_5h.saturating_add(tokens);
    }
}

/// Remaining quota percentage counting DOWN from `100.0` to `0.0` (0.1 precision).
fn remaining_percent_u32(used: u32, budget: u32) -> f64 {
    if budget == 0 {
        return 0.0;
    }
    let used_pct = (f64::from(used) / f64::from(budget)) * 100.0;
    let rem = (100.0 - used_pct).clamp(0.0, 100.0);
    (rem * 10.0).round() / 10.0
}

fn window_reset_secs(
    cooldown_until: Option<DateTime<Utc>>,
    oldest_5h: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<u64> {
    if let Some(until) = cooldown_until
        && until > now
    {
        return u64::try_from((until - now).num_seconds()).ok();
    }
    if let Some(oldest) = oldest_5h {
        let window_end = oldest + Duration::hours(5);
        if window_end > now {
            return u64::try_from((window_end - now).num_seconds()).ok();
        }
    }
    None
}

fn update_oldest(slot: &mut Option<DateTime<Utc>>, ts: DateTime<Utc>) {
    if slot.is_none_or(|prev| ts < prev) {
        *slot = Some(ts);
    }
}

/// Safe `u64 / u64 -> percentage (0.1 precision)` without `as` conversions.
fn ratio_percent_u64(num: u64, den: u64) -> f64 {
    if den == 0 {
        return 0.0;
    }
    let scaled = u128::from(num)
        .saturating_mul(1000)
        .saturating_div(u128::from(den));
    let clamped = u32::try_from(scaled.min(u128::from(u32::MAX))).unwrap_or(u32::MAX);
    f64::from(clamped) / 10.0
}

/// Format reset countdown seconds into dot-matrix clock format (`2:23`, `0:11`, or `5:00` when ready).
pub fn format_clock_hm(secs: Option<u64>) -> String {
    let Some(total) = secs else {
        return "5:00".to_owned();
    };
    if total == 0 {
        return "0:00".to_owned();
    }
    let hours = total / 3600;
    let mins = (total % 3600) / 60;
    format!("{hours}:{mins:02}")
}

/// Format reset countdown seconds into compact human text (e.g. `4h 12m` or `18m`).
pub fn format_reset_secs(secs: Option<u64>) -> String {
    let Some(total) = secs else {
        return String::new();
    };
    if total == 0 {
        return String::new();
    }
    let hours = total / 3600;
    let mins = (total % 3600) / 60;
    if hours >= 1 {
        format!("{hours}h {mins}m")
    } else {
        format!("{}m", mins.max(1))
    }
}

/// Format large token counts compactly (`1.4M`, `240K`, `850`).
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let tenths = u32::try_from((n / 100_000).min(u64::from(u32::MAX))).unwrap_or(u32::MAX);
        format!("{:.1}M", f64::from(tenths) / 10.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Build the static list of enforced rules `R1..R5` for UI transparency.
pub fn enforced_rules_status() -> Vec<RuleStatus> {
    RuleId::all()
        .into_iter()
        .map(|rule| RuleStatus {
            rule,
            code: rule.code().to_owned(),
            title: rule.title().to_owned(),
            description: rule.description().to_owned(),
            enforced: true,
        })
        .collect()
}

/// Domain policy enforcement for Rules R1..R5.
pub mod policy {
    use agswitch_primitives::{AppError, RuleId};
    use agswitch_spec::AccountProfile;

    /// Rule R1: never allow disk deletion or login reset against the primary OS profile.
    pub fn check_can_delete_disk(profile: &AccountProfile, delete_disk: bool) -> Result<(), AppError> {
        if profile.is_default_profile && delete_disk {
            return Err(AppError::PolicyBlocked {
                rule: RuleId::R1,
                message: "cannot delete disk files for the primary OS profile; hide it instead"
                    .to_owned(),
            });
        }
        Ok(())
    }

    /// Rule R1: never allow resetting login token / UserData on the primary OS profile.
    pub fn check_can_reset_login(profile: &AccountProfile) -> Result<(), AppError> {
        if profile.is_default_profile {
            return Err(AppError::PolicyBlocked {
                rule: RuleId::R1,
                message: "cannot reset login for primary OS profile; sign out inside Antigravity"
                    .to_owned(),
            });
        }
        Ok(())
    }

    /// Rule R4: require explicit destructive confirmation before deleting disk vaults or resetting logins.
    pub fn check_destructive_confirmed(confirmed: bool, action: &str) -> Result<(), AppError> {
        if !confirmed {
            return Err(AppError::PolicyBlocked {
                rule: RuleId::R4,
                message: format!("destructive action '{action}' requires explicit confirmation"),
            });
        }
        Ok(())
    }

    /// Rule R2: redact any accidental token strings from centralized audit summaries.
    pub fn redact_audit_summary(raw: &str) -> String {
        let mut out = raw.to_owned();
        for needle in ["ya29.", "Bearer ", "oauth_token=", "jetski-standalone-oauth-token"] {
            if out.contains(needle) {
                out = out.replace(needle, "[REDACTED]");
            }
        }
        out
    }
}

/// Pure parser for an optional local multi-provider telemetry JSON payload (e.g. `ai-usage-tracker`
/// `/api/usage.json` or `usage-data.json`). Never accesses network, filesystem, or credentials.
pub fn parse_external_telemetry_json(
    raw: &str,
    source_label: &str,
) -> Option<ExternalTelemetrySnapshot> {
    let val: serde_json::Value = serde_json::from_str(raw).ok()?;
    let obj = val.as_object()?;

    let updated_at = obj
        .get("meta")
        .and_then(|m| m.get("timestamp_bkk").or_else(|| m.get("timestamp_utc")))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let burn_per_day_usd = obj
        .get("burn_rate")
        .and_then(|b| {
            b.get("snapshot")
                .and_then(|s| s.get("burn_per_day"))
                .and_then(serde_json::Value::as_f64)
                .filter(|v| *v > 0.0)
                .or_else(|| {
                    b.get("router_7d_per_day")
                        .and_then(serde_json::Value::as_f64)
                        .filter(|v| *v > 0.0)
                })
        });

    let runway_days = obj.get("burn_rate").and_then(|b| {
        b.get("snapshot")
            .and_then(|s| s.get("runway_days"))
            .and_then(serde_json::Value::as_f64)
            .filter(|v| *v > 0.0)
            .or_else(|| {
                b.get("router_7d_runway_days")
                    .and_then(serde_json::Value::as_f64)
                    .filter(|v| *v > 0.0)
            })
    });

    let mut antigravity_quotas = Vec::new();
    if let Some(arr) = obj
        .get("antigravity_quotas")
        .and_then(serde_json::Value::as_array)
    {
        for item in arr {
            let ok = item
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let name = item
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            let email = item
                .get("email")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            let buckets = item.get("buckets");
            let gemini_5h = buckets.and_then(|b| parse_server_bucket(b.get("gemini_5h")));
            let gemini_weekly = buckets.and_then(|b| parse_server_bucket(b.get("gemini_weekly")));
            let claude_gpt_5h = buckets.and_then(|b| parse_server_bucket(b.get("claude_gpt_5h")));
            let claude_gpt_weekly =
                buckets.and_then(|b| parse_server_bucket(b.get("claude_gpt_weekly")));

            if !name.is_empty() || !email.is_empty() {
                antigravity_quotas.push(ExternalAntigravityQuota {
                    name,
                    email,
                    ok,
                    gemini_5h,
                    gemini_weekly,
                    claude_gpt_5h,
                    claude_gpt_weekly,
                });
            }
        }
    }

    let mut providers = Vec::new();

    // 1. MaxPlus
    if let Some(mp) = obj.get("maxplus").and_then(serde_json::Value::as_object) {
        let credit = mp
            .get("credit_usd")
            .and_then(serde_json::Value::as_f64);
        let cost_24h = mp
            .get("periods")
            .and_then(|p| p.get("24h"))
            .and_then(|p| p.get("total_cost"))
            .and_then(serde_json::Value::as_f64);
        let cost_7d = mp
            .get("periods")
            .and_then(|p| p.get("7d"))
            .and_then(|p| p.get("total_cost"))
            .and_then(serde_json::Value::as_f64);
        let keys_count = mp
            .get("keys")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        let ok = credit.is_some();
        let headline = match credit {
            Some(c) => format!("${c:.2} credit"),
            None => "Unavailable".to_owned(),
        };
        let mut sub_parts = Vec::new();
        if let Some(c24) = cost_24h {
            sub_parts.push(format!("24h ${c24:.2}"));
        }
        if let Some(c7) = cost_7d {
            sub_parts.push(format!("7d ${c7:.2}"));
        }
        if let Some(runway) = runway_days {
            sub_parts.push(format!("{runway:.1}d runway"));
        } else if keys_count > 0 {
            sub_parts.push(format!("{keys_count} keys"));
        }
        let status_badge = match credit {
            Some(c) if c < 3.0 => "LOW",
            Some(c) if c < 10.0 => "WARN",
            Some(_) => "LIVE",
            None => "OFF",
        };
        let rem_pct = credit.map(|c| ((c / 50.0) * 100.0).clamp(0.0, 100.0));
        providers.push(ExternalProviderSummary {
            id: "maxplus".to_owned(),
            name: "MaxPlus".to_owned(),
            ok,
            headline,
            subline: sub_parts.join(" · "),
            remaining_percent: rem_pct,
            status_badge: status_badge.to_owned(),
        });
    }

    // 2. OpenCode Go
    if let Some(oc) = obj.get("opencode_go").and_then(serde_json::Value::as_object) {
        let ok = oc
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let rolling = oc
            .get("rolling")
            .and_then(|r| r.get("percent"))
            .and_then(serde_json::Value::as_f64);
        let weekly = oc
            .get("weekly")
            .and_then(|r| r.get("percent"))
            .and_then(serde_json::Value::as_f64);
        let monthly = oc
            .get("monthly")
            .and_then(|r| r.get("percent"))
            .and_then(serde_json::Value::as_f64);
        let max_used = [rolling, weekly, monthly]
            .into_iter()
            .flatten()
            .fold(0.0_f64, f64::max);
        let remaining = if ok {
            Some((100.0 - max_used).clamp(0.0, 100.0))
        } else {
            None
        };
        let headline = if ok {
            format!(
                "5h {:.0}% · Wk {:.0}%",
                rolling.unwrap_or(0.0),
                weekly.unwrap_or(0.0)
            )
        } else {
            "Offline".to_owned()
        };
        let total_reqs = oc
            .get("local_usage")
            .and_then(|l| l.get("combined"))
            .and_then(|c| c.get("requests"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let subline = format!(
            "Mo {:.0}% used · {} reqs",
            monthly.unwrap_or(0.0),
            total_reqs
        );
        let status_badge = if !ok {
            "OFF"
        } else if max_used >= 90.0 {
            "LOW"
        } else if max_used >= 70.0 {
            "WARN"
        } else {
            "LIVE"
        };
        providers.push(ExternalProviderSummary {
            id: "opencode_go".to_owned(),
            name: "OpenCode Go".to_owned(),
            ok,
            headline,
            subline,
            remaining_percent: remaining,
            status_badge: status_badge.to_owned(),
        });
    }

    // 3. TokenMe
    if let Some(tm) = obj.get("tokenme").and_then(serde_json::Value::as_object) {
        let elapsed = tm
            .get("cycle_elapsed_pct")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
            .clamp(0.0, 100.0);
        let reqs = tm
            .get("combined_cycle")
            .and_then(|c| c.get("requests"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let toks = tm
            .get("combined_cycle")
            .and_then(|c| c.get("total_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let days_left = tm
            .get("dec17_days")
            .and_then(serde_json::Value::as_u64);
        let headline = format!("Cycle {elapsed:.0}% elapsed");
        let mut sub = format!("{reqs} reqs · {}", format_tokens(toks));
        if let Some(d) = days_left {
            sub.push_str(&format!(" · {d}d left"));
        }
        providers.push(ExternalProviderSummary {
            id: "tokenme".to_owned(),
            name: "TokenMe".to_owned(),
            ok: true,
            headline,
            subline: sub,
            remaining_percent: Some((100.0 - elapsed).clamp(0.0, 100.0)),
            status_badge: "LIVE".to_owned(),
        });
    }

    // 4. Firecrawl
    if let Some(fc) = obj.get("firecrawl").and_then(serde_json::Value::as_object) {
        let ok = fc
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let rem = fc
            .get("remaining_credits")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let plan = fc
            .get("plan_credits")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let rem_pct = fc
            .get("remaining_percent")
            .and_then(serde_json::Value::as_f64)
            .map(|v| v.clamp(0.0, 100.0));
        let headline = if ok {
            format!("{rem} / {plan} credits")
        } else {
            "Offline".to_owned()
        };
        let used = fc
            .get("used_credits")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let subline = format!(
            "{used} used · {:.0}% remaining",
            rem_pct.unwrap_or(0.0)
        );
        let status_badge = if !ok {
            "OFF"
        } else if rem_pct.is_some_and(|p| p < 15.0) {
            "LOW"
        } else if rem_pct.is_some_and(|p| p < 35.0) {
            "WARN"
        } else {
            "LIVE"
        };
        providers.push(ExternalProviderSummary {
            id: "firecrawl".to_owned(),
            name: "Firecrawl".to_owned(),
            ok,
            headline,
            subline,
            remaining_percent: rem_pct,
            status_badge: status_badge.to_owned(),
        });
    }

    Some(ExternalTelemetrySnapshot {
        connected: true,
        source_label: source_label.to_owned(),
        updated_at,
        burn_per_day_usd,
        runway_days,
        antigravity_quotas,
        providers,
    })
}

fn parse_server_bucket(raw: Option<&serde_json::Value>) -> Option<ServerQuotaBucket> {
    let obj = raw?.as_object()?;
    let remaining_percent = obj
        .get("remaining_percent")
        .and_then(serde_json::Value::as_f64)
        .or_else(|| {
            obj.get("remaining_fraction")
                .and_then(serde_json::Value::as_f64)
                .map(|f| (f * 100.0).clamp(0.0, 100.0))
        })?
        .clamp(0.0, 100.0);
    let used_percent = obj
        .get("used_percent")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| (100.0 - remaining_percent).clamp(0.0, 100.0))
        .clamp(0.0, 100.0);
    let reset_at = obj
        .get("reset_at")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Some(ServerQuotaBucket {
        remaining_percent,
        used_percent,
        reset_at,
    })
}

/// Match an `AccountProfile` to an external server-side Antigravity quota record (pure & case-insensitive).
pub fn match_external_quota<'a>(
    profile: &AccountProfile,
    quotas: &'a [ExternalAntigravityQuota],
) -> Option<&'a ExternalAntigravityQuota> {
    let sub = profile.subtitle.trim().to_ascii_lowercase();
    let name = profile.display_name.trim().to_ascii_lowercase();

    // 1. Exact email match against subtitle or display_name
    if !sub.is_empty()
        && let Some(found) = quotas
            .iter()
            .find(|q| !q.email.is_empty() && q.email.trim().to_ascii_lowercase() == sub)
    {
        return Some(found);
    }
    if !name.is_empty()
        && let Some(found) = quotas
            .iter()
            .find(|q| !q.email.is_empty() && q.email.trim().to_ascii_lowercase() == name)
    {
        return Some(found);
    }

    // 2. Email substring match (e.g. subtitle contains "Work (dev@example.com)")
    if !sub.is_empty()
        && let Some(found) = quotas
            .iter()
            .find(|q| !q.email.is_empty() && sub.contains(&q.email.trim().to_ascii_lowercase()))
    {
        return Some(found);
    }

    // 3. Strict name match against connection name
    if !name.is_empty()
        && let Some(found) = quotas
            .iter()
            .find(|q| !q.name.is_empty() && q.name.trim().to_ascii_lowercase() == name)
    {
        return Some(found);
    }

    // Strict-match only: never fall back to an unrelated external account
    None
}

/// Format an RFC3339 reset timestamp into a compact human countdown (`5d 18h`, `2h 19m`, `42m`).
pub fn format_reset_countdown_from_iso(iso: &str, now: DateTime<Utc>) -> Option<String> {
    let secs = parse_reset_secs(iso, now)?;
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        Some(format!("{days}d {hours}h"))
    } else if hours > 0 {
        Some(format!("{hours}h {mins}m"))
    } else {
        Some(format!("{}m", mins.max(1)))
    }
}

/// Overlay exact server-side Antigravity quota percentages & reset timers onto a `ModelUsageBreakdown`.
pub fn apply_server_quota_overlay(
    models: &mut ModelUsageBreakdown,
    quota: &ExternalAntigravityQuota,
    now: DateTime<Utc>,
) {
    if !quota.ok {
        return;
    }
    if let Some(b) = &quota.gemini_5h {
        models.gemini_remaining_5h_pct = b.remaining_percent.clamp(0.0, 100.0);
        if let Some(secs) = parse_reset_secs(&b.reset_at, now) {
            models.gemini_reset_5h_secs = Some(secs);
            models.gemini_reset_5h_label = format_reset_secs(Some(secs));
        }
    }
    if let Some(b) = &quota.gemini_weekly {
        models.gemini_remaining_7d_pct = b.remaining_percent.clamp(0.0, 100.0);
    }
    if let Some(b) = &quota.claude_gpt_5h {
        models.claude_gpt_remaining_5h_pct = b.remaining_percent.clamp(0.0, 100.0);
        if let Some(secs) = parse_reset_secs(&b.reset_at, now) {
            models.claude_gpt_reset_5h_secs = Some(secs);
            models.claude_gpt_reset_5h_label = format_reset_secs(Some(secs));
        }
    }
    if let Some(b) = &quota.claude_gpt_weekly {
        models.claude_gpt_remaining_7d_pct = b.remaining_percent.clamp(0.0, 100.0);
    }
}

fn parse_reset_secs(iso: &str, now: DateTime<Utc>) -> Option<u64> {
    let trimmed = iso.trim();
    if trimmed.is_empty() {
        return None;
    }
    let dt = DateTime::parse_from_rfc3339(trimmed).ok()?.with_timezone(&Utc);
    if dt <= now {
        return None;
    }
    u64::try_from((dt - now).num_seconds()).ok()
}

/// Return true if a matched server quota shows any 5h or weekly pool is effectively exhausted (`<= 1%`).
pub fn is_server_quota_exhausted(quota: &ExternalAntigravityQuota) -> bool {
    if !quota.ok {
        return false;
    }
    [
        quota.gemini_5h.as_ref(),
        quota.gemini_weekly.as_ref(),
        quota.claude_gpt_5h.as_ref(),
        quota.claude_gpt_weekly.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|b| b.remaining_percent <= 1.0)
}

#[cfg(test)]
mod tests {
    use agswitch_primitives::{AccountId, AccountTier, RuleId, RuntimeOs};
    use agswitch_spec::{AccountProfile, TranscriptFeed, UsageLedger};
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn cmdline_parser_handles_macos_and_windows_paths() -> Result<(), String> {
        let win_profile = AccountProfile::create_isolated(
            AccountId::new("acc2").map_err(|e| e.to_string())?,
            "Acc2".to_owned(),
            String::new(),
            "#6FD8EE".to_owned(),
            r"C:\AntigravityProfiles\Acc2".to_owned(),
        );
        let mac_profile = AccountProfile::create_isolated(
            AccountId::new("work").map_err(|e| e.to_string())?,
            "Work".to_owned(),
            String::new(),
            "#9FC8FF".to_owned(),
            "/Users/dev/Library/Application Support/AgSwitch/Profiles/Work".to_owned(),
        );
        let profiles = vec![AccountProfile::create_default(), win_profile, mac_profile];

        let win_cmd = r#""C:\App\Antigravity.exe" --user-data-dir="C:\AntigravityProfiles\Acc2\UserData""#;
        assert_eq!(match_profile_id(win_cmd, &profiles, RuntimeOs::Windows), "acc2");

        let mac_cmd = r#"/Applications/Antigravity.app/Contents/MacOS/Antigravity --user-data-dir="/Users/dev/Library/Application Support/AgSwitch/Profiles/Work/UserData""#;
        assert_eq!(match_profile_id(mac_cmd, &profiles, RuntimeOs::MacOs), "work");

        // Whole-argument quoted form (e.g. from OS cmdline formatter with spaces in "Application Support")
        let mac_whole_quoted = r#"/Applications/Antigravity.app/Contents/MacOS/Antigravity "--user-data-dir=/Users/dev/Library/Application Support/AgSwitch/Profiles/Work/UserData""#;
        assert_eq!(
            match_profile_id(mac_whole_quoted, &profiles, RuntimeOs::MacOs),
            "work"
        );

        let default_cmd = "/Applications/Antigravity.app/Contents/MacOS/Antigravity";
        assert_eq!(match_profile_id(default_cmd, &profiles, RuntimeOs::MacOs), "default");

        assert!(is_child_process("/Applications/Antigravity.app/Contents/Frameworks/Antigravity Helper.app --type=renderer"));
        assert!(!is_child_process(mac_cmd));
        Ok(())
    }

    #[test]
    fn launch_spec_builds_isolated_macos_and_windows_envs() -> Result<(), String> {
        let mac_p = AccountProfile::create_isolated(
            AccountId::new("work").map_err(|e| e.to_string())?,
            "Work".to_owned(),
            String::new(),
            "#6FD8EE".to_owned(),
            "/Users/dev/AgSwitch/Work".to_owned(),
        );
        let mac_spec = build_launch_spec(
            &mac_p,
            "/Applications/Antigravity.app/Contents/MacOS/Antigravity",
            RuntimeOs::MacOs,
        );
        assert_eq!(
            mac_spec.env_overrides.get("HOME").map(String::as_str),
            Some("/Users/dev/AgSwitch/Work")
        );
        assert_eq!(
            mac_spec.env_overrides.get("SSH_CONNECTION").map(String::as_str),
            Some(MOCK_SSH_CONNECTION)
        );
        assert_eq!(
            mac_spec.arguments.first().map(String::as_str),
            Some("--user-data-dir=/Users/dev/AgSwitch/Work/UserData")
        );

        let win_p = AccountProfile::create_isolated(
            AccountId::new("acc2").map_err(|e| e.to_string())?,
            "Acc2".to_owned(),
            String::new(),
            "#6FD8EE".to_owned(),
            r"C:\AntigravityProfiles\Acc2".to_owned(),
        );
        let win_spec = build_launch_spec(&win_p, r"C:\App\Antigravity.exe", RuntimeOs::Windows);
        assert_eq!(
            win_spec.env_overrides.get("USERPROFILE").map(String::as_str),
            Some(r"C:\AntigravityProfiles\Acc2")
        );
        assert_eq!(
            win_spec.env_overrides.get("APPDATA").map(String::as_str),
            Some(r"C:\AntigravityProfiles\Acc2\AppData\Roaming")
        );
        Ok(())
    }

    #[test]
    fn rotation_skips_active_cooldown_accounts() -> Result<(), String> {
        let p0 = AccountProfile::create_default();
        let p1 = AccountProfile::create_isolated(
            AccountId::new("work").map_err(|e| e.to_string())?,
            "Work".to_owned(),
            String::new(),
            "#6FD8EE".to_owned(),
            "/tmp/work".to_owned(),
        );
        let p2 = AccountProfile::create_isolated(
            AccountId::new("backup").map_err(|e| e.to_string())?,
            "Backup".to_owned(),
            String::new(),
            "#9FC8FF".to_owned(),
            "/tmp/backup".to_owned(),
        );
        let profiles = vec![p0, p1, p2];
        let running = BTreeSet::from(["default".to_owned()]);
        let cooldown = BTreeSet::from(["work".to_owned()]);

        let next = choose_next_profile(&profiles, &running, Some("default"), &cooldown)
            .map_err(|e| e.to_string())?;
        assert_eq!(next.id.as_str(), "backup");
        Ok(())
    }

    #[test]
    fn transcript_parser_ignores_malformed_lines_and_computes_usage() -> Result<(), String> {
        let now = Utc
            .with_ymd_and_hms(2026, 11, 24, 15, 0, 0)
            .single()
            .ok_or_else(|| "invalid test timestamp".to_owned())?;

        assert!(parse_transcript_line("{corrupt json", now).is_none());
        let user_step = parse_transcript_line(
            r#"{"type":"USER_INPUT","created_at":"2026-11-24T14:00:00Z"}"#,
            now,
        )
        .ok_or_else(|| "expected USER_INPUT step".to_owned())?;
        let model_step = parse_transcript_line(
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-24T14:01:00Z","model":"gemini-4-neon-high","input_tokens":10000,"cache_read_tokens":40000,"output_tokens":2000}"#,
            now,
        )
        .ok_or_else(|| "expected PLANNER_RESPONSE step".to_owned())?;
        let claude_weekly_step = parse_transcript_line(
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-21T10:00:00Z","model":"claude-sonnet-4-5","input_tokens":5000,"cache_read_tokens":0,"output_tokens":1000}"#,
            now,
        )
        .ok_or_else(|| "expected Claude weekly step".to_owned())?;
        let gpt_5h_step = parse_transcript_line(
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-24T13:30:00Z","model":"gpt-5.2-codex","input_tokens":4000,"cache_read_tokens":0,"output_tokens":1000}"#,
            now,
        )
        .ok_or_else(|| "expected GPT 5h step".to_owned())?;
        let expired_step = parse_transcript_line(
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-10T10:00:00Z","model":"claude-opus-4","input_tokens":99999,"cache_read_tokens":0,"output_tokens":1}"#,
            now,
        )
        .ok_or_else(|| "expected expired step".to_owned())?;

        let profiles = vec![AccountProfile::create_default()];
        let ledger = UsageLedger::default();
        let feeds = vec![TranscriptFeed {
            fixed_owner_profile_id: None,
            last_write_utc: now,
            steps: vec![
                expired_step,
                claude_weekly_step,
                gpt_5h_step,
                user_step,
                model_step,
            ],
        }];

        let snap = compute_usage_snapshot(&profiles, &ledger, &feeds, now);
        assert_eq!(snap.total_turns_24h, 1);
        assert_eq!(snap.total_tokens_24h, 57_000);
        assert_eq!(snap.total_turns_7d, 1);
        assert_eq!(snap.total_tokens_7d, 63_000);
        assert_eq!(snap.active_context_tokens, 52_000);
        assert!((snap.active_context_budget_percent - 5.2).abs() < 0.2);

        let metrics = snap
            .by_profile_id
            .get("default")
            .ok_or_else(|| "missing default metrics".to_owned())?;
        assert_eq!(metrics.tier, AccountTier::Pro);
        assert_eq!(metrics.turns_5h, 1);
        assert_eq!(metrics.steps_5h, 2);
        assert_eq!(metrics.steps_7d, 3);
        assert_eq!(metrics.models.gemini_calls_5h, 1);
        assert_eq!(metrics.models.gemini_calls_7d, 1);
        assert_eq!(metrics.models.claude_calls_5h, 0);
        assert_eq!(metrics.models.claude_calls_7d, 1);
        assert_eq!(metrics.models.gpt_calls_5h, 1);
        assert_eq!(metrics.models.gpt_calls_7d, 1);
        assert!(metrics.models.gemini_remaining_5h_pct < 100.0);
        assert!(metrics.models.gemini_remaining_5h_pct > 95.0);
        assert!(metrics.models.claude_gpt_remaining_5h_pct < 100.0);
        assert!(metrics.models.by_model.contains_key("gemini-4-neon-high"));
        assert!(metrics.models.by_model.contains_key("claude-sonnet-4-5"));
        assert!(metrics.models.by_model.contains_key("gpt-5.2-codex"));
        Ok(())
    }

    #[test]
    fn r2_audit_redaction_and_marker_paths() -> Result<(), String> {
        let redacted = policy::redact_audit_summary("failed with Bearer secret and ya29.abc");
        assert!(!redacted.contains("Bearer "));
        assert!(!redacted.contains("ya29."));

        let p = AccountProfile::create_default();
        let res = policy::check_can_delete_disk(&p, true);
        assert!(matches!(
            res,
            Err(AppError::PolicyBlocked { rule: RuleId::R1, .. })
        ));
        Ok(())
    }

    #[test]
    fn external_telemetry_parses_matches_and_overlays_server_quotas() -> Result<(), String> {
        let sample = r#"{
            "meta": {"timestamp_bkk": "2026-11-25 14:00:00 +07"},
            "maxplus": {
                "credit_usd": 24.50,
                "keys": [{"id": "k1"}, {"id": "k2"}],
                "periods": {
                    "24h": {"total_cost": 1.25},
                    "7d": {"total_cost": 8.40}
                }
            },
            "opencode_go": {
                "ok": true,
                "rolling": {"percent": 15.0},
                "weekly": {"percent": 22.0},
                "monthly": {"percent": 10.0},
                "local_usage": {"combined": {"requests": 42}}
            },
            "tokenme": {
                "cycle_elapsed_pct": 40.0,
                "dec17_days": 20,
                "combined_cycle": {"requests": 100, "total_tokens": 500000}
            },
            "firecrawl": {
                "ok": true,
                "remaining_credits": 450,
                "plan_credits": 500,
                "used_credits": 50,
                "remaining_percent": 90.0
            },
            "burn_rate": {
                "snapshot": {"burn_per_day": 2.0, "runway_days": 12.25}
            },
            "antigravity_quotas": [
                {
                    "name": "acc-alpha",
                    "email": "alpha@example.com",
                    "ok": true,
                    "buckets": {
                        "gemini_5h": {"remaining_percent": 72.5, "used_percent": 27.5, "reset_at": "2026-11-25T16:30:00Z"},
                        "gemini_weekly": {"remaining_percent": 88.0, "used_percent": 12.0, "reset_at": ""},
                        "claude_gpt_5h": {"remaining_percent": 0.5, "used_percent": 99.5, "reset_at": "2026-11-25T15:00:00Z"},
                        "claude_gpt_weekly": {"remaining_percent": 65.0, "used_percent": 35.0, "reset_at": ""}
                    }
                }
            ]
        }"#;

        assert!(parse_external_telemetry_json("{bad json", "test").is_none());
        let parsed = parse_external_telemetry_json(sample, "localhost:8289")
            .ok_or_else(|| "expected telemetry parse".to_owned())?;
        assert!(parsed.connected);
        assert_eq!(parsed.providers.len(), 4);
        assert_eq!(parsed.antigravity_quotas.len(), 1);
        assert_eq!(parsed.burn_per_day_usd, Some(2.0));
        assert_eq!(parsed.runway_days, Some(12.25));

        let profile = AccountProfile::create_isolated(
            AccountId::new("alpha").map_err(|e| e.to_string())?,
            "Alpha".to_owned(),
            "alpha@example.com".to_owned(),
            "#6FD8EE".to_owned(),
            "/tmp/alpha".to_owned(),
        );
        let matched = match_external_quota(&profile, &parsed.antigravity_quotas)
            .ok_or_else(|| "expected quota match".to_owned())?;
        assert!(is_server_quota_exhausted(matched));

        // Strict-match guarantee: Primary Account with no matching email must NOT bind to alpha@example.com
        let primary = AccountProfile::create_default();
        assert!(match_external_quota(&primary, &parsed.antigravity_quotas).is_none());

        let now = Utc
            .with_ymd_and_hms(2026, 11, 25, 14, 0, 0)
            .single()
            .ok_or_else(|| "invalid ts".to_owned())?;
        assert_eq!(
            format_reset_countdown_from_iso("2026-12-01T08:00:00Z", now).as_deref(),
            Some("5d 18h")
        );
        assert_eq!(
            format_reset_countdown_from_iso("2026-11-25T16:30:00Z", now).as_deref(),
            Some("2h 30m")
        );
        let mut models = ModelUsageBreakdown::default();
        apply_server_quota_overlay(&mut models, matched, now);
        assert!((models.gemini_remaining_5h_pct - 72.5).abs() < 0.01);
        assert!((models.gemini_remaining_7d_pct - 88.0).abs() < 0.01);
        assert!((models.claude_gpt_remaining_5h_pct - 0.5).abs() < 0.01);
        assert!((models.claude_gpt_remaining_7d_pct - 65.0).abs() < 0.01);
        assert_eq!(models.gemini_reset_5h_secs, Some(9_000));
        assert_eq!(models.claude_gpt_reset_5h_secs, Some(3_600));
        Ok(())
    }
}

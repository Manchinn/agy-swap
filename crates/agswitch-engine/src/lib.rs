use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

pub use agswitch_domain as domain;
use agswitch_primitives::{AccountId, AppError, RuleId, RuntimeOs};
pub use agswitch_spec as spec;
use agswitch_spec::{
    AccessPolicy, AccountProfile, AccountView, AddAccountInput, AuditEntry, AuditOutcome,
    CallerContext, CommandId, CommandRequest, CommandResponse, DashboardSnapshot,
    DeleteAccountInput, ExternalTelemetrySnapshot, LaunchSpec, RunningInstance, SwitcherConfig,
    TranscriptFeed, UpdateAccountInput,
};
use agswitch_store::StateStore;
use chrono::{DateTime, Utc};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Port trait for OS process discovery, launching, focusing, and graceful termination.
pub trait ProcessPort: Send + Sync {
    fn binary_exists(&self, path: &str) -> bool;
    fn list_running_main_instances(&self) -> Result<Vec<RunningInstance>, AppError>;
    fn close_instance(&self, pid: u32) -> BoxFut<'_, Result<(), AppError>>;
    fn close_orphan_sidecars<'a>(&'a self, profile: &'a AccountProfile)
    -> BoxFut<'a, Result<(), AppError>>;
    fn launch_spec(&self, spec: &LaunchSpec) -> Result<(), AppError>;
    fn focus_instance(&self, pid: u32) -> Result<bool, AppError>;
}

/// Port trait for OS directory discovery, symlinks/junctions, OAuth existence checks, and safe removal.
pub trait WorkspaceLinkPort: Send + Sync {
    fn discover_vault_folders(&self, profiles_root: &str) -> Result<Vec<String>, AppError>;
    fn ensure_dirs(&self, dirs: &[String]) -> Result<(), AppError>;
    fn is_authenticated(&self, profile: &AccountProfile) -> bool;
    fn is_shared_mode_enabled(&self, profile: &AccountProfile) -> bool;
    fn enable_shared_mode(&self, profile: &AccountProfile) -> Result<(), AppError>;
    fn disable_shared_mode(&self, profile: &AccountProfile) -> Result<(), AppError>;
    fn reset_profile_login(&self, profile: &AccountProfile) -> Result<(), AppError>;
    fn delete_vault_directory_safely(&self, root_path: &str) -> Result<(), AppError>;
}

/// Port trait for reading recent local conversation transcripts.
pub trait TranscriptPort: Send + Sync {
    fn load_recent_feeds(
        &self,
        profiles: &[AccountProfile],
        now: DateTime<Utc>,
    ) -> Result<Vec<TranscriptFeed>, AppError>;
}

/// Port trait for deterministic UTC time in tests and real wall time in production.
pub trait ClockPort: Send + Sync {
    fn now_utc(&self) -> DateTime<Utc>;
}

/// Optional read-only local telemetry bridge port (e.g. `ai-usage-tracker` on localhost).
/// Never handles OAuth tokens directly (Rule R2 preserved in `agswitch-engine`).
pub trait TelemetryFeedPort: Send + Sync {
    fn load_external_telemetry(&self, force_fresh: bool) -> Option<ExternalTelemetrySnapshot>;
}

/// Default no-op telemetry bridge when external telemetry is disabled or not wired.
#[derive(Debug, Default)]
pub struct NoopTelemetryFeedAdapter;

impl TelemetryFeedPort for NoopTelemetryFeedAdapter {
    fn load_external_telemetry(&self, _force_fresh: bool) -> Option<ExternalTelemetrySnapshot> {
        None
    }
}

/// Default-deny registry mapping every allowed `CommandId` to its `AccessPolicy` (Rule R5).
#[derive(Debug, Clone)]
pub struct CommandRegistry {
    policies: BTreeMap<CommandId, AccessPolicy>,
}

impl CommandRegistry {
    pub fn default_switcher_registry() -> Self {
        let mut policies = BTreeMap::new();
        policies.insert(CommandId::GetDashboard, AccessPolicy::LocalUser);
        policies.insert(CommandId::SwitchNext, AccessPolicy::LocalUser);
        policies.insert(CommandId::SwitchToProfile, AccessPolicy::LocalUser);
        policies.insert(CommandId::OpenAlongside, AccessPolicy::LocalUser);
        policies.insert(CommandId::StopProfile, AccessPolicy::LocalUser);
        policies.insert(CommandId::AddAccount, AccessPolicy::LocalUser);
        policies.insert(CommandId::UpdateAccount, AccessPolicy::LocalUser);
        policies.insert(CommandId::DeleteAccount, AccessPolicy::LocalUser);
        policies.insert(CommandId::RestoreDefault, AccessPolicy::LocalUser);
        policies.insert(CommandId::ToggleCooldown, AccessPolicy::LocalUser);
        policies.insert(CommandId::SetSharedChats, AccessPolicy::LocalUser);
        policies.insert(CommandId::ResetLogin, AccessPolicy::DestructiveLocalUser);
        policies.insert(CommandId::ListAudit, AccessPolicy::LocalUser);
        policies.insert(CommandId::RefreshTelemetry, AccessPolicy::LocalUser);
        Self { policies }
    }

    pub fn empty_for_test() -> Self {
        Self {
            policies: BTreeMap::new(),
        }
    }

    pub fn authorize(&self, id: CommandId, caller: CallerContext) -> Result<(), AppError> {
        let Some(policy) = self.policies.get(&id) else {
            return Err(AppError::Unauthorized(format!(
                "command '{}' is not registered (default-deny Rule R5)",
                id.as_str()
            )));
        };

        if !caller.local_trusted {
            return Err(AppError::Unauthorized(format!(
                "untrusted caller rejected for '{}'",
                id.as_str()
            )));
        }

        if matches!(policy, AccessPolicy::DestructiveLocalUser) && !caller.confirmed_destructive {
            return Err(AppError::PolicyBlocked {
                rule: RuleId::R4,
                message: format!(
                    "destructive command '{}' requires explicit confirmation",
                    id.as_str()
                ),
            });
        }

        Ok(())
    }
}

/// Safe zero-`unsafe` `catch_unwind` future wrapper for Rule R3 panic isolation.
struct CatchUnwindFut<F>(F);

impl<F: Future + Unpin> Future for CatchUnwindFut<F> {
    type Output = Result<F::Output, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let fut = Pin::new(&mut self.0);
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.poll(cx))) {
            Ok(Poll::Ready(out)) => Poll::Ready(Ok(out)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        }
    }
}

/// Configuration defaults injected from the app shell into `Engine`.
#[derive(Debug, Clone)]
pub struct EngineDefaults {
    pub profiles_root_dir: String,
    pub antigravity_bin_path: String,
    pub runtime_os: RuntimeOs,
}

/// Core command execution engine owning policy checks, panic boundaries, and centralized audit logging.
pub struct Engine {
    defaults: EngineDefaults,
    registry: CommandRegistry,
    store: Arc<dyn StateStore>,
    process: Arc<dyn ProcessPort>,
    workspace: Arc<dyn WorkspaceLinkPort>,
    transcripts: Arc<dyn TranscriptPort>,
    clock: Arc<dyn ClockPort>,
    telemetry: Arc<dyn TelemetryFeedPort>,
}

impl Engine {
    pub fn new(
        defaults: EngineDefaults,
        store: Arc<dyn StateStore>,
        process: Arc<dyn ProcessPort>,
        workspace: Arc<dyn WorkspaceLinkPort>,
        transcripts: Arc<dyn TranscriptPort>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self::with_registry(
            defaults,
            CommandRegistry::default_switcher_registry(),
            store,
            process,
            workspace,
            transcripts,
            clock,
        )
    }

    pub fn with_registry(
        defaults: EngineDefaults,
        registry: CommandRegistry,
        store: Arc<dyn StateStore>,
        process: Arc<dyn ProcessPort>,
        workspace: Arc<dyn WorkspaceLinkPort>,
        transcripts: Arc<dyn TranscriptPort>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self {
            defaults,
            registry,
            store,
            process,
            workspace,
            transcripts,
            clock,
            telemetry: Arc::new(NoopTelemetryFeedAdapter),
        }
    }

    /// Attach an optional local telemetry bridge adapter (e.g. HTTP/file reader for `ai-usage-tracker`).
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<dyn TelemetryFeedPort>) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Fixed execution pipeline: policy check -> panic boundary -> handler -> centralized audit trail.
    pub async fn execute(
        &self,
        request: CommandRequest,
        caller: CallerContext,
    ) -> Result<CommandResponse, AppError> {
        let cmd_id = request.command_id();
        let now = self.clock.now_utc();

        if let Err(err) = self.registry.authorize(cmd_id, caller) {
            self.record_audit(
                now,
                cmd_id,
                &err.to_string(),
                AuditOutcome::Blocked,
                extract_rule(&err),
            );
            return Err(err);
        }

        let handler_fut = Box::pin(self.dispatch(request, caller, now));
        match CatchUnwindFut(handler_fut).await {
            Ok(Ok((response, audit_summary))) => {
                if let Some(summary) = audit_summary {
                    self.record_audit(now, cmd_id, &summary, AuditOutcome::Ok, None);
                }
                Ok(response)
            }
            Ok(Err(err)) => {
                let outcome = if matches!(err, AppError::PolicyBlocked { .. } | AppError::Unauthorized(_))
                {
                    AuditOutcome::Blocked
                } else {
                    AuditOutcome::Failed
                };
                self.record_audit(now, cmd_id, &err.to_string(), outcome, extract_rule(&err));
                Err(err)
            }
            Err(()) => {
                let err = AppError::InternalPanic(cmd_id.as_str().to_owned());
                self.record_audit(
                    now,
                    cmd_id,
                    &err.to_string(),
                    AuditOutcome::Panicked,
                    Some(RuleId::R3),
                );
                Err(err)
            }
        }
    }

    async fn dispatch(
        &self,
        request: CommandRequest,
        caller: CallerContext,
        now: DateTime<Utc>,
    ) -> Result<(CommandResponse, Option<String>), AppError> {
        match request {
            CommandRequest::GetDashboard => {
                let snap = self.build_dashboard(now, false)?;
                Ok((CommandResponse::Dashboard(snap), None))
            }
            CommandRequest::RefreshTelemetry => {
                let snap = self.build_dashboard(now, true)?;
                let src = snap
                    .external_telemetry
                    .as_ref()
                    .filter(|t| t.connected)
                    .map_or("offline", |t| t.source_label.as_str());
                let summary = format!("Refreshed external telemetry ({src})");
                Ok((CommandResponse::Dashboard(snap), Some(summary)))
            }
            CommandRequest::ListAudit { limit } => {
                let entries = self.store.load_recent_audits(limit.clamp(1, 200))?;
                Ok((CommandResponse::AuditLog(entries), None))
            }
            CommandRequest::SwitchNext => {
                let cfg = self.load_or_init_config()?;
                let running_map = self.running_pids_by_profile(&cfg)?;
                let running_ids: BTreeSet<String> = running_map.keys().cloned().collect();
                let ledger = self.store.load_ledger()?;
                let mut cooldown_ids = domain::active_cooldown_ids(&ledger, now);
                if let Some(ext) = self.telemetry.load_external_telemetry(false)
                    && ext.connected
                {
                    for p in &cfg.profiles {
                        if let Some(q) = domain::match_external_quota(p, &ext.antigravity_quotas)
                            && domain::is_server_quota_exhausted(q)
                        {
                            cooldown_ids.insert(p.id.as_str().to_owned());
                        }
                    }
                }
                let next = domain::choose_next_profile(
                    &cfg.profiles,
                    &running_ids,
                    cfg.last_active_profile_id.as_deref(),
                    &cooldown_ids,
                )?;
                let target_id = next.id.as_str().to_owned();
                let (resp, summary) = self.switch_to_profile(&target_id, now).await?;
                Ok((resp, Some(summary)))
            }
            CommandRequest::SwitchToProfile { profile_id } => {
                let (resp, summary) = self.switch_to_profile(&profile_id, now).await?;
                Ok((resp, Some(summary)))
            }
            CommandRequest::OpenAlongside { profile_id } => {
                let (resp, summary) = self.open_alongside(&profile_id, now)?;
                Ok((resp, Some(summary)))
            }
            CommandRequest::StopProfile { profile_id } => {
                let closed = self.stop_profile(&profile_id).await?;
                let summary = format!("Stopped profile '{profile_id}' ({closed} window(s))");
                Ok((
                    CommandResponse::Stopped {
                        profile_id,
                        closed_count: closed,
                    },
                    Some(summary),
                ))
            }
            CommandRequest::AddAccount(input) => {
                let (profile, summary) = self.add_account(input, now)?;
                Ok((CommandResponse::AccountSaved(profile), Some(summary)))
            }
            CommandRequest::UpdateAccount(input) => {
                let (profile, summary) = self.update_account(input).await?;
                Ok((CommandResponse::AccountSaved(profile), Some(summary)))
            }
            CommandRequest::DeleteAccount(input) => {
                let (resp, summary) = self.delete_account(input, caller).await?;
                Ok((resp, Some(summary)))
            }
            CommandRequest::RestoreDefault => {
                let (profile, summary) = self.restore_default()?;
                Ok((CommandResponse::AccountSaved(profile), Some(summary)))
            }
            CommandRequest::ToggleCooldown { profile_id } => {
                let cfg = self.load_or_init_config()?;
                let profile = find_profile(&cfg, &profile_id)?;
                let mut ledger = self.store.load_ledger()?;
                let now_cd = domain::toggle_cooldown_5h(&mut ledger, profile.id.as_str(), now);
                self.store.save_ledger(&ledger)?;
                let summary = if now_cd {
                    format!("Marked '{}' on 5h cooldown", profile.display_name)
                } else {
                    format!("Cleared cooldown for '{}'", profile.display_name)
                };
                Ok((
                    CommandResponse::CooldownToggled {
                        profile_id: profile.id.as_str().to_owned(),
                        is_on_cooldown: now_cd,
                    },
                    Some(summary),
                ))
            }
            CommandRequest::SetSharedChats {
                profile_id,
                enabled,
            } => {
                let cfg = self.load_or_init_config()?;
                let profile = find_profile(&cfg, &profile_id)?.clone();
                if !profile.is_default_profile {
                    // Rule R4: stop active instance before mutating directory symlinks/junctions
                    let _ = self.stop_profile(profile.id.as_str()).await?;
                    if enabled {
                        self.workspace.enable_shared_mode(&profile)?;
                    } else {
                        self.workspace.disable_shared_mode(&profile)?;
                    }
                }
                let summary = format!(
                    "{} shared chats for '{}'",
                    if enabled { "Enabled" } else { "Disabled" },
                    profile.display_name
                );
                Ok((
                    CommandResponse::SharedChatsUpdated {
                        profile_id: profile.id.as_str().to_owned(),
                        enabled,
                    },
                    Some(summary),
                ))
            }
            CommandRequest::ResetLogin {
                profile_id,
                launch_alongside,
            } => {
                let cfg = self.load_or_init_config()?;
                let profile = find_profile(&cfg, &profile_id)?.clone();
                domain::policy::check_can_reset_login(&profile)?;
                domain::policy::check_destructive_confirmed(
                    caller.confirmed_destructive,
                    "reset_login",
                )?;
                let _ = self.stop_profile(profile.id.as_str()).await?;
                self.workspace.reset_profile_login(&profile)?;
                if launch_alongside {
                    let _ = self.open_alongside(profile.id.as_str(), now)?;
                } else {
                    let _ = self.switch_to_profile(profile.id.as_str(), now).await?;
                }
                let summary = format!("Reset login vault for '{}'", profile.display_name);
                Ok((
                    CommandResponse::LoginReset {
                        profile_id: profile.id.as_str().to_owned(),
                    },
                    Some(summary),
                ))
            }
        }
    }

    fn load_or_init_config(&self) -> Result<SwitcherConfig, AppError> {
        let mut cfg = self.store.load_config()?.unwrap_or_default();
        let mut dirty = false;

        if cfg.profiles_root_dir.trim().is_empty() {
            cfg.profiles_root_dir = self.defaults.profiles_root_dir.clone();
            dirty = true;
        }
        if cfg.antigravity_bin_path.trim().is_empty() {
            cfg.antigravity_bin_path = self.defaults.antigravity_bin_path.clone();
            dirty = true;
        }

        let has_default = cfg.profiles.iter().any(|p| p.is_default_profile);
        if !has_default && !cfg.hide_default_profile {
            cfg.profiles.insert(0, AccountProfile::create_default());
            dirty = true;
        }

        let ignored: BTreeSet<String> = cfg
            .ignored_profile_paths
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| domain::normalize_path(s).to_ascii_lowercase())
            .collect();

        if let Ok(vault_dirs) = self.workspace.discover_vault_folders(&cfg.profiles_root_dir) {
            for dir in vault_dirs {
                let norm = domain::normalize_path(&dir);
                if ignored.contains(&norm.to_ascii_lowercase()) {
                    continue;
                }
                let tracked = cfg.profiles.iter().any(|p| {
                    !p.is_default_profile
                        && domain::normalize_path(&p.root_path).eq_ignore_ascii_case(&norm)
                });
                if !tracked {
                    let folder_name = norm
                        .rsplit('/')
                        .next()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("Account")
                        .to_owned();
                    let existing_ids: Vec<String> =
                        cfg.profiles.iter().map(|p| p.id.as_str().to_owned()).collect();
                    let slug = domain::slugify(&folder_name, &existing_ids);
                    let palette_idx = cfg.profiles.len() % domain::ACCENT_PALETTE.len();
                    let accent = domain::ACCENT_PALETTE
                        .get(palette_idx)
                        .copied()
                        .unwrap_or("#6FD8EE");
                    let id = AccountId::new(slug)?;
                    cfg.profiles.push(AccountProfile::create_isolated(
                        id,
                        folder_name.clone(),
                        format!("Isolated Vault ({folder_name})"),
                        accent.to_owned(),
                        dir,
                    ));
                    dirty = true;
                }
            }
        }

        if dirty {
            self.store.save_config(&cfg)?;
        }
        Ok(cfg)
    }

    fn running_pids_by_profile(
        &self,
        cfg: &SwitcherConfig,
    ) -> Result<BTreeMap<String, Vec<u32>>, AppError> {
        let mut map: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for inst in self.process.list_running_main_instances()? {
            let id =
                domain::match_profile_id(&inst.command_line, &cfg.profiles, self.defaults.runtime_os);
            map.entry(id).or_default().push(inst.pid);
        }
        Ok(map)
    }

    fn build_dashboard(
        &self,
        now: DateTime<Utc>,
        force_fresh_telemetry: bool,
    ) -> Result<DashboardSnapshot, AppError> {
        let cfg = self.load_or_init_config()?;
        let running_map = self.running_pids_by_profile(&cfg)?;
        let active_ids: Vec<String> = running_map
            .iter()
            .filter(|(_, pids)| !pids.is_empty())
            .map(|(id, _)| id.clone())
            .collect();

        let mut ledger = self.store.load_ledger()?;
        if !active_ids.is_empty() && domain::record_active_heartbeat(&mut ledger, &active_ids, now) {
            let _ = self.store.save_ledger(&ledger);
        }

        let feeds = self
            .transcripts
            .load_recent_feeds(&cfg.profiles, now)
            .unwrap_or_default();
        let usage = domain::compute_usage_snapshot(&cfg.profiles, &ledger, &feeds, now);
        let external_telemetry = self.telemetry.load_external_telemetry(force_fresh_telemetry);
        let mut cooldown_ids = domain::active_cooldown_ids(&ledger, now);
        if let Some(ext) = &external_telemetry
            && ext.connected
        {
            for p in &cfg.profiles {
                if let Some(q) = domain::match_external_quota(p, &ext.antigravity_quotas)
                    && domain::is_server_quota_exhausted(q)
                {
                    cooldown_ids.insert(p.id.as_str().to_owned());
                }
            }
        }
        let running_set: BTreeSet<String> = active_ids.into_iter().collect();

        let next_profile = domain::choose_next_profile(
            &cfg.profiles,
            &running_set,
            cfg.last_active_profile_id.as_deref(),
            &cooldown_ids,
        )
        .ok();

        let running_count = cfg
            .profiles
            .iter()
            .filter(|p| running_map.get(p.id.as_str()).is_some_and(|v| !v.is_empty()))
            .count();

        let active_names: Vec<String> = cfg
            .profiles
            .iter()
            .filter(|p| running_map.get(p.id.as_str()).is_some_and(|v| !v.is_empty()))
            .map(|p| p.display_name.clone())
            .collect();

        let mut accounts = Vec::with_capacity(cfg.profiles.len());
        for p in &cfg.profiles {
            let pids = running_map.get(p.id.as_str()).cloned().unwrap_or_default();
            let is_running = !pids.is_empty();
            let is_authenticated = self.workspace.is_authenticated(p);
            let is_shared_chats = self.workspace.is_shared_mode_enabled(p);
            let metrics = usage.by_profile_id.get(p.id.as_str());
            let tier = metrics.map_or(domain::resolve_tier(p, &ledger), |m| m.tier);
            let mut models = metrics.map_or_else(
                agswitch_spec::ModelUsageBreakdown::default,
                |m| m.models.clone(),
            );
            let server_quota = external_telemetry
                .as_ref()
                .filter(|t| t.connected)
                .and_then(|t| domain::match_external_quota(p, &t.antigravity_quotas))
                .cloned();
            let server_exhausted = server_quota
                .as_ref()
                .is_some_and(domain::is_server_quota_exhausted);
            if let Some(sq) = &server_quota {
                domain::apply_server_quota_overlay(&mut models, sq, now);
            }
            let effective_reset_secs = metrics
                .and_then(|m| m.reset_in_secs)
                .or(models.gemini_reset_5h_secs)
                .or(models.claude_gpt_reset_5h_secs);
            let reset_label = domain::format_reset_secs(effective_reset_secs);

            accounts.push(AccountView {
                id: p.id.as_str().to_owned(),
                display_name: p.display_name.clone(),
                subtitle: p.subtitle.clone(),
                initials: p.initials(),
                accent_hex: p.accent_hex.clone(),
                root_path: p.root_path.clone(),
                is_default_profile: p.is_default_profile,
                is_running,
                running_pids: pids,
                is_sole_active: is_running && running_count == 1,
                is_authenticated,
                is_shared_chats,
                tier,
                tier_override: ledger.tier_overrides.get(p.id.as_str()).copied(),
                turns_5h: metrics.map_or(0, |m| m.turns_5h),
                steps_5h: metrics.map_or(0, |m| m.steps_5h),
                tokens_5h: metrics.map_or(0, |m| m.tokens_5h),
                tokens_24h: metrics.map_or(0, |m| m.tokens_24h),
                turns_7d: metrics.map_or(0, |m| m.turns_7d),
                steps_7d: metrics.map_or(0, |m| m.steps_7d),
                tokens_7d: metrics.map_or(0, |m| m.tokens_7d),
                load_percent_5h: metrics.map_or(0.0, |m| m.load_percent_5h),
                load_percent_7d: metrics.map_or(0.0, |m| m.load_percent_7d),
                models,
                is_on_cooldown: metrics.is_some_and(|m| m.is_on_cooldown) || server_exhausted,
                reset_in_secs: effective_reset_secs,
                reset_label,
                server_quota,
            });
        }

        let bin_exists = self.process.binary_exists(&cfg.antigravity_bin_path);

        Ok(DashboardSnapshot {
            antigravity_bin_path: cfg.antigravity_bin_path,
            antigravity_bin_exists: bin_exists,
            profiles_root_dir: cfg.profiles_root_dir,
            hide_default_profile: cfg.hide_default_profile,
            active_names,
            next_account_id: next_profile.map(|p| p.id.as_str().to_owned()),
            next_account_name: next_profile.map(|p| p.display_name.clone()),
            active_context_tokens: usage.active_context_tokens,
            context_budget_percent: usage.active_context_budget_percent,
            active_cache_hit_percent: usage.active_cache_hit_percent,
            total_tokens_24h: usage.total_tokens_24h,
            total_turns_24h: usage.total_turns_24h,
            total_tokens_7d: usage.total_tokens_7d,
            total_turns_7d: usage.total_turns_7d,
            models_7d: usage.models_7d,
            accounts,
            rules: domain::enforced_rules_status(),
            external_telemetry,
        })
    }

    async fn switch_to_profile(
        &self,
        target_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(CommandResponse, String), AppError> {
        let mut cfg = self.load_or_init_config()?;
        let target = find_profile(&cfg, target_id)?.clone();

        let instances = self.process.list_running_main_instances()?;
        let mut to_close = Vec::new();
        let mut target_pids = Vec::new();

        for inst in instances {
            let owner =
                domain::match_profile_id(&inst.command_line, &cfg.profiles, self.defaults.runtime_os);
            if owner.eq_ignore_ascii_case(target.id.as_str()) {
                target_pids.push(inst.pid);
            } else {
                to_close.push(inst.pid);
            }
        }

        let closed_count = to_close.len();
        for pid in to_close {
            self.process.close_instance(pid).await?;
        }

        for other in cfg
            .profiles
            .iter()
            .filter(|p| !p.is_default_profile && !p.id.eq_ignore_case(target.id.as_str()))
        {
            let _ = self.process.close_orphan_sidecars(other).await;
        }

        if let Some(&first_pid) = target_pids.first() {
            let _ = self.process.focus_instance(first_pid)?;
        } else {
            let spec = domain::build_launch_spec(
                &target,
                &cfg.antigravity_bin_path,
                self.defaults.runtime_os,
            );
            self.workspace.ensure_dirs(&spec.dirs_to_ensure)?;
            self.process.launch_spec(&spec)?;
        }

        cfg.last_active_profile_id = Some(target.id.as_str().to_owned());
        self.store.save_config(&cfg)?;

        let mut ledger = self.store.load_ledger()?;
        if domain::record_active_heartbeat(&mut ledger, &[target.id.as_str().to_owned()], now) {
            let _ = self.store.save_ledger(&ledger);
        }

        let summary = format!(
            "Switched active account to '{}' (closed {} other window(s))",
            target.display_name, closed_count
        );
        Ok((
            CommandResponse::Switched {
                target_id: target.id.as_str().to_owned(),
                target_name: target.display_name,
                closed_count,
            },
            summary,
        ))
    }

    fn open_alongside(
        &self,
        profile_id: &str,
        now: DateTime<Utc>,
    ) -> Result<(CommandResponse, String), AppError> {
        let mut cfg = self.load_or_init_config()?;
        let target = find_profile(&cfg, profile_id)?.clone();
        let running = self.running_pids_by_profile(&cfg)?;

        let focused_existing = if let Some(pids) = running.get(target.id.as_str())
            && let Some(&first_pid) = pids.first()
        {
            let _ = self.process.focus_instance(first_pid)?;
            true
        } else {
            let spec = domain::build_launch_spec(
                &target,
                &cfg.antigravity_bin_path,
                self.defaults.runtime_os,
            );
            self.workspace.ensure_dirs(&spec.dirs_to_ensure)?;
            self.process.launch_spec(&spec)?;
            false
        };

        cfg.last_active_profile_id = Some(target.id.as_str().to_owned());
        self.store.save_config(&cfg)?;

        let mut ledger = self.store.load_ledger()?;
        if domain::record_active_heartbeat(&mut ledger, &[target.id.as_str().to_owned()], now) {
            let _ = self.store.save_ledger(&ledger);
        }

        let summary = if focused_existing {
            format!("Focused running window for '{}'", target.display_name)
        } else {
            format!("Opened '{}' alongside active sessions", target.display_name)
        };
        Ok((
            CommandResponse::OpenedAlongside {
                target_id: target.id.as_str().to_owned(),
                target_name: target.display_name,
                focused_existing,
            },
            summary,
        ))
    }

    async fn stop_profile(&self, profile_id: &str) -> Result<usize, AppError> {
        let cfg = self.load_or_init_config()?;
        let profile = cfg
            .profiles
            .iter()
            .find(|p| p.id.eq_ignore_case(profile_id))
            .cloned();
        let running = self.running_pids_by_profile(&cfg)?;
        let mut closed = 0;

        if let Some(pids) = running.get(profile_id) {
            for &pid in pids {
                self.process.close_instance(pid).await?;
                closed += 1;
            }
        }
        if let Some(p) = profile
            && !p.is_default_profile
        {
            let _ = self.process.close_orphan_sidecars(&p).await;
        }
        Ok(closed)
    }

    fn add_account(
        &self,
        input: AddAccountInput,
        now: DateTime<Utc>,
    ) -> Result<(AccountProfile, String), AppError> {
        let clean_name = input.display_name.trim().to_owned();
        if clean_name.is_empty() {
            return Err(AppError::Validation("account name is required".to_owned()));
        }

        let mut cfg = self.load_or_init_config()?;
        let existing_ids: Vec<String> = cfg
            .profiles
            .iter()
            .map(|p| p.id.as_str().to_owned())
            .collect();
        let slug = domain::slugify(&clean_name, &existing_ids);
        let safe_folder = domain::sanitize_folder_name(&clean_name);

        let mut root_path =
            domain::join_os_path(&cfg.profiles_root_dir, &[&safe_folder], self.defaults.runtime_os);
        let mut suffix: u32 = 2;
        while cfg.profiles.iter().any(|p| {
            domain::normalize_path(&p.root_path).eq_ignore_ascii_case(&domain::normalize_path(&root_path))
        }) {
            let numbered = format!("{safe_folder}_{suffix}");
            root_path = domain::join_os_path(
                &cfg.profiles_root_dir,
                &[&numbered],
                self.defaults.runtime_os,
            );
            suffix = suffix.saturating_add(1);
        }

        let palette_idx = cfg.profiles.len() % domain::ACCENT_PALETTE.len();
        let accent = input.accent_hex.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
            domain::ACCENT_PALETTE
                .get(palette_idx)
                .copied()
                .unwrap_or("#6FD8EE")
                .to_owned()
        });
        let subtitle = input.subtitle.trim().to_owned();
        let profile = AccountProfile::create_isolated(
            AccountId::new(slug)?,
            clean_name,
            subtitle,
            accent,
            root_path.clone(),
        );

        let spec = domain::build_launch_spec(
            &profile,
            &cfg.antigravity_bin_path,
            self.defaults.runtime_os,
        );
        self.workspace.ensure_dirs(&spec.dirs_to_ensure)?;

        if input.share_chats_and_skills {
            self.workspace.enable_shared_mode(&profile)?;
        }

        let norm_new = domain::normalize_path(&root_path);
        cfg.ignored_profile_paths
            .retain(|x| !domain::normalize_path(x).eq_ignore_ascii_case(&norm_new));
        cfg.profiles.push(profile.clone());
        self.store.save_config(&cfg)?;

        if let Some(tier) = input.tier_override {
            let mut ledger = self.store.load_ledger()?;
            ledger
                .tier_overrides
                .insert(profile.id.as_str().to_owned(), tier);
            self.store.save_ledger(&ledger)?;
        }

        if input.open_and_sign_in {
            let _ = self.open_alongside(profile.id.as_str(), now)?;
        }

        let summary = format!("Added account '{}'", profile.display_name);
        Ok((profile, summary))
    }

    async fn update_account(
        &self,
        input: UpdateAccountInput,
    ) -> Result<(AccountProfile, String), AppError> {
        let mut cfg = self.load_or_init_config()?;
        let idx = cfg
            .profiles
            .iter()
            .position(|p| p.id.eq_ignore_case(&input.profile_id))
            .ok_or_else(|| AppError::NotFound(format!("profile '{}'", input.profile_id)))?;

        if let Some(profile) = cfg.profiles.get_mut(idx) {
            if !input.display_name.trim().is_empty() {
                profile.display_name = input.display_name.trim().to_owned();
            }
            profile.subtitle = input.subtitle.trim().to_owned();
            if let Some(accent) = input.accent_hex.filter(|s| !s.trim().is_empty()) {
                profile.accent_hex = accent;
            }
        }

        let updated = cfg
            .profiles
            .get(idx)
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("profile '{}'", input.profile_id)))?;
        self.store.save_config(&cfg)?;

        let mut ledger = self.store.load_ledger()?;
        match input.tier_override {
            Some(tier) => {
                ledger
                    .tier_overrides
                    .insert(updated.id.as_str().to_owned(), tier);
            }
            None => {
                ledger.tier_overrides.remove(updated.id.as_str());
            }
        }
        self.store.save_ledger(&ledger)?;

        if let Some(want_shared) = input.share_chats_and_skills
            && !updated.is_default_profile
            && self.workspace.is_shared_mode_enabled(&updated) != want_shared
        {
            let _ = self.stop_profile(updated.id.as_str()).await?;
            if want_shared {
                self.workspace.enable_shared_mode(&updated)?;
            } else {
                self.workspace.disable_shared_mode(&updated)?;
            }
        }

        let summary = format!("Updated account '{}'", updated.display_name);
        Ok((updated, summary))
    }

    async fn delete_account(
        &self,
        input: DeleteAccountInput,
        caller: CallerContext,
    ) -> Result<(CommandResponse, String), AppError> {
        let mut cfg = self.load_or_init_config()?;
        let profile = find_profile(&cfg, &input.profile_id)?.clone();

        // Rule R1: primary OS home can NEVER be deleted from disk
        domain::policy::check_can_delete_disk(&profile, input.delete_disk_directory)?;

        if input.delete_disk_directory {
            // Rule R4: destructive disk delete requires explicit confirmation
            domain::policy::check_destructive_confirmed(
                caller.confirmed_destructive,
                "delete_disk_directory",
            )?;
        }

        if profile.is_default_profile {
            cfg.hide_default_profile = true;
            cfg.profiles
                .retain(|p| !p.id.eq_ignore_case(profile.id.as_str()));
            if cfg
                .last_active_profile_id
                .as_deref()
                .is_some_and(|id| profile.id.eq_ignore_case(id))
            {
                cfg.last_active_profile_id =
                    cfg.profiles.first().map(|p| p.id.as_str().to_owned());
            }
            self.store.save_config(&cfg)?;
            let summary = format!("Hid primary profile '{}'", profile.display_name);
            return Ok((
                CommandResponse::AccountRemoved {
                    profile_id: profile.id.as_str().to_owned(),
                    hid_default: true,
                    deleted_disk: false,
                },
                summary,
            ));
        }

        // Rule R4: stop running processes for this isolated vault before removal
        let _ = self.stop_profile(profile.id.as_str()).await?;

        let norm_root = domain::normalize_path(&profile.root_path);
        if input.delete_disk_directory && !norm_root.is_empty() {
            self.workspace
                .delete_vault_directory_safely(&profile.root_path)?;
            cfg.ignored_profile_paths
                .retain(|x| !domain::normalize_path(x).eq_ignore_ascii_case(&norm_root));
        } else if !norm_root.is_empty()
            && !cfg
                .ignored_profile_paths
                .iter()
                .any(|x| domain::normalize_path(x).eq_ignore_ascii_case(&norm_root))
        {
            cfg.ignored_profile_paths.push(profile.root_path.clone());
        }

        cfg.profiles
            .retain(|p| !p.id.eq_ignore_case(profile.id.as_str()));
        if cfg
            .last_active_profile_id
            .as_deref()
            .is_some_and(|id| profile.id.eq_ignore_case(id))
        {
            cfg.last_active_profile_id = cfg.profiles.first().map(|p| p.id.as_str().to_owned());
        }
        self.store.save_config(&cfg)?;

        let summary = if input.delete_disk_directory {
            format!("Removed '{}' and deleted local vault", profile.display_name)
        } else {
            format!("Removed '{}' from list (vault preserved)", profile.display_name)
        };
        Ok((
            CommandResponse::AccountRemoved {
                profile_id: profile.id.as_str().to_owned(),
                hid_default: false,
                deleted_disk: input.delete_disk_directory,
            },
            summary,
        ))
    }

    fn restore_default(&self) -> Result<(AccountProfile, String), AppError> {
        let mut cfg = self.load_or_init_config()?;
        cfg.hide_default_profile = false;
        let existing = cfg.profiles.iter().find(|p| p.is_default_profile).cloned();
        let profile = match existing {
            Some(p) => p,
            None => {
                let def = AccountProfile::create_default();
                cfg.profiles.insert(0, def.clone());
                def
            }
        };
        self.store.save_config(&cfg)?;
        Ok((profile, "Restored primary OS profile".to_owned()))
    }

    fn record_audit(
        &self,
        timestamp_utc: DateTime<Utc>,
        command: CommandId,
        raw_summary: &str,
        outcome: AuditOutcome,
        rule: Option<RuleId>,
    ) {
        let entry = AuditEntry {
            timestamp_utc,
            command,
            summary: domain::policy::redact_audit_summary(raw_summary),
            outcome,
            rule,
        };
        let _ = self.store.append_audit(&entry);
    }
}

fn find_profile<'a>(
    cfg: &'a SwitcherConfig,
    profile_id: &str,
) -> Result<&'a AccountProfile, AppError> {
    cfg.profiles
        .iter()
        .find(|p| p.id.eq_ignore_case(profile_id))
        .ok_or_else(|| AppError::NotFound(format!("profile '{profile_id}'")))
}

const fn extract_rule(err: &AppError) -> Option<RuleId> {
    match err {
        AppError::PolicyBlocked { rule, .. } => Some(*rule),
        AppError::Unauthorized(_) => Some(RuleId::R5),
        AppError::InternalPanic(_) => Some(RuleId::R3),
        _ => None,
    }
}

#[cfg(test)]
mod engine_tests {
    use std::sync::Mutex;

    use agswitch_primitives::{AccountTier, RuleId, RuntimeOs};
    use agswitch_spec::{
        AddAccountInput, AuditOutcome, CallerContext, CommandId, CommandRequest, CommandResponse,
        DeleteAccountInput, LaunchSpec, RunningInstance, TranscriptFeed,
    };
    use agswitch_store::MemoryStore;
    use chrono::{TimeZone, Utc};

    use super::*;

    struct FakeProcess {
        running: Mutex<Vec<RunningInstance>>,
        closed: Mutex<Vec<u32>>,
        launched: Mutex<Vec<LaunchSpec>>,
        focused: Mutex<Vec<u32>>,
        panic_on_list: bool,
    }

    impl FakeProcess {
        fn new() -> Self {
            Self {
                running: Mutex::new(Vec::new()),
                closed: Mutex::new(Vec::new()),
                launched: Mutex::new(Vec::new()),
                focused: Mutex::new(Vec::new()),
                panic_on_list: false,
            }
        }
    }

    impl ProcessPort for FakeProcess {
        fn binary_exists(&self, _path: &str) -> bool {
            true
        }

        fn list_running_main_instances(&self) -> Result<Vec<RunningInstance>, AppError> {
            if self.panic_on_list {
                std::panic::resume_unwind(Box::new("simulated adapter panic"));
            }
            let guard = self
                .running
                .lock()
                .map_err(|e| AppError::Integration(e.to_string()))?;
            Ok(guard.clone())
        }

        fn close_instance(&self, pid: u32) -> BoxFut<'_, Result<(), AppError>> {
            Box::pin(async move {
                if let Ok(mut closed) = self.closed.lock() {
                    closed.push(pid);
                }
                if let Ok(mut running) = self.running.lock() {
                    running.retain(|r| r.pid != pid);
                }
                Ok(())
            })
        }

        fn close_orphan_sidecars<'a>(
            &'a self,
            _profile: &'a AccountProfile,
        ) -> BoxFut<'a, Result<(), AppError>> {
            Box::pin(async { Ok(()) })
        }

        fn launch_spec(&self, spec: &LaunchSpec) -> Result<(), AppError> {
            if let Ok(mut launched) = self.launched.lock() {
                launched.push(spec.clone());
            }
            let cmd = if spec.arguments.is_empty() {
                spec.exe_path.clone()
            } else {
                format!("{} {}", spec.exe_path, spec.arguments.join(" "))
            };
            if let Ok(mut running) = self.running.lock() {
                let next_pid = u32::try_from(1000 + running.len()).unwrap_or(1000);
                running.push(RunningInstance {
                    pid: next_pid,
                    user_data_dir: domain::extract_user_data_dir(&cmd),
                    command_line: cmd,
                });
            }
            Ok(())
        }

        fn focus_instance(&self, pid: u32) -> Result<bool, AppError> {
            if let Ok(mut f) = self.focused.lock() {
                f.push(pid);
            }
            Ok(true)
        }
    }

    struct FakeWorkspace {
        shared: Mutex<BTreeSet<String>>,
        deleted_dirs: Mutex<Vec<String>>,
    }

    impl FakeWorkspace {
        fn new() -> Self {
            Self {
                shared: Mutex::new(BTreeSet::new()),
                deleted_dirs: Mutex::new(Vec::new()),
            }
        }
    }

    impl WorkspaceLinkPort for FakeWorkspace {
        fn discover_vault_folders(&self, _profiles_root: &str) -> Result<Vec<String>, AppError> {
            Ok(Vec::new())
        }

        fn ensure_dirs(&self, _dirs: &[String]) -> Result<(), AppError> {
            Ok(())
        }

        fn is_authenticated(&self, _profile: &AccountProfile) -> bool {
            true
        }

        fn is_shared_mode_enabled(&self, profile: &AccountProfile) -> bool {
            if profile.is_default_profile {
                return true;
            }
            self.shared
                .lock()
                .is_ok_and(|s| s.contains(profile.id.as_str()))
        }

        fn enable_shared_mode(&self, profile: &AccountProfile) -> Result<(), AppError> {
            if let Ok(mut s) = self.shared.lock() {
                s.insert(profile.id.as_str().to_owned());
            }
            Ok(())
        }

        fn disable_shared_mode(&self, profile: &AccountProfile) -> Result<(), AppError> {
            if let Ok(mut s) = self.shared.lock() {
                s.remove(profile.id.as_str());
            }
            Ok(())
        }

        fn reset_profile_login(&self, _profile: &AccountProfile) -> Result<(), AppError> {
            Ok(())
        }

        fn delete_vault_directory_safely(&self, root_path: &str) -> Result<(), AppError> {
            if let Ok(mut d) = self.deleted_dirs.lock() {
                d.push(root_path.to_owned());
            }
            Ok(())
        }
    }

    struct FakeTranscripts;
    impl TranscriptPort for FakeTranscripts {
        fn load_recent_feeds(
            &self,
            _profiles: &[AccountProfile],
            _now: DateTime<Utc>,
        ) -> Result<Vec<TranscriptFeed>, AppError> {
            Ok(Vec::new())
        }
    }

    struct FixedClock(DateTime<Utc>);
    impl ClockPort for FixedClock {
        fn now_utc(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn make_engine(
        proc: Arc<FakeProcess>,
        ws: Arc<FakeWorkspace>,
        registry: CommandRegistry,
    ) -> Result<(Engine, Arc<MemoryStore>), String> {
        let now = Utc
            .with_ymd_and_hms(2026, 11, 24, 15, 0, 0)
            .single()
            .ok_or_else(|| "bad clock".to_owned())?;
        let store = Arc::new(MemoryStore::new());
        let engine = Engine::with_registry(
            EngineDefaults {
                profiles_root_dir: "/tmp/agswitch-test/Profiles".to_owned(),
                antigravity_bin_path: "/Applications/Antigravity.app/Contents/MacOS/Antigravity"
                    .to_owned(),
                runtime_os: RuntimeOs::MacOs,
            },
            registry,
            store.clone(),
            proc,
            ws,
            Arc::new(FakeTranscripts),
            Arc::new(FixedClock(now)),
        );
        Ok((engine, store))
    }

    #[tokio::test]
    async fn r1_blocks_primary_disk_delete_and_login_reset() -> Result<(), String> {
        let proc = Arc::new(FakeProcess::new());
        let ws = Arc::new(FakeWorkspace::new());
        let (engine, _store) =
            make_engine(proc, ws, CommandRegistry::default_switcher_registry())?;

        let del_res = engine
            .execute(
                CommandRequest::DeleteAccount(DeleteAccountInput {
                    profile_id: "default".to_owned(),
                    delete_disk_directory: true,
                }),
                CallerContext::local_confirmed(),
            )
            .await;
        assert!(matches!(
            del_res,
            Err(AppError::PolicyBlocked { rule: RuleId::R1, .. })
        ));

        let reset_res = engine
            .execute(
                CommandRequest::ResetLogin {
                    profile_id: "default".to_owned(),
                    launch_alongside: true,
                },
                CallerContext::local_confirmed(),
            )
            .await;
        assert!(matches!(
            reset_res,
            Err(AppError::PolicyBlocked { rule: RuleId::R1, .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn r3_isolates_handler_panic_and_records_audit() -> Result<(), String> {
        let mut proc = FakeProcess::new();
        proc.panic_on_list = true;
        let ws = Arc::new(FakeWorkspace::new());
        let (engine, store) =
            make_engine(Arc::new(proc), ws, CommandRegistry::default_switcher_registry())?;

        let res = engine
            .execute(CommandRequest::GetDashboard, CallerContext::local_interactive())
            .await;
        assert!(matches!(res, Err(AppError::InternalPanic(_))));

        let audits = store.load_recent_audits(10).map_err(|e| e.to_string())?;
        let last = audits.last().ok_or_else(|| "missing audit entry".to_owned())?;
        assert_eq!(last.outcome, AuditOutcome::Panicked);
        assert_eq!(last.rule, Some(RuleId::R3));
        Ok(())
    }

    #[tokio::test]
    async fn r4_requires_confirmation_for_destructive_ops() -> Result<(), String> {
        let proc = Arc::new(FakeProcess::new());
        let ws = Arc::new(FakeWorkspace::new());
        let (engine, _store) =
            make_engine(proc, ws, CommandRegistry::default_switcher_registry())?;

        let _ = engine
            .execute(
                CommandRequest::AddAccount(AddAccountInput {
                    display_name: "Work".to_owned(),
                    subtitle: "work@co.com".to_owned(),
                    accent_hex: None,
                    tier_override: Some(AccountTier::Pro),
                    share_chats_and_skills: true,
                    open_and_sign_in: false,
                }),
                CallerContext::local_interactive(),
            )
            .await
            .map_err(|e| e.to_string())?;

        // Unconfirmed disk delete must be blocked by R4
        let blocked = engine
            .execute(
                CommandRequest::DeleteAccount(DeleteAccountInput {
                    profile_id: "work".to_owned(),
                    delete_disk_directory: true,
                }),
                CallerContext::local_interactive(),
            )
            .await;
        assert!(matches!(
            blocked,
            Err(AppError::PolicyBlocked { rule: RuleId::R4, .. })
        ));

        // Confirmed disk delete succeeds
        let ok = engine
            .execute(
                CommandRequest::DeleteAccount(DeleteAccountInput {
                    profile_id: "work".to_owned(),
                    delete_disk_directory: true,
                }),
                CallerContext::local_confirmed(),
            )
            .await
            .map_err(|e| e.to_string())?;
        assert!(matches!(
            ok,
            CommandResponse::AccountRemoved {
                deleted_disk: true,
                ..
            }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn r5_default_deny_and_centralized_audit_trail() -> Result<(), String> {
        let proc = Arc::new(FakeProcess::new());
        let ws = Arc::new(FakeWorkspace::new());
        let (engine, store) = make_engine(proc, ws, CommandRegistry::empty_for_test())?;

        let denied = engine
            .execute(CommandRequest::SwitchNext, CallerContext::local_interactive())
            .await;
        assert!(matches!(denied, Err(AppError::Unauthorized(_))));

        let audits = store.load_recent_audits(10).map_err(|e| e.to_string())?;
        let last = audits.last().ok_or_else(|| "missing audit".to_owned())?;
        assert_eq!(last.command, CommandId::SwitchNext);
        assert_eq!(last.outcome, AuditOutcome::Blocked);
        assert_eq!(last.rule, Some(RuleId::R5));
        Ok(())
    }

    #[tokio::test]
    async fn switch_next_rotates_and_skips_cooldown() -> Result<(), String> {
        let proc = Arc::new(FakeProcess::new());
        if let Ok(mut r) = proc.running.lock() {
            r.push(RunningInstance {
                pid: 42,
                user_data_dir: None,
                command_line: "/Applications/Antigravity.app/Contents/MacOS/Antigravity".to_owned(),
            });
        }
        let ws = Arc::new(FakeWorkspace::new());
        let (engine, _store) =
            make_engine(proc.clone(), ws, CommandRegistry::default_switcher_registry())?;

        for name in ["Work", "Backup"] {
            let _ = engine
                .execute(
                    CommandRequest::AddAccount(AddAccountInput {
                        display_name: name.to_owned(),
                        subtitle: String::new(),
                        accent_hex: None,
                        tier_override: None,
                        share_chats_and_skills: true,
                        open_and_sign_in: false,
                    }),
                    CallerContext::local_interactive(),
                )
                .await
                .map_err(|e| e.to_string())?;
        }

        // Put Work on 5h cooldown
        let _ = engine
            .execute(
                CommandRequest::ToggleCooldown {
                    profile_id: "work".to_owned(),
                },
                CallerContext::local_interactive(),
            )
            .await
            .map_err(|e| e.to_string())?;

        // Switching next from default must skip Work and open Backup
        let res = engine
            .execute(CommandRequest::SwitchNext, CallerContext::local_interactive())
            .await
            .map_err(|e| e.to_string())?;

        match res {
            CommandResponse::Switched {
                target_id,
                closed_count,
                ..
            } => {
                assert_eq!(target_id, "backup");
                assert_eq!(closed_count, 1);
            }
            other => return Err(format!("unexpected response: {other:?}")),
        }
        Ok(())
    }

    #[tokio::test]
    async fn switch_next_skips_server_quota_exhausted_account() -> Result<(), String> {
        struct FakeTelemetry(ExternalTelemetrySnapshot);
        impl TelemetryFeedPort for FakeTelemetry {
            fn load_external_telemetry(&self, _force_fresh: bool) -> Option<ExternalTelemetrySnapshot> {
                Some(self.0.clone())
            }
        }

        let proc = Arc::new(FakeProcess::new());
        if let Ok(mut r) = proc.running.lock() {
            r.push(RunningInstance {
                pid: 77,
                user_data_dir: None,
                command_line: "/Applications/Antigravity.app/Contents/MacOS/Antigravity".to_owned(),
            });
        }
        let ws = Arc::new(FakeWorkspace::new());
        let (base_engine, _store) =
            make_engine(proc.clone(), ws, CommandRegistry::default_switcher_registry())?;

        let ext = ExternalTelemetrySnapshot {
            connected: true,
            source_label: "mock-bridge".to_owned(),
            updated_at: "2026-11-25 14:00:00".to_owned(),
            burn_per_day_usd: Some(1.5),
            runway_days: Some(10.0),
            antigravity_quotas: vec![agswitch_spec::ExternalAntigravityQuota {
                name: "work".to_owned(),
                email: "work@example.com".to_owned(),
                ok: true,
                gemini_5h: Some(agswitch_spec::ServerQuotaBucket {
                    remaining_percent: 0.0,
                    used_percent: 100.0,
                    reset_at: String::new(),
                }),
                gemini_weekly: None,
                claude_gpt_5h: None,
                claude_gpt_weekly: None,
            }],
            providers: Vec::new(),
        };
        let engine = base_engine.with_telemetry(Arc::new(FakeTelemetry(ext)));

        for (name, sub) in [("Work", "work@example.com"), ("Backup", "backup@example.com")] {
            let _ = engine
                .execute(
                    CommandRequest::AddAccount(AddAccountInput {
                        display_name: name.to_owned(),
                        subtitle: sub.to_owned(),
                        accent_hex: None,
                        tier_override: None,
                        share_chats_and_skills: true,
                        open_and_sign_in: false,
                    }),
                    CallerContext::local_interactive(),
                )
                .await
                .map_err(|e| e.to_string())?;
        }

        let res = engine
            .execute(CommandRequest::SwitchNext, CallerContext::local_interactive())
            .await
            .map_err(|e| e.to_string())?;
        match res {
            CommandResponse::Switched { target_id, .. } => {
                assert_eq!(target_id, "backup");
            }
            other => return Err(format!("unexpected response: {other:?}")),
        }

        let refreshed = engine
            .execute(
                CommandRequest::RefreshTelemetry,
                CallerContext::local_interactive(),
            )
            .await
            .map_err(|e| e.to_string())?;
        match refreshed {
            CommandResponse::Dashboard(snap) => {
                assert!(snap.external_telemetry.is_some_and(|t| t.connected));
            }
            other => return Err(format!("unexpected response: {other:?}")),
        }
        Ok(())
    }
}

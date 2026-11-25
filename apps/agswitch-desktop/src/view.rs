use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agswitch_domain::{format_clock_hm, format_tokens, is_child_process};
use agswitch_engine::Engine;
use agswitch_integrations::{
    default_telemetry_base_url, open_directory_in_file_manager, open_local_url,
};
use agswitch_primitives::{AccountTier, AppError, ModelFamily, PhaseId};
use agswitch_spec::{
    AccountView, AddAccountInput, AuditEntry, AuditOutcome, CallerContext, CommandRequest,
    CommandResponse, DashboardSnapshot, DeleteAccountInput, SingleModelUsage, UpdateAccountInput,
};
use gpui::{
    AnyElement, Context, FocusHandle, Focusable, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window,
    div, px, rgba, size,
};
use serde::{Deserialize, Serialize};

use crate::phosphor::{
    LedMode, LedSpec, PhosphorPool, PhosphorTheme, dot_grid_backdrop, dot_matrix_readout,
    hardware_switch_track, hw_pill, led_housing, segmented_led_bar,
};
use crate::{COMPACT_POPOVER_H, COMPACT_POPOVER_W, FULL_POPOVER_H, FULL_POPOVER_W};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileOrbPref {
    mode: String,
    theme: String,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedOrb {
    label: &'static str,
    mode: LedMode,
    theme: PhosphorTheme,
}

/// Persisted lightweight UI preferences (`ui-prefs.json` inside `data_dir`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UiPrefs {
    #[serde(default)]
    hide_emails: bool,
    #[serde(default)]
    light_theme: bool,
    #[serde(default)]
    compact_mode: bool,
    #[serde(default)]
    profile_orbs: BTreeMap<String, ProfileOrbPref>,
    /// Legacy fallback so existing preferences keep working seamlessly.
    #[serde(default)]
    pinned_orb_mode: Option<String>,
    #[serde(default)]
    pinned_orb_theme: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PopoverPage {
    SplitOverview,
    AppSettings,
    ModelQuotaDetail(String),
    AccountDetail(String),
    AccountForm,
    RulesAndAudit,
    PhosphorLab,
    Providers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Name,
    Email,
}

#[derive(Debug, Clone)]
struct AccountFormState {
    editing_id: Option<String>,
    is_default_profile: bool,
    name: String,
    email: String,
    tier_override: Option<AccountTier>,
    share_chats: bool,
    active_field: FormField,
    error: Option<String>,
}

#[derive(Debug, Clone)]
enum ConfirmAction {
    DeleteAccount {
        profile_id: String,
        display_name: String,
        is_default_profile: bool,
        delete_disk: bool,
    },
    ResetLogin {
        profile_id: String,
        display_name: String,
    },
}

/// S2 — Samo Instrument palette (`#0C0C0E` obsidian, `#08080A` recessed rail, `#F4F4F5` pearl CTA).
struct SamoPalette {
    is_light: bool,
    shell_bg: Hsla,
    rail_bg: Hsla,
    card_bg: Hsla,
    row_bg: Hsla,
    row_active: Hsla,
    row_hover: Hsla,
    btn_dark: Hsla,
    border_outer: Hsla,
    border_hairline: Hsla,
    border_subtle: Hsla,
    text_primary: Hsla,
    text_secondary: Hsla,
    text_muted: Hsla,
    digit_lit: Hsla,
    digit_unlit: Hsla,
    led_unlit: Hsla,
    pearl_bg: Hsla,
    pearl_fg: Hsla,
    accent_cyan: Hsla,
    status_ok: Hsla,
    status_warn: Hsla,
    status_danger: Hsla,
    scrim: Hsla,
}

impl SamoPalette {
    fn for_theme(light: bool) -> Self {
        if light { Self::light() } else { Self::dark() }
    }

    fn dark() -> Self {
        Self {
            is_light: false,
            shell_bg: rgba(0x0C0C_0EFF).into(),
            rail_bg: rgba(0x0808_0AFF).into(),
            card_bg: rgba(0x1111_15FF).into(),
            row_bg: rgba(0xFFFF_FF06).into(),
            row_active: rgba(0xFFFF_FF10).into(),
            row_hover: rgba(0xFFFF_FF0B).into(),
            btn_dark: rgba(0x1818_1DFF).into(),
            border_outer: rgba(0xFFFF_FF26).into(),
            border_hairline: rgba(0xFFFF_FF16).into(),
            border_subtle: rgba(0xFFFF_FF0E).into(),
            text_primary: rgba(0xF4F4_F5FF).into(),
            text_secondary: rgba(0xA1A1_AAFF).into(),
            text_muted: rgba(0x7171_7AFF).into(),
            digit_lit: rgba(0xF4F4_F6FF).into(),
            digit_unlit: rgba(0xFFFF_FF12).into(),
            led_unlit: rgba(0xFFFF_FF18).into(),
            pearl_bg: rgba(0xF4F4_F5FF).into(),
            pearl_fg: rgba(0x0909_0BFF).into(),
            accent_cyan: rgba(0x6FD8_EEFF).into(),
            status_ok: rgba(0x34D3_99FF).into(),
            status_warn: rgba(0xFBBF_24FF).into(),
            status_danger: rgba(0xFF5A_47FF).into(),
            scrim: rgba(0x0000_00BC).into(),
        }
    }

    fn light() -> Self {
        Self {
            is_light: true,
            shell_bg: rgba(0xF5F5_F7FF).into(),
            rail_bg: rgba(0xEBEB_F0FF).into(),
            card_bg: rgba(0xFFFF_FFFF).into(),
            row_bg: rgba(0xFFFF_FFCC).into(),
            row_active: rgba(0xFFFF_FFFF).into(),
            row_hover: rgba(0xFFFF_FFEE).into(),
            btn_dark: rgba(0xE4E4_EAFF).into(),
            border_outer: rgba(0x0000_0028).into(),
            border_hairline: rgba(0x0000_0018).into(),
            border_subtle: rgba(0x0000_0010).into(),
            text_primary: rgba(0x1111_14FF).into(),
            text_secondary: rgba(0x5252_5BFF).into(),
            text_muted: rgba(0x7171_7AFF).into(),
            digit_lit: rgba(0x1111_14FF).into(),
            digit_unlit: rgba(0x0000_0014).into(),
            led_unlit: rgba(0x0000_0018).into(),
            pearl_bg: rgba(0x1111_14FF).into(),
            pearl_fg: rgba(0xF9FA_FBFF).into(),
            accent_cyan: rgba(0x0284_C7FF).into(),
            status_ok: rgba(0x0596_69FF).into(),
            status_warn: rgba(0xD977_06FF).into(),
            status_danger: rgba(0xDC26_26FF).into(),
            scrim: rgba(0x0000_0066).into(),
        }
    }
}

pub struct AgSwitchView {
    engine: Arc<Engine>,
    rt: Arc<tokio::runtime::Runtime>,
    shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
    ui_prefs_path: PathBuf,
    is_sandbox: bool,
    on_close_notify: Option<Box<dyn Fn() + 'static>>,
    phosphor: PhosphorPool,
    focus_handle: FocusHandle,
    page: PopoverPage,
    selected_profile_id: Option<String>,
    snapshot: Option<DashboardSnapshot>,
    audits: Vec<AuditEntry>,
    in_flight_phase: Option<PhaseId>,
    toast_line: Option<String>,
    policy_banner: Option<String>,
    form_state: Option<AccountFormState>,
    confirm_action: Option<ConfirmAction>,
    hide_emails: bool,
    light_theme: bool,
    compact_mode: bool,
    profile_orbs: BTreeMap<String, ResolvedOrb>,
    showcase_mode: LedMode,
    showcase_label: &'static str,
    showcase_theme: PhosphorTheme,
}

impl Focusable for AgSwitchView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AgSwitchView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        engine: Arc<Engine>,
        rt: Arc<tokio::runtime::Runtime>,
        shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
        ui_prefs_path: PathBuf,
        is_sandbox: bool,
        demo_page: Option<String>,
        on_close: impl Fn() + 'static,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);

        let initial_snap = shared_snap.lock().ok().and_then(|g| g.clone());
        let prefs = load_ui_prefs(&ui_prefs_path);
        let mut profile_orbs = BTreeMap::new();
        for (pid, pref) in &prefs.profile_orbs {
            if let Some((label, mode)) = parse_showcase_mode(&pref.mode) {
                let theme = parse_theme_name(&pref.theme).unwrap_or(PhosphorTheme::Red);
                profile_orbs.insert(pid.clone(), ResolvedOrb { label, mode, theme });
            }
        }
        // Migrate legacy global pinned orb onto "default" if per-profile map is empty
        if profile_orbs.is_empty()
            && let Some((label, mode)) = prefs
                .pinned_orb_mode
                .as_deref()
                .and_then(parse_showcase_mode)
        {
            let theme = prefs
                .pinned_orb_theme
                .as_deref()
                .and_then(parse_theme_name)
                .unwrap_or(PhosphorTheme::Red);
            profile_orbs.insert("default".to_owned(), ResolvedOrb { label, mode, theme });
        }

        let first_orb = profile_orbs.values().next().copied();
        let showcase_label = first_orb.map_or("Orbital", |o| o.label);
        let showcase_mode = first_orb.map_or(LedMode::Orbital, |o| o.mode);
        let showcase_theme = first_orb.map_or(PhosphorTheme::Red, |o| o.theme);

        let mut initial_page = PopoverPage::SplitOverview;
        let mut light_theme = prefs.light_theme;
        let mut compact_mode = prefs.compact_mode;
        let mut initial_form = None;
        if let Some(dp) = demo_page.as_deref() {
            match dp {
                "overview" => {
                    light_theme = false;
                    compact_mode = false;
                }
                "overview-light" => {
                    light_theme = true;
                    compact_mode = false;
                }
                "compact" => {
                    light_theme = false;
                    compact_mode = true;
                }
                "compact-light" => {
                    light_theme = true;
                    compact_mode = true;
                }
                "models" => {
                    light_theme = false;
                    compact_mode = false;
                    initial_page = PopoverPage::ModelQuotaDetail("default".to_owned());
                }
                "account" => {
                    light_theme = false;
                    compact_mode = false;
                    initial_page = PopoverPage::AccountDetail("trade".to_owned());
                }
                "edit" => {
                    light_theme = false;
                    compact_mode = false;
                    initial_page = PopoverPage::AccountForm;
                    initial_form = Some(AccountFormState {
                        editing_id: Some("trade".to_owned()),
                        is_default_profile: false,
                        name: "Trade".to_owned(),
                        email: "trade.demo@gmail.com".to_owned(),
                        tier_override: Some(AccountTier::Pro),
                        share_chats: true,
                        active_field: FormField::Name,
                        error: None,
                    });
                }
                "settings" => {
                    light_theme = false;
                    compact_mode = false;
                    initial_page = PopoverPage::AppSettings;
                }
                "orb" => {
                    light_theme = false;
                    compact_mode = false;
                    initial_page = PopoverPage::PhosphorLab;
                }
                "rules" => {
                    light_theme = false;
                    compact_mode = false;
                    initial_page = PopoverPage::RulesAndAudit;
                }
                "providers" => {
                    light_theme = false;
                    compact_mode = false;
                    initial_page = PopoverPage::Providers;
                }
                _ => {}
            }
        }

        let view = Self {
            engine,
            rt,
            shared_snap,
            ui_prefs_path,
            is_sandbox,
            on_close_notify: Some(Box::new(on_close)),
            phosphor: PhosphorPool::new(),
            focus_handle,
            page: initial_page,
            selected_profile_id: None,
            snapshot: initial_snap,
            audits: Vec::new(),
            in_flight_phase: None,
            toast_line: None,
            policy_banner: None,
            form_state: initial_form,
            confirm_action: None,
            hide_emails: prefs.hide_emails,
            light_theme,
            compact_mode,
            profile_orbs,
            showcase_mode,
            showcase_label,
            showcase_theme,
        };

        // Shared 30 FPS PHOSPHOR clock (only ticks while popover is open; 0% CPU when closed)
        cx.spawn(async move |this, cx| {
            let mut last = Instant::now();
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                let now = Instant::now();
                let dt = now.duration_since(last).as_secs_f32();
                last = now;

                let alive = this.update(cx, |v, cx| {
                    if !v.phosphor.paused {
                        v.phosphor.tick(dt);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Immediate + 2s live telemetry refresh while popover is open
        cx.spawn(async move |this, cx| {
            loop {
                let ctx_opt = this
                    .update(cx, |v, _| {
                        (v.engine.clone(), v.rt.clone(), v.shared_snap.clone())
                    })
                    .ok();
                let Some((engine, rt, shared)) = ctx_opt else {
                    break;
                };

                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let dash = rt.block_on(engine.execute(
                            CommandRequest::GetDashboard,
                            CallerContext::local_interactive(),
                        ));
                        let audit = rt.block_on(engine.execute(
                            CommandRequest::ListAudit { limit: 30 },
                            CallerContext::local_interactive(),
                        ));
                        (dash, audit)
                    })
                    .await;

                let alive = this.update(cx, |v, cx| {
                    if let Ok(CommandResponse::Dashboard(snap)) = result.0 {
                        if let Ok(mut guard) = shared.lock() {
                            *guard = Some(snap.clone());
                        }
                        v.snapshot = Some(snap);
                    }
                    if let Ok(CommandResponse::AuditLog(entries)) = result.1 {
                        v.audits = entries;
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(2))
                    .await;
            }
        })
        .detach();

        view
    }

    fn persist_ui_prefs(&self) {
        let profile_orbs = self
            .profile_orbs
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ProfileOrbPref {
                        mode: v.label.to_owned(),
                        theme: v.theme.label().to_owned(),
                    },
                )
            })
            .collect();
        let prefs = UiPrefs {
            hide_emails: self.hide_emails,
            light_theme: self.light_theme,
            compact_mode: self.compact_mode,
            profile_orbs,
            pinned_orb_mode: None,
            pinned_orb_theme: None,
        };
        save_ui_prefs(&self.ui_prefs_path, &prefs);
    }

    fn sync_window_size(&self, window: &mut Window) {
        let use_compact = self.compact_mode && self.page == PopoverPage::SplitOverview;
        let (w, h) = if use_compact {
            (COMPACT_POPOVER_W, COMPACT_POPOVER_H)
        } else {
            (FULL_POPOVER_W, FULL_POPOVER_H)
        };
        window.resize(size(px(w), px(h)));
    }

    fn navigate_to(&mut self, page: PopoverPage, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        self.sync_window_size(window);
        cx.notify();
    }

    fn effective_selected_profile(&self) -> Option<&AccountView> {
        let snap = self.snapshot.as_ref()?;
        self.selected_profile_id
            .as_deref()
            .and_then(|sid| snap.accounts.iter().find(|a| a.id == sid))
            .or_else(|| snap.accounts.iter().find(|a| a.is_running))
            .or_else(|| snap.accounts.first())
    }

    fn resolve_profile_orb(&self, acc: &AccountView, is_compact_row: bool) -> (LedMode, PhosphorTheme) {
        if let Some(custom) = self.profile_orbs.get(&acc.id) {
            return (custom.mode, custom.theme);
        }
        if !acc.is_authenticated {
            (LedMode::Error, PhosphorTheme::Red)
        } else if acc.is_on_cooldown {
            (LedMode::Warning, PhosphorTheme::Amber)
        } else if acc.is_running {
            (LedMode::Pulse, PhosphorTheme::Emerald)
        } else if is_compact_row {
            (LedMode::Verified5, PhosphorTheme::Cyan)
        } else {
            (LedMode::Verified, PhosphorTheme::Cyan)
        }
    }

    fn open_phosphor_lab_for_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sel) = self.effective_selected_profile() {
            let pid = sel.id.clone();
            self.selected_profile_id = Some(pid.clone());
            if let Some(custom) = self.profile_orbs.get(&pid).copied() {
                self.showcase_label = custom.label;
                self.showcase_mode = custom.mode;
                self.showcase_theme = custom.theme;
            }
        }
        self.navigate_to(PopoverPage::PhosphorLab, window, cx);
    }

    fn dismiss_popover(&mut self, window: &mut Window) {
        if let Some(cb) = &self.on_close_notify {
            cb();
        }
        window.remove_window();
    }

    fn run_command(
        &mut self,
        request: CommandRequest,
        caller: CallerContext,
        phase: PhaseId,
        cx: &mut Context<Self>,
    ) {
        if self.in_flight_phase.is_some() {
            return;
        }
        self.in_flight_phase = Some(phase);
        self.policy_banner = None;
        self.toast_line = Some(format!("{}…", phase.label()));
        cx.notify();

        let engine = self.engine.clone();
        let rt = self.rt.clone();
        let shared = self.shared_snap.clone();

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let res = rt.block_on(engine.execute(request, caller));
                    let dash = rt.block_on(engine.execute(
                        CommandRequest::GetDashboard,
                        CallerContext::local_interactive(),
                    ));
                    let audit = rt.block_on(engine.execute(
                        CommandRequest::ListAudit { limit: 30 },
                        CallerContext::local_interactive(),
                    ));
                    (res, dash, audit)
                })
                .await;

            let _ = this.update(cx, |v, cx| {
                v.in_flight_phase = None;
                match outcome.0 {
                    Ok(resp) => {
                        v.toast_line = Some(describe_response(&resp));
                        v.policy_banner = None;
                        if matches!(resp, CommandResponse::AccountRemoved { .. }) {
                            v.page = PopoverPage::SplitOverview;
                        }
                    }
                    Err(err) => {
                        if let AppError::PolicyBlocked { rule, message } = &err {
                            v.policy_banner = Some(format!("Rule {}: {message}", rule.code()));
                        }
                        v.toast_line = Some(err.to_string());
                    }
                }
                if let Ok(CommandResponse::Dashboard(snap)) = outcome.1 {
                    if let Ok(mut guard) = shared.lock() {
                        *guard = Some(snap.clone());
                    }
                    v.snapshot = Some(snap);
                }
                if let Ok(CommandResponse::AuditLog(entries)) = outcome.2 {
                    v.audits = entries;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_add_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next_num = self.snapshot.as_ref().map_or(2, |s| s.accounts.len() + 1);
        self.form_state = Some(AccountFormState {
            editing_id: None,
            is_default_profile: false,
            name: format!("Account {next_num}"),
            email: String::new(),
            tier_override: None,
            share_chats: true,
            active_field: FormField::Email,
            error: None,
        });
        self.page = PopoverPage::AccountForm;
        self.sync_window_size(window);
        self.focus_handle.focus(window);
        cx.notify();
    }

    fn open_edit_form(&mut self, acc: &AccountView, window: &mut Window, cx: &mut Context<Self>) {
        self.form_state = Some(AccountFormState {
            editing_id: Some(acc.id.clone()),
            is_default_profile: acc.is_default_profile,
            name: acc.display_name.clone(),
            email: acc.subtitle.clone(),
            tier_override: acc.tier_override,
            share_chats: acc.is_shared_chats,
            active_field: FormField::Name,
            error: None,
        });
        self.page = PopoverPage::AccountForm;
        self.sync_window_size(window);
        self.focus_handle.focus(window);
        cx.notify();
    }

    fn submit_form(&mut self, open_and_sign_in: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.form_state.clone() else {
            return;
        };
        if form.name.trim().is_empty() {
            if let Some(mut f) = self.form_state.take() {
                f.error = Some("Enter an account name.".to_owned());
                self.form_state = Some(f);
            }
            cx.notify();
            return;
        }
        self.form_state = None;
        self.page = PopoverPage::SplitOverview;
        self.sync_window_size(window);

        if let Some(profile_id) = form.editing_id {
            self.run_command(
                CommandRequest::UpdateAccount(UpdateAccountInput {
                    profile_id,
                    display_name: form.name,
                    subtitle: form.email,
                    accent_hex: None,
                    tier_override: form.tier_override,
                    share_chats_and_skills: if form.is_default_profile {
                        None
                    } else {
                        Some(form.share_chats)
                    },
                }),
                CallerContext::local_interactive(),
                PhaseId::LinkingWorkspace,
                cx,
            );
        } else {
            self.run_command(
                CommandRequest::AddAccount(AddAccountInput {
                    display_name: form.name,
                    subtitle: form.email,
                    accent_hex: None,
                    tier_override: form.tier_override,
                    share_chats_and_skills: form.share_chats,
                    open_and_sign_in,
                }),
                CallerContext::local_interactive(),
                PhaseId::LaunchingTarget,
                cx,
            );
        }
    }

    fn handle_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();

        if key == "escape" {
            if self.confirm_action.is_some() {
                self.confirm_action = None;
                cx.notify();
            } else if self.page != PopoverPage::SplitOverview {
                self.form_state = None;
                self.navigate_to(PopoverPage::SplitOverview, window, cx);
            } else {
                self.dismiss_popover(window);
            }
            return;
        }

        if self.page != PopoverPage::AccountForm {
            return;
        }

        let Some(form) = self.form_state.as_mut() else {
            return;
        };

        if key == "tab" {
            form.active_field = match form.active_field {
                FormField::Name => FormField::Email,
                FormField::Email => FormField::Name,
            };
            cx.notify();
            return;
        }

        if key == "enter" {
            let is_new = form.editing_id.is_none();
            self.submit_form(is_new, window, cx);
            return;
        }

        let target = match form.active_field {
            FormField::Name => &mut form.name,
            FormField::Email => &mut form.email,
        };

        if key == "backspace" {
            target.pop();
            form.error = None;
            cx.notify();
            return;
        }

        if !ev.keystroke.modifiers.control
            && !ev.keystroke.modifiers.platform
            && !ev.keystroke.modifiers.alt
            && let Some(ch) = ev.keystroke.key_char.as_deref()
            && !ch.is_empty()
            && !is_child_process(ch)
        {
            target.push_str(ch);
            form.error = None;
            cx.notify();
        }
    }

    /// L4 Split Header (`46px`): PHOSPHOR LED + `AGY Swap` + prominent `+ Add Gmail` + Sync + Quit.
    fn render_header(
        &mut self,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active_acc_for_hdr = self
            .snapshot
            .as_ref()
            .and_then(|s| s.accounts.iter().find(|a| a.is_running))
            .or_else(|| self.effective_selected_profile());

        let (led_mode, led_theme) = if self.in_flight_phase.is_some() {
            (LedMode::Analysing, PhosphorTheme::Red)
        } else if let Some(acc) = active_acc_for_hdr {
            self.resolve_profile_orb(acc, true)
        } else {
            (LedMode::Standby, PhosphorTheme::White)
        };

        let hdr_led = self.phosphor.render_matrix(
            "agy-hdr-led",
            LedSpec {
                rows: 5,
                cols: 5,
                trim_corners: !matches!(
                    led_mode,
                    LedMode::Verified5 | LedMode::Scanner | LedMode::Progress(_) | LedMode::Syncing
                ),
                cell_size: 2.5,
                gap: 0.9,
                mode: led_mode,
                theme: led_theme,
                phase_offset: 0.0,
            },
        );

        let is_compact_overview =
            self.compact_mode && self.page == PopoverPage::SplitOverview;
        let show_back = self.page != PopoverPage::SplitOverview;
        let sub_title = match &self.page {
            PopoverPage::SplitOverview => self
                .toast_line
                .as_deref()
                .unwrap_or("Multi-Account Deck")
                .to_owned(),
            PopoverPage::AppSettings => "App Settings".to_owned(),
            PopoverPage::ModelQuotaDetail(pid) => {
                let name = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.accounts.iter().find(|a| a.id == *pid))
                    .map_or("Profile", |a| a.display_name.as_str());
                format!("Local Activity · {name}")
            }
            PopoverPage::AccountDetail(_) => "Account Settings".to_owned(),
            PopoverPage::AccountForm => {
                if self
                    .form_state
                    .as_ref()
                    .is_some_and(|f| f.editing_id.is_some())
                {
                    "Edit Account".to_owned()
                } else {
                    "Add Google Account".to_owned()
                }
            }
            PopoverPage::RulesAndAudit => "Guards R1–R5 & Audit".to_owned(),
            PopoverPage::PhosphorLab => {
                let target = self
                    .effective_selected_profile()
                    .map_or("Orb", |a| a.display_name.as_str());
                format!("PHOSPHOR Orb · {target}")
            }
            PopoverPage::Providers => "Providers & Live Telemetry".to_owned(),
        };

        let left = if show_back {
            div()
                .id("hdr-back")
                .flex()
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .on_click(cx.listener(|v, _, window, cx| {
                    v.form_state = None;
                    v.navigate_to(PopoverPage::SplitOverview, window, cx);
                }))
                .child(
                    div()
                        .w(px(24.0))
                        .h(px(24.0))
                        .rounded(px(7.0))
                        .bg(p.btn_dark)
                        .border_1()
                        .border_color(p.border_hairline)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.0))
                        .text_color(p.text_primary)
                        .child("←"),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(p.text_primary)
                        .child(sub_title),
                )
                .into_any_element()
        } else {
            let mut row = div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .items_center()
                .gap(px(7.0))
                .child(led_housing(hdr_led, None).p(px(4.0)).rounded(px(7.0)))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(13.5))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(p.text_primary)
                        .child("AGY Swap"),
                );

            if self.is_sandbox {
                row = row.child(
                    div()
                        .flex_shrink_0()
                        .px(px(5.5))
                        .py(px(1.5))
                        .rounded(px(5.0))
                        .bg(rgba(0xFBBF_2422))
                        .border_1()
                        .border_color(rgba(0xFBBF_2466))
                        .text_size(px(8.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(p.status_warn)
                        .child(if is_compact_overview { "SB" } else { "SANDBOX" }),
                );
            }

            if !is_compact_overview {
                row = row.child(
                    div()
                        .whitespace_nowrap()
                        .text_size(px(11.0))
                        .text_color(p.text_muted)
                        .child(format!("· {}", clip_text(&sub_title, 28))),
                );
            }

            row.into_any_element()
        };

        let is_settings_page = self.page == PopoverPage::AppSettings;
        let show_add_btn = self.page == PopoverPage::SplitOverview;

        let mut right_bar = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(6.0));

        if show_add_btn {
            right_bar = right_bar.child(
                div()
                    .id("hdr-add-gmail")
                    .h(px(26.0))
                    .px(px(if is_compact_overview { 8.5 } else { 10.5 }))
                    .rounded(px(999.0))
                    .bg(p.pearl_bg)
                    .text_color(p.pearl_fg)
                    .text_size(px(if is_compact_overview { 13.0 } else { 11.0 }))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|v, _, window, cx| {
                        v.open_add_form(window, cx);
                    }))
                    .child(if is_compact_overview { "+" } else { "+ Add Gmail" }),
            );
        }

        // Settings gear icon (`⚙`)
        right_bar = right_bar.child(
            div()
                .id("hdr-settings")
                .w(px(26.0))
                .h(px(26.0))
                .rounded(px(999.0))
                .bg(if is_settings_page {
                    rgba(0x6FD8_EE24).into()
                } else {
                    p.btn_dark
                })
                .border_1()
                .border_color(if is_settings_page {
                    p.accent_cyan
                } else {
                    p.border_hairline
                })
                .text_size(px(12.5))
                .text_color(if is_settings_page {
                    p.accent_cyan
                } else {
                    p.text_secondary
                })
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.text_color(p.text_primary))
                .on_click(cx.listener(move |v, _, window, cx| {
                    if is_settings_page {
                        v.navigate_to(PopoverPage::SplitOverview, window, cx);
                    } else {
                        v.navigate_to(PopoverPage::AppSettings, window, cx);
                    }
                }))
                .child("⚙"),
        );

        right_bar = right_bar
            .child(
                div()
                    .id("hdr-sync")
                    .w(px(26.0))
                    .h(px(26.0))
                    .rounded(px(999.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .text_size(px(12.0))
                    .text_color(p.text_secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.text_primary))
                    .on_click(cx.listener(|v, _, _, cx| {
                        v.run_command(
                            CommandRequest::RefreshTelemetry,
                            CallerContext::local_interactive(),
                            PhaseId::CheckingPolicy,
                            cx,
                        );
                    }))
                    .child("↻"),
            );

        // Windows: the borderless tray popover has no system caption buttons, so offer an
        // explicit "hide to tray" (×) next to Quit (⏻). The app keeps running in the System Tray;
        // left-click the tray icon to bring the popover back.
        if cfg!(target_os = "windows") {
            right_bar = right_bar.child(
                div()
                    .id("hdr-hide")
                    .w(px(26.0))
                    .h(px(26.0))
                    .rounded(px(999.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .text_size(px(15.0))
                    .text_color(p.text_secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.text_primary))
                    .on_click(cx.listener(|v, _, window, _cx| {
                        v.dismiss_popover(window);
                    }))
                    .child("×"),
            );
        }

        right_bar = right_bar
            .child(
                div()
                    .id("hdr-quit")
                    .w(px(26.0))
                    .h(px(26.0))
                    .rounded(px(999.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .text_size(px(11.5))
                    .text_color(p.text_secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.status_danger))
                    .on_click(cx.listener(|_v, _, _, cx| {
                        cx.quit();
                    }))
                    .child("⏻"),
            );

        div()
            .h(px(46.0))
            .px(px(14.0))
            .gap(px(10.0))
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(p.border_hairline)
            .child(left)
            .child(right_bar)
    }

    /// L4 Split 2-Col Overview: Left = Account Switcher + Swap CTA, Right = Selected Profile Inspector + Countdown Quota (100% -> 0%) + Controls.
    fn render_split_overview(
        &mut self,
        snap: &DashboardSnapshot,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ready_count = snap
            .accounts
            .iter()
            .filter(|a| a.is_authenticated && !a.is_on_cooldown)
            .count();
        let total = snap.accounts.len();

        let selected_acc = self
            .selected_profile_id
            .as_deref()
            .and_then(|sid| snap.accounts.iter().find(|a| a.id == sid))
            .or_else(|| snap.accounts.iter().find(|a| a.is_running))
            .or_else(|| snap.accounts.first())
            .cloned();
        let selected_id = selected_acc.as_ref().map(|a| a.id.clone());

        // Left column: Account Switcher List
        let mut acc_list = div()
            .id("split-acc-scroll")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(8.0));

        if snap.accounts.is_empty() {
            acc_list = acc_list.child(
                div()
                    .p(px(12.0))
                    .rounded(px(10.0))
                    .bg(p.row_bg)
                    .border_1()
                    .border_color(p.border_subtle)
                    .text_size(px(11.5))
                    .text_color(p.text_muted)
                    .child("No accounts yet. Click + Add Gmail above."),
            );
        } else {
            for (idx, acc) in snap.accounts.iter().enumerate() {
                let is_selected = selected_id.as_deref() == Some(acc.id.as_str());
                acc_list = acc_list.child(self.render_left_account_row(
                    acc,
                    idx,
                    is_selected,
                    p,
                    cx,
                ));
            }
        }

        if snap.hide_default_profile {
            acc_list = acc_list.child(
                div()
                    .id("split-restore-def")
                    .px(px(10.0))
                    .py(px(8.0))
                    .rounded(px(10.0))
                    .bg(p.row_bg)
                    .border_1()
                    .border_color(p.border_subtle)
                    .text_size(px(11.0))
                    .text_color(p.accent_cyan)
                    .cursor_pointer()
                    .on_click(cx.listener(|v, _, _, cx| {
                        v.run_command(
                            CommandRequest::RestoreDefault,
                            CallerContext::local_interactive(),
                            PhaseId::PersistingState,
                            cx,
                        );
                    }))
                    .child("↺ Restore Primary Account"),
            );
        }

        let has_multiple = snap.accounts.len() >= 2;
        let swap_cta_label = if let Some(phase) = self.in_flight_phase {
            format!("{}…", phase.label())
        } else if !has_multiple {
            "+  Add 2nd Account to Swap".to_owned()
        } else if let Some(next) = &snap.next_account_name {
            format!("⇄  Swap → {}", clip_text(next, 18))
        } else {
            "⇄  Swap Next Account".to_owned()
        };

        let left_col_base = if self.compact_mode {
            div().flex_1().min_w_0().overflow_hidden().h_full().p(px(14.0))
        } else {
            div()
                .w(px(274.0))
                .flex_shrink_0()
                .min_w_0()
                .overflow_hidden()
                .h_full()
                .p(px(14.0))
                .border_r_1()
                .border_color(p.border_hairline)
        };

        let left_col = left_col_base
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(2.0))
                    .child(
                        div()
                            .text_size(px(9.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(p.text_muted)
                            .child("SWITCH ACCOUNT"),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(p.status_ok)
                            .child(format!("{ready_count}/{total} ready")),
                    ),
            )
            .child(acc_list)
            .child(
                div()
                    .id("left-swap-cta")
                    .h(px(36.0))
                    .px(px(12.0))
                    .rounded(px(10.0))
                    .bg(p.pearl_bg)
                    .text_color(p.pearl_fg)
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |v, _, window, cx| {
                        if has_multiple {
                            v.run_command(
                                CommandRequest::SwitchNext,
                                CallerContext::local_interactive(),
                                PhaseId::StoppingActive,
                                cx,
                            );
                        } else {
                            v.open_add_form(window, cx);
                        }
                    }))
                    .child(div().whitespace_nowrap().child(swap_cta_label)),
            );

        // Compact ("Minimize") mode: show ONLY the Switch Account deck!
        if self.compact_mode {
            return div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .overflow_hidden()
                .child(left_col)
                .into_any_element();
        }

        // Right column: Selected Profile Inspector + Countdown Quota (100% -> 0%) + Controls
        let (orb_mode, orb_theme) = if self.in_flight_phase.is_some() {
            (LedMode::Analysing, PhosphorTheme::Red)
        } else if let Some(sel) = &selected_acc {
            self.resolve_profile_orb(sel, false)
        } else {
            (LedMode::Standby, PhosphorTheme::White)
        };
        let right_orb = self
            .phosphor
            .render_matrix("split-orb", LedSpec::hero_7x7(orb_mode, orb_theme, 0.0));

        let is_sel_cd = selected_acc.as_ref().is_some_and(|a| a.is_on_cooldown);
        let has_server_quota = selected_acc
            .as_ref()
            .and_then(|a| a.server_quota.as_ref())
            .is_some_and(|sq| sq.ok);
        let models = selected_acc
            .as_ref()
            .map(|a| a.models.clone())
            .unwrap_or_else(|| snap.models_7d.clone());

        let g_secs = models.gemini_reset_5h_secs;
        let cg_secs = models.claude_gpt_reset_5h_secs;
        let g_clock = format_clock_hm(g_secs);
        let cg_clock = format_clock_hm(cg_secs);

        let g_theme = if has_server_quota && models.gemini_remaining_5h_pct <= 15.0 {
            PhosphorTheme::Amber
        } else {
            timer_bar_theme(g_secs, is_sel_cd, PhosphorTheme::Cyan)
        };
        let cg_theme = if has_server_quota && models.claude_gpt_remaining_5h_pct <= 15.0 {
            PhosphorTheme::Amber
        } else {
            timer_bar_theme(cg_secs, is_sel_cd, PhosphorTheme::Emerald)
        };

        let g_digits = dot_matrix_readout(&g_clock, 1.7, 0.6, p.digit_lit, p.digit_unlit);
        let cg_digits = dot_matrix_readout(&cg_clock, 1.7, 0.6, p.digit_lit, p.digit_unlit);
        let g_fraction = if has_server_quota {
            percent_to_f32_fraction(models.gemini_remaining_5h_pct)
        } else {
            window_5h_fraction(g_secs)
        };
        let cg_fraction = if has_server_quota {
            percent_to_f32_fraction(models.claude_gpt_remaining_5h_pct)
        } else {
            window_5h_fraction(cg_secs)
        };
        let g_bar = segmented_led_bar(18, g_fraction, g_theme, p.led_unlit, 15.0, 5.5, 3.0);
        let cg_bar = segmented_led_bar(18, cg_fraction, cg_theme, p.led_unlit, 15.0, 5.5, 3.0);

        let g_unit = timer_unit_label(g_secs, is_sel_cd);
        let cg_unit = timer_unit_label(cg_secs, is_sel_cd);

        let g_sub_left = if has_server_quota {
            format!(
                "5h: {:.0}% left · Wk: {:.0}%",
                models.gemini_remaining_5h_pct, models.gemini_remaining_7d_pct
            )
        } else if models.gemini_calls_5h > 0 {
            format!("5h: {} calls", models.gemini_calls_5h)
        } else {
            "5h: 0 calls · ready".to_owned()
        };
        let g_sub_right = format!(
            "7d: {}x · {}",
            models.gemini_calls_7d,
            format_tokens(models.gemini_tokens_7d)
        );

        let cg_calls_5h = models.claude_calls_5h.saturating_add(models.gpt_calls_5h);
        let cg_sub_left = if has_server_quota {
            format!(
                "5h: {:.0}% left · Wk: {:.0}%",
                models.claude_gpt_remaining_5h_pct, models.claude_gpt_remaining_7d_pct
            )
        } else if cg_calls_5h > 0 {
            format!("5h: {cg_calls_5h} calls")
        } else {
            "5h: 0 calls · ready".to_owned()
        };
        let cg_sub_right = format!(
            "7d: C:{}x · GPT:{}x",
            models.claude_calls_7d, models.gpt_calls_7d
        );

        let profiles_dir = snap.profiles_root_dir.clone();

        let mut right_top = div()
            .flex()
            .flex_col()
            .gap(px(12.0));

        // Optional policy or missing-binary alert in right rail
        if !snap.antigravity_bin_exists {
            right_top = right_top.child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .bg(rgba(0xFF9F_1C1A))
                    .border_1()
                    .border_color(p.status_warn)
                    .text_size(px(10.0))
                    .text_color(p.status_warn)
                    .child("Antigravity binary not found"),
            );
        } else if let Some(banner) = &self.policy_banner {
            right_top = right_top.child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .bg(rgba(0xFF3B_281A))
                    .border_1()
                    .border_color(p.status_danger)
                    .text_size(px(10.0))
                    .text_color(p.status_danger)
                    .child(clip_text(banner, 52)),
            );
        }

        // Section 1: Selected Profile Header Card
        let header_card = if let Some(sel) = selected_acc.clone() {
            let sel_email =
                format_email_display(&sel.subtitle, sel.is_default_profile, self.hide_emails);
            let (status_txt, status_col, status_bg) = if !sel.is_authenticated {
                ("SIGN IN", p.status_danger, rgba(0xFF5A_4722))
            } else if sel.is_on_cooldown {
                ("5H LIMIT", p.status_warn, rgba(0xFBBF_2422))
            } else if sel.is_running {
                ("LIVE", p.status_ok, rgba(0x34D3_9922))
            } else {
                ("READY", p.accent_cyan, rgba(0x6FD8_EE20))
            };
            let tier_badge = match sel.tier {
                AccountTier::Ultra => "ULTRA",
                AccountTier::Pro => "PRO",
            };
            let sel_for_edit = sel.clone();

            div()
                .p(px(12.0))
                .rounded(px(12.0))
                .bg(p.card_bg)
                .border_1()
                .border_color(p.border_hairline)
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .id("right-orb-btn")
                        .flex_shrink_0()
                        .cursor_pointer()
                        .on_click(cx.listener(|v, _, window, cx| {
                            v.open_phosphor_lab_for_selected(window, cx);
                        }))
                        .child(led_housing(right_orb, None).p(px(5.0))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .px(px(6.0))
                                        .py(px(1.5))
                                        .rounded(px(4.0))
                                        .bg(status_bg)
                                        .text_size(px(8.5))
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .text_color(status_col)
                                        .child(status_txt),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(9.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(p.text_muted)
                                        .child(tier_badge),
                                ),
                        )
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_size(px(13.5))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(p.text_primary)
                                .child(clip_text(&sel.display_name, 22)),
                        )
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_size(px(10.5))
                                .text_color(p.text_secondary)
                                .child(clip_text(&sel_email, 32)),
                        ),
                )
                .child(
                    div()
                        .id("right-edit-btn")
                        .flex_shrink_0()
                        .h(px(26.0))
                        .px(px(9.0))
                        .rounded(px(7.0))
                        .bg(p.btn_dark)
                        .border_1()
                        .border_color(p.border_hairline)
                        .text_size(px(10.5))
                        .text_color(p.text_secondary)
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .hover(|s| s.text_color(p.text_primary))
                        .on_click(cx.listener(move |v, _, window, cx| {
                            v.open_edit_form(&sel_for_edit, window, cx);
                        }))
                        .child("Edit"),
                )
        } else {
            div()
                .p(px(12.0))
                .rounded(px(12.0))
                .bg(p.card_bg)
                .border_1()
                .border_color(p.border_hairline)
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(led_housing(right_orb, None).p(px(5.0)))
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(p.text_secondary)
                        .child("Add a Google account to inspect usage & swap."),
                )
        };

        // Section 2: Clickable 2-Pool Countdown Quota Card (100% -> 0%)
        let quota_target_id = selected_id.clone();
        let telemetry_card = div()
            .id("right-quota-card")
            .p(px(12.0))
            .rounded(px(12.0))
            .bg(p.card_bg)
            .border_1()
            .border_color(p.border_hairline)
            .flex()
            .flex_col()
            .gap(px(11.0))
            .cursor_pointer()
            .hover(|s| s.border_color(p.border_outer))
            .on_click(cx.listener(move |v, _, window, cx| {
                if let Some(pid) = &quota_target_id {
                    v.navigate_to(PopoverPage::ModelQuotaDetail(pid.clone()), window, cx);
                }
            }))
            // Card Header with explicit clickable "Models ›" pill
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .child(
                                div()
                                    .w(px(5.5))
                                    .h(px(5.5))
                                    .rounded(px(999.0))
                                    .bg(if has_server_quota {
                                        p.status_ok
                                    } else {
                                        p.text_muted
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if has_server_quota {
                                        p.status_ok
                                    } else {
                                        p.text_muted
                                    })
                                    .child(if has_server_quota {
                                        "LIVE SERVER QUOTA · 5H & WK"
                                    } else {
                                        "LOCAL 5H WINDOW · TIMER"
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .px(px(7.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .bg(rgba(0x6FD8_EE18))
                            .text_size(px(9.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(p.accent_cyan)
                            .child("Models ›"),
                    ),
            )
            // Pool 1: GEMINI MODELS (5h local timer + exact calls/tokens)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .w(px(6.0))
                                            .h(px(6.0))
                                            .rounded(px(2.0))
                                            .bg(g_theme.accent()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.5))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(p.text_primary)
                                            .child("GEMINI MODELS"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_end()
                                    .gap(px(4.0))
                                    .child(g_digits)
                                    .child(
                                        div()
                                            .text_size(px(9.0))
                                            .text_color(p.text_muted)
                                            .child(g_unit),
                                    ),
                            ),
                    )
                    .child(g_bar)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(9.5))
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_color(p.text_secondary)
                                    .child(g_sub_left),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_color(p.text_muted)
                                    .child(g_sub_right),
                            ),
                    ),
            )
            // Pool 2: CLAUDE & GPT MODELS (5h local timer + exact calls/tokens)
            .child(
                div()
                    .pt(px(9.0))
                    .border_t_1()
                    .border_color(p.border_subtle)
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .w(px(6.0))
                                            .h(px(6.0))
                                            .rounded(px(2.0))
                                            .bg(cg_theme.accent()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.5))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(p.text_primary)
                                            .child("CLAUDE & GPT"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_end()
                                    .gap(px(4.0))
                                    .child(cg_digits)
                                    .child(
                                        div()
                                            .text_size(px(9.0))
                                            .text_color(p.text_muted)
                                            .child(cg_unit),
                                    ),
                            ),
                    )
                    .child(cg_bar)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(9.5))
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_color(p.text_secondary)
                                    .child(cg_sub_left),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_color(p.text_muted)
                                    .child(cg_sub_right),
                            ),
                    ),
            )
            // Context + Cache hairline footer
            .child(
                div()
                    .pt(px(8.0))
                    .border_t_1()
                    .border_color(p.border_subtle)
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(9.5))
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_color(p.text_muted)
                            .child(format!(
                                "CTX {} / 1M",
                                format_tokens(snap.active_context_tokens),
                            )),
                    )
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_color(p.text_secondary)
                            .child(format!(
                                "{:.0}% cache · 24h {}",
                                snap.active_cache_hit_percent,
                                format_tokens(snap.total_tokens_24h)
                            )),
                    ),
            );

        right_top = right_top.child(header_card).child(telemetry_card);

        // Section 3: Per-Profile Action & Toggle Controls (cleanly grouped with breathing room)
        if let Some(sel) = selected_acc.clone() {
            let id_primary = sel.id.clone();
            let id_second = sel.id.clone();
            let id_more = sel.id.clone();
            let id_share = sel.id.clone();
            let id_cd = sel.id.clone();
            let is_default = sel.is_default_profile;
            let is_shared = sel.is_shared_chats;
            let is_running = sel.is_running;
            let is_sole_active = sel.is_sole_active;
            let is_auth = sel.is_authenticated;

            let primary_label = if !is_auth {
                "Sign in Google ↗"
            } else if is_sole_active {
                "Focus Window"
            } else {
                "Switch Active"
            };
            let primary_is_pearl = !is_sole_active;

            let controls_group = div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .child(
                            div()
                                .id("sel-act-primary")
                                .flex_1()
                                .h(px(32.0))
                                .px(px(10.0))
                                .rounded(px(9.0))
                                .bg(if primary_is_pearl {
                                    p.pearl_bg
                                } else {
                                    p.btn_dark
                                })
                                .border_1()
                                .border_color(if primary_is_pearl {
                                    p.pearl_bg
                                } else {
                                    p.border_hairline
                                })
                                .text_size(px(11.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(if primary_is_pearl {
                                    p.pearl_fg
                                } else {
                                    p.text_primary
                                })
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .on_click(cx.listener(move |v, _, _, cx| {
                                    if !is_auth || is_sole_active {
                                        v.run_command(
                                            CommandRequest::OpenAlongside {
                                                profile_id: id_primary.clone(),
                                            },
                                            CallerContext::local_interactive(),
                                            PhaseId::LaunchingTarget,
                                            cx,
                                        );
                                    } else {
                                        v.run_command(
                                            CommandRequest::SwitchToProfile {
                                                profile_id: id_primary.clone(),
                                            },
                                            CallerContext::local_interactive(),
                                            PhaseId::StoppingActive,
                                            cx,
                                        );
                                    }
                                }))
                                .child(primary_label),
                        )
                        .child(
                            div()
                                .id("sel-act-second")
                                .flex_1()
                                .h(px(32.0))
                                .px(px(10.0))
                                .rounded(px(9.0))
                                .bg(p.btn_dark)
                                .border_1()
                                .border_color(p.border_hairline)
                                .text_size(px(11.0))
                                .text_color(if is_running {
                                    p.status_warn
                                } else {
                                    p.text_primary
                                })
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .hover(|s| s.border_color(p.border_outer))
                                .on_click(cx.listener(move |v, _, _, cx| {
                                    if is_running {
                                        v.run_command(
                                            CommandRequest::StopProfile {
                                                profile_id: id_second.clone(),
                                            },
                                            CallerContext::local_interactive(),
                                            PhaseId::StoppingActive,
                                            cx,
                                        );
                                    } else {
                                        v.run_command(
                                            CommandRequest::OpenAlongside {
                                                profile_id: id_second.clone(),
                                            },
                                            CallerContext::local_interactive(),
                                            PhaseId::LaunchingTarget,
                                            cx,
                                        );
                                    }
                                }))
                                .child(if is_running { "Stop Window" } else { "Alongside ↗" }),
                        )
                        .child(
                            div()
                                .id("sel-act-more")
                                .flex_shrink_0()
                                .w(px(34.0))
                                .h(px(32.0))
                                .rounded(px(9.0))
                                .bg(p.btn_dark)
                                .border_1()
                                .border_color(p.border_hairline)
                                .text_size(px(12.0))
                                .text_color(p.text_secondary)
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .hover(|s| s.text_color(p.text_primary))
                                .on_click(cx.listener(move |v, _, window, cx| {
                                    v.navigate_to(PopoverPage::AccountDetail(id_more.clone()), window, cx);
                                }))
                                .child("⋯"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .child(if is_default {
                            div()
                                .flex_1()
                                .h(px(34.0))
                                .px(px(10.0))
                                .rounded(px(9.0))
                                .bg(p.card_bg)
                                .border_1()
                                .border_color(p.border_subtle)
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(p.text_secondary)
                                        .child("Shared Chats"),
                                )
                                .child(
                                    div()
                                        .text_size(px(9.5))
                                        .text_color(p.text_muted)
                                        .child("Primary"),
                                )
                                .into_any_element()
                        } else {
                            div()
                                .id("right-shared-row")
                                .flex_1()
                                .h(px(34.0))
                                .px(px(10.0))
                                .rounded(px(9.0))
                                .bg(p.card_bg)
                                .border_1()
                                .border_color(p.border_hairline)
                                .flex()
                                .items_center()
                                .justify_between()
                                .cursor_pointer()
                                .on_click(cx.listener(move |v, _, _, cx| {
                                    v.run_command(
                                        CommandRequest::SetSharedChats {
                                            profile_id: id_share.clone(),
                                            enabled: !is_shared,
                                        },
                                        CallerContext::local_interactive(),
                                        PhaseId::LinkingWorkspace,
                                        cx,
                                    );
                                }))
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(p.text_primary)
                                        .child("Shared Chats"),
                                )
                                .child(hardware_switch_track(
                                    is_shared,
                                    PhosphorTheme::Cyan,
                                    p.is_light,
                                ))
                                .into_any_element()
                        })
                        .child(
                            div()
                                .id("right-cd-row")
                                .flex_1()
                                .h(px(34.0))
                                .px(px(10.0))
                                .rounded(px(9.0))
                                .bg(p.card_bg)
                                .border_1()
                                .border_color(p.border_hairline)
                                .flex()
                                .items_center()
                                .justify_between()
                                .cursor_pointer()
                                .on_click(cx.listener(move |v, _, _, cx| {
                                    v.run_command(
                                        CommandRequest::ToggleCooldown {
                                            profile_id: id_cd.clone(),
                                        },
                                        CallerContext::local_interactive(),
                                        PhaseId::PersistingState,
                                        cx,
                                    );
                                }))
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(p.text_primary)
                                        .child("5h Cooldown"),
                                )
                                .child(hardware_switch_track(
                                    sel.is_on_cooldown,
                                    PhosphorTheme::Amber,
                                    p.is_light,
                                )),
                        ),
                );

            right_top = right_top.child(controls_group);
        }

        // Bottom Right Utility Trio (Orb Lab + Folder + Rules/Audit) — flush with left Swap CTA!
        let selected_custom_orb = selected_id
            .as_deref()
            .and_then(|pid| self.profile_orbs.get(pid))
            .copied();

        let bridge_live = snap
            .external_telemetry
            .as_ref()
            .is_some_and(|t| t.connected);

        let right_footer = div()
            .flex()
            .gap(px(6.0))
            .child(
                div()
                    .id("right-providers-btn")
                    .flex_1()
                    .h(px(32.0))
                    .rounded(px(9.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(if bridge_live {
                        p.status_ok
                    } else {
                        p.border_hairline
                    })
                    .text_size(px(10.0))
                    .text_color(if bridge_live {
                        p.status_ok
                    } else {
                        p.text_secondary
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.text_primary))
                    .on_click(cx.listener(|v, _, window, cx| {
                        v.navigate_to(PopoverPage::Providers, window, cx);
                    }))
                    .child(if bridge_live {
                        "● Providers ›"
                    } else {
                        "Providers ›"
                    }),
            )
            .child(
                div()
                    .id("right-lab-btn")
                    .flex_1()
                    .h(px(32.0))
                    .rounded(px(9.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(if let Some(o) = selected_custom_orb {
                        o.theme.accent()
                    } else {
                        p.border_hairline
                    })
                    .text_size(px(10.0))
                    .text_color(if let Some(o) = selected_custom_orb {
                        o.theme.accent()
                    } else {
                        p.text_secondary
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.text_primary))
                    .on_click(cx.listener(|v, _, window, cx| {
                        v.open_phosphor_lab_for_selected(window, cx);
                    }))
                    .child(if let Some(o) = selected_custom_orb {
                        format!("Orb · {}", o.label)
                    } else {
                        "Orb Lab ›".to_owned()
                    }),
            )
            .child(
                div()
                    .id("right-folder-btn")
                    .flex_1()
                    .h(px(32.0))
                    .rounded(px(9.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .text_size(px(10.0))
                    .text_color(p.text_secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.text_primary))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        let path = PathBuf::from(&profiles_dir);
                        v.toast_line = Some(match open_directory_in_file_manager(&path) {
                            Ok(()) => "Opened Profiles folder ↗".to_owned(),
                            Err(e) => e.to_string(),
                        });
                        cx.notify();
                    }))
                    .child("Folder ↗"),
            )
            .child(
                div()
                    .id("right-rules-btn")
                    .flex_1()
                    .h(px(32.0))
                    .rounded(px(9.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .text_size(px(10.0))
                    .text_color(p.text_secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(p.text_primary))
                    .on_click(cx.listener(|v, _, window, cx| {
                        v.navigate_to(PopoverPage::RulesAndAudit, window, cx);
                    }))
                    .child(format!("R1–R5 ({}) ›", self.audits.len())),
            );

        let right_col = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .h_full()
            .p(px(14.0))
            .bg(p.rail_bg)
            .flex()
            .flex_col()
            .justify_between()
            .child(right_top)
            .child(right_footer);

        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .overflow_hidden()
            .child(left_col)
            .child(right_col)
            .into_any_element()
    }

    /// Clean Left-Column Account Row: reflects each profile's custom PHOSPHOR Orb immediately!
    fn render_left_account_row(
        &mut self,
        acc: &AccountView,
        idx: usize,
        is_selected: bool,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = acc.id.clone();
        let id_select = id.clone();
        let id_switch = id.clone();
        let id_detail = id.clone();

        let (led_mode, led_theme) = self.resolve_profile_orb(acc, true);

        let phase = f32::from(u8::try_from(idx % 24).unwrap_or(0)) * 0.45;
        let led = self.phosphor.render_matrix(
            &format!("l4-led-{id}"),
            LedSpec {
                rows: 5,
                cols: 5,
                trim_corners: !matches!(
                    led_mode,
                    LedMode::Verified5 | LedMode::Scanner | LedMode::Progress(_) | LedMode::Syncing
                ),
                cell_size: 2.3,
                gap: 0.9,
                mode: led_mode,
                theme: led_theme,
                phase_offset: phase,
            },
        );

        let email_sub =
            format_email_display(&acc.subtitle, acc.is_default_profile, self.hide_emails);

        let tier_short = match acc.tier {
            AccountTier::Ultra => "ULT",
            AccountTier::Pro => "PRO",
        };

        let primary_control = if acc.is_sole_active {
            div()
                .flex_shrink_0()
                .px(px(7.0))
                .h(px(23.0))
                .rounded(px(7.0))
                .bg(rgba(0x34D3_9920))
                .text_size(px(9.5))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(p.status_ok)
                .flex()
                .items_center()
                .child("LIVE")
                .into_any_element()
        } else if !acc.is_authenticated {
            div()
                .id(SharedString::from(format!("l4-signin-{id}")))
                .flex_shrink_0()
                .px(px(7.0))
                .h(px(23.0))
                .rounded(px(7.0))
                .bg(p.pearl_bg)
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(p.pearl_fg)
                .flex()
                .items_center()
                .cursor_pointer()
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.selected_profile_id = Some(id_switch.clone());
                    v.run_command(
                        CommandRequest::OpenAlongside {
                            profile_id: id_switch.clone(),
                        },
                        CallerContext::local_interactive(),
                        PhaseId::LaunchingTarget,
                        cx,
                    );
                }))
                .child("Sign in")
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("l4-sw-{id}")))
                .flex_shrink_0()
                .px(px(8.0))
                .h(px(23.0))
                .rounded(px(7.0))
                .bg(p.btn_dark)
                .border_1()
                .border_color(p.border_hairline)
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(if acc.is_on_cooldown {
                    p.status_warn
                } else {
                    p.text_primary
                })
                .flex()
                .items_center()
                .cursor_pointer()
                .hover(|s| s.bg(p.pearl_bg).text_color(p.pearl_fg))
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.selected_profile_id = Some(id_switch.clone());
                    v.run_command(
                        CommandRequest::SwitchToProfile {
                            profile_id: id_switch.clone(),
                        },
                        CallerContext::local_interactive(),
                        PhaseId::StoppingActive,
                        cx,
                    );
                }))
                .child(if acc.is_on_cooldown { "5h cd" } else { "Switch" })
                .into_any_element()
        };

        div()
            .px(px(10.0))
            .py(px(9.0))
            .rounded(px(11.0))
            .bg(if is_selected {
                p.row_active
            } else {
                p.row_bg
            })
            .border_1()
            .border_color(if is_selected {
                p.accent_cyan
            } else if acc.is_running {
                p.border_outer
            } else {
                p.border_subtle
            })
            .flex()
            .items_center()
            .justify_between()
            .gap(px(6.0))
            .hover(|s| s.bg(p.row_hover))
            .child(
                div()
                    .id(SharedString::from(format!("l4-sel-{id}")))
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selected_profile_id = Some(id_select.clone());
                        cx.notify();
                    }))
                    .child(led_housing(led, None).p(px(4.0)).rounded(px(7.0)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .child(
                                        div()
                                            .whitespace_nowrap()
                                            .text_size(px(12.0))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(p.text_primary)
                                            .child(clip_text(&acc.display_name, 14)),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(8.5))
                                            .text_color(p.text_muted)
                                            .child(tier_short),
                                    ),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(9.5))
                                    .text_color(p.text_muted)
                                    .child(clip_text(&email_sub, 20)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(primary_control)
                    .child(
                        div()
                            .id(SharedString::from(format!("l4-det-{id}")))
                            .w(px(20.0))
                            .h(px(23.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.5))
                            .text_color(p.text_muted)
                            .cursor_pointer()
                            .hover(|s| s.bg(p.btn_dark).text_color(p.text_primary))
                            .on_click(cx.listener(move |v, _, window, cx| {
                                v.selected_profile_id = Some(id_detail.clone());
                                v.navigate_to(PopoverPage::AccountDetail(id_detail.clone()), window, cx);
                            }))
                            .child("⋯"),
                    ),
            )
    }

    /// Dedicated S2 Samo Instrument Local Activity & 5h Window Detail Page (2-pool local timer + exact detected models).
    fn render_model_quota_detail(
        &mut self,
        profile_id: &str,
        snap: &DashboardSnapshot,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(acc) = snap.accounts.iter().find(|a| a.id == profile_id).cloned() else {
            return div()
                .p(px(14.0))
                .text_size(px(12.0))
                .text_color(p.text_muted)
                .child("Profile not found.")
                .into_any_element();
        };

        let m = &acc.models;
        let g_secs = m.gemini_reset_5h_secs;
        let cg_secs = m.claude_gpt_reset_5h_secs;
        let g_clock = format_clock_hm(g_secs);
        let cg_clock = format_clock_hm(cg_secs);
        let g_5h_theme = timer_bar_theme(g_secs, acc.is_on_cooldown, PhosphorTheme::Cyan);
        let cg_5h_theme = timer_bar_theme(cg_secs, acc.is_on_cooldown, PhosphorTheme::Emerald);

        let cg_5h_calls = m.claude_calls_5h.saturating_add(m.gpt_calls_5h);
        let cg_7d_calls = m.claude_calls_7d.saturating_add(m.gpt_calls_7d);
        let cg_7d_tok = m.claude_tokens_7d.saturating_add(m.gpt_tokens_7d);

        let left_pools = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(12.0))
            // Pool 1: GEMINI MODELS
            .child(
                div()
                    .p(px(12.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .w(px(6.0))
                                            .h(px(6.0))
                                            .rounded(px(2.0))
                                            .bg(PhosphorTheme::Cyan.accent()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .text_color(p.text_primary)
                                            .child("GEMINI MODELS"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .text_color(p.text_muted)
                                    .child(format!(
                                        "{}x (7d) · {}",
                                        m.gemini_calls_7d,
                                        format_tokens(m.gemini_tokens_7d)
                                    )),
                            ),
                    )
                    // 5h Local Rolling Timer
                    .child(render_timer_pool_row(
                        "5-HOUR ROLLING TIMER",
                        &g_clock,
                        timer_unit_label(g_secs, acc.is_on_cooldown),
                        window_5h_fraction(g_secs),
                        g_5h_theme,
                        if m.gemini_calls_5h > 0 && !m.gemini_reset_5h_label.is_empty() {
                            format!(
                                "{} calls in 5h · resets in {}",
                                m.gemini_calls_5h, m.gemini_reset_5h_label
                            )
                        } else {
                            "0 calls in last 5h · full 5:00 window ready".to_owned()
                        },
                        p,
                    ))
                    // 7-Day Exact Local Volume
                    .child(
                        div()
                            .pt(px(8.0))
                            .border_t_1()
                            .border_color(p.border_subtle)
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_muted)
                                    .child("7-DAY LOCAL VOLUME"),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(10.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(p.text_primary)
                                    .child(format!(
                                        "{} calls · {} tok",
                                        m.gemini_calls_7d,
                                        format_tokens(m.gemini_tokens_7d)
                                    )),
                            ),
                    ),
            )
            // Pool 2: CLAUDE & GPT MODELS
            .child(
                div()
                    .p(px(12.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .w(px(6.0))
                                            .h(px(6.0))
                                            .rounded(px(2.0))
                                            .bg(PhosphorTheme::Amber.accent()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .text_color(p.text_primary)
                                            .child("CLAUDE & GPT MODELS"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .text_color(p.text_muted)
                                    .child(format!(
                                        "{cg_7d_calls}x (7d) · {}",
                                        format_tokens(cg_7d_tok)
                                    )),
                            ),
                    )
                    .child(render_timer_pool_row(
                        "5-HOUR ROLLING TIMER",
                        &cg_clock,
                        timer_unit_label(cg_secs, acc.is_on_cooldown),
                        window_5h_fraction(cg_secs),
                        cg_5h_theme,
                        if cg_5h_calls > 0 && !m.claude_gpt_reset_5h_label.is_empty() {
                            format!(
                                "{cg_5h_calls} calls in 5h · resets in {}",
                                m.claude_gpt_reset_5h_label
                            )
                        } else {
                            "0 calls in last 5h · full 5:00 window ready".to_owned()
                        },
                        p,
                    ))
                    .child(
                        div()
                            .pt(px(8.0))
                            .border_t_1()
                            .border_color(p.border_subtle)
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_muted)
                                    .child("7-DAY LOCAL VOLUME"),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(10.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(p.text_primary)
                                    .child(format!(
                                        "Claude {}x · GPT {}x · {}",
                                        m.claude_calls_7d,
                                        m.gpt_calls_7d,
                                        format_tokens(cg_7d_tok)
                                    )),
                            ),
                    ),
            );

        // Right column: Exact detected models from local transcript.jsonl
        let mut sorted_models: Vec<&SingleModelUsage> = m.by_model.values().collect();
        sorted_models.sort_by(|a, b| b.calls_7d.cmp(&a.calls_7d).then(a.model_name.cmp(&b.model_name)));

        let mut model_list = div()
            .id("models-detail-scroll")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.0));

        if sorted_models.is_empty() {
            model_list = model_list.child(
                div()
                    .p(px(12.0))
                    .rounded(px(10.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_subtle)
                    .text_size(px(10.5))
                    .text_color(p.text_muted)
                    .child("No model calls logged in the last 7 days for this profile."),
            );
        } else {
            for item in sorted_models {
                let fam_col = match item.family {
                    ModelFamily::Gemini => PhosphorTheme::Cyan.accent(),
                    ModelFamily::Claude => PhosphorTheme::Amber.accent(),
                    ModelFamily::Gpt => PhosphorTheme::Emerald.accent(),
                    ModelFamily::Other => p.text_muted,
                };
                model_list = model_list.child(
                    div()
                        .px(px(10.0))
                        .py(px(8.0))
                        .rounded(px(9.0))
                        .bg(p.card_bg)
                        .border_1()
                        .border_color(p.border_subtle)
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .child(
                                            div()
                                                .w(px(5.5))
                                                .h(px(5.5))
                                                .rounded(px(999.0))
                                                .bg(fam_col),
                                        )
                                        .child(
                                            div()
                                                .whitespace_nowrap()
                                                .text_size(px(10.5))
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(p.text_primary)
                                                .child(clip_text(&item.model_name, 22)),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(9.5))
                                        .text_color(if item.calls_5h > 0 {
                                            p.status_ok
                                        } else {
                                            p.text_muted
                                        })
                                        .child(format!("{} / 5h", item.calls_5h)),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .text_size(px(9.5))
                                .text_color(p.text_muted)
                                .child(format!("7d: {} calls", item.calls_7d))
                                .child(format!("{} tok", format_tokens(item.tokens_7d))),
                        ),
                );
            }
        }

        let right_models = div()
            .w(px(268.0))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(9.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(p.text_muted)
                            .child("DETECTED MODELS"),
                    )
                    .child(
                        div()
                            .id("models-refresh-btn")
                            .px(px(8.0))
                            .py(px(2.5))
                            .rounded(px(6.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_hairline)
                            .text_size(px(9.5))
                            .text_color(p.text_secondary)
                            .cursor_pointer()
                            .hover(|s| s.text_color(p.text_primary))
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.run_command(
                                    CommandRequest::GetDashboard,
                                    CallerContext::local_interactive(),
                                    PhaseId::CheckingPolicy,
                                    cx,
                                );
                            }))
                            .child("↻ Refresh"),
                    ),
            )
            .child(model_list)
            .child(
                div()
                    .text_size(px(9.0))
                    .text_color(p.text_muted)
                    .child("100% local transcript & 5h timer · Zero OAuth access (Rule R2)."),
            );

        div()
            .flex_1()
            .p(px(14.0))
            .flex()
            .flex_row()
            .gap(px(14.0))
            .overflow_hidden()
            .child(left_pools)
            .child(right_models)
            .into_any_element()
    }

    fn render_account_detail(
        &mut self,
        profile_id: &str,
        snap: &DashboardSnapshot,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(acc) = snap.accounts.iter().find(|a| a.id == profile_id).cloned() else {
            return div()
                .p(px(14.0))
                .text_size(px(12.0))
                .text_color(p.text_muted)
                .child("Account not found.")
                .into_any_element();
        };

        let id = acc.id.clone();
        let id_sw = id.clone();
        let id_open = id.clone();
        let id_stop = id.clone();
        let id_cd = id.clone();
        let id_share = id.clone();
        let is_shared = acc.is_shared_chats;
        let is_default = acc.is_default_profile;
        let acc_edit = acc.clone();
        let id_relogin = id.clone();
        let name_relogin = acc.display_name.clone();
        let id_del = id.clone();
        let name_del = acc.display_name.clone();

        let (mode, theme) = self.resolve_profile_orb(&acc, false);
        let led = self.phosphor.render_matrix(
            "det-led",
            LedSpec {
                rows: 7,
                cols: 7,
                trim_corners: !matches!(
                    mode,
                    LedMode::Scanner | LedMode::Progress(_) | LedMode::Syncing
                ),
                cell_size: 4.4,
                gap: 1.5,
                mode,
                theme,
                phase_offset: 0.0,
            },
        );

        let mut left = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .p(px(12.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(led_housing(led, None))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(13.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_primary)
                                    .child(clip_text(&acc.display_name, 26)),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(10.5))
                                    .text_color(p.text_secondary)
                                    .child(clip_text(
                                        &if acc.subtitle.trim().is_empty() {
                                            acc.tier.label().to_owned()
                                        } else {
                                            format!(
                                                "{} · {}",
                                                acc.tier.label(),
                                                format_email_display(
                                                    &acc.subtitle,
                                                    acc.is_default_profile,
                                                    self.hide_emails
                                                )
                                            )
                                        },
                                        36,
                                    )),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(10.0))
                                    .text_color(p.text_muted)
                                    .child({
                                        let calls_5h = acc
                                            .models
                                            .gemini_calls_5h
                                            .saturating_add(acc.models.claude_calls_5h)
                                            .saturating_add(acc.models.gpt_calls_5h);
                                        if acc.is_on_cooldown {
                                            format!(
                                                "5h Cooldown ({}) · {calls_5h} calls in 5h",
                                                format_clock_hm(acc.reset_in_secs)
                                            )
                                        } else if let Some(secs) = acc.reset_in_secs {
                                            format!(
                                                "5h: {calls_5h}x calls · reset in {}",
                                                format_clock_hm(Some(secs))
                                            )
                                        } else {
                                            "5h: 0 calls · window ready (5:00)".to_owned()
                                        }
                                    }),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(9.5))
                                    .text_color(p.text_muted)
                                    .child({
                                        let total_tok_7d = acc
                                            .models
                                            .gemini_tokens_7d
                                            .saturating_add(acc.models.claude_tokens_7d)
                                            .saturating_add(acc.models.gpt_tokens_7d);
                                        format!(
                                            "7d: G:{}x · C:{}x · GPT:{}x · {} tok",
                                            acc.models.gemini_calls_7d,
                                            acc.models.claude_calls_7d,
                                            acc.models.gpt_calls_7d,
                                            format_tokens(total_tok_7d),
                                        )
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("det-sw")
                            .flex_1()
                            .h(px(32.0))
                            .rounded(px(8.0))
                            .bg(p.pearl_bg)
                            .text_color(p.pearl_fg)
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |v, _, _, cx| {
                                v.run_command(
                                    CommandRequest::SwitchToProfile {
                                        profile_id: id_sw.clone(),
                                    },
                                    CallerContext::local_interactive(),
                                    PhaseId::StoppingActive,
                                    cx,
                                );
                            }))
                            .child("Switch Active"),
                    )
                    .child(
                        div()
                            .id("det-open")
                            .flex_1()
                            .h(px(32.0))
                            .rounded(px(8.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_hairline)
                            .text_color(p.text_primary)
                            .text_size(px(11.5))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |v, _, _, cx| {
                                v.run_command(
                                    CommandRequest::OpenAlongside {
                                        profile_id: id_open.clone(),
                                    },
                                    CallerContext::local_interactive(),
                                    PhaseId::LaunchingTarget,
                                    cx,
                                );
                            }))
                            .child(if acc.is_running { "Focus" } else { "Open Alongside" }),
                    ),
            );

        if acc.is_running {
            left = left.child(
                div()
                    .id("det-stop")
                    .h(px(30.0))
                    .px(px(10.0))
                    .rounded(px(8.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_subtle)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.0))
                    .text_color(p.status_warn)
                    .cursor_pointer()
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.run_command(
                            CommandRequest::StopProfile {
                                profile_id: id_stop.clone(),
                            },
                            CallerContext::local_interactive(),
                            PhaseId::StoppingActive,
                            cx,
                        );
                    }))
                    .child("Stop Running Window & Sidecars"),
            );
        }

        let mut right_top = div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .text_size(px(9.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(p.text_muted)
                    .child("PREFERENCES"),
            )
            .child(
                div()
                    .id("det-cd")
                    .h(px(38.0))
                    .px(px(12.0))
                    .rounded(px(10.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .items_center()
                    .justify_between()
                    .cursor_pointer()
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.run_command(
                            CommandRequest::ToggleCooldown {
                                profile_id: id_cd.clone(),
                            },
                            CallerContext::local_interactive(),
                            PhaseId::PersistingState,
                            cx,
                        );
                    }))
                    .child(div().text_size(px(11.5)).text_color(p.text_primary).child("5h Cooldown"))
                    .child(hardware_switch_track(
                        acc.is_on_cooldown,
                        PhosphorTheme::Amber,
                        p.is_light,
                    )),
            );

        if !is_default {
            right_top = right_top.child(
                div()
                    .id("det-share")
                    .h(px(38.0))
                    .px(px(12.0))
                    .rounded(px(10.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .items_center()
                    .justify_between()
                    .cursor_pointer()
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.run_command(
                            CommandRequest::SetSharedChats {
                                profile_id: id_share.clone(),
                                enabled: !is_shared,
                            },
                            CallerContext::local_interactive(),
                            PhaseId::LinkingWorkspace,
                            cx,
                        );
                    }))
                    .child(div().text_size(px(11.5)).text_color(p.text_primary).child("Shared Chats"))
                    .child(hardware_switch_track(
                        is_shared,
                        PhosphorTheme::Cyan,
                        p.is_light,
                    )),
            );
        }

        right_top = right_top
            .child(
                div()
                    .pt(px(6.0))
                    .text_size(px(9.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(p.text_muted)
                    .child("ACCOUNT"),
            )
            .child(
                div()
                    .id("det-edit")
                    .h(px(36.0))
                    .px(px(12.0))
                    .rounded(px(9.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .items_center()
                    .justify_between()
                    .cursor_pointer()
                    .hover(|s| s.border_color(p.border_outer))
                    .on_click(cx.listener(move |v, _, window, cx| {
                        v.open_edit_form(&acc_edit, window, cx);
                    }))
                    .child(div().text_size(px(11.5)).text_color(p.text_primary).child("Edit Name / Email"))
                    .child(div().text_size(px(11.5)).text_color(p.text_muted).child("›")),
            );

        if !is_default {
            right_top = right_top.child(
                div()
                    .id("det-relogin")
                    .h(px(36.0))
                    .px(px(12.0))
                    .rounded(px(9.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .hover(|s| s.border_color(p.border_outer))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.confirm_action = Some(ConfirmAction::ResetLogin {
                            profile_id: id_relogin.clone(),
                            display_name: name_relogin.clone(),
                        });
                        cx.notify();
                    }))
                    .child(div().text_size(px(11.5)).text_color(p.text_secondary).child("Relogin Google…")),
            );
        }

        let right_del = div()
            .id("det-del")
            .h(px(36.0))
            .px(px(12.0))
            .rounded(px(9.0))
            .bg(rgba(0xFF3B_281A))
            .border_1()
            .border_color(rgba(0xFF3B_2848))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_click(cx.listener(move |v, _, _, cx| {
                v.confirm_action = Some(ConfirmAction::DeleteAccount {
                    profile_id: id_del.clone(),
                    display_name: name_del.clone(),
                    is_default_profile: is_default,
                    delete_disk: false,
                });
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(11.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(p.status_danger)
                    .child(if is_default { "Hide Primary…" } else { "Remove Account…" }),
            );

        let right = div()
            .w(px(244.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .justify_between()
            .child(right_top)
            .child(right_del);

        div()
            .flex_1()
            .p(px(16.0))
            .flex()
            .flex_row()
            .gap(px(16.0))
            .child(left)
            .child(right)
            .into_any_element()
    }

    fn render_account_form(&self, p: &SamoPalette, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = &self.form_state else {
            return div().into_any_element();
        };
        let is_edit = form.editing_id.is_some();
        let tier_label = match form.tier_override {
            None => "Auto",
            Some(AccountTier::Ultra) => "Ultra",
            Some(AccountTier::Pro) => "Pro",
        };

        let mut form_top = div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(p.text_muted)
                    .child("Type directly on keyboard · Tab switches field · Enter saves & opens Google Sign-In"),
            )
            // Side-by-side Name & Google Email inputs with generous spacing
            .child(
                div()
                    .p(px(14.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .flex_row()
                    .gap(px(14.0))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(7.0))
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_secondary)
                                    .child("NICKNAME / LABEL"),
                            )
                            .child(
                                div()
                                    .id("frm-name")
                                    .h(px(38.0))
                                    .px(px(12.0))
                                    .rounded(px(9.0))
                                    .bg(p.rail_bg)
                                    .border_1()
                                    .border_color(if form.active_field == FormField::Name {
                                        p.accent_cyan
                                    } else {
                                        p.border_hairline
                                    })
                                    .flex()
                                    .items_center()
                                    .text_size(px(12.0))
                                    .text_color(p.text_primary)
                                    .cursor_pointer()
                                    .on_click(cx.listener(|v, _, _, cx| {
                                        if let Some(f) = v.form_state.as_mut() {
                                            f.active_field = FormField::Name;
                                            cx.notify();
                                        }
                                    }))
                                    .child(if form.name.is_empty() {
                                        "e.g. SC, Trade, Work".to_owned()
                                    } else if form.active_field == FormField::Name {
                                        format!("{}|", form.name)
                                    } else {
                                        form.name.clone()
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(7.0))
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_secondary)
                                    .child("GOOGLE EMAIL (OPTIONAL)"),
                            )
                            .child(
                                div()
                                    .id("frm-email")
                                    .h(px(38.0))
                                    .px(px(12.0))
                                    .rounded(px(9.0))
                                    .bg(p.rail_bg)
                                    .border_1()
                                    .border_color(if form.active_field == FormField::Email {
                                        p.accent_cyan
                                    } else {
                                        p.border_hairline
                                    })
                                    .flex()
                                    .items_center()
                                    .text_size(px(12.0))
                                    .text_color(if form.email.is_empty() {
                                        p.text_muted
                                    } else {
                                        p.text_primary
                                    })
                                    .cursor_pointer()
                                    .on_click(cx.listener(|v, _, _, cx| {
                                        if let Some(f) = v.form_state.as_mut() {
                                            f.active_field = FormField::Email;
                                            cx.notify();
                                        }
                                    }))
                                    .child(if form.email.is_empty() {
                                        if form.active_field == FormField::Email {
                                            "|".to_owned()
                                        } else {
                                            "you@gmail.com".to_owned()
                                        }
                                    } else if form.active_field == FormField::Email {
                                        format!("{}|", form.email)
                                    } else {
                                        form.email.clone()
                                    }),
                            ),
                    ),
            )
            // Options Row: Plan Tier + Share Chats Switch
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(14.0))
                    .child(
                        div()
                            .id("frm-tier")
                            .flex_1()
                            .h(px(42.0))
                            .px(px(14.0))
                            .rounded(px(10.0))
                            .bg(p.card_bg)
                            .border_1()
                            .border_color(p.border_hairline)
                            .flex()
                            .items_center()
                            .justify_between()
                            .cursor_pointer()
                            .on_click(cx.listener(|v, _, _, cx| {
                                if let Some(f) = v.form_state.as_mut() {
                                    f.tier_override = match f.tier_override {
                                        None => Some(AccountTier::Ultra),
                                        Some(AccountTier::Ultra) => Some(AccountTier::Pro),
                                        Some(AccountTier::Pro) => None,
                                    };
                                    cx.notify();
                                }
                            }))
                            .child(div().text_size(px(11.5)).text_color(p.text_secondary).child("Plan Tier"))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_primary)
                                    .child(tier_label),
                            ),
                    )
                    .child(if form.is_default_profile {
                        div()
                            .flex_1()
                            .h(px(42.0))
                            .px(px(14.0))
                            .rounded(px(10.0))
                            .bg(p.card_bg)
                            .border_1()
                            .border_color(p.border_subtle)
                            .flex()
                            .items_center()
                            .text_size(px(11.0))
                            .text_color(p.text_muted)
                            .child("Primary owns shared chats")
                            .into_any_element()
                    } else {
                        div()
                            .id("frm-share")
                            .flex_1()
                            .h(px(42.0))
                            .px(px(14.0))
                            .rounded(px(10.0))
                            .bg(p.card_bg)
                            .border_1()
                            .border_color(p.border_hairline)
                            .flex()
                            .items_center()
                            .justify_between()
                            .cursor_pointer()
                            .on_click(cx.listener(|v, _, _, cx| {
                                if let Some(f) = v.form_state.as_mut() {
                                    f.share_chats = !f.share_chats;
                                    cx.notify();
                                }
                            }))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(p.text_secondary)
                                    .child("Share chats"),
                            )
                            .child(hardware_switch_track(
                                form.share_chats,
                                PhosphorTheme::Cyan,
                                p.is_light,
                            ))
                            .into_any_element()
                    }),
            );

        if let Some(err) = &form.error {
            form_top = form_top.child(
                div()
                    .text_size(px(11.0))
                    .text_color(p.status_danger)
                    .child(err.clone()),
            );
        }

        let mut footer = div().flex().gap(px(10.0)).child(
            div()
                .id("frm-save")
                .flex_1()
                .h(px(36.0))
                .rounded(px(10.0))
                .bg(if is_edit { p.pearl_bg } else { p.btn_dark })
                .border_1()
                .border_color(if is_edit { p.pearl_bg } else { p.border_hairline })
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if is_edit { p.pearl_fg } else { p.text_primary })
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .on_click(cx.listener(|v, _, window, cx| {
                    v.submit_form(false, window, cx);
                }))
                .child(if is_edit { "Save Changes" } else { "Save Slot Only" }),
        );

        if !is_edit {
            footer = footer.child(
                div()
                    .id("frm-add-signin")
                    .flex_1()
                    .h(px(36.0))
                    .rounded(px(10.0))
                    .bg(p.pearl_bg)
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(p.pearl_fg)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|v, _, window, cx| {
                        v.submit_form(true, window, cx);
                    }))
                    .child("Add & Sign in ↗"),
            );
        }

        div()
            .flex_1()
            .p(px(18.0))
            .flex()
            .flex_col()
            .justify_between()
            .child(form_top)
            .child(footer)
            .into_any_element()
    }

    /// Dedicated App Settings Page (`⚙`): Theme (Dark / Light), Window Layout (Full / Compact), Privacy (Hide Emails), and Quick Tools.
    fn render_app_settings(
        &self,
        snap: Option<&DashboardSnapshot>,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_light = self.light_theme;
        let is_compact = self.compact_mode;
        let hide_emails = self.hide_emails;
        let profiles_dir = snap.map(|s| s.profiles_root_dir.clone());

        let left_col = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(14.0))
            // Appearance: Dark / Light
            .child(
                div()
                    .p(px(14.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_muted)
                                    .child("APPEARANCE"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(p.text_secondary)
                                    .child("Switch between Samo Obsidian Dark and Alabaster Light"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("set-theme-dark")
                                    .flex_1()
                                    .h(px(36.0))
                                    .rounded(px(9.0))
                                    .bg(if !is_light { p.pearl_bg } else { p.btn_dark })
                                    .border_1()
                                    .border_color(if !is_light {
                                        p.pearl_bg
                                    } else {
                                        p.border_hairline
                                    })
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if !is_light {
                                        p.pearl_fg
                                    } else {
                                        p.text_primary
                                    })
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|v, _, _, cx| {
                                        v.light_theme = false;
                                        v.persist_ui_prefs();
                                        v.toast_line = Some("Theme: Dark Obsidian".to_owned());
                                        cx.notify();
                                    }))
                                    .child("☾  Dark"),
                            )
                            .child(
                                div()
                                    .id("set-theme-light")
                                    .flex_1()
                                    .h(px(36.0))
                                    .rounded(px(9.0))
                                    .bg(if is_light { p.pearl_bg } else { p.btn_dark })
                                    .border_1()
                                    .border_color(if is_light {
                                        p.pearl_bg
                                    } else {
                                        p.border_hairline
                                    })
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if is_light {
                                        p.pearl_fg
                                    } else {
                                        p.text_primary
                                    })
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|v, _, _, cx| {
                                        v.light_theme = true;
                                        v.persist_ui_prefs();
                                        v.toast_line = Some("Theme: Light Alabaster".to_owned());
                                        cx.notify();
                                    }))
                                    .child("☀  Light"),
                            ),
                    ),
            )
            // Layout Mode: Full (Split 2-Col) vs Compact (Switcher Only)
            .child(
                div()
                    .p(px(14.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_muted)
                                    .child("POPOVER LAYOUT"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(p.text_secondary)
                                    .child("Full shows 2-column Quota & Inspector · Compact shows Switch Account only"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("set-layout-full")
                                    .flex_1()
                                    .h(px(36.0))
                                    .rounded(px(9.0))
                                    .bg(if !is_compact { p.pearl_bg } else { p.btn_dark })
                                    .border_1()
                                    .border_color(if !is_compact {
                                        p.pearl_bg
                                    } else {
                                        p.border_hairline
                                    })
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if !is_compact {
                                        p.pearl_fg
                                    } else {
                                        p.text_primary
                                    })
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|v, _, window, cx| {
                                        v.compact_mode = false;
                                        v.persist_ui_prefs();
                                        v.toast_line = Some("Layout: Full (2-Column)".to_owned());
                                        v.navigate_to(PopoverPage::SplitOverview, window, cx);
                                    }))
                                    .child("Full (2-Col)"),
                            )
                            .child(
                                div()
                                    .id("set-layout-compact")
                                    .flex_1()
                                    .h(px(36.0))
                                    .rounded(px(9.0))
                                    .bg(if is_compact { p.pearl_bg } else { p.btn_dark })
                                    .border_1()
                                    .border_color(if is_compact {
                                        p.pearl_bg
                                    } else {
                                        p.border_hairline
                                    })
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(if is_compact {
                                        p.pearl_fg
                                    } else {
                                        p.text_primary
                                    })
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .on_click(cx.listener(|v, _, window, cx| {
                                        v.compact_mode = true;
                                        v.persist_ui_prefs();
                                        v.toast_line = Some("Layout: Compact Switcher".to_owned());
                                        v.navigate_to(PopoverPage::SplitOverview, window, cx);
                                    }))
                                    .child("Compact (Mini)"),
                            ),
                    ),
            );

        let right_col = div()
            .w(px(260.0))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(14.0))
            // Privacy: Hide Emails toggle
            .child(
                div()
                    .id("set-hide-emails")
                    .p(px(14.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(if hide_emails {
                        p.accent_cyan
                    } else {
                        p.border_hairline
                    })
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|v, _, _, cx| {
                        v.hide_emails = !v.hide_emails;
                        v.persist_ui_prefs();
                        v.toast_line = Some(if v.hide_emails {
                            "Privacy: Emails hidden".to_owned()
                        } else {
                            "Privacy: Emails visible".to_owned()
                        });
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_primary)
                                    .child("Hide Emails"),
                            )
                            .child(hardware_switch_track(
                                hide_emails,
                                PhosphorTheme::Cyan,
                                p.is_light,
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(p.text_muted)
                            .child("Masks Gmail addresses as ko•••@gmail.com across the switcher for screenshots and streaming."),
                    ),
            )
            // Shortcuts card
            .child(
                div()
                    .p(px(14.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(9.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(p.text_muted)
                            .child("TOOLS & SAFETY"),
                    )
                    .child(
                        div()
                            .id("set-providers")
                            .h(px(34.0))
                            .px(px(10.0))
                            .rounded(px(8.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_subtle)
                            .flex()
                            .items_center()
                            .justify_between()
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.border_outer))
                            .on_click(cx.listener(|v, _, window, cx| {
                                v.navigate_to(PopoverPage::Providers, window, cx);
                            }))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(p.text_primary)
                                    .child("Providers & Live Telemetry"),
                            )
                            .child(div().text_size(px(11.0)).text_color(p.text_muted).child("›")),
                    )
                    .child(
                        div()
                            .id("set-orb-lab")
                            .h(px(34.0))
                            .px(px(10.0))
                            .rounded(px(8.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_subtle)
                            .flex()
                            .items_center()
                            .justify_between()
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.border_outer))
                            .on_click(cx.listener(|v, _, window, cx| {
                                v.open_phosphor_lab_for_selected(window, cx);
                            }))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(p.text_primary)
                                    .child("PHOSPHOR Orb Lab"),
                            )
                            .child(div().text_size(px(11.0)).text_color(p.text_muted).child("›")),
                    )
                    .child(
                        div()
                            .id("set-rules")
                            .h(px(34.0))
                            .px(px(10.0))
                            .rounded(px(8.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_subtle)
                            .flex()
                            .items_center()
                            .justify_between()
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.border_outer))
                            .on_click(cx.listener(|v, _, window, cx| {
                                v.navigate_to(PopoverPage::RulesAndAudit, window, cx);
                            }))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(p.text_primary)
                                    .child(format!("Guards R1–R5 & Audit ({})", self.audits.len())),
                            )
                            .child(div().text_size(px(11.0)).text_color(p.text_muted).child("›")),
                    )
                    .child(
                        div()
                            .id("set-folder")
                            .h(px(34.0))
                            .px(px(10.0))
                            .rounded(px(8.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_subtle)
                            .flex()
                            .items_center()
                            .justify_between()
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.border_outer))
                            .on_click(cx.listener(move |v, _, _, cx| {
                                if let Some(dir) = &profiles_dir {
                                    let path = PathBuf::from(dir);
                                    v.toast_line = Some(match open_directory_in_file_manager(&path) {
                                        Ok(()) => "Opened Profiles folder ↗".to_owned(),
                                        Err(e) => e.to_string(),
                                    });
                                    cx.notify();
                                }
                            }))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(p.text_primary)
                                    .child("Open Profiles Folder"),
                            )
                            .child(div().text_size(px(11.0)).text_color(p.text_muted).child("↗")),
                    ),
            );

        div()
            .flex_1()
            .p(px(16.0))
            .flex()
            .flex_row()
            .gap(px(16.0))
            .overflow_hidden()
            .child(left_col)
            .child(right_col)
            .into_any_element()
    }

    fn render_rules_and_audit(
        &mut self,
        snap: &DashboardSnapshot,
        p: &SamoPalette,
    ) -> AnyElement {
        let lock_led = self.phosphor.render_matrix(
            "rules-lock",
            LedSpec::compact_5x5(LedMode::Encrypted, PhosphorTheme::White, 0.0),
        );

        let mut left = div()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(led_housing(lock_led, None).p(px(3.0)))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(p.text_secondary)
                            .child("ENGINE RULES (R1–R5)"),
                    ),
            );

        for r in &snap.rules {
            left = left.child(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_subtle)
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(p.text_primary)
                            .child(format!("{} · {}", r.code, r.title)),
                    )
                    .child(
                        div()
                            .text_size(px(9.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(p.status_ok)
                            .child("ON"),
                    ),
            );
        }

        let mut right = div()
            .w(px(210.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(p.text_secondary)
                    .child(format!("AUDIT LOG ({})", self.audits.len())),
            );

        if self.audits.is_empty() {
            right = right.child(
                div()
                    .text_size(px(10.5))
                    .text_color(p.text_muted)
                    .child("No mutations recorded yet."),
            );
        } else {
            for e in self.audits.iter().rev().take(6) {
                right = right.child(render_compact_audit_row(e, p));
            }
        }

        div()
            .id("rules-scroll")
            .flex_1()
            .overflow_y_scroll()
            .p(px(12.0))
            .flex()
            .flex_row()
            .gap(px(12.0))
            .child(left)
            .child(right)
            .into_any_element()
    }

    fn render_phosphor_lab(&mut self, p: &SamoPalette, cx: &mut Context<Self>) -> AnyElement {
        let dot_col: Hsla = rgba(0xFFFF_FF14).into();
        let target_profile = self.effective_selected_profile().cloned();
        let target_id = target_profile
            .as_ref()
            .map_or_else(|| "default".to_owned(), |a| a.id.clone());
        let target_name = target_profile
            .as_ref()
            .map_or_else(|| "Primary".to_owned(), |a| a.display_name.clone());

        let theme = self.showcase_theme;
        let active_mode = self.showcase_mode;
        let active_label = self.showcase_label;
        let is_pinned = self.profile_orbs.contains_key(&target_id);
        let paused = self.phosphor.paused;

        let hero_led = self.phosphor.render_matrix(
            "lab-hero",
            LedSpec {
                rows: 7,
                cols: 7,
                trim_corners: !matches!(
                    active_mode,
                    LedMode::Scanner | LedMode::Progress(_) | LedMode::Syncing
                ),
                cell_size: 4.4,
                gap: 1.5,
                mode: active_mode,
                theme,
                phase_offset: 0.0,
            },
        );
        let pin_led = self.phosphor.render_matrix(
            "lab-power",
            LedSpec::compact_5x5(
                if is_pinned {
                    LedMode::Power5
                } else {
                    LedMode::Standby
                },
                if is_pinned {
                    theme
                } else {
                    PhosphorTheme::White
                },
                0.2,
            ),
        );

        let mut ramps = div().flex().items_center().gap(px(6.0));
        for t in PhosphorTheme::ALL {
            let active = t == theme;
            let pid_ramp = target_id.clone();
            let pname_ramp = target_name.clone();
            ramps = ramps.child(
                div()
                    .id(SharedString::from(format!("lab-r-{}", t.label())))
                    .px(px(10.0))
                    .h(px(24.0))
                    .rounded(px(999.0))
                    .bg(if active { p.btn_dark } else { p.rail_bg })
                    .border_1()
                    .border_color(if active {
                        t.accent()
                    } else {
                        p.border_subtle
                    })
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_pointer()
                    .hover(|s| s.border_color(t.accent()))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.showcase_theme = t;
                        v.profile_orbs.insert(
                            pid_ramp.clone(),
                            ResolvedOrb {
                                label: v.showcase_label,
                                mode: v.showcase_mode,
                                theme: t,
                            },
                        );
                        v.persist_ui_prefs();
                        v.toast_line = Some(format!(
                            "{pname_ramp}: {} ({})",
                            v.showcase_label,
                            t.label()
                        ));
                        cx.notify();
                    }))
                    .child(div().w(px(6.0)).h(px(6.0)).rounded(px(999.0)).bg(t.accent()))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(if active {
                                p.text_primary
                            } else {
                                p.text_secondary
                            })
                            .child(t.label()),
                    ),
            );
        }

        // Explicit symmetric 6 x 2 interactive grid — click any tile to preview & apply to this profile's Orb!
        let mut grid = div().flex().flex_col().gap(px(8.0));
        for row_idx in 0..2_usize {
            let mut row = div().flex().flex_row().gap(px(8.0));
            let start = row_idx * 6;
            let end = (start + 6).min(LedMode::SHOWCASE_MODES.len());
            for (offset, (name, mode, col_theme)) in LedMode::SHOWCASE_MODES[start..end]
                .iter()
                .copied()
                .enumerate()
            {
                let idx = start + offset;
                let is_selected = active_label == name;
                let phase = f32::from(u8::try_from(idx).unwrap_or(0)) * 0.45;
                let pid_tile = target_id.clone();
                let pname_tile = target_name.clone();
                let led = self.phosphor.render_matrix(
                    &format!("lab-m-{name}"),
                    LedSpec {
                        rows: 7,
                        cols: 7,
                        trim_corners: !matches!(
                            mode,
                            LedMode::Scanner | LedMode::Progress(_) | LedMode::Syncing
                        ),
                        cell_size: 3.4,
                        gap: 1.1,
                        mode,
                        theme: if is_selected { theme } else { col_theme },
                        phase_offset: phase,
                    },
                );
                row = row.child(
                    div()
                        .id(SharedString::from(format!("lab-tile-{name}")))
                        .flex_1()
                        .py(px(12.0))
                        .rounded(px(10.0))
                        .bg(if is_selected {
                            p.row_active
                        } else {
                            p.card_bg
                        })
                        .border_1()
                        .border_color(if is_selected {
                            theme.accent()
                        } else {
                            p.border_hairline
                        })
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(7.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(p.row_hover).border_color(p.border_outer))
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.showcase_mode = mode;
                            v.showcase_label = name;
                            v.showcase_theme = col_theme;
                            v.profile_orbs.insert(
                                pid_tile.clone(),
                                ResolvedOrb {
                                    label: name,
                                    mode,
                                    theme: col_theme,
                                },
                            );
                            v.persist_ui_prefs();
                            v.toast_line =
                                Some(format!("{pname_tile}: {name} ({})", col_theme.label()));
                            cx.notify();
                        }))
                        .child(led_housing(led, None).p(px(5.0)).rounded(px(8.0)))
                        .child(
                            div()
                                .text_size(px(9.5))
                                .font_weight(if is_selected {
                                    gpui::FontWeight::SEMIBOLD
                                } else {
                                    gpui::FontWeight::NORMAL
                                })
                                .text_color(if is_selected {
                                    p.text_primary
                                } else {
                                    p.text_muted
                                })
                                .child(name),
                        ),
                );
            }
            grid = grid.child(row);
        }

        let pid_toggle = target_id.clone();
        let pname_toggle = target_name.clone();

        div()
            .id("lab-scroll")
            .flex_1()
            .overflow_y_scroll()
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(ramps)
                    .child(
                        div()
                            .id("lab-pause")
                            .px(px(10.0))
                            .h(px(24.0))
                            .rounded(px(999.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_hairline)
                            .text_size(px(10.0))
                            .text_color(p.text_primary)
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.phosphor.paused = !v.phosphor.paused;
                                cx.notify();
                            }))
                            .child(if paused { "Resume 30Hz" } else { "Pause 30Hz" }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(10.0))
                    .child(
                        div()
                            .relative()
                            .overflow_hidden()
                            .flex_1()
                            .py(px(16.0))
                            .rounded(px(12.0))
                            .bg(p.rail_bg)
                            .border_1()
                            .border_color(p.border_hairline)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(dot_grid_backdrop(dot_col, 10.0))
                            .child(div().relative().child(hw_pill(
                                hero_led,
                                active_label,
                                Some((theme.label().to_uppercase(), theme.accent())),
                            ))),
                    )
                    .child(
                        div()
                            .id("lab-sw")
                            .w(px(228.0))
                            .p(px(12.0))
                            .rounded(px(12.0))
                            .bg(p.card_bg)
                            .border_1()
                            .border_color(if is_pinned {
                                theme.accent()
                            } else {
                                p.border_hairline
                            })
                            .flex()
                            .flex_col()
                            .justify_between()
                            .gap(px(8.0))
                            .cursor_pointer()
                            .on_click(cx.listener(move |v, _, _, cx| {
                                if v.profile_orbs.contains_key(&pid_toggle) {
                                    v.profile_orbs.remove(&pid_toggle);
                                    v.toast_line =
                                        Some(format!("{pname_toggle}: Auto Status Orb"));
                                } else {
                                    v.profile_orbs.insert(
                                        pid_toggle.clone(),
                                        ResolvedOrb {
                                            label: v.showcase_label,
                                            mode: v.showcase_mode,
                                            theme: v.showcase_theme,
                                        },
                                    );
                                    v.toast_line = Some(format!(
                                        "{pname_toggle}: {} ({})",
                                        v.showcase_label,
                                        v.showcase_theme.label()
                                    ));
                                }
                                v.persist_ui_prefs();
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(led_housing(pin_led, None).p(px(4.0)))
                                    .child(hardware_switch_track(is_pinned, theme, p.is_light)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_primary)
                                    .child(if is_pinned {
                                        format!("{} · {active_label}", clip_text(&target_name, 12))
                                    } else {
                                        format!("{} · Auto Status", clip_text(&target_name, 12))
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(p.text_muted)
                    .child(format!(
                        "Customizing Orb for '{}' — updates both Switch Account list & Inspector immediately",
                        clip_text(&target_name, 20)
                    )),
            )
            .child(grid)
            .into_any_element()
    }

    fn render_confirm_overlay(
        &self,
        action: &ConfirmAction,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (title, desc, del_disk_opt, ok_label) = match action {
            ConfirmAction::DeleteAccount {
                display_name,
                is_default_profile,
                delete_disk,
                ..
            } => {
                if *is_default_profile {
                    (
                        format!("Hide {display_name}?"),
                        "Primary OS files & shared chats stay untouched (Rule R1).".to_owned(),
                        None,
                        "Hide",
                    )
                } else {
                    (
                        format!("Remove {display_name}?"),
                        "Shared chat links are unlinked first so primary chats stay safe (Rule R1)."
                            .to_owned(),
                        Some(*delete_disk),
                        if *delete_disk { "Delete Vault" } else { "Remove" },
                    )
                }
            }
            ConfirmAction::ResetLogin { display_name, .. } => (
                format!("Reset {display_name} login?"),
                "Clears isolated OAuth token & UserData (Rule R4) and opens fresh Google Sign-In."
                    .to_owned(),
                None,
                "Reset & Sign in",
            ),
        };

        let mut card = div()
            .w(px(320.0))
            .p(px(14.0))
            .rounded(px(14.0))
            .bg(p.shell_bg)
            .border_1()
            .border_color(p.border_outer)
            .flex()
            .flex_col()
            .gap(px(9.0))
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(p.text_primary)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(p.text_secondary)
                    .child(desc),
            );

        if let Some(delete_disk) = del_disk_opt {
            card = card.child(
                div()
                    .id("cfm-disk-toggle")
                    .px(px(8.0))
                    .h(px(30.0))
                    .rounded(px(8.0))
                    .bg(p.rail_bg)
                    .border_1()
                    .border_color(if delete_disk {
                        p.status_danger
                    } else {
                        p.border_subtle
                    })
                    .flex()
                    .items_center()
                    .justify_between()
                    .cursor_pointer()
                    .on_click(cx.listener(|v, _, _, cx| {
                        if let Some(ConfirmAction::DeleteAccount { delete_disk, .. }) =
                            v.confirm_action.as_mut()
                        {
                            *delete_disk = !*delete_disk;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(p.text_secondary)
                            .child("Also delete local folder"),
                    )
                    .child(hardware_switch_track(
                        delete_disk,
                        PhosphorTheme::Red,
                        p.is_light,
                    )),
            );
        }

        let action_clone = action.clone();
        card = card.child(
            div()
                .flex()
                .gap(px(6.0))
                .pt(px(2.0))
                .child(
                    div()
                        .id("cfm-cancel")
                        .flex_1()
                        .h(px(30.0))
                        .rounded(px(8.0))
                        .bg(p.btn_dark)
                        .border_1()
                        .border_color(p.border_hairline)
                        .text_size(px(11.5))
                        .text_color(p.text_secondary)
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.confirm_action = None;
                            cx.notify();
                        }))
                        .child("Cancel"),
                )
                .child(
                    div()
                        .id("cfm-ok")
                        .flex_1()
                        .h(px(30.0))
                        .rounded(px(8.0))
                        .bg(p.status_danger)
                        .text_size(px(11.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(p.pearl_fg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.confirm_action = None;
                            match &action_clone {
                                ConfirmAction::DeleteAccount {
                                    profile_id,
                                    delete_disk,
                                    ..
                                } => {
                                    v.run_command(
                                        CommandRequest::DeleteAccount(DeleteAccountInput {
                                            profile_id: profile_id.clone(),
                                            delete_disk_directory: *delete_disk,
                                        }),
                                        CallerContext::local_confirmed(),
                                        PhaseId::StoppingActive,
                                        cx,
                                    );
                                }
                                ConfirmAction::ResetLogin { profile_id, .. } => {
                                    v.run_command(
                                        CommandRequest::ResetLogin {
                                            profile_id: profile_id.clone(),
                                            launch_alongside: true,
                                        },
                                        CallerContext::local_confirmed(),
                                        PhaseId::StoppingActive,
                                        cx,
                                    );
                                }
                            }
                        }))
                        .child(ok_label),
                ),
        );

        div()
            .absolute()
            .inset_0()
            .bg(p.scrim)
            .flex()
            .items_center()
            .justify_center()
            .child(card)
    }

    fn render_providers_page(
        &self,
        snap: &DashboardSnapshot,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ext = snap.external_telemetry.as_ref().filter(|t| t.connected);
        let base_url = default_telemetry_base_url();
        let console_url = format!("{base_url}/opendesign/mockups/ai-usage-tracker/index.html");
        let mini_hud_url =
            format!("{base_url}/opendesign/mockups/ai-usage-tracker/widget.html?mode=mini");

        let (status_badge, status_col, status_bg, sub_meta) = if let Some(t) = ext {
            let mut parts = vec![format!("Source: {}", t.source_label)];
            if !t.updated_at.is_empty() {
                parts.push(t.updated_at.clone());
            }
            if let Some(burn) = t.burn_per_day_usd {
                parts.push(format!("Burn ${burn:.2}/d"));
            }
            if let Some(rw) = t.runway_days {
                parts.push(format!("{rw:.1}d runway"));
            }
            (
                "BRIDGE LIVE",
                p.status_ok,
                rgba(0x34D3_9922),
                parts.join(" · "),
            )
        } else {
            (
                "BRIDGE OFFLINE",
                p.status_warn,
                rgba(0xFBBF_2422),
                format!(
                    "Start local tracker on {base_url} or set AGSWITCH_TELEMETRY_FILE"
                ),
            )
        };

        let top_bar = div()
            .p(px(12.0))
            .rounded(px(12.0))
            .bg(p.card_bg)
            .border_1()
            .border_color(p.border_hairline)
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .px(px(6.0))
                                    .py(px(1.5))
                                    .rounded(px(4.0))
                                    .bg(status_bg)
                                    .text_size(px(8.5))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(status_col)
                                    .child(status_badge),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_primary)
                                    .child("Multi-Provider Telemetry"),
                            ),
                    )
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_size(px(9.5))
                            .text_color(p.text_muted)
                            .child(clip_text(&sub_meta, 56)),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("prov-refresh-btn")
                            .h(px(28.0))
                            .px(px(9.0))
                            .rounded(px(7.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_hairline)
                            .text_size(px(10.0))
                            .text_color(p.text_primary)
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.border_outer))
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.run_command(
                                    CommandRequest::RefreshTelemetry,
                                    CallerContext::local_interactive(),
                                    PhaseId::CheckingPolicy,
                                    cx,
                                );
                            }))
                            .child("↻ Fresh"),
                    )
                    .child(
                        div()
                            .id("prov-console-btn")
                            .h(px(28.0))
                            .px(px(9.0))
                            .rounded(px(7.0))
                            .bg(p.btn_dark)
                            .border_1()
                            .border_color(p.border_hairline)
                            .text_size(px(10.0))
                            .text_color(p.accent_cyan)
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.accent_cyan))
                            .on_click(cx.listener(move |v, _, _, cx| {
                                v.toast_line = Some(match open_local_url(&console_url) {
                                    Ok(()) => "Opened Full Console ↗".to_owned(),
                                    Err(e) => e.to_string(),
                                });
                                cx.notify();
                            }))
                            .child("Console ↗"),
                    )
                    .child(
                        div()
                            .id("prov-hud-btn")
                            .h(px(28.0))
                            .px(px(9.0))
                            .rounded(px(7.0))
                            .bg(p.pearl_bg)
                            .text_size(px(10.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(p.pearl_fg)
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |v, _, _, cx| {
                                v.toast_line = Some(match open_local_url(&mini_hud_url) {
                                    Ok(()) => "Opened Mini HUD Strip ↗".to_owned(),
                                    Err(e) => e.to_string(),
                                });
                                cx.notify();
                            }))
                            .child("Mini HUD ↗"),
                    ),
            );

        let mut content = div()
            .id("providers-scroll")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(top_bar);

        if let Some(t) = ext {
            // 2-Column Provider Grid
            if !t.providers.is_empty() {
                let mut rows_col = div().flex().flex_col().gap(px(8.0));
                for pair in t.providers.chunks(2) {
                    let mut row = div().flex().flex_row().gap(px(8.0));
                    for prov in pair {
                        let (badge_col, badge_bg, led_theme) = match prov.status_badge.as_str() {
                            "LOW" => (p.status_danger, rgba(0xFF5A_4722), PhosphorTheme::Red),
                            "WARN" => (p.status_warn, rgba(0xFBBF_2422), PhosphorTheme::Amber),
                            "OFF" => (p.text_muted, rgba(0xFFFF_FF10), PhosphorTheme::White),
                            _ => (p.status_ok, rgba(0x34D3_9922), PhosphorTheme::Emerald),
                        };
                        let mut card = div()
                            .flex_1()
                            .min_w_0()
                            .p(px(11.0))
                            .rounded(px(11.0))
                            .bg(p.card_bg)
                            .border_1()
                            .border_color(p.border_hairline)
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(11.5))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(p.text_primary)
                                            .child(prov.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .px(px(5.5))
                                            .py(px(1.5))
                                            .rounded(px(4.0))
                                            .bg(badge_bg)
                                            .text_size(px(8.0))
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .text_color(badge_col)
                                            .child(prov.status_badge.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(p.text_primary)
                                    .child(clip_text(&prov.headline, 28)),
                            );

                        if let Some(pct) = prov.remaining_percent {
                            card = card.child(segmented_led_bar(
                                16,
                                percent_to_f32_fraction(pct),
                                led_theme,
                                p.led_unlit,
                                11.0,
                                4.5,
                                2.5,
                            ));
                        }

                        card = card.child(
                            div()
                                .whitespace_nowrap()
                                .text_size(px(9.5))
                                .text_color(p.text_muted)
                                .child(clip_text(&prov.subline, 38)),
                        );

                        row = row.child(card);
                    }
                    if pair.len() == 1 {
                        row = row.child(div().flex_1());
                    }
                    rows_col = rows_col.child(row);
                }
                content = content.child(rows_col);
            }

            // Tracked Antigravity Server Quotas
            if !t.antigravity_quotas.is_empty() {
                let mut ag_card = div()
                    .p(px(12.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(p.text_muted)
                                    .child("ANTIGRAVITY SERVER QUOTAS (5H / WEEKLY)"),
                            )
                            .child(
                                div()
                                    .text_size(px(9.5))
                                    .text_color(p.text_muted)
                                    .child(format!("{} accounts", t.antigravity_quotas.len())),
                            ),
                    );

                for q in &t.antigravity_quotas {
                    let display_id = if !q.email.is_empty() {
                        format_email_display(&q.email, false, self.hide_emails)
                    } else {
                        q.name.clone()
                    };
                    let g5 = q
                        .gemini_5h
                        .as_ref()
                        .map_or_else(|| "–".to_owned(), |b| format!("{:.0}%", b.remaining_percent));
                    let gw = q
                        .gemini_weekly
                        .as_ref()
                        .map_or_else(|| "–".to_owned(), |b| format!("{:.0}%", b.remaining_percent));
                    let cg5 = q
                        .claude_gpt_5h
                        .as_ref()
                        .map_or_else(|| "–".to_owned(), |b| format!("{:.0}%", b.remaining_percent));
                    let cgw = q
                        .claude_gpt_weekly
                        .as_ref()
                        .map_or_else(|| "–".to_owned(), |b| format!("{:.0}%", b.remaining_percent));

                    ag_card = ag_card.child(
                        div()
                            .px(px(10.0))
                            .py(px(7.0))
                            .rounded(px(8.0))
                            .bg(p.row_bg)
                            .border_1()
                            .border_color(p.border_subtle)
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(p.text_primary)
                                    .child(clip_text(&display_id, 28)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.0))
                                    .text_size(px(10.0))
                                    .child(
                                        div()
                                            .text_color(p.accent_cyan)
                                            .child(format!("G {g5} / {gw}w")),
                                    )
                                    .child(
                                        div()
                                            .text_color(p.status_ok)
                                            .child(format!("C&G {cg5} / {cgw}w")),
                                    ),
                            ),
                    );
                }

                content = content.child(ag_card);
            }
        } else {
            content = content.child(
                div()
                    .p(px(16.0))
                    .rounded(px(12.0))
                    .bg(p.card_bg)
                    .border_1()
                    .border_color(p.border_hairline)
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(p.text_primary)
                            .child("Local Telemetry Bridge not connected"),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(p.text_secondary)
                            .child(
                                "AGY Swap works standalone with local 5h timers (Zero OAuth). To overlay live multi-provider credits and exact server quotas, start your local AI Usage Tracker server on localhost:8289 or click ↻ Fresh.",
                            ),
                    ),
            );
        }

        content.into_any_element()
    }
}

impl Render for AgSwitchView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = SamoPalette::for_theme(self.light_theme);
        let header = self.render_header(&p, cx);
        let snap_clone = self.snapshot.clone();
        let page_clone = self.page.clone();

        let body: AnyElement = match (&snap_clone, page_clone) {
            (_, PopoverPage::AppSettings) => {
                self.render_app_settings(snap_clone.as_ref(), &p, cx)
            }
            (_, PopoverPage::PhosphorLab) => self.render_phosphor_lab(&p, cx),
            (_, PopoverPage::AccountForm) => self.render_account_form(&p, cx),
            (None, _) => {
                let led = self.phosphor.render_matrix(
                    "pop-boot",
                    LedSpec::compact_5x5(LedMode::Analysing, PhosphorTheme::Red, 0.0),
                );
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(hw_pill(led, "Loading AGY Swap…", None))
                    .into_any_element()
            }
            (Some(snap), PopoverPage::SplitOverview) => self.render_split_overview(snap, &p, cx),
            (Some(snap), PopoverPage::ModelQuotaDetail(pid)) => {
                self.render_model_quota_detail(&pid, snap, &p, cx)
            }
            (Some(snap), PopoverPage::AccountDetail(pid)) => {
                self.render_account_detail(&pid, snap, &p, cx)
            }
            (Some(snap), PopoverPage::RulesAndAudit) => self.render_rules_and_audit(snap, &p),
            (Some(snap), PopoverPage::Providers) => self.render_providers_page(snap, &p, cx),
        };

        let mut root = div()
            .id("agyswap-popover")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .relative()
            .size_full()
            .rounded(px(16.0))
            .overflow_hidden()
            .bg(p.shell_bg)
            .border_1()
            .border_color(p.border_outer)
            .text_color(p.text_primary)
            .flex()
            .flex_col()
            .child(header)
            .child(body);

        if let Some(action) = &self.confirm_action {
            root = root.child(self.render_confirm_overlay(action, &p, cx));
        }

        root
    }
}

fn render_timer_pool_row(
    label: &'static str,
    clock_str: &str,
    unit_label: &'static str,
    fill_fraction: f32,
    theme: PhosphorTheme,
    caption: String,
    p: &SamoPalette,
) -> impl IntoElement + use<> {
    let digits = dot_matrix_readout(clock_str, 1.7, 0.6, p.digit_lit, p.digit_unlit);
    let bar = segmented_led_bar(
        16,
        fill_fraction,
        theme,
        p.led_unlit,
        16.0,
        5.5,
        3.0,
    );

    div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .child(
            div()
                .flex()
                .items_end()
                .justify_between()
                .child(
                    div()
                        .text_size(px(9.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(p.text_muted)
                        .child(label),
                )
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap(px(4.0))
                        .child(digits)
                        .child(
                            div()
                                .text_size(px(9.0))
                                .text_color(p.text_muted)
                                .child(unit_label),
                        ),
                ),
        )
        .child(bar)
        .child(
            div()
                .whitespace_nowrap()
                .text_size(px(9.5))
                .text_color(p.text_secondary)
                .child(caption),
        )
}

/// Converts remaining seconds in a 5-hour (18,000s) local window into a `0.0..=1.0` LED bar fill fraction.
/// `None` means no active 5h rolling window (full 5:00 ready -> `1.0`).
fn window_5h_fraction(secs: Option<u64>) -> f32 {
    let Some(remaining) = secs else {
        return 1.0;
    };
    let clamped = remaining.min(18_000);
    let secs_u16 = u16::try_from(clamped).unwrap_or(18_000);
    f32::from(secs_u16) / 18_000.0
}

/// Converts a `0.0..=100.0` remaining percentage into a `0.0..=1.0` LED bar fill fraction without `as` casts.
fn percent_to_f32_fraction(pct: f64) -> f32 {
    let clamped = pct.clamp(0.0, 100.0);
    let parsed: f32 = format!("{clamped:.1}").parse().unwrap_or(0.0);
    (parsed / 100.0).clamp(0.0, 1.0)
}

fn timer_bar_theme(
    secs: Option<u64>,
    is_cooldown: bool,
    active_theme: PhosphorTheme,
) -> PhosphorTheme {
    if is_cooldown {
        PhosphorTheme::Amber
    } else if secs.is_some() {
        active_theme
    } else {
        PhosphorTheme::Emerald
    }
}

fn timer_unit_label(secs: Option<u64>, is_cooldown: bool) -> &'static str {
    if is_cooldown {
        "cooldown"
    } else if secs.is_some() {
        "left"
    } else {
        "ready"
    }
}

fn clip_text(raw: &str, max_chars: usize) -> String {
    let count = raw.chars().count();
    if count <= max_chars {
        return raw.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let prefix: String = raw.chars().take(keep).collect();
    format!("{prefix}…")
}

fn render_compact_audit_row(entry: &AuditEntry, p: &SamoPalette) -> impl IntoElement + use<> {
    let col = match entry.outcome {
        AuditOutcome::Ok => p.status_ok,
        AuditOutcome::Blocked => p.status_warn,
        AuditOutcome::Failed | AuditOutcome::Panicked => p.status_danger,
    };
    div()
        .px(px(8.0))
        .py(px(5.0))
        .rounded(px(7.0))
        .bg(p.card_bg)
        .border_1()
        .border_color(p.border_subtle)
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(p.text_primary)
                        .child(entry.command.as_str()),
                )
                .child(
                    div()
                        .text_size(px(9.0))
                        .text_color(col)
                        .child(entry.timestamp_utc.format("%H:%M:%S").to_string()),
                ),
        )
        .child(
            div()
                .text_size(px(9.5))
                .text_color(p.text_muted)
                .child(entry.summary.clone()),
        )
}

fn describe_response(resp: &CommandResponse) -> String {
    match resp {
        CommandResponse::Dashboard(_) => "Synced ✓".to_owned(),
        CommandResponse::Switched { target_name, .. } => format!("Swapped → {target_name}"),
        CommandResponse::OpenedAlongside {
            target_name,
            focused_existing,
            ..
        } => {
            if *focused_existing {
                format!("Focused {target_name}")
            } else {
                format!("Opened {target_name}")
            }
        }
        CommandResponse::Stopped { profile_id, .. } => format!("Stopped {profile_id}"),
        CommandResponse::AccountSaved(p) => format!("Saved {}", p.display_name),
        CommandResponse::AccountRemoved { hid_default, .. } => {
            if *hid_default {
                "Hid primary profile".to_owned()
            } else {
                "Removed account".to_owned()
            }
        }
        CommandResponse::CooldownToggled { is_on_cooldown, .. } => {
            if *is_on_cooldown {
                "5h cooldown active".to_owned()
            } else {
                "Cooldown cleared".to_owned()
            }
        }
        CommandResponse::SharedChatsUpdated { enabled, .. } => {
            if *enabled {
                "Shared chats linked".to_owned()
            } else {
                "Isolated chats active".to_owned()
            }
        }
        CommandResponse::LoginReset { .. } => "Reset login & opened Sign-In".to_owned(),
        CommandResponse::AuditLog(_) => "Loaded audit log".to_owned(),
    }
}

fn format_email_display(raw: &str, is_default: bool, hide_emails: bool) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return if is_default {
            "Primary OS profile".to_owned()
        } else {
            "Isolated Gmail vault".to_owned()
        };
    }
    if !hide_emails {
        return trimmed.to_owned();
    }
    if let Some((local, domain)) = trimmed.split_once('@') {
        let prefix: String = local.chars().take(2).collect();
        format!("{prefix}•••@{domain}")
    } else {
        "••••••".to_owned()
    }
}

fn load_ui_prefs(path: &Path) -> UiPrefs {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_ui_prefs(path: &Path, prefs: &UiPrefs) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(prefs) {
        let _ = fs::write(path, json);
    }
}

fn parse_showcase_mode(name: &str) -> Option<(&'static str, LedMode)> {
    LedMode::SHOWCASE_MODES
        .into_iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
        .map(|(n, m, _)| (n, m))
}

fn parse_theme_name(name: &str) -> Option<PhosphorTheme> {
    PhosphorTheme::ALL
        .into_iter()
        .find(|t| t.label().eq_ignore_ascii_case(name))
}

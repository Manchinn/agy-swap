use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agswitch_domain::format_clock_hm;
use agswitch_engine::Engine;
use agswitch_spec::{
    AccountView, CallerContext, CommandRequest, CommandResponse, DashboardSnapshot,
    ExternalTelemetrySnapshot,
};
use gpui::{
    AnyElement, Context, Hsla, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, WindowControlArea, div, px, rgba,
};

use crate::phosphor::{PhosphorTheme, segmented_led_bar};
use crate::view::{
    SamoPalette, clip_text, percent_to_f32_fraction, server_pct_theme, timer_bar_theme,
    window_5h_fraction,
};
use crate::{DesktopIntent, PopoverTarget};

pub struct MiniHudView {
    engine: Arc<Engine>,
    rt: Arc<tokio::runtime::Runtime>,
    shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
    ui_prefs_path: PathBuf,
    intent_tx: Sender<DesktopIntent>,
    on_close_notify: Rc<dyn Fn() + 'static>,
    snapshot: Option<DashboardSnapshot>,
    light_theme: bool,
    busy_label: Option<String>,
    focus_handle: gpui::FocusHandle,
}

impl MiniHudView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        engine: Arc<Engine>,
        rt: Arc<tokio::runtime::Runtime>,
        shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
        ui_prefs_path: PathBuf,
        intent_tx: Sender<DesktopIntent>,
        on_close: impl Fn() + 'static,
    ) -> Self {
        let initial_snap = shared_snap.lock().ok().and_then(|g| g.clone());
        let light_theme = read_light_theme(&ui_prefs_path);
        let focus_handle = cx.focus_handle();
        let on_close_notify: Rc<dyn Fn() + 'static> = Rc::new(on_close);
        let on_close_for_os = on_close_notify.clone();

        window.on_window_should_close(cx, move |_window, _cx| {
            on_close_for_os();
            true
        });

        let needs_initial_fetch = initial_snap.is_none();
        let view = Self {
            engine,
            rt,
            shared_snap,
            ui_prefs_path,
            intent_tx,
            on_close_notify,
            snapshot: initial_snap,
            light_theme,
            busy_label: None,
            focus_handle,
        };

        if needs_initial_fetch {
            view.dispatch_command(CommandRequest::GetDashboard, None, cx);
        }

        // Lightweight 1s sync loop reading shared_snap (exits immediately when window closes -> 0% CPU)
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let keep_alive = weak
                    .update(cx, |v, cx| {
                        let next_light = read_light_theme(&v.ui_prefs_path);
                        let mut changed = next_light != v.light_theme;
                        v.light_theme = next_light;

                        if let Ok(guard) = v.shared_snap.lock()
                            && let Some(latest) = guard.as_ref()
                            && v.snapshot.as_ref() != Some(latest)
                        {
                            v.snapshot = Some(latest.clone());
                            changed = true;
                        }
                        if changed {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !keep_alive {
                    break;
                }
            }
        })
        .detach();

        view
    }

    fn dispatch_command(
        &self,
        req: CommandRequest,
        busy_text: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine.clone();
        let rt = self.rt.clone();
        let shared = self.shared_snap.clone();
        let weak = cx.entity().downgrade();

        if let Some(label) = busy_text {
            let _ = weak.update(cx, |v, cx| {
                v.busy_label = Some(label.to_owned());
                cx.notify();
            });
        }

        cx.spawn(async move |_this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let _ = rt.block_on(engine.execute(req, CallerContext::local_interactive()));
                    rt.block_on(engine.execute(
                        CommandRequest::GetDashboard,
                        CallerContext::local_interactive(),
                    ))
                })
                .await;

            let _ = weak.update(cx, |v, cx| {
                v.busy_label = None;
                if let Ok(CommandResponse::Dashboard(dash)) = result {
                    if let Ok(mut guard) = shared.lock() {
                        *guard = Some(dash.clone());
                    }
                    v.snapshot = Some(dash);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn close_hud(&self, window: &mut Window) {
        (self.on_close_notify)();
        window.remove_window();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if event.keystroke.key.as_str() == "escape" {
            self.close_hud(window);
        }
    }

    fn open_popover_target(&self, target: PopoverTarget) {
        let _ = self.intent_tx.send(DesktopIntent::OpenPopoverPage(target));
    }

    fn render_opencode_cell(
        &self,
        ext: Option<&ExternalTelemetrySnapshot>,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (headline, subline, frac, theme, dot_col) = if let Some(oc) =
            ext.and_then(|t| t.opencode_go.as_ref())
        {
            let rem_5h = (100.0 - oc.rolling_used_pct).clamp(0.0, 100.0);
            let rem_wk = (100.0 - oc.weekly_used_pct).clamp(0.0, 100.0);
            let worst = rem_5h.min(rem_wk);
            let theme = server_pct_theme(worst, PhosphorTheme::Cyan);
            (
                format!("{rem_5h:.0}%"),
                format!("5h · Wk {rem_wk:.0}%"),
                percent_to_f32_fraction(rem_5h),
                theme,
                theme.accent(),
            )
        } else {
            (
                "OFF".to_owned(),
                "Feeder offline".to_owned(),
                0.0,
                PhosphorTheme::White,
                p.text_muted,
            )
        };

        render_hud_cell(
            "hud-cell-opencode",
            "OPENCODE",
            headline,
            subline,
            frac,
            theme,
            dot_col,
            p,
            cx.listener(|v, _, _, _| v.open_popover_target(PopoverTarget::Providers)),
        )
    }

    fn render_antigravity_cell(
        &self,
        snap: Option<&DashboardSnapshot>,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active: Option<&AccountView> = snap.and_then(|s| {
            s.accounts
                .iter()
                .find(|a| a.is_running)
                .or_else(|| s.accounts.first())
        });

        let (title, headline, subline, frac, theme, dot_col) = if let Some(acc) = active {
            let short_name = clip_text(&acc.display_name, 11);
            if let Some(sq) = acc.server_quota.as_ref().filter(|q| q.ok) {
                let g5 = sq.gemini_5h.as_ref().map(|b| b.remaining_percent);
                let cg5 = sq.claude_gpt_5h.as_ref().map(|b| b.remaining_percent);
                let worst = match (g5, cg5) {
                    (Some(a), Some(b)) => a.min(b),
                    (Some(a), None) => a,
                    (None, Some(b)) => b,
                    (None, None) => 100.0,
                };
                let g_txt = g5.map_or_else(|| "–".to_owned(), |v| format!("{v:.0}%"));
                let c_txt = cg5.map_or_else(|| "–".to_owned(), |v| format!("{v:.0}%"));
                let theme = server_pct_theme(worst, PhosphorTheme::Emerald);
                (
                    short_name,
                    format!("G{g_txt} C{c_txt}"),
                    if acc.is_running {
                        "Active · Live".to_owned()
                    } else {
                        "Idle · Live".to_owned()
                    },
                    percent_to_f32_fraction(worst),
                    theme,
                    theme.accent(),
                )
            } else {
                let secs = acc.models.gemini_reset_5h_secs.or(acc.reset_in_secs);
                let clock = format_clock_hm(secs);
                let theme =
                    timer_bar_theme(secs, acc.is_on_cooldown, PhosphorTheme::Emerald);
                (
                    short_name,
                    clock,
                    if acc.is_on_cooldown {
                        "Cooldown".to_owned()
                    } else if acc.is_running {
                        "Active · Local".to_owned()
                    } else {
                        "Ready".to_owned()
                    },
                    window_5h_fraction(secs),
                    theme,
                    theme.accent(),
                )
            }
        } else {
            (
                "ANTIGRAV".to_owned(),
                "–".to_owned(),
                "No accounts".to_owned(),
                0.0,
                PhosphorTheme::White,
                p.text_muted,
            )
        };

        let bar = segmented_led_bar(8, frac, theme, p.led_unlit, 6.0, 4.0, 2.0);
        let swapping = self.busy_label.as_deref() == Some("Swapping");

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(10.0))
            .bg(p.row_bg)
            .border_1()
            .border_color(p.border_subtle)
            .flex()
            .flex_col()
            .justify_between()
            .gap(px(2.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(4.0))
                    .child(
                        div()
                            .id("hud-cell-ag-open-hdr")
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(4.5))
                            .cursor_pointer()
                            .on_click(cx.listener(|v, _, _, _| {
                                v.open_popover_target(PopoverTarget::SplitOverview);
                            }))
                            .child(
                                div()
                                    .w(px(5.5))
                                    .h(px(5.5))
                                    .rounded(px(999.0))
                                    .bg(dot_col)
                                    .flex_shrink_0(),
                            )
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .text_size(px(8.5))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(p.text_muted)
                                    .child(title),
                            ),
                    )
                    .child(
                        div()
                            .id("hud-ag-swap-btn")
                            .h(px(16.0))
                            .px(px(5.0))
                            .rounded(px(4.0))
                            .bg(if swapping {
                                rgba(0x6FD8_EE28).into()
                            } else {
                                p.btn_dark
                            })
                            .border_1()
                            .border_color(p.border_hairline)
                            .text_size(px(9.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(p.accent_cyan)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|s| s.border_color(p.accent_cyan))
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.dispatch_command(
                                    CommandRequest::SwitchNext,
                                    Some("Swapping"),
                                    cx,
                                );
                            }))
                            .child(if swapping { "…" } else { "⇄" }),
                    ),
            )
            .child(
                div()
                    .id("hud-cell-ag-open-body")
                    .flex()
                    .flex_col()
                    .gap(px(2.5))
                    .cursor_pointer()
                    .on_click(cx.listener(|v, _, _, _| {
                        v.open_popover_target(PopoverTarget::SplitOverview);
                    }))
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(p.text_primary)
                            .child(headline),
                    )
                    .child(bar)
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_size(px(8.0))
                            .text_color(p.text_muted)
                            .child(clip_text(&subline, 18)),
                    ),
            )
            .into_any_element()
    }

    fn render_maxplus_cell(
        &self,
        ext: Option<&ExternalTelemetrySnapshot>,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (headline, subline, frac, theme, dot_col) = if let Some(t) = ext
            && let Some(mp) = t.maxplus.as_ref()
        {
            let theme = if mp.credit_usd < 3.0 {
                PhosphorTheme::Red
            } else if mp.credit_usd < 10.0 {
                PhosphorTheme::Amber
            } else {
                PhosphorTheme::Emerald
            };
            // Normalize bar visually against a $30 reference tank (clamped 0..=100%)
            let tank_pct = ((mp.credit_usd / 30.0) * 100.0).clamp(0.0, 100.0);
            let sub = match (t.runway_days, t.burn_per_day_usd) {
                (Some(rw), Some(b)) => format!("{rw:.1}d · ${b:.2}/d"),
                (Some(rw), None) => format!("{rw:.1}d runway"),
                (None, Some(b)) => format!("${b:.2}/d burn"),
                (None, None) => "Pay-as-you-go".to_owned(),
            };
            (
                format!("${:.2}", mp.credit_usd),
                sub,
                percent_to_f32_fraction(tank_pct),
                theme,
                theme.accent(),
            )
        } else {
            (
                "–".to_owned(),
                "Offline".to_owned(),
                0.0,
                PhosphorTheme::White,
                p.text_muted,
            )
        };

        render_hud_cell(
            "hud-cell-maxplus",
            "MAXPLUS",
            headline,
            subline,
            frac,
            theme,
            dot_col,
            p,
            cx.listener(|v, _, _, _| v.open_popover_target(PopoverTarget::Providers)),
        )
    }

    fn render_tokenme_cell(
        &self,
        ext: Option<&ExternalTelemetrySnapshot>,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (headline, subline, frac, theme, dot_col) = if let Some(tm) =
            ext.and_then(|t| t.tokenme.as_ref())
        {
            let sub = match (tm.days_left, tm.hours_left) {
                (Some(d), Some(h)) => format!("{d}d {h}h left"),
                (Some(d), None) => format!("{d}d left"),
                (None, Some(h)) => format!("{h}h left"),
                (None, None) => format!("{} reqs", tm.cycle_requests),
            };
            (
                format!("{:.0}% cyc", tm.cycle_elapsed_pct),
                sub,
                percent_to_f32_fraction(tm.cycle_elapsed_pct),
                PhosphorTheme::Cyan,
                PhosphorTheme::Cyan.accent(),
            )
        } else {
            (
                "–".to_owned(),
                "Offline".to_owned(),
                0.0,
                PhosphorTheme::White,
                p.text_muted,
            )
        };

        render_hud_cell(
            "hud-cell-tokenme",
            "TOKENME",
            headline,
            subline,
            frac,
            theme,
            dot_col,
            p,
            cx.listener(|v, _, _, _| v.open_popover_target(PopoverTarget::Providers)),
        )
    }

    fn render_firecrawl_cell(
        &self,
        ext: Option<&ExternalTelemetrySnapshot>,
        p: &SamoPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (headline, subline, frac, theme, dot_col) = if let Some(fc) =
            ext.and_then(|t| t.firecrawl.as_ref())
        {
            let theme = server_pct_theme(fc.remaining_percent, PhosphorTheme::Emerald);
            (
                format!("{}/{}", fc.remaining_credits, fc.plan_credits),
                format!("{:.0}% left", fc.remaining_percent),
                percent_to_f32_fraction(fc.remaining_percent),
                theme,
                theme.accent(),
            )
        } else {
            (
                "–".to_owned(),
                "Offline".to_owned(),
                0.0,
                PhosphorTheme::White,
                p.text_muted,
            )
        };

        render_hud_cell(
            "hud-cell-firecrawl",
            "FIRECRAWL",
            headline,
            subline,
            frac,
            theme,
            dot_col,
            p,
            cx.listener(|v, _, _, _| v.open_popover_target(PopoverTarget::Providers)),
        )
    }
}

impl Render for MiniHudView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = SamoPalette::for_theme(self.light_theme);
        let snap = self.snapshot.as_ref();
        let ext = snap
            .and_then(|s| s.external_telemetry.as_ref())
            .filter(|t| t.connected);
        let refreshing = self.busy_label.as_deref() == Some("Refreshing");

        let drag_grip = div()
            .id("hud-drag-grip")
            .window_control_area(WindowControlArea::Drag)
            .w(px(18.0))
            .h_full()
            .rounded(px(8.0))
            .bg(p.row_bg)
            .border_1()
            .border_color(p.border_subtle)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(2.5))
            .cursor_grab()
            .child(grip_dot_pair(&p))
            .child(grip_dot_pair(&p))
            .child(grip_dot_pair(&p));

        let controls = div()
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .justify_between()
            .gap(px(4.0))
            .child(
                div()
                    .id("hud-refresh-btn")
                    .w(px(24.0))
                    .flex_1()
                    .rounded(px(6.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .text_size(px(10.0))
                    .text_color(if refreshing {
                        p.accent_cyan
                    } else {
                        p.text_primary
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.border_color(p.border_outer))
                    .on_click(cx.listener(|v, _, _, cx| {
                        v.dispatch_command(
                            CommandRequest::RefreshTelemetry,
                            Some("Refreshing"),
                            cx,
                        );
                    }))
                    .child(if refreshing { "·" } else { "↻" }),
            )
            .child(
                div()
                    .id("hud-close-btn")
                    .w(px(24.0))
                    .flex_1()
                    .rounded(px(6.0))
                    .bg(p.btn_dark)
                    .border_1()
                    .border_color(p.border_hairline)
                    .text_size(px(10.5))
                    .text_color(p.text_muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.border_color(p.status_danger).text_color(p.status_danger))
                    .on_click(cx.listener(|v, _, window, _cx| {
                        v.close_hud(window);
                    }))
                    .child("×"),
            );

        div()
            .id("agyswap-mini-hud")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .p(px(5.0))
            .rounded(px(14.0))
            .overflow_hidden()
            .bg(p.shell_bg)
            .border_1()
            .border_color(p.border_outer)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .child(drag_grip)
            .child(self.render_opencode_cell(ext, &p, cx))
            .child(self.render_antigravity_cell(snap, &p, cx))
            .child(self.render_maxplus_cell(ext, &p, cx))
            .child(self.render_tokenme_cell(ext, &p, cx))
            .child(self.render_firecrawl_cell(ext, &p, cx))
            .child(controls)
    }
}

#[allow(clippy::too_many_arguments)]
fn render_hud_cell(
    id: &'static str,
    label: &'static str,
    headline: String,
    subline: String,
    frac: f32,
    theme: PhosphorTheme,
    dot_col: Hsla,
    p: &SamoPalette,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let bar = segmented_led_bar(8, frac, theme, p.led_unlit, 6.0, 4.0, 2.0);
    div()
        .id(id)
        .flex_1()
        .min_w_0()
        .h_full()
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(10.0))
        .bg(p.row_bg)
        .border_1()
        .border_color(p.border_subtle)
        .hover(|s| s.border_color(p.border_outer))
        .cursor_pointer()
        .on_click(on_click)
        .flex()
        .flex_col()
        .justify_between()
        .gap(px(2.5))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.5))
                .child(
                    div()
                        .w(px(5.5))
                        .h(px(5.5))
                        .rounded(px(999.0))
                        .bg(dot_col)
                        .flex_shrink_0(),
                )
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_size(px(8.5))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(p.text_muted)
                        .child(label),
                ),
        )
        .child(
            div()
                .whitespace_nowrap()
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(p.text_primary)
                .child(clip_text(&headline, 12)),
        )
        .child(bar)
        .child(
            div()
                .whitespace_nowrap()
                .text_size(px(8.0))
                .text_color(p.text_muted)
                .child(clip_text(&subline, 18)),
        )
        .into_any_element()
}

fn grip_dot_pair(p: &SamoPalette) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_row()
        .gap(px(2.5))
        .child(div().w(px(2.5)).h(px(2.5)).rounded(px(999.0)).bg(p.text_muted))
        .child(div().w(px(2.5)).h(px(2.5)).rounded(px(999.0)).bg(p.text_muted))
}

fn read_light_theme(ui_prefs_path: &Path) -> bool {
    fs::read_to_string(ui_prefs_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("light_theme").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod hud;
mod phosphor;
mod tray;
mod view;

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agswitch_engine::{Engine, EngineDefaults, ProcessPort, TelemetryFeedPort, TranscriptPort};
use agswitch_integrations::{
    LocalTelemetryBridgeAdapter, OsProcessAdapter, OsTranscriptAdapter, OsWorkspaceLinkAdapter,
    SystemClockAdapter, default_antigravity_bin_path, default_data_dir, default_profiles_root_dir,
    open_directory_in_file_manager, primary_home_dir, sandbox,
};
use agswitch_primitives::RuntimeOs;
use agswitch_spec::{CallerContext, CommandRequest, CommandResponse, DashboardSnapshot};
use agswitch_store::JsonFileStore;
use gpui::{
    App, AppContext, Application, Bounds, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, point, px, size,
};

use crate::hud::MiniHudView;
use crate::tray::{TrayAction, TrayAnchorRect, TrayController};
use crate::view::AgSwitchView;

pub const FULL_POPOVER_W: f32 = 680.0;
pub const FULL_POPOVER_H: f32 = 524.0;
pub const COMPACT_POPOVER_W: f32 = 320.0;
pub const COMPACT_POPOVER_H: f32 = 420.0;
pub const MINI_HUD_W: f32 = 780.0;
pub const MINI_HUD_H: f32 = 76.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopoverTarget {
    SplitOverview,
    Providers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopIntent {
    ToggleMiniHud,
    OpenPopoverPage(PopoverTarget),
}

struct PopoverSlot {
    handle: Option<WindowHandle<AgSwitchView>>,
    last_toggle: Instant,
}

struct HudSlot {
    handle: Option<WindowHandle<MiniHudView>>,
}

fn main() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => Arc::new(rt),
        Err(err) => {
            eprintln!("failed to initialize async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    let args: Vec<String> = std::env::args().collect();
    let is_sandbox = args.iter().any(|a| a == "--sandbox" || a == "--demo")
        || std::env::var("AGSWITCH_SANDBOX")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let open_hud_on_start = args.iter().any(|a| a == "--hud");
    let demo_page = if is_sandbox {
        args.windows(2)
            .find(|w| w.first().is_some_and(|a| a == "--demo-page"))
            .and_then(|w| w.get(1).cloned())
    } else {
        None
    };

    let (data_dir, profiles_root, engine) = if is_sandbox {
        let base = sandbox::default_sandbox_dir();
        let paths = match sandbox::ensure_seeded(&base) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("failed to initialize sandbox: {err}");
                return ExitCode::FAILURE;
            }
        };
        let proc_adapter: Arc<dyn ProcessPort> = Arc::new(sandbox::SandboxProcessAdapter::new(
            paths.running_state_file,
        ));
        let tx_adapter: Arc<dyn TranscriptPort> = Arc::new(sandbox::SandboxTranscriptAdapter);
        let tel_adapter: Arc<dyn TelemetryFeedPort> = Arc::new(sandbox::SandboxTelemetryAdapter);
        let engine = Arc::new(
            Engine::new(
                EngineDefaults {
                    profiles_root_dir: paths.profiles_root.to_string_lossy().into_owned(),
                    antigravity_bin_path: "/sandbox/Antigravity".to_owned(),
                    runtime_os: RuntimeOs::current(),
                },
                Arc::new(JsonFileStore::new(paths.data_dir.clone())),
                proc_adapter,
                Arc::new(OsWorkspaceLinkAdapter::new(paths.fake_home)),
                tx_adapter,
                Arc::new(SystemClockAdapter),
            )
            .with_telemetry(tel_adapter),
        );
        (paths.data_dir, paths.profiles_root, engine)
    } else {
        let data_dir = default_data_dir();
        let profiles_root = default_profiles_root_dir(&data_dir);
        let primary_home = primary_home_dir();
        let engine = Arc::new(
            Engine::new(
                EngineDefaults {
                    profiles_root_dir: profiles_root.to_string_lossy().into_owned(),
                    antigravity_bin_path: default_antigravity_bin_path(),
                    runtime_os: RuntimeOs::current(),
                },
                Arc::new(JsonFileStore::new(data_dir.clone())),
                Arc::new(OsProcessAdapter::new()),
                Arc::new(OsWorkspaceLinkAdapter::new(primary_home.clone())),
                Arc::new(OsTranscriptAdapter::new(primary_home)),
                Arc::new(SystemClockAdapter),
            )
            .with_telemetry(Arc::new(LocalTelemetryBridgeAdapter::from_defaults())),
        );
        (data_dir, profiles_root, engine)
    };

    let ui_prefs_path = data_dir.join("ui-prefs.json");
    let shared_snap: Arc<Mutex<Option<DashboardSnapshot>>> = Arc::new(Mutex::new(None));
    let hud_open = Arc::new(AtomicBool::new(false));
    let (intent_tx, intent_rx) = mpsc::channel::<DesktopIntent>();

    Application::new().run(move |cx: &mut App| {
        // Initialize top Menu Bar (macOS) / System Tray (Windows) icon on main UI thread
        let tray = Rc::new(RefCell::new(TrayController::try_create()));
        #[cfg(target_os = "windows")]
        open_windows_keepalive_window(cx);
        let popover_slot = Rc::new(RefCell::new(PopoverSlot {
            handle: None,
            last_toggle: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        }));
        let hud_slot = Rc::new(RefCell::new(HudSlot { handle: None }));

        if open_hud_on_start {
            open_mini_hud(
                cx,
                &hud_slot,
                engine.clone(),
                rt.clone(),
                shared_snap.clone(),
                ui_prefs_path.clone(),
                intent_tx.clone(),
                hud_open.clone(),
            );
        }

        // Open initial Samo-style menu-bar popover unless launched directly in --hud-only mode
        if !open_hud_on_start || demo_page.is_some() {
            let initial_anchor = tray.borrow().as_ref().and_then(TrayController::anchor_rect);
            open_popover(
                cx,
                initial_anchor,
                &popover_slot,
                engine.clone(),
                rt.clone(),
                shared_snap.clone(),
                ui_prefs_path.clone(),
                is_sandbox,
                demo_page.clone(),
                None,
                intent_tx.clone(),
                hud_open.clone(),
            );
        }

        // App-level tray, cross-window intent, & background menu sync loop
        let tray_loop = tray.clone();
        let slot_loop = popover_slot.clone();
        let hud_slot_loop = hud_slot.clone();
        let engine_loop = engine.clone();
        let rt_loop = rt.clone();
        let snap_loop = shared_snap.clone();
        let profiles_dir_loop = profiles_root.clone();
        let prefs_loop = ui_prefs_path.clone();
        let intent_tx_loop = intent_tx.clone();
        let hud_open_loop = hud_open.clone();

        cx.spawn(async move |cx| {
            let mut ticks: u32 = 0;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(65))
                    .await;
                ticks = ticks.wrapping_add(1);

                // Every ~2s sync the native tray menu tooltip & items with latest snapshot
                if ticks.is_multiple_of(30) {
                    let eng = engine_loop.clone();
                    let rt_bg = rt_loop.clone();
                    let snap_bg = snap_loop.clone();
                    let refreshed = cx
                        .background_executor()
                        .spawn(async move {
                            rt_bg.block_on(eng.execute(
                                CommandRequest::GetDashboard,
                                CallerContext::local_interactive(),
                            ))
                        })
                        .await;
                    if let Ok(CommandResponse::Dashboard(dash)) = refreshed {
                        if let Ok(mut guard) = snap_bg.lock() {
                            *guard = Some(dash.clone());
                        }
                        let _ = cx.update(|_cx| {
                            if let Some(t) = tray_loop.borrow_mut().as_mut() {
                                t.sync_theme();
                                t.sync_snapshot(&dash);
                            }
                        });
                    }
                }

                // Drain cross-window UI intents (Mini HUD toggle, HUD cell -> Popover navigation)
                while let Ok(intent) = intent_rx.try_recv() {
                    match intent {
                        DesktopIntent::ToggleMiniHud => {
                            let _ = cx.update(|cx| {
                                toggle_mini_hud(
                                    cx,
                                    &hud_slot_loop,
                                    engine_loop.clone(),
                                    rt_loop.clone(),
                                    snap_loop.clone(),
                                    prefs_loop.clone(),
                                    intent_tx_loop.clone(),
                                    hud_open_loop.clone(),
                                );
                            });
                        }
                        DesktopIntent::OpenPopoverPage(target) => {
                            let _ = cx.update(|cx| {
                                let anchor = tray_loop
                                    .borrow()
                                    .as_ref()
                                    .and_then(TrayController::anchor_rect);
                                open_or_focus_popover(
                                    cx,
                                    anchor,
                                    &slot_loop,
                                    engine_loop.clone(),
                                    rt_loop.clone(),
                                    snap_loop.clone(),
                                    prefs_loop.clone(),
                                    is_sandbox,
                                    target,
                                    intent_tx_loop.clone(),
                                    hud_open_loop.clone(),
                                );
                            });
                        }
                    }
                }

                let actions: Vec<TrayAction> = cx
                    .update(|_cx| {
                        tray_loop
                            .borrow()
                            .as_ref()
                            .map_or_else(Vec::new, TrayController::drain_actions)
                    })
                    .unwrap_or_default();

                for action in actions {
                    match action {
                        TrayAction::TogglePanel(anchor) => {
                            let _ = cx.update(|cx| {
                                toggle_popover(
                                    cx,
                                    anchor,
                                    &slot_loop,
                                    engine_loop.clone(),
                                    rt_loop.clone(),
                                    snap_loop.clone(),
                                    prefs_loop.clone(),
                                    is_sandbox,
                                    intent_tx_loop.clone(),
                                    hud_open_loop.clone(),
                                );
                            });
                        }
                        TrayAction::SwitchNext => {
                            let eng = engine_loop.clone();
                            let rt_bg = rt_loop.clone();
                            let _ = cx
                                .background_executor()
                                .spawn(async move {
                                    let _ = rt_bg.block_on(eng.execute(
                                        CommandRequest::SwitchNext,
                                        CallerContext::local_interactive(),
                                    ));
                                })
                                .await;
                        }
                        TrayAction::SwitchAccount(profile_id) => {
                            let eng = engine_loop.clone();
                            let rt_bg = rt_loop.clone();
                            let _ = cx
                                .background_executor()
                                .spawn(async move {
                                    let _ = rt_bg.block_on(eng.execute(
                                        CommandRequest::SwitchToProfile { profile_id },
                                        CallerContext::local_interactive(),
                                    ));
                                })
                                .await;
                        }
                        TrayAction::OpenUsageConsole => {
                            let _ = cx.update(|cx| {
                                let anchor = tray_loop
                                    .borrow()
                                    .as_ref()
                                    .and_then(TrayController::anchor_rect);
                                open_or_focus_popover(
                                    cx,
                                    anchor,
                                    &slot_loop,
                                    engine_loop.clone(),
                                    rt_loop.clone(),
                                    snap_loop.clone(),
                                    prefs_loop.clone(),
                                    is_sandbox,
                                    PopoverTarget::Providers,
                                    intent_tx_loop.clone(),
                                    hud_open_loop.clone(),
                                );
                            });
                        }
                        TrayAction::ToggleMiniHud => {
                            let _ = cx.update(|cx| {
                                toggle_mini_hud(
                                    cx,
                                    &hud_slot_loop,
                                    engine_loop.clone(),
                                    rt_loop.clone(),
                                    snap_loop.clone(),
                                    prefs_loop.clone(),
                                    intent_tx_loop.clone(),
                                    hud_open_loop.clone(),
                                );
                            });
                        }
                        TrayAction::OpenProfilesFolder => {
                            let _ =
                                open_directory_in_file_manager(&PathBuf::from(&profiles_dir_loop));
                        }
                        TrayAction::Quit => {
                            let _ = cx.update(|cx| cx.quit());
                            return;
                        }
                    }
                }
            }
        })
        .detach();
    });

    ExitCode::SUCCESS
}

#[allow(clippy::too_many_arguments)]
fn toggle_popover(
    cx: &mut App,
    anchor: Option<TrayAnchorRect>,
    slot: &Rc<RefCell<PopoverSlot>>,
    engine: Arc<Engine>,
    rt: Arc<tokio::runtime::Runtime>,
    shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
    ui_prefs_path: PathBuf,
    is_sandbox: bool,
    intent_tx: Sender<DesktopIntent>,
    hud_open: Arc<AtomicBool>,
) {
    let existing = slot.borrow().handle;
    if let Some(handle) = existing {
        // If window still exists, close it (toggle off -> 0% CPU while closed!)
        let was_open = cx
            .update_window(handle.into(), |_view, window, _cx| {
                window.remove_window();
            })
            .is_ok();
        slot.borrow_mut().handle = None;
        slot.borrow_mut().last_toggle = Instant::now();
        if was_open {
            return;
        }
    }

    open_popover(
        cx,
        anchor,
        slot,
        engine,
        rt,
        shared_snap,
        ui_prefs_path,
        is_sandbox,
        None,
        None,
        intent_tx,
        hud_open,
    );
}

#[allow(clippy::too_many_arguments)]
fn open_or_focus_popover(
    cx: &mut App,
    anchor: Option<TrayAnchorRect>,
    slot: &Rc<RefCell<PopoverSlot>>,
    engine: Arc<Engine>,
    rt: Arc<tokio::runtime::Runtime>,
    shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
    ui_prefs_path: PathBuf,
    is_sandbox: bool,
    target: PopoverTarget,
    intent_tx: Sender<DesktopIntent>,
    hud_open: Arc<AtomicBool>,
) {
    let existing = slot.borrow().handle;
    if let Some(handle) = existing {
        let updated = handle
            .update(cx, |view, window, cx| {
                view.navigate_to_target(target, window, cx);
                window.activate_window();
            })
            .is_ok();
        if updated {
            cx.activate(true);
            return;
        }
        slot.borrow_mut().handle = None;
    }

    open_popover(
        cx,
        anchor,
        slot,
        engine,
        rt,
        shared_snap,
        ui_prefs_path,
        is_sandbox,
        None,
        Some(target),
        intent_tx,
        hud_open,
    );
}

#[allow(clippy::too_many_arguments)]
fn open_popover(
    cx: &mut App,
    anchor: Option<TrayAnchorRect>,
    slot: &Rc<RefCell<PopoverSlot>>,
    engine: Arc<Engine>,
    rt: Arc<tokio::runtime::Runtime>,
    shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
    ui_prefs_path: PathBuf,
    is_sandbox: bool,
    demo_page: Option<String>,
    initial_target: Option<PopoverTarget>,
    intent_tx: Sender<DesktopIntent>,
    hud_open: Arc<AtomicBool>,
) {
    let is_compact = match (demo_page.as_deref(), initial_target) {
        (_, Some(PopoverTarget::Providers)) => false,
        (Some("compact" | "compact-light"), _) => true,
        (Some(_), _) => false,
        (None, _) => read_initial_compact_pref(&ui_prefs_path),
    };
    let bounds = compute_popover_bounds(cx, anchor, is_compact);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // Borderless Samo-style popover: NO system titlebar, NO traffic lights!
        titlebar: None,
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::PopUp,
        focus: true,
        show: true,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        ..WindowOptions::default()
    };

    let slot_for_close = slot.clone();
    match cx.open_window(options, move |window, cx| {
        cx.new(|cx| {
            AgSwitchView::new(
                window,
                cx,
                engine,
                rt,
                shared_snap,
                ui_prefs_path,
                is_sandbox,
                demo_page,
                initial_target,
                intent_tx,
                hud_open,
                move || {
                    slot_for_close.borrow_mut().handle = None;
                },
            )
        })
    }) {
        Ok(handle) => {
            slot.borrow_mut().handle = Some(handle);
            slot.borrow_mut().last_toggle = Instant::now();
            cx.activate(true);
        }
        Err(err) => {
            eprintln!("failed to open GPUI popover: {err}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn toggle_mini_hud(
    cx: &mut App,
    slot: &Rc<RefCell<HudSlot>>,
    engine: Arc<Engine>,
    rt: Arc<tokio::runtime::Runtime>,
    shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
    ui_prefs_path: PathBuf,
    intent_tx: Sender<DesktopIntent>,
    hud_open: Arc<AtomicBool>,
) {
    let existing = slot.borrow().handle;
    if let Some(handle) = existing {
        let was_open = cx
            .update_window(handle.into(), |_view, window, _cx| {
                window.remove_window();
            })
            .is_ok();
        slot.borrow_mut().handle = None;
        hud_open.store(false, Ordering::Relaxed);
        if was_open {
            return;
        }
    }

    open_mini_hud(
        cx,
        slot,
        engine,
        rt,
        shared_snap,
        ui_prefs_path,
        intent_tx,
        hud_open,
    );
}

#[allow(clippy::too_many_arguments)]
fn open_mini_hud(
    cx: &mut App,
    slot: &Rc<RefCell<HudSlot>>,
    engine: Arc<Engine>,
    rt: Arc<tokio::runtime::Runtime>,
    shared_snap: Arc<Mutex<Option<DashboardSnapshot>>>,
    ui_prefs_path: PathBuf,
    intent_tx: Sender<DesktopIntent>,
    hud_open: Arc<AtomicBool>,
) {
    let bounds = compute_hud_bounds(cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // Frameless transparent floating strip: movable via native drag grip (WindowControlArea::Drag)
        titlebar: None,
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::PopUp,
        focus: true,
        show: true,
        is_movable: true,
        is_resizable: false,
        is_minimizable: false,
        ..WindowOptions::default()
    };

    let slot_for_close = slot.clone();
    let hud_open_for_close = hud_open.clone();
    match cx.open_window(options, move |window, cx| {
        cx.new(|cx| {
            MiniHudView::new(
                window,
                cx,
                engine,
                rt,
                shared_snap,
                ui_prefs_path,
                intent_tx,
                move || {
                    slot_for_close.borrow_mut().handle = None;
                    hud_open_for_close.store(false, Ordering::Relaxed);
                },
            )
        })
    }) {
        Ok(handle) => {
            slot.borrow_mut().handle = Some(handle);
            hud_open.store(true, Ordering::Relaxed);
        }
        Err(err) => {
            eprintln!("failed to open GPUI Mini HUD Strip: {err}");
        }
    }
}

fn compute_hud_bounds(cx: &App) -> Bounds<gpui::Pixels> {
    let (disp_x, disp_y, disp_w, disp_h) = if let Some(display) = cx.primary_display() {
        let b = display.bounds();
        let x: f32 = b.origin.x.into();
        let y: f32 = b.origin.y.into();
        let w: f32 = b.size.width.into();
        let h: f32 = b.size.height.into();
        (x, y, w, h)
    } else {
        (0.0, 0.0, 1440.0, 900.0)
    };

    let (origin_x, origin_y) = hud_origin(
        (disp_x, disp_y, disp_w, disp_h),
        (MINI_HUD_W, MINI_HUD_H),
        cfg!(target_os = "windows"),
    );

    Bounds::new(
        point(px(origin_x), px(origin_y)),
        size(px(MINI_HUD_W), px(MINI_HUD_H)),
    )
}

/// Default origin for the floating 5-Cell Mini HUD Strip:
/// - Windows: bottom-center just above the taskbar.
/// - macOS / other: top-center just below the menu bar.
fn hud_origin(disp: (f32, f32, f32, f32), hud: (f32, f32), is_windows: bool) -> (f32, f32) {
    let (disp_x, disp_y, disp_w, disp_h) = disp;
    let (hud_w, hud_h) = hud;

    let raw_x = disp_x + (disp_w - hud_w) * 0.5;
    let raw_y = if is_windows {
        disp_y + disp_h - hud_h - 56.0
    } else {
        disp_y + 38.0
    };

    let min_x = disp_x + 10.0;
    let max_x = (disp_x + disp_w - hud_w - 10.0).max(min_x);
    let min_y = disp_y + 8.0;
    let max_y = (disp_y + disp_h - hud_h - 8.0).max(min_y);
    (raw_x.clamp(min_x, max_x), raw_y.clamp(min_y, max_y))
}

/// GPUI 0.2.2's Windows backend posts `WM_QUIT` as soon as its LAST window is destroyed
/// (`close_one_window` -> `PostQuitMessage`), which would kill the tray app every time the popover
/// is toggled off or dismissed with Escape. A never-shown 1x1 window keeps the event loop alive;
/// hidden windows receive no `WM_PAINT`, so it costs no frames. "Quit AGY Swap" still exits.
#[cfg(target_os = "windows")]
struct KeepAliveView;

#[cfg(target_os = "windows")]
impl gpui::Render for KeepAliveView {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
    }
}

#[cfg(target_os = "windows")]
fn open_windows_keepalive_window(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(0.0), px(0.0)),
            size(px(1.0), px(1.0)),
        ))),
        titlebar: None,
        window_background: WindowBackgroundAppearance::Transparent,
        kind: WindowKind::PopUp,
        focus: false,
        show: false,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        ..WindowOptions::default()
    };
    if let Err(err) = cx.open_window(options, |_window, cx| cx.new(|_cx| KeepAliveView)) {
        eprintln!("failed to open keep-alive window: {err}");
    }
}

fn read_initial_compact_pref(ui_prefs_path: &PathBuf) -> bool {
    std::fs::read_to_string(ui_prefs_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("compact_mode").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Position popover beneath the macOS Menu Bar icon, or above the Windows tray icon (DPI-aware).
fn compute_popover_bounds(
    cx: &App,
    anchor: Option<TrayAnchorRect>,
    is_compact: bool,
) -> Bounds<gpui::Pixels> {
    let pop_w = if is_compact {
        COMPACT_POPOVER_W
    } else {
        FULL_POPOVER_W
    };
    let pop_h = if is_compact {
        COMPACT_POPOVER_H
    } else {
        FULL_POPOVER_H
    };

    let (disp_x, disp_y, disp_w, disp_h) = if let Some(display) = cx.primary_display() {
        let b = display.bounds();
        let x: f32 = b.origin.x.into();
        let y: f32 = b.origin.y.into();
        let w: f32 = b.size.width.into();
        let h: f32 = b.size.height.into();
        (x, y, w, h)
    } else {
        (0.0, 0.0, 1440.0, 900.0)
    };

    let (origin_x, origin_y) = popover_origin(
        anchor,
        (disp_x, disp_y, disp_w, disp_h),
        (pop_w, pop_h),
        cfg!(target_os = "windows"),
    );

    Bounds::new(
        point(px(origin_x), px(origin_y)),
        size(px(pop_w), px(pop_h)),
    )
}

/// Popover origin in logical px for a given display `(x, y, w, h)` and popover size.
///
/// On Windows the popover opens ABOVE a bottom taskbar, but GPUI's `Window::resize` keeps the
/// top-left corner fixed and there is no move API. Since the view resizes Compact <-> Full on
/// layout toggles and page navigation, Windows positions the window by the FULL-size slot so a
/// later grow never extends past the right or bottom screen edge.
fn popover_origin(
    anchor: Option<TrayAnchorRect>,
    disp: (f32, f32, f32, f32),
    pop: (f32, f32),
    is_windows: bool,
) -> (f32, f32) {
    let (disp_x, disp_y, disp_w, disp_h) = disp;
    let (slot_w, slot_h) = if is_windows {
        (pop.0.max(FULL_POPOVER_W), pop.1.max(FULL_POPOVER_H))
    } else {
        pop
    };

    let mut origin_x = disp_x + disp_w - slot_w - 14.0;
    let mut origin_y = if is_windows {
        (disp_y + disp_h - slot_h - 54.0).max(disp_y + 12.0)
    } else {
        disp_y + 34.0
    };

    if let Some(rect) = anchor
        && rect.width > 1.0
        && rect.height > 1.0
        && (rect.x > 1.0 || rect.y > 1.0)
    {
        if is_windows {
            (origin_x, origin_y) = windows_anchor_origin(rect, disp, slot_w, slot_h);
        } else {
            // tray-icon returns physical pixels on macOS Retina (menu bar height >= 42px physical -> 2x)
            let scale = if rect.height >= 42.0 || rect.x > f64::from(disp_w) {
                2.0_f64
            } else {
                1.0_f64
            };
            let logical_cx = f64_to_f32((rect.x + rect.width * 0.5) / scale);
            let logical_bottom = f64_to_f32((rect.y + rect.height) / scale);
            origin_x = logical_cx - slot_w * 0.5;
            origin_y = (logical_bottom + 6.0).clamp(disp_y + 28.0, disp_y + 46.0);
        }
    }

    let min_x = disp_x + 10.0;
    let max_x = (disp_x + disp_w - slot_w - 10.0).max(min_x);
    let min_y = disp_y + 8.0;
    let max_y = (disp_y + disp_h - slot_h - 8.0).max(min_y);
    (origin_x.clamp(min_x, max_x), origin_y.clamp(min_y, max_y))
}

/// Infer the primary monitor's DPI scale on Windows from the tray icon's PHYSICAL rect.
///
/// GPUI reports display bounds in logical pixels (physical / scale) while `tray-icon` reports
/// `Shell_NotifyIconGetRect` in physical pixels, and GPUI exposes no scale factor before a window
/// exists. The notification area always lives on the primary monitor's taskbar (origin `0,0`)
/// docked to a screen edge, so the icon's far edge / logical display extent approximates the
/// scale; it is snapped to Windows' 25% DPI steps (100%, 125%, 150%, 175%, ...).
fn estimate_windows_tray_scale(rect: TrayAnchorRect, disp: (f32, f32, f32, f32)) -> f64 {
    let (disp_x, disp_y, disp_w, disp_h) = disp;
    let logical_right = f64::from(disp_x + disp_w);
    let logical_bottom = f64::from(disp_y + disp_h);
    if logical_right <= 1.0 || logical_bottom <= 1.0 {
        return 1.0;
    }
    let ratio_bottom = (rect.y + rect.height) / logical_bottom;
    let ratio_right = (rect.x + rect.width) / logical_right;
    let raw = ratio_bottom.max(ratio_right);
    if !raw.is_finite() || raw < 0.9 {
        // Taskbar docked top/left (Windows 10 only): the ratio carries no scale information.
        return 1.0;
    }
    ((raw * 4.0).round() / 4.0).clamp(1.0, 4.0)
}

/// Windows popover origin (logical px): horizontally centered on the tray icon, opening above a
/// bottom taskbar (or below a top one). The caller clamps the result fully on-screen.
fn windows_anchor_origin(
    rect: TrayAnchorRect,
    disp: (f32, f32, f32, f32),
    pop_w: f32,
    pop_h: f32,
) -> (f32, f32) {
    let (_, disp_y, _, disp_h) = disp;
    let scale = estimate_windows_tray_scale(rect, disp);
    let logical_cx = f64_to_f32((rect.x + rect.width * 0.5) / scale);
    let logical_top = f64_to_f32(rect.y / scale);
    let logical_bottom = f64_to_f32((rect.y + rect.height) / scale);
    let icon_mid_y = (logical_top + logical_bottom) * 0.5;

    let origin_x = logical_cx - pop_w * 0.5;
    let origin_y = if icon_mid_y >= disp_y + disp_h * 0.5 {
        logical_top - pop_h - 8.0
    } else {
        logical_bottom + 8.0
    };
    (origin_x, origin_y)
}

/// Safe `f64 -> f32` screen coordinate conversion without `as` casts.
fn f64_to_f32(val: f64) -> f32 {
    let clamped = val.clamp(-16_000.0, 16_000.0);
    let negative = clamped < 0.0;
    let abs_scaled = clamped.abs() * 10.0;
    let mut lo: u32 = 0;
    let mut hi: u32 = 160_000;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if f64::from(mid) <= abs_scaled {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    let tenths = lo;
    let mag = f32::from(u16::try_from(tenths / 10).unwrap_or(u16::MAX))
        + f32::from(u8::try_from(tenths % 10).unwrap_or(0)) * 0.1;
    if negative { -mag } else { mag }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Win11 tray icon rect (physical px) for a 1920x1080 panel at a given DPI scale.
    fn win11_icon(scale: f64) -> TrayAnchorRect {
        TrayAnchorRect {
            x: 1920.0 - 260.0 * scale,
            y: 1080.0 - 44.0 * scale,
            width: 24.0 * scale,
            height: 40.0 * scale,
        }
    }

    fn display_at(scale: f32) -> (f32, f32, f32, f32) {
        (0.0, 0.0, 1920.0 / scale, 1080.0 / scale)
    }

    #[test]
    fn windows_scale_inferred_across_common_dpi_steps() {
        for (scale, expected) in [
            (1.0_f32, 1.0_f64),
            (1.25, 1.25),
            (1.5, 1.5),
            (1.75, 1.75),
            (2.0, 2.0),
        ] {
            let got = estimate_windows_tray_scale(win11_icon(f64::from(scale)), display_at(scale));
            assert!(
                (got - expected).abs() < f64::EPSILON,
                "scale {scale}: got {got}"
            );
        }
    }

    #[test]
    fn windows_popover_stays_on_screen_and_above_taskbar() {
        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0] {
            let disp = display_at(scale);
            let (_, _, disp_w, disp_h) = disp;
            for (pop_w, pop_h) in [
                (FULL_POPOVER_W, FULL_POPOVER_H),
                (COMPACT_POPOVER_W, COMPACT_POPOVER_H),
            ] {
                let icon = win11_icon(f64::from(scale));
                let (x, y) = windows_anchor_origin(icon, disp, pop_w, pop_h);
                let icon_top_logical = f64_to_f32(icon.y / f64::from(scale));
                // Same clamping as `compute_popover_bounds`.
                let clamped_x = x.clamp(10.0, (disp_w - pop_w - 10.0).max(10.0));
                let clamped_y = y.clamp(8.0, (disp_h - pop_h - 8.0).max(8.0));
                assert!(
                    clamped_x >= 0.0 && clamped_x + pop_w <= disp_w,
                    "scale {scale}: x={clamped_x}"
                );
                assert!(clamped_y >= 0.0, "scale {scale}: y={clamped_y}");
                // Whenever the panel fits above the tray icon it opens there, never over the taskbar.
                if pop_h + 16.0 <= icon_top_logical {
                    assert!(
                        clamped_y + pop_h <= icon_top_logical,
                        "scale {scale}: y={clamped_y}"
                    );
                }
            }
        }
    }

    #[test]
    fn windows_compact_popover_can_grow_to_full_without_leaving_screen() {
        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0] {
            let disp = display_at(scale);
            let (_, _, disp_w, disp_h) = disp;
            for anchor in [Some(win11_icon(f64::from(scale))), None] {
                let (x, y) = popover_origin(
                    anchor,
                    disp,
                    (COMPACT_POPOVER_W, COMPACT_POPOVER_H),
                    true,
                );
                // Navigating to Settings / toggling Full resizes from the fixed top-left corner.
                assert!(x >= 0.0 && x + FULL_POPOVER_W <= disp_w, "scale {scale}: x={x}");
                if FULL_POPOVER_H + 16.0 <= disp_h {
                    assert!(y >= 0.0 && y + FULL_POPOVER_H <= disp_h, "scale {scale}: y={y}");
                }
            }
        }
    }

    #[test]
    fn non_windows_origin_is_unchanged_menu_bar_drop_down() {
        let (_, y) = popover_origin(None, (0.0, 0.0, 1512.0, 982.0), (FULL_POPOVER_W, FULL_POPOVER_H), false);
        assert!((y - 34.0).abs() < f32::EPSILON);
    }

    #[test]
    fn windows_scale_falls_back_for_top_docked_taskbar() {
        let icon = TrayAnchorRect {
            x: 1500.0,
            y: 2.0,
            width: 24.0,
            height: 36.0,
        };
        let got = estimate_windows_tray_scale(icon, display_at(1.0));
        assert!((got - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn hud_origin_centers_above_taskbar_on_windows_and_top_on_macos() {
        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0] {
            let disp = display_at(scale);
            let (_, _, disp_w, disp_h) = disp;
            let (wx, wy) = hud_origin(disp, (MINI_HUD_W, MINI_HUD_H), true);
            assert!(wx >= 10.0 && wx + MINI_HUD_W <= disp_w, "scale {scale}: wx={wx}");
            assert!(
                wy >= 8.0 && wy + MINI_HUD_H <= disp_h - 8.0,
                "scale {scale}: wy={wy}"
            );
            // Must stay above the ~48px Windows taskbar when display height allows
            if disp_h >= MINI_HUD_H + 64.0 {
                assert!(wy + MINI_HUD_H <= disp_h - 48.0, "scale {scale}: wy={wy}");
            }
        }

        let (mx, my) = hud_origin((0.0, 0.0, 1512.0, 982.0), (MINI_HUD_W, MINI_HUD_H), false);
        assert!((my - 38.0).abs() < f32::EPSILON);
        assert!((mx - (1512.0 - MINI_HUD_W) * 0.5).abs() < 0.5);
    }
}

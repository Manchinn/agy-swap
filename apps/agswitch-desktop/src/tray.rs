use agswitch_spec::DashboardSnapshot;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, Rect, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub const MENU_TOGGLE_PANEL: &str = "agswitch.toggle_panel";
pub const MENU_SWITCH_NEXT: &str = "agswitch.switch_next";
pub const MENU_OPEN_CONSOLE: &str = "agswitch.open_console";
pub const MENU_OPEN_MINI_HUD: &str = "agswitch.open_mini_hud";
pub const MENU_OPEN_FOLDER: &str = "agswitch.open_folder";
pub const MENU_QUIT: &str = "agswitch.quit";
pub const MENU_ACCOUNT_PREFIX: &str = "agswitch.acc.";

/// Screen-space rectangle of the top Menu Bar / System Tray icon (physical pixels).
#[derive(Debug, Clone, Copy)]
pub struct TrayAnchorRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl From<Rect> for TrayAnchorRect {
    fn from(r: Rect) -> Self {
        Self {
            x: r.position.x,
            y: r.position.y,
            width: f64::from(r.size.width),
            height: f64::from(r.size.height),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TrayAction {
    TogglePanel(Option<TrayAnchorRect>),
    SwitchNext,
    SwitchAccount(String),
    OpenUsageConsole,
    OpenMiniHud,
    OpenProfilesFolder,
    Quit,
}

/// Cross-platform top Menu Bar (macOS) / System Tray (Windows) controller.
pub struct TrayController {
    tray: TrayIcon,
    last_signature: String,
    /// Windows only: whether the taskbar currently uses the light theme (glyph drawn dark).
    light_taskbar: bool,
}

impl TrayController {
    /// Must be called on the main UI thread after the native application event loop initializes.
    pub fn try_create() -> Option<Self> {
        let light_taskbar = taskbar_uses_light_theme();
        let icon = build_embedded_icon(light_taskbar)?;
        let menu = build_menu(None);
        let builder = TrayIconBuilder::new()
            .with_tooltip("AGY Swap — Antigravity Account Swapper")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false);

        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon);

        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon);

        let tray = builder.build().ok()?;
        Some(Self {
            tray,
            last_signature: String::new(),
            light_taskbar,
        })
    }

    pub fn anchor_rect(&self) -> Option<TrayAnchorRect> {
        let r = TrayAnchorRect::from(self.tray.rect()?);
        // On macOS startup before the first runloop layout tick, NSStatusItem can report (0,0,0,0).
        if r.width <= 1.0 || r.height <= 1.0 || (r.x <= 1.0 && r.y <= 1.0) {
            return None;
        }
        Some(r)
    }

    /// Re-tint the tray glyph when the Windows taskbar switches between light and dark themes
    /// (macOS template icons are tinted by the system automatically).
    pub fn sync_theme(&mut self) {
        let light = taskbar_uses_light_theme();
        if light == self.light_taskbar {
            return;
        }
        self.light_taskbar = light;
        if let Some(icon) = build_embedded_icon(light) {
            let _ = self.tray.set_icon(Some(icon));
        }
    }

    pub fn sync_snapshot(&mut self, snap: &DashboardSnapshot) {
        let next_name = snap.next_account_name.as_deref().unwrap_or("None");
        let sig = snap
            .accounts
            .iter()
            .map(|a| format!("{}:{}:{}", a.id, a.display_name, a.is_running))
            .collect::<Vec<String>>()
            .join("|")
            + "|"
            + next_name;

        if sig == self.last_signature {
            return;
        }
        self.last_signature = sig;
        let menu = build_menu(Some(snap));
        self.tray.set_menu(Some(Box::new(menu)));

        let active_label = if snap.active_names.is_empty() {
            "Idle".to_owned()
        } else {
            snap.active_names.join(", ")
        };
        let _ = self
            .tray
            .set_tooltip(Some(format!("AGY Swap • Active: {active_label}")));
    }

    pub fn drain_actions(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();

        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = ev
            {
                actions.push(TrayAction::TogglePanel(Some(TrayAnchorRect::from(rect))));
            }
        }

        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            let id = ev.id.0.as_str();
            if id == MENU_TOGGLE_PANEL {
                actions.push(TrayAction::TogglePanel(self.anchor_rect()));
            } else if id == MENU_SWITCH_NEXT {
                actions.push(TrayAction::SwitchNext);
            } else if id == MENU_OPEN_CONSOLE {
                actions.push(TrayAction::OpenUsageConsole);
            } else if id == MENU_OPEN_MINI_HUD {
                actions.push(TrayAction::OpenMiniHud);
            } else if id == MENU_OPEN_FOLDER {
                actions.push(TrayAction::OpenProfilesFolder);
            } else if id == MENU_QUIT {
                actions.push(TrayAction::Quit);
            } else if let Some(acc_id) = id.strip_prefix(MENU_ACCOUNT_PREFIX) {
                actions.push(TrayAction::SwitchAccount(acc_id.to_owned()));
            }
        }

        actions
    }
}

fn build_menu(snap: Option<&DashboardSnapshot>) -> Menu {
    let menu = Menu::new();
    let next_label = snap
        .and_then(|s| s.next_account_name.as_deref())
        .map_or_else(
            || "Swap to Next Ready".to_owned(),
            |name| format!("Swap Next  ->  {name}"),
        );

    let item_next = MenuItem::with_id(MenuId::new(MENU_SWITCH_NEXT), next_label, true, None);
    let _ = menu.append(&item_next);
    let _ = menu.append(&PredefinedMenuItem::separator());

    if let Some(dashboard) = snap {
        for acc in &dashboard.accounts {
            let prefix = if acc.is_running {
                "● "
            } else if acc.is_on_cooldown {
                "◐ "
            } else {
                "○ "
            };
            let suffix = if acc.is_running {
                " (Running)"
            } else if acc.is_on_cooldown {
                " (Cooldown)"
            } else {
                ""
            };
            let label = format!("{prefix}{}{suffix}", acc.display_name);
            let item = MenuItem::with_id(
                MenuId::new(format!("{MENU_ACCOUNT_PREFIX}{}", acc.id)),
                label,
                true,
                None,
            );
            let _ = menu.append(&item);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    let item_toggle = MenuItem::with_id(
        MenuId::new(MENU_TOGGLE_PANEL),
        "Toggle AGY Swap",
        true,
        None,
    );
    let item_console = MenuItem::with_id(
        MenuId::new(MENU_OPEN_CONSOLE),
        "Open Usage Console ↗",
        true,
        None,
    );
    let item_mini_hud = MenuItem::with_id(
        MenuId::new(MENU_OPEN_MINI_HUD),
        "Open Mini HUD Strip ↗",
        true,
        None,
    );
    let item_folder = MenuItem::with_id(
        MenuId::new(MENU_OPEN_FOLDER),
        "Open Profiles Folder",
        true,
        None,
    );
    let item_quit = MenuItem::with_id(MenuId::new(MENU_QUIT), "Quit AGY Swap", true, None);

    let _ = menu.append(&item_toggle);
    let _ = menu.append(&item_console);
    let _ = menu.append(&item_mini_hud);
    let _ = menu.append(&item_folder);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_quit);
    menu
}

/// Windows 10/11 taskbar theme (`SystemUsesLightTheme`), read through the safe registry API.
#[cfg(target_os = "windows")]
fn taskbar_uses_light_theme() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|key| key.get_u32("SystemUsesLightTheme"))
        .is_ok_and(|v| v == 1)
}

#[cfg(not(target_os = "windows"))]
fn taskbar_uses_light_theme() -> bool {
    false
}

/// Draw a crisp 22x22 RGBA orbital gauge mark in memory for macOS Menu Bar & Windows Tray.
/// White glyph for dark taskbars / macOS templates, near-black glyph for light Windows taskbars.
fn build_embedded_icon(dark_glyph: bool) -> Option<Icon> {
    let (r, g, b, a) = if dark_glyph {
        (26_u8, 26_u8, 26_u8, 240_u8)
    } else {
        (255_u8, 255_u8, 255_u8, 245_u8)
    };
    const SIZE: u32 = 22;
    let mut rgba = vec![0_u8; usize::try_from(SIZE * SIZE * 4).ok()?];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = i32::try_from(x).ok()? - 11;
            let dy = i32::try_from(y).ok()? - 11;
            let dist_sq = dx * dx + dy * dy;

            let on_outer_ring = (49..=81).contains(&dist_sq);
            let on_center_dot = dist_sq <= 9;
            let on_orbit_dot = (dx - 6) * (dx - 6) + (dy + 6) * (dy + 6) <= 5;

            if on_outer_ring || on_center_dot || on_orbit_dot {
                let idx = usize::try_from((y * SIZE + x) * 4).ok()?;
                if let Some(slice) = rgba.get_mut(idx..idx + 4) {
                    slice[0] = r;
                    slice[1] = g;
                    slice[2] = b;
                    slice[3] = a;
                }
            }
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).ok()
}

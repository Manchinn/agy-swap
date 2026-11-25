use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use agswitch_engine::domain;
use agswitch_engine::spec::{
    AccountProfile, ExternalTelemetrySnapshot, LaunchSpec, RunningInstance, TranscriptFeed,
    TranscriptStep,
};
use agswitch_engine::{
    BoxFut, ClockPort, ProcessPort, TelemetryFeedPort, TranscriptPort, WorkspaceLinkPort,
};
use agswitch_primitives::{AppError, RuntimeOs};
use chrono::{DateTime, Duration, Utc};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const MAX_TRANSCRIPT_BYTES: u64 = 8 * 1024 * 1024;
const GRACEFUL_CLOSE_POLLS: usize = 18;

/// Resolve platform-appropriate application data directory (overrideable via `AGSWITCH_DATA_DIR`).
pub fn default_data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("AGSWITCH_DATA_DIR")
        && !override_dir.trim().is_empty()
    {
        return PathBuf::from(override_dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("AgSwitch")
}

/// Resolve isolated account vaults directory (respects legacy `C:\AntigravityProfiles` on Windows).
pub fn default_profiles_root_dir(data_dir: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let legacy = PathBuf::from(r"C:\AntigravityProfiles");
        if legacy.exists() {
            return legacy;
        }
    }
    data_dir.join("Profiles")
}

/// Resolve default Antigravity executable path across macOS and Windows.
pub fn default_antigravity_bin_path() -> String {
    if let Ok(override_bin) = std::env::var("AGSWITCH_ANTIGRAVITY_BIN")
        && !override_bin.trim().is_empty()
    {
        return override_bin;
    }

    #[cfg(target_os = "macos")]
    {
        let system_app = PathBuf::from("/Applications/Antigravity.app/Contents/MacOS/Antigravity");
        if system_app.exists() {
            return system_app.to_string_lossy().into_owned();
        }
        if let Some(home) = dirs::home_dir() {
            let user_app = home.join("Applications/Antigravity.app/Contents/MacOS/Antigravity");
            if user_app.exists() {
                return user_app.to_string_lossy().into_owned();
            }
        }
        "/Applications/Antigravity.app/Contents/MacOS/Antigravity".to_owned()
    }

    #[cfg(target_os = "windows")]
    {
        let candidates = windows_antigravity_candidates();
        candidates
            .iter()
            .find(|p| p.is_file())
            .or_else(|| candidates.first())
            .map_or_else(
                || r"C:\Program Files\Antigravity\Antigravity.exe".to_owned(),
                |p| p.to_string_lossy().into_owned(),
            )
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "/usr/bin/antigravity".to_owned()
    }
}

/// Known per-user and machine-wide Antigravity install locations on Windows, in priority order.
#[cfg(target_os = "windows")]
fn windows_antigravity_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(local) = dirs::data_local_dir() {
        let programs = local.join("Programs");
        out.push(programs.join("Antigravity").join("Antigravity.exe"));
        out.push(programs.join("Antigravity IDE").join("Antigravity.exe"));
        out.push(local.join("Antigravity").join("Antigravity.exe"));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(pf) = std::env::var(var)
            && !pf.trim().is_empty()
        {
            out.push(
                PathBuf::from(pf)
                    .join("Antigravity")
                    .join("Antigravity.exe"),
            );
        }
    }
    out
}

/// Resolve primary OS user home directory.
pub fn primary_home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Wall-clock UTC adapter.
#[derive(Debug, Default, Clone)]
pub struct SystemClockAdapter;

impl ClockPort for SystemClockAdapter {
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Cross-platform OS process controller built with pure-Rust `sysinfo` (zero `unsafe`).
#[derive(Debug, Default)]
pub struct OsProcessAdapter;

impl OsProcessAdapter {
    pub fn new() -> Self {
        Self
    }

    fn is_antigravity_main_name(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        lower == "antigravity" || lower == "antigravity.exe"
    }

    fn is_sidecar_name(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        lower == "antigravity"
            || lower == "antigravity.exe"
            || lower.starts_with("language_server")
            || lower == "node"
            || lower == "node.exe"
            || lower == "chrome_crashpad_handler"
            || lower == "chrome_crashpad_handler.exe"
            || lower == "cmd.exe"
    }

    fn process_exists(pid: u32) -> bool {
        let Ok(mut sys) = System::new() else {
            return false;
        };
        let target = Pid::from_u32(pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[target]),
            true,
            ProcessRefreshKind::nothing(),
        );
        sys.process(target).is_some()
    }

    fn format_cmdline(cmd: &[std::ffi::OsString]) -> String {
        cmd.iter()
            .map(|part| {
                let s = part.to_string_lossy();
                if s.contains(' ') && !s.starts_with('"') && !s.contains("=\"") {
                    if let Some((flag, val)) = s.split_once('=')
                        && flag.starts_with("--")
                    {
                        format!("{flag}=\"{val}\"")
                    } else {
                        format!("\"{s}\"")
                    }
                } else {
                    s.into_owned()
                }
            })
            .collect::<Vec<String>>()
            .join(" ")
    }

    fn extract_user_data_dir_from_argv(cmd: &[std::ffi::OsString]) -> Option<String> {
        let mut iter = cmd.iter();
        while let Some(arg) = iter.next() {
            let s = arg.to_string_lossy();
            if let Some(val) = s.strip_prefix("--user-data-dir=") {
                let norm = domain::normalize_path(val);
                if !norm.is_empty() {
                    return Some(norm);
                }
            } else if s.eq_ignore_ascii_case("--user-data-dir")
                && let Some(next_arg) = iter.next()
            {
                let norm = domain::normalize_path(&next_arg.to_string_lossy());
                if !norm.is_empty() {
                    return Some(norm);
                }
            }
        }
        None
    }

    fn send_graceful_terminate(pid: u32) {
        #[cfg(target_os = "windows")]
        {
            let _ = hidden_command("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    fn send_force_kill(pid: u32) {
        #[cfg(target_os = "windows")]
        {
            let _ = hidden_command("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

impl ProcessPort for OsProcessAdapter {
    fn binary_exists(&self, path: &str) -> bool {
        !path.trim().is_empty() && Path::new(path).exists()
    }

    fn list_running_main_instances(&self) -> Result<Vec<RunningInstance>, AppError> {
        let mut sys = System::new()
            .map_err(|e| AppError::Integration(format!("sysinfo init failed: {e}")))?;
        let refresh = ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always);
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

        let mut results = Vec::new();
        for (pid, process) in sys.processes() {
            let name = process.name().to_string_lossy();
            if !Self::is_antigravity_main_name(&name) {
                continue;
            }
            let cmd = Self::format_cmdline(process.cmd());
            if cmd.trim().is_empty() || domain::is_child_process(&cmd) {
                continue;
            }
            let user_data_dir = Self::extract_user_data_dir_from_argv(process.cmd())
                .or_else(|| domain::extract_user_data_dir(&cmd));
            results.push(RunningInstance {
                pid: pid.as_u32(),
                user_data_dir,
                command_line: cmd,
            });
        }
        results.sort_by_key(|r| r.pid);
        Ok(results)
    }

    fn close_instance(&self, pid: u32) -> BoxFut<'_, Result<(), AppError>> {
        Box::pin(async move {
            Self::send_graceful_terminate(pid);
            for _ in 0..GRACEFUL_CLOSE_POLLS {
                if !Self::process_exists(pid) {
                    return Ok(());
                }
                tokio::time::sleep(StdDuration::from_millis(100)).await;
            }
            if Self::process_exists(pid) {
                Self::send_force_kill(pid);
            }
            Ok(())
        })
    }

    fn close_orphan_sidecars<'a>(
        &'a self,
        profile: &'a AccountProfile,
    ) -> BoxFut<'a, Result<(), AppError>> {
        Box::pin(async move {
            if profile.is_default_profile || profile.root_path.trim().is_empty() {
                return Ok(());
            }
            let norm_slash = domain::normalize_path(&profile.root_path);
            let norm_backslash = norm_slash.replace('/', "\\");
            let escaped = norm_slash.replace(' ', "%20");
            let self_pid = std::process::id();

            let mut sys = System::new()
                .map_err(|e| AppError::Integration(format!("sysinfo init failed: {e}")))?;
            let refresh = ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always);
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

            let mut matching_pids = Vec::new();
            for (pid, proc_info) in sys.processes() {
                let raw_pid = pid.as_u32();
                if raw_pid == self_pid {
                    continue;
                }
                let name = proc_info.name().to_string_lossy();
                if !Self::is_sidecar_name(&name) {
                    continue;
                }
                let cmd = Self::format_cmdline(proc_info.cmd());
                let cmd_lower = cmd.to_ascii_lowercase();
                if cmd_lower.contains(&norm_slash.to_ascii_lowercase())
                    || cmd_lower.contains(&norm_backslash.to_ascii_lowercase())
                    || cmd_lower.contains(&escaped.to_ascii_lowercase())
                {
                    matching_pids.push(raw_pid);
                }
            }

            for pid in matching_pids {
                Self::send_force_kill(pid);
            }
            Ok(())
        })
    }

    fn launch_spec(&self, spec: &LaunchSpec) -> Result<(), AppError> {
        let exe = Path::new(&spec.exe_path);
        if !exe.exists() {
            return Err(AppError::Integration(format!(
                "Antigravity executable not found at '{}'",
                spec.exe_path
            )));
        }

        let mut cmd = Command::new(exe);
        cmd.args(&spec.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(parent) = exe.parent() {
            cmd.current_dir(parent);
        }
        for key in &spec.env_removals {
            cmd.env_remove(key);
        }
        for (k, v) in &spec.env_overrides {
            cmd.env(k, v);
        }

        #[cfg(target_os = "windows")]
        {
            // Fully detach Antigravity from AGY Swap / the CLI console: no inherited console,
            // and its own process group so Ctrl+C in a terminal never propagates to it.
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }

        cmd.spawn()
            .map_err(|e| AppError::Integration(format!("failed to launch Antigravity: {e}")))?;
        Ok(())
    }

    fn focus_instance(&self, pid: u32) -> Result<bool, AppError> {
        #[cfg(target_os = "macos")]
        {
            let script = format!(
                "tell application \"System Events\" to set frontmost of first process whose unix id is {pid} to true"
            );
            let status = Command::new("osascript")
                .args(["-e", &script])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|e| AppError::Integration(format!("osascript focus failed: {e}")))?;
            Ok(status.success())
        }

        #[cfg(target_os = "windows")]
        {
            // `AppActivate` returns a bool; surface it via the exit code (PowerShell exits 0 otherwise).
            let script = format!(
                "if ((New-Object -ComObject WScript.Shell).AppActivate({pid})) {{ exit 0 }} else {{ exit 1 }}"
            );
            let status = hidden_command("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|e| AppError::Integration(format!("powershell focus failed: {e}")))?;
            Ok(status.success())
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = pid;
            Ok(false)
        }
    }
}

/// Cross-platform workspace symlink/junction and safe vault lifecycle adapter.
#[derive(Debug, Clone)]
pub struct OsWorkspaceLinkAdapter {
    primary_home: PathBuf,
    runtime_os: RuntimeOs,
}

impl OsWorkspaceLinkAdapter {
    pub fn new(primary_home: PathBuf) -> Self {
        Self {
            primary_home,
            runtime_os: RuntimeOs::current(),
        }
    }

    fn is_link_or_reparse_point(path: &Path) -> bool {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return true;
        }
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
        }
        #[cfg(not(target_os = "windows"))]
        {
            false
        }
    }

    fn create_dir_link(link_path: &Path, target_path: &Path) -> Result<(), AppError> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target_path, link_path).map_err(|e| {
                AppError::Integration(format!(
                    "failed to create symlink '{}' -> '{}': {e}",
                    link_path.display(),
                    target_path.display()
                ))
            })
        }

        #[cfg(target_os = "windows")]
        {
            let args = format!(
                "/c mklink /J \"{}\" \"{}\"",
                link_path.display(),
                target_path.display()
            );
            let output = hidden_command("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(link_path)
                .arg(target_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .map_err(|e| {
                    AppError::Integration(format!("failed to invoke mklink ({args}): {e}"))
                })?;
            if !output.status.success() || !Self::is_link_or_reparse_point(link_path) {
                let err = String::from_utf8_lossy(&output.stderr);
                return Err(AppError::Integration(format!(
                    "mklink /J failed for '{}': {err}",
                    link_path.display()
                )));
            }
            Ok(())
        }
    }

    fn remove_link_only(link_path: &Path) -> Result<(), AppError> {
        #[cfg(unix)]
        {
            fs::remove_file(link_path).map_err(|e| {
                AppError::Integration(format!("failed to unlink '{}': {e}", link_path.display()))
            })
        }

        #[cfg(target_os = "windows")]
        {
            // `fs::remove_dir` (`RemoveDirectoryW`) on an NTFS junction / directory symlink deletes
            // ONLY the reparse point and never recurses into the target. File symlinks need
            // `remove_file` instead.
            fs::remove_dir(link_path)
                .or_else(|_| fs::remove_file(link_path))
                .map_err(|e| {
                    AppError::Integration(format!(
                        "failed to remove junction '{}': {e}",
                        link_path.display()
                    ))
                })
        }
    }

    fn unlink_all_links_under(root: &Path) -> Result<(), AppError> {
        if !root.exists() && !Self::is_link_or_reparse_point(root) {
            return Ok(());
        }
        if Self::is_link_or_reparse_point(root) {
            return Self::remove_link_only(root);
        }
        let Ok(entries) = fs::read_dir(root) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if Self::is_link_or_reparse_point(&path) {
                Self::remove_link_only(&path)?;
            } else if path.is_dir() {
                Self::unlink_all_links_under(&path)?;
            }
        }
        Ok(())
    }

    fn rescue_standalone_conversations(src_ag: &Path, dst_ag: &Path) {
        let src_conv = src_ag.join("conversations");
        let dst_conv = dst_ag.join("conversations");
        if src_conv.is_dir()
            && fs::create_dir_all(&dst_conv).is_ok()
            && let Ok(entries) = fs::read_dir(&src_conv)
        {
            for entry in entries.flatten() {
                let src_file = entry.path();
                if src_file.is_file() {
                    let dst_file = dst_conv.join(entry.file_name());
                    if !dst_file.exists() {
                        let _ = fs::copy(&src_file, &dst_file);
                    }
                }
            }
        }

        let src_brain = src_ag.join("brain");
        let dst_brain = dst_ag.join("brain");
        if src_brain.is_dir()
            && fs::create_dir_all(&dst_brain).is_ok()
            && let Ok(entries) = fs::read_dir(&src_brain)
        {
            for entry in entries.flatten() {
                let src_dir = entry.path();
                if src_dir.is_dir() {
                    let dst_dir = dst_brain.join(entry.file_name());
                    if !dst_dir.exists() {
                        copy_dir_recursive_no_overwrite(&src_dir, &dst_dir);
                    }
                }
            }
        }
    }
}

impl WorkspaceLinkPort for OsWorkspaceLinkAdapter {
    fn discover_vault_folders(&self, profiles_root: &str) -> Result<Vec<String>, AppError> {
        let root = Path::new(profiles_root);
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut dirs_found = Vec::new();
        let entries = fs::read_dir(root)
            .map_err(|e| AppError::Integration(format!("read profiles root failed: {e}")))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs_found.push(path.to_string_lossy().into_owned());
            }
        }
        dirs_found.sort();
        Ok(dirs_found)
    }

    fn ensure_dirs(&self, dirs: &[String]) -> Result<(), AppError> {
        for dir in dirs {
            fs::create_dir_all(dir)
                .map_err(|e| AppError::Integration(format!("create dir '{dir}' failed: {e}")))?;
        }
        Ok(())
    }

    fn is_authenticated(&self, profile: &AccountProfile) -> bool {
        // Rule R2: existence check ONLY — never open or read OAuth token bytes.
        let home_str = self.primary_home.to_string_lossy();
        let marker = domain::oauth_marker_path(profile, &home_str, self.runtime_os);
        if Path::new(&marker).exists() {
            return true;
        }
        if profile.is_default_profile {
            let install_marker = domain::primary_install_marker_path(&home_str, self.runtime_os);
            return Path::new(&install_marker).exists();
        }
        false
    }

    fn is_shared_mode_enabled(&self, profile: &AccountProfile) -> bool {
        if profile.is_default_profile {
            return true;
        }
        if profile.root_path.trim().is_empty() {
            return false;
        }
        let ag_dir = Path::new(&profile.root_path)
            .join(".gemini")
            .join("antigravity");
        Self::is_link_or_reparse_point(&ag_dir)
    }

    fn enable_shared_mode(&self, profile: &AccountProfile) -> Result<(), AppError> {
        if profile.is_default_profile || profile.root_path.trim().is_empty() {
            return Ok(());
        }
        let profile_gemini = Path::new(&profile.root_path).join(".gemini");
        let primary_gemini = self.primary_home.join(".gemini");
        fs::create_dir_all(&profile_gemini)
            .map_err(|e| AppError::Integration(format!("create profile .gemini failed: {e}")))?;
        fs::create_dir_all(&primary_gemini)
            .map_err(|e| AppError::Integration(format!("create primary .gemini failed: {e}")))?;

        let stamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
        for folder in domain::SHARED_GEMINI_FOLDERS {
            let target_dir = primary_gemini.join(folder);
            let link_dir = profile_gemini.join(folder);
            fs::create_dir_all(&target_dir).map_err(|e| {
                AppError::Integration(format!("create target '{folder}' failed: {e}"))
            })?;

            if Self::is_link_or_reparse_point(&link_dir) {
                Self::remove_link_only(&link_dir)?;
            } else if link_dir.exists() {
                if folder == "antigravity" {
                    Self::rescue_standalone_conversations(&link_dir, &target_dir);
                }
                let backup = profile_gemini.join(format!("_isolated_backup_{folder}_{stamp}"));
                fs::rename(&link_dir, &backup).map_err(|e| {
                    AppError::Integration(format!("backup standalone '{folder}' failed: {e}"))
                })?;
            }

            Self::create_dir_link(&link_dir, &target_dir)?;
        }
        Ok(())
    }

    fn disable_shared_mode(&self, profile: &AccountProfile) -> Result<(), AppError> {
        if profile.is_default_profile || profile.root_path.trim().is_empty() {
            return Ok(());
        }
        let profile_gemini = Path::new(&profile.root_path).join(".gemini");
        if !profile_gemini.exists() {
            return Ok(());
        }
        for folder in domain::SHARED_GEMINI_FOLDERS {
            let link_dir = profile_gemini.join(folder);
            if Self::is_link_or_reparse_point(&link_dir) {
                Self::remove_link_only(&link_dir)?;
                fs::create_dir_all(&link_dir).map_err(|e| {
                    AppError::Integration(format!("recreate standalone '{folder}' failed: {e}"))
                })?;
            }
        }
        Ok(())
    }

    fn reset_profile_login(&self, profile: &AccountProfile) -> Result<(), AppError> {
        domain::policy::check_can_reset_login(profile)?;
        let home_str = self.primary_home.to_string_lossy();
        let marker = domain::oauth_marker_path(profile, &home_str, self.runtime_os);
        let marker_path = Path::new(&marker);
        if marker_path.exists() {
            fs::remove_file(marker_path)
                .map_err(|e| AppError::Integration(format!("remove OAuth marker failed: {e}")))?;
        }
        if let Some(ud) = domain::user_data_dir(profile, self.runtime_os) {
            let ud_path = Path::new(&ud);
            if ud_path.exists() {
                Self::unlink_all_links_under(ud_path)?;
                fs::remove_dir_all(ud_path).map_err(|e| {
                    AppError::Integration(format!("clear UserData directory failed: {e}"))
                })?;
            }
            fs::create_dir_all(ud_path).map_err(|e| {
                AppError::Integration(format!("recreate UserData directory failed: {e}"))
            })?;
        }
        Ok(())
    }

    fn delete_vault_directory_safely(&self, root_path: &str) -> Result<(), AppError> {
        let path = Path::new(root_path);
        if !path.exists() {
            return Ok(());
        }
        // Rule R1: ALWAYS unlink all symlinks/junctions before recursive deletion
        Self::unlink_all_links_under(path)?;
        fs::remove_dir_all(path)
            .map_err(|e| AppError::Integration(format!("delete vault dir failed: {e}")))?;
        Ok(())
    }
}

fn copy_dir_recursive_no_overwrite(src: &Path, dst: &Path) {
    if fs::create_dir_all(dst).is_err() {
        return;
    }
    let Ok(entries) = fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        if sp.is_dir() {
            copy_dir_recursive_no_overwrite(&sp, &dp);
        } else if !dp.exists() {
            let _ = fs::copy(&sp, &dp);
        }
    }
}

/// Cached, byte-capped local `transcript.jsonl` reader.
#[derive(Debug)]
pub struct OsTranscriptAdapter {
    primary_home: PathBuf,
    cache: Mutex<HashMap<PathBuf, CachedTranscript>>,
}

#[derive(Debug, Clone)]
struct CachedTranscript {
    mtime: SystemTime,
    len: u64,
    steps: Vec<TranscriptStep>,
}

impl OsTranscriptAdapter {
    pub fn new(primary_home: PathBuf) -> Self {
        Self {
            primary_home,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn read_cached(&self, path: &Path, mtime: SystemTime, len: u64) -> Vec<TranscriptStep> {
        if let Ok(cache) = self.cache.lock()
            && let Some(hit) = cache.get(path)
            && hit.mtime == mtime
            && hit.len == len
        {
            return hit.steps.clone();
        }

        let fallback_ts: DateTime<Utc> = mtime.into();
        let mut steps = Vec::new();
        if let Ok(file) = fs::File::open(path) {
            let reader = BufReader::new(file.take(MAX_TRANSCRIPT_BYTES));
            for line in reader.lines().map_while(Result::ok) {
                if let Some(step) = domain::parse_transcript_line(&line, fallback_ts) {
                    steps.push(step);
                }
            }
        }

        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                path.to_path_buf(),
                CachedTranscript {
                    mtime,
                    len,
                    steps: steps.clone(),
                },
            );
        }
        steps
    }
}

impl TranscriptPort for OsTranscriptAdapter {
    fn load_recent_feeds(
        &self,
        profiles: &[AccountProfile],
        now: DateTime<Utc>,
    ) -> Result<Vec<TranscriptFeed>, AppError> {
        let cutoff = now - Duration::days(8);
        let mut brain_dirs: Vec<(PathBuf, Option<String>)> = Vec::new();

        let primary_brain = self
            .primary_home
            .join(".gemini")
            .join("antigravity")
            .join("brain");
        if primary_brain.is_dir() {
            brain_dirs.push((primary_brain, None));
        }

        for profile in profiles
            .iter()
            .filter(|p| !p.is_default_profile && !p.root_path.trim().is_empty())
        {
            let ag_dir = Path::new(&profile.root_path)
                .join(".gemini")
                .join("antigravity");
            if ag_dir.is_dir() && !OsWorkspaceLinkAdapter::is_link_or_reparse_point(&ag_dir) {
                let iso_brain = ag_dir.join("brain");
                if iso_brain.is_dir() {
                    brain_dirs.push((iso_brain, Some(profile.id.as_str().to_owned())));
                }
            }
        }

        let mut feeds = Vec::new();
        for (brain_dir, fixed_owner) in brain_dirs {
            let Ok(conv_dirs) = fs::read_dir(&brain_dir) else {
                continue;
            };
            for conv in conv_dirs.flatten() {
                let tf = conv
                    .path()
                    .join(".system_generated")
                    .join("logs")
                    .join("transcript.jsonl");
                let Ok(meta) = fs::metadata(&tf) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let Ok(mtime) = meta.modified() else {
                    continue;
                };
                let last_write_utc: DateTime<Utc> = mtime.into();
                if last_write_utc < cutoff {
                    continue;
                }
                let steps = self.read_cached(&tf, mtime, meta.len());
                feeds.push(TranscriptFeed {
                    fixed_owner_profile_id: fixed_owner.clone(),
                    last_write_utc,
                    steps,
                });
            }
        }
        Ok(feeds)
    }
}

#[cfg(target_os = "windows")]
fn hidden_command(program: &str) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null());
    cmd
}

/// Convenience helper for app shells to open the local profiles directory in Finder / Explorer.
pub fn open_directory_in_file_manager(dir: &Path) -> Result<(), AppError> {
    fs::create_dir_all(dir)
        .map_err(|e| AppError::Integration(format!("create folder failed: {e}")))?;
    #[cfg(target_os = "macos")]
    let res = Command::new("open").arg(dir).spawn();
    // Explorer silently falls back to "Documents" when handed forward slashes.
    #[cfg(target_os = "windows")]
    let res = Command::new("explorer.exe")
        .arg(dir.to_string_lossy().replace('/', "\\"))
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let res = Command::new("xdg-open").arg(dir).spawn();

    res.map_err(|e| AppError::Integration(format!("open file manager failed: {e}")))?;
    Ok(())
}

/// Default local telemetry server base URL (overrideable via `AGSWITCH_TELEMETRY_URL`).
pub fn default_telemetry_base_url() -> String {
    if let Ok(raw) = std::env::var("AGSWITCH_TELEMETRY_URL") {
        let trimmed = raw.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    "http://127.0.0.1:8289".to_owned()
}

/// Default local JSON fallback path for `ai-usage-tracker` (overrideable via `AGSWITCH_TELEMETRY_FILE`).
pub fn default_telemetry_file_path() -> PathBuf {
    if let Ok(raw) = std::env::var("AGSWITCH_TELEMETRY_FILE")
        && !raw.trim().is_empty()
    {
        return PathBuf::from(raw.trim());
    }
    primary_home_dir()
        .join("opendesign")
        .join("mockups")
        .join("ai-usage-tracker")
        .join("usage-data.json")
}

/// Open a strictly validated loopback HTTP URL (`http://127.0.0.1:...` or `http://localhost:...`)
/// in the default desktop browser (e.g. Full Console or Mini HUD Strip).
pub fn open_local_url(url: &str) -> Result<(), AppError> {
    let trimmed = url.trim();
    let is_loopback = trimmed.starts_with("http://127.0.0.1:")
        || trimmed.starts_with("http://localhost:");
    if !is_loopback
        || trimmed.contains('"')
        || trimmed.contains('&')
        || trimmed.contains('|')
        || trimmed.contains(';')
        || trimmed.contains('\n')
        || trimmed.contains('\r')
    {
        return Err(AppError::Validation(
            "only clean http://127.0.0.1 or http://localhost URLs may be opened".to_owned(),
        ));
    }

    #[cfg(target_os = "macos")]
    let res = Command::new("open").arg(trimmed).spawn();
    #[cfg(target_os = "windows")]
    let res = hidden_command("cmd")
        .args(["/C", "start", "", trimmed])
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let res = Command::new("xdg-open").arg(trimmed).spawn();

    res.map_err(|e| AppError::Integration(format!("open local URL failed: {e}")))?;
    Ok(())
}

struct CachedTelemetry {
    fetched_at: Instant,
    snapshot: Option<ExternalTelemetrySnapshot>,
}

/// Read-only loopback bridge adapter for an optional local multi-provider telemetry server
/// (`http://127.0.0.1:8289/api/usage.json`) with automatic JSON file fallback.
/// Preserves Rule R2: never touches OAuth tokens, `.env`, or SQLite databases directly.
pub struct LocalTelemetryBridgeAdapter {
    base_url: String,
    fallback_file: PathBuf,
    cache_ttl: StdDuration,
    cache: Mutex<Option<CachedTelemetry>>,
}

impl LocalTelemetryBridgeAdapter {
    pub fn from_defaults() -> Self {
        Self::new(
            default_telemetry_base_url(),
            default_telemetry_file_path(),
            StdDuration::from_secs(15),
        )
    }

    pub fn new(base_url: String, fallback_file: PathBuf, cache_ttl: StdDuration) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            fallback_file,
            cache_ttl,
            cache: Mutex::new(None),
        }
    }

    fn fetch_live(&self, force_fresh: bool) -> Option<ExternalTelemetrySnapshot> {
        let query_path = if force_fresh {
            "/api/usage.json?fresh=1"
        } else {
            "/api/usage.json"
        };
        let read_timeout = if force_fresh {
            StdDuration::from_millis(8_000)
        } else {
            StdDuration::from_millis(2_500)
        };

        if let Some((port, label)) = parse_loopback_port(&self.base_url)
            && let Some(body) = http_get_loopback(port, query_path, read_timeout)
            && let Some(snap) = domain::parse_external_telemetry_json(&body, &label)
        {
            return Some(snap);
        }

        // Offline fallback: read last generated usage-data.json (bounded to 4 MB)
        if let Ok(meta) = fs::metadata(&self.fallback_file)
            && meta.len() <= 4 * 1024 * 1024
            && let Ok(raw) = fs::read_to_string(&self.fallback_file)
            && let Some(snap) = domain::parse_external_telemetry_json(&raw, "usage-data.json")
        {
            return Some(snap);
        }

        None
    }
}

impl TelemetryFeedPort for LocalTelemetryBridgeAdapter {
    fn load_external_telemetry(&self, force_fresh: bool) -> Option<ExternalTelemetrySnapshot> {
        if !force_fresh
            && let Ok(guard) = self.cache.lock()
            && let Some(entry) = guard.as_ref()
            && entry.fetched_at.elapsed() < self.cache_ttl
        {
            return entry.snapshot.clone();
        }

        let snap = self.fetch_live(force_fresh);
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some(CachedTelemetry {
                fetched_at: Instant::now(),
                snapshot: snap.clone(),
            });
        }
        snap
    }
}

fn parse_loopback_port(base_url: &str) -> Option<(u16, String)> {
    let rest = base_url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| base_url.strip_prefix("http://localhost:"))?;
    let port_str = rest.split('/').next()?;
    let port: u16 = port_str.parse().ok()?;
    Some((port, format!("localhost:{port}")))
}

fn http_get_loopback(port: u16, path: &str, read_timeout: StdDuration) -> Option<String> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut stream = TcpStream::connect_timeout(&addr, StdDuration::from_millis(350)).ok()?;
    let _ = stream.set_read_timeout(Some(read_timeout));
    let _ = stream.set_write_timeout(Some(StdDuration::from_millis(350)));

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;

    let mut raw = Vec::new();
    let mut limited = stream.take(4 * 1024 * 1024);
    limited.read_to_end(&mut raw).ok()?;

    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header_bytes = raw.get(..sep)?;
    let body_bytes = raw.get(sep + 4..)?;
    let headers = std::str::from_utf8(header_bytes).ok()?;
    let status_line = headers.lines().next()?;
    if !status_line.contains(" 200 ") {
        return None;
    }

    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let decoded = decode_chunked_body(body_bytes)?;
        String::from_utf8(decoded).ok()
    } else {
        String::from_utf8(body_bytes.to_vec()).ok()
    }
}

fn decode_chunked_body(mut input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let crlf = input.windows(2).position(|w| w == b"\r\n")?;
        let size_line = std::str::from_utf8(input.get(..crlf)?).ok()?;
        let hex_part = size_line.split(';').next()?.trim();
        let size = usize::from_str_radix(hex_part, 16).ok()?;
        input = input.get(crlf + 2..)?;
        if size == 0 {
            break;
        }
        let chunk = input.get(..size)?;
        out.extend_from_slice(chunk);
        input = input.get(size + 2..)?;
    }
    Some(out)
}

/// Safe in-memory / temp-folder sandbox adapters so users and automated tests can exercise
/// Switch, Alongside, Stop, Add, Cooldown, Shared Chats, and Rules R1–R5 without ever touching
/// the real Antigravity application or `~/.gemini`.
pub mod sandbox {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use agswitch_engine::domain;
    use agswitch_engine::spec::{
        AccountProfile, ExternalAntigravityQuota, ExternalProviderSummary,
        ExternalTelemetrySnapshot, FirecrawlDetail, LaunchSpec, MaxPlusDetail, MaxPlusKeyRow,
        MaxPlusPeriodStat, OpenCodeGoDetail, RunningInstance, ServerQuotaBucket, SwitcherConfig,
        TokenMeDetail, TranscriptFeed, TranscriptStep, UsageLedger,
    };
    use agswitch_engine::{BoxFut, ProcessPort, TelemetryFeedPort, TranscriptPort};
    use agswitch_primitives::{AccountId, AccountTier, AppError, RuntimeOs};
    use chrono::{DateTime, Duration, Utc};

    /// Default isolated sandbox directory under the OS temp folder (overrideable via `AGSWITCH_SANDBOX_DIR`).
    pub fn default_sandbox_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("AGSWITCH_SANDBOX_DIR")
            && !dir.trim().is_empty()
        {
            return PathBuf::from(dir);
        }
        std::env::temp_dir().join("agyswap-sandbox")
    }

    /// Resolved sandbox paths (completely isolated from `~/.gemini` and `~/Library/Application Support/AgSwitch`).
    #[derive(Debug, Clone)]
    pub struct SandboxPaths {
        pub data_dir: PathBuf,
        pub profiles_root: PathBuf,
        pub fake_home: PathBuf,
        pub running_state_file: PathBuf,
    }

    /// Seed an isolated sandbox environment with 3 demo accounts (`SC`, `Trade`, `Work Claude`)
    /// if it has not been initialized yet. Never touches real user data or OAuth tokens.
    pub fn ensure_seeded(base_dir: &Path) -> Result<SandboxPaths, AppError> {
        let data_dir = base_dir.join("data");
        let profiles_root = data_dir.join("Profiles");
        let fake_home = base_dir.join("fake_home");
        let running_state_file = base_dir.join("sandbox-running.json");

        fs::create_dir_all(&data_dir)
            .map_err(|e| AppError::Integration(format!("sandbox mkdir failed: {e}")))?;
        fs::create_dir_all(&profiles_root)
            .map_err(|e| AppError::Integration(format!("sandbox profiles mkdir failed: {e}")))?;
        fs::create_dir_all(fake_home.join(".gemini").join("antigravity").join("brain"))
            .map_err(|e| AppError::Integration(format!("sandbox fake_home mkdir failed: {e}")))?;

        let cfg_file = data_dir.join("switcher-config.json");
        if !cfg_file.exists() {
            let trade_root = profiles_root.join("Trade");
            let work_root = profiles_root.join("WorkClaude");

            let mut primary = AccountProfile::create_default();
            primary.display_name = "SC".to_owned();
            primary.subtitle = "sc.demo@gmail.com".to_owned();

            let trade = AccountProfile::create_isolated(
                AccountId::new("trade")?,
                "Trade".to_owned(),
                "trade.demo@gmail.com".to_owned(),
                "#6FD8EE".to_owned(),
                trade_root.to_string_lossy().into_owned(),
            );
            let work = AccountProfile::create_isolated(
                AccountId::new("work-claude")?,
                "Work Claude".to_owned(),
                "work.claude@gmail.com".to_owned(),
                "#FBBF24".to_owned(),
                work_root.to_string_lossy().into_owned(),
            );

            // Create empty dummy auth markers via domain::oauth_marker_path (Rule R2: zero secret material)
            let fake_home_str = fake_home.to_string_lossy();
            let runtime_os = RuntimeOs::current();
            for p in [&primary, &trade, &work] {
                let marker =
                    PathBuf::from(domain::oauth_marker_path(p, &fake_home_str, runtime_os));
                if let Some(parent) = marker.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&marker, "");
            }

            let cfg = SwitcherConfig {
                profiles_root_dir: profiles_root.to_string_lossy().into_owned(),
                antigravity_bin_path: "/sandbox/Antigravity".to_owned(),
                last_active_profile_id: Some("default".to_owned()),
                hide_default_profile: false,
                ignored_profile_paths: Vec::new(),
                profiles: vec![primary, trade, work],
            };
            let cfg_json = serde_json::to_string_pretty(&cfg)
                .map_err(|e| AppError::Store(format!("serialize sandbox config: {e}")))?;
            fs::write(&cfg_file, cfg_json)
                .map_err(|e| AppError::Store(format!("write sandbox config: {e}")))?;

            let mut tiers = BTreeMap::new();
            tiers.insert("default".to_owned(), AccountTier::Ultra);
            tiers.insert("trade".to_owned(), AccountTier::Pro);
            tiers.insert("work-claude".to_owned(), AccountTier::Pro);
            let ledger = UsageLedger {
                intervals: Vec::new(),
                cooldown_until_utc: BTreeMap::new(),
                tier_overrides: tiers,
            };
            let ledger_json = serde_json::to_string_pretty(&ledger)
                .map_err(|e| AppError::Store(format!("serialize sandbox ledger: {e}")))?;
            fs::write(data_dir.join("usage-ledger.json"), ledger_json)
                .map_err(|e| AppError::Store(format!("write sandbox ledger: {e}")))?;
        }

        if !running_state_file.exists() {
            let initial = vec![RunningInstance {
                pid: 90_001,
                user_data_dir: None,
                command_line: "/sandbox/Antigravity".to_owned(),
            }];
            let json = serde_json::to_string_pretty(&initial)
                .map_err(|e| AppError::Store(format!("serialize sandbox running: {e}")))?;
            fs::write(&running_state_file, json)
                .map_err(|e| AppError::Store(format!("write sandbox running: {e}")))?;
        }

        Ok(SandboxPaths {
            data_dir,
            profiles_root,
            fake_home,
            running_state_file,
        })
    }

    /// Simulated process adapter backed by a JSON file inside the sandbox directory.
    /// Zero OS kills and zero real Antigravity launches.
    #[derive(Debug)]
    pub struct SandboxProcessAdapter {
        state_file: PathBuf,
        lock: Mutex<()>,
    }

    impl SandboxProcessAdapter {
        pub fn new(state_file: PathBuf) -> Self {
            Self {
                state_file,
                lock: Mutex::new(()),
            }
        }

        fn read_instances(&self) -> Vec<RunningInstance> {
            fs::read_to_string(&self.state_file)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        }

        fn write_instances(&self, instances: &[RunningInstance]) -> Result<(), AppError> {
            let json = serde_json::to_string_pretty(instances)
                .map_err(|e| AppError::Integration(format!("sandbox serialize: {e}")))?;
            fs::write(&self.state_file, json)
                .map_err(|e| AppError::Integration(format!("sandbox write: {e}")))
        }
    }

    impl ProcessPort for SandboxProcessAdapter {
        fn binary_exists(&self, _path: &str) -> bool {
            true
        }

        fn list_running_main_instances(&self) -> Result<Vec<RunningInstance>, AppError> {
            let _guard = self.lock.lock();
            Ok(self.read_instances())
        }

        fn close_instance(&self, pid: u32) -> BoxFut<'_, Result<(), AppError>> {
            Box::pin(async move {
                let _guard = self.lock.lock();
                let mut list = self.read_instances();
                list.retain(|i| i.pid != pid);
                self.write_instances(&list)
            })
        }

        fn close_orphan_sidecars<'a>(
            &'a self,
            _profile: &'a AccountProfile,
        ) -> BoxFut<'a, Result<(), AppError>> {
            Box::pin(async move { Ok(()) })
        }

        fn launch_spec(&self, spec: &LaunchSpec) -> Result<(), AppError> {
            let _guard = self.lock.lock();
            let mut list = self.read_instances();
            let next_pid = list
                .iter()
                .map(|i| i.pid)
                .max()
                .unwrap_or(90_000)
                .saturating_add(1);
            let user_data_dir = spec
                .arguments
                .iter()
                .find_map(|a| a.strip_prefix("--user-data-dir="))
                .map(ToOwned::to_owned);
            let command_line = if let Some(dir) = &user_data_dir {
                format!("{} --user-data-dir=\"{dir}\"", spec.exe_path)
            } else {
                spec.exe_path.clone()
            };
            list.push(RunningInstance {
                pid: next_pid,
                user_data_dir,
                command_line,
            });
            self.write_instances(&list)
        }

        fn focus_instance(&self, pid: u32) -> Result<bool, AppError> {
            let _guard = self.lock.lock();
            Ok(self.read_instances().iter().any(|i| i.pid == pid))
        }
    }

    /// Simulated multi-model transcript adapter showing realistic Gemini, Claude, and GPT usage
    /// across both 5-hour and Weekly (7-day) windows for each sandbox profile.
    #[derive(Debug, Default)]
    pub struct SandboxTranscriptAdapter;

    impl TranscriptPort for SandboxTranscriptAdapter {
        fn load_recent_feeds(
            &self,
            _profiles: &[AccountProfile],
            now: DateTime<Utc>,
        ) -> Result<Vec<TranscriptFeed>, AppError> {
            let mut sc_steps = Vec::new();
            // 5h window: 12 Gemini, 4 Claude, 2 GPT
            for i in 0..12_u32 {
                sc_steps.push(demo_step(
                    now - Duration::minutes(i64::from(i) * 12 + 5),
                    "gemini-4-neon-high",
                    42_000,
                    210_000,
                    2_200,
                ));
            }
            for i in 0..4_u32 {
                sc_steps.push(demo_step(
                    now - Duration::minutes(i64::from(i) * 35 + 15),
                    "claude-sonnet-4-5",
                    18_000,
                    95_000,
                    1_900,
                ));
            }
            for i in 0..2_u32 {
                sc_steps.push(demo_step(
                    now - Duration::minutes(i64::from(i) * 50 + 25),
                    "gpt-5.2-codex",
                    14_000,
                    60_000,
                    1_400,
                ));
            }
            // Weekly (days 2..5): +36 Gemini, +8 Claude, +3 GPT
            for i in 0..36_u32 {
                sc_steps.push(demo_step(
                    now - Duration::hours(i64::from(i) * 3 + 12),
                    "gemini-4-neon-high",
                    35_000,
                    180_000,
                    1_800,
                ));
            }
            for i in 0..8_u32 {
                sc_steps.push(demo_step(
                    now - Duration::hours(i64::from(i) * 11 + 18),
                    "claude-opus-4",
                    22_000,
                    110_000,
                    2_600,
                ));
            }
            for i in 0..3_u32 {
                sc_steps.push(demo_step(
                    now - Duration::hours(i64::from(i) * 20 + 30),
                    "gpt-5.2-codex",
                    16_000,
                    70_000,
                    1_500,
                ));
            }

            let mut trade_steps = Vec::new();
            for i in 0..9_u32 {
                trade_steps.push(demo_step(
                    now - Duration::minutes(i64::from(i) * 18 + 10),
                    "claude-sonnet-4-5",
                    20_000,
                    84_000,
                    1_700,
                ));
            }
            for i in 0..11_u32 {
                trade_steps.push(demo_step(
                    now - Duration::hours(i64::from(i) * 9 + 8),
                    "gpt-5.2-codex",
                    15_000,
                    62_000,
                    1_300,
                ));
            }
            for i in 0..8_u32 {
                trade_steps.push(demo_step(
                    now - Duration::hours(i64::from(i) * 14 + 22),
                    "gemini-3-pro",
                    25_000,
                    90_000,
                    1_600,
                ));
            }

            let mut work_steps = Vec::new();
            for i in 0..6_u32 {
                work_steps.push(demo_step(
                    now - Duration::minutes(i64::from(i) * 22 + 8),
                    "claude-opus-4",
                    28_000,
                    125_000,
                    2_400,
                ));
            }
            for i in 0..28_u32 {
                work_steps.push(demo_step(
                    now - Duration::hours(i64::from(i) * 4 + 10),
                    "claude-sonnet-4-5",
                    19_000,
                    88_000,
                    1_900,
                ));
            }
            for i in 0..6_u32 {
                work_steps.push(demo_step(
                    now - Duration::hours(i64::from(i) * 16 + 15),
                    "gpt-5.2",
                    12_000,
                    48_000,
                    1_100,
                ));
            }

            Ok(vec![
                TranscriptFeed {
                    fixed_owner_profile_id: Some("default".to_owned()),
                    last_write_utc: now - Duration::minutes(2),
                    steps: sc_steps,
                },
                TranscriptFeed {
                    fixed_owner_profile_id: Some("trade".to_owned()),
                    last_write_utc: now - Duration::minutes(10),
                    steps: trade_steps,
                },
                TranscriptFeed {
                    fixed_owner_profile_id: Some("work-claude".to_owned()),
                    last_write_utc: now - Duration::minutes(8),
                    steps: work_steps,
                },
            ])
        }
    }

    /// Deterministic sandbox telemetry feed showcasing live multi-provider cards & exact server quotas
    /// without contacting localhost or reading real host files.
    #[derive(Debug, Default)]
    pub struct SandboxTelemetryAdapter;

    impl TelemetryFeedPort for SandboxTelemetryAdapter {
        fn load_external_telemetry(&self, _force_fresh: bool) -> Option<ExternalTelemetrySnapshot> {
            let now = Utc::now();
            let g_reset = (now + Duration::minutes(164)).to_rfc3339();
            let g_wk_reset = (now + Duration::hours(138)).to_rfc3339();
            let cg_reset = (now + Duration::minutes(118)).to_rfc3339();
            let cg_wk_reset = (now + Duration::hours(164)).to_rfc3339();
            let oc_5h_reset = (now + Duration::minutes(195)).to_rfc3339();
            let oc_wk_reset = (now + Duration::hours(112)).to_rfc3339();
            let oc_mo_reset = (now + Duration::days(19)).to_rfc3339();
            let fc_reset = (now + Duration::days(21)).to_rfc3339();
            Some(ExternalTelemetrySnapshot {
                connected: true,
                source_label: "sandbox-bridge".to_owned(),
                updated_at: now.format("%Y-%m-%d %H:%M UTC").to_string(),
                burn_per_day_usd: Some(1.85),
                runway_days: Some(16.2),
                antigravity_quotas: vec![
                    ExternalAntigravityQuota {
                        name: "SC".to_owned(),
                        email: "sc.demo@gmail.com".to_owned(),
                        ok: true,
                        gemini_5h: Some(ServerQuotaBucket {
                            remaining_percent: 82.0,
                            used_percent: 18.0,
                            reset_at: g_reset.clone(),
                        }),
                        gemini_weekly: Some(ServerQuotaBucket {
                            remaining_percent: 73.0,
                            used_percent: 27.0,
                            reset_at: g_wk_reset.clone(),
                        }),
                        claude_gpt_5h: Some(ServerQuotaBucket {
                            remaining_percent: 64.0,
                            used_percent: 36.0,
                            reset_at: cg_reset.clone(),
                        }),
                        claude_gpt_weekly: Some(ServerQuotaBucket {
                            remaining_percent: 78.0,
                            used_percent: 22.0,
                            reset_at: cg_wk_reset.clone(),
                        }),
                    },
                    ExternalAntigravityQuota {
                        name: "Trade".to_owned(),
                        email: "trade.demo@gmail.com".to_owned(),
                        ok: true,
                        gemini_5h: Some(ServerQuotaBucket {
                            remaining_percent: 100.0,
                            used_percent: 0.0,
                            reset_at: String::new(),
                        }),
                        gemini_weekly: Some(ServerQuotaBucket {
                            remaining_percent: 96.0,
                            used_percent: 4.0,
                            reset_at: g_wk_reset,
                        }),
                        claude_gpt_5h: Some(ServerQuotaBucket {
                            remaining_percent: 85.0,
                            used_percent: 15.0,
                            reset_at: cg_reset,
                        }),
                        claude_gpt_weekly: Some(ServerQuotaBucket {
                            remaining_percent: 88.0,
                            used_percent: 12.0,
                            reset_at: cg_wk_reset,
                        }),
                    },
                ],
                providers: vec![
                    ExternalProviderSummary {
                        id: "maxplus".to_owned(),
                        name: "MaxPlus".to_owned(),
                        ok: true,
                        headline: "$29.90 credit".to_owned(),
                        subline: "24h $1.40 · 7d $9.80 · 16.2d runway".to_owned(),
                        remaining_percent: Some(59.8),
                        status_badge: "LIVE".to_owned(),
                    },
                    ExternalProviderSummary {
                        id: "opencode_go".to_owned(),
                        name: "OpenCode Go".to_owned(),
                        ok: true,
                        headline: "5h 18% · Wk 24%".to_owned(),
                        subline: "Mo 12% used · 148 reqs".to_owned(),
                        remaining_percent: Some(76.0),
                        status_badge: "LIVE".to_owned(),
                    },
                    ExternalProviderSummary {
                        id: "tokenme".to_owned(),
                        name: "TokenMe".to_owned(),
                        ok: true,
                        headline: "Cycle 38% elapsed".to_owned(),
                        subline: "312 reqs · 4.2M · 19d left".to_owned(),
                        remaining_percent: Some(62.0),
                        status_badge: "LIVE".to_owned(),
                    },
                    ExternalProviderSummary {
                        id: "firecrawl".to_owned(),
                        name: "Firecrawl".to_owned(),
                        ok: true,
                        headline: "420 / 500 credits".to_owned(),
                        subline: "80 used · 84% remaining".to_owned(),
                        remaining_percent: Some(84.0),
                        status_badge: "LIVE".to_owned(),
                    },
                ],
                opencode_go: Some(OpenCodeGoDetail {
                    ok: true,
                    rolling_used_pct: 18.0,
                    rolling_reset_at: oc_5h_reset,
                    weekly_used_pct: 24.0,
                    weekly_reset_at: oc_wk_reset,
                    monthly_used_pct: 12.0,
                    monthly_reset_at: oc_mo_reset,
                    combined_requests: 148,
                    combined_tokens: 1_940_000,
                }),
                maxplus: Some(MaxPlusDetail {
                    credit_usd: 29.90,
                    daily_free_credit: Some(0.50),
                    total_used_7keys_usd: Some(20.10),
                    periods: vec![
                        MaxPlusPeriodStat {
                            label: "24h".to_owned(),
                            request_count: 64,
                            total_tokens: 410_000,
                            total_cost_usd: 1.40,
                            per_day_usd: Some(1.40),
                        },
                        MaxPlusPeriodStat {
                            label: "7d".to_owned(),
                            request_count: 412,
                            total_tokens: 2_890_000,
                            total_cost_usd: 9.80,
                            per_day_usd: Some(1.40),
                        },
                        MaxPlusPeriodStat {
                            label: "30d".to_owned(),
                            request_count: 1_520,
                            total_tokens: 11_400_000,
                            total_cost_usd: 36.20,
                            per_day_usd: Some(1.21),
                        },
                        MaxPlusPeriodStat {
                            label: "All".to_owned(),
                            request_count: 2_840,
                            total_tokens: 21_600_000,
                            total_cost_usd: 64.50,
                            per_day_usd: None,
                        },
                    ],
                    keys: vec![
                        MaxPlusKeyRow {
                            name: "primary-cli".to_owned(),
                            pool: "main".to_owned(),
                            used_usd: 8.40,
                            limit_usd: Some(25.0),
                            limit_period: "monthly".to_owned(),
                            active: true,
                        },
                        MaxPlusKeyRow {
                            name: "router-agent".to_owned(),
                            pool: "main".to_owned(),
                            used_usd: 6.10,
                            limit_usd: Some(20.0),
                            limit_period: "monthly".to_owned(),
                            active: true,
                        },
                        MaxPlusKeyRow {
                            name: "code-review".to_owned(),
                            pool: "aux".to_owned(),
                            used_usd: 3.20,
                            limit_usd: Some(10.0),
                            limit_period: "weekly".to_owned(),
                            active: true,
                        },
                        MaxPlusKeyRow {
                            name: "batch-eval".to_owned(),
                            pool: "aux".to_owned(),
                            used_usd: 1.55,
                            limit_usd: Some(10.0),
                            limit_period: "monthly".to_owned(),
                            active: false,
                        },
                        MaxPlusKeyRow {
                            name: "sandbox-ci".to_owned(),
                            pool: "dev".to_owned(),
                            used_usd: 0.85,
                            limit_usd: Some(5.0),
                            limit_period: "daily".to_owned(),
                            active: true,
                        },
                    ],
                }),
                tokenme: Some(TokenMeDetail {
                    cycle_elapsed_pct: 38.0,
                    days_left: Some(19),
                    hours_left: Some(7),
                    cycle_requests: 312,
                    cycle_tokens: 4_200_000,
                    all_tokens: 18_700_000,
                    cycle_end_str: "Dec 17".to_owned(),
                }),
                firecrawl: Some(FirecrawlDetail {
                    ok: true,
                    remaining_credits: 420,
                    plan_credits: 500,
                    used_credits: 80,
                    remaining_percent: 84.0,
                    billing_period_end: fc_reset,
                }),
            })
        }
    }

    fn demo_step(
        ts: DateTime<Utc>,
        model: &str,
        input: u64,
        cache: u64,
        output: u64,
    ) -> TranscriptStep {
        TranscriptStep {
            timestamp_utc: ts,
            is_user_turn: false,
            model: Some(model.to_owned()),
            input_tokens: input,
            cache_read_tokens: cache,
            output_tokens: output,
        }
    }
}

#[cfg(test)]
mod tests {
    use agswitch_primitives::AccountId;

    use super::*;

    #[test]
    fn os_workspace_unlink_preserves_target() -> Result<(), String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let primary_home = tmp.path().join("primary_home");
        let vault_root = tmp.path().join("vault_acc2");

        let primary_ag = primary_home.join(".gemini").join("antigravity");
        fs::create_dir_all(&primary_ag).map_err(|e| e.to_string())?;
        let precious_chat = primary_ag.join("important_chat.json");
        fs::write(&precious_chat, r#"{"ok":true}"#).map_err(|e| e.to_string())?;

        let adapter = OsWorkspaceLinkAdapter::new(primary_home);
        let profile = AccountProfile::create_isolated(
            AccountId::new("acc2").map_err(|e| e.to_string())?,
            "Acc2".to_owned(),
            String::new(),
            "#6FD8EE".to_owned(),
            vault_root.to_string_lossy().into_owned(),
        );

        adapter
            .enable_shared_mode(&profile)
            .map_err(|e| e.to_string())?;
        assert!(adapter.is_shared_mode_enabled(&profile));

        // Deleting the isolated vault MUST unlink symlinks/junctions first and preserve primary chats
        adapter
            .delete_vault_directory_safely(&profile.root_path)
            .map_err(|e| e.to_string())?;
        assert!(!vault_root.exists());
        assert!(precious_chat.exists());
        Ok(())
    }

    #[test]
    fn os_workspace_links_with_spaces_toggle_and_preserve_target() -> Result<(), String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let primary_home = tmp.path().join("Name With Space");
        let vault_root = tmp.path().join("Profiles Dir").join("Work Acc");

        let primary_skills = primary_home.join(".gemini").join("skills");
        fs::create_dir_all(&primary_skills).map_err(|e| e.to_string())?;
        let precious_skill = primary_skills.join("my skill.md");
        fs::write(&precious_skill, "keep me").map_err(|e| e.to_string())?;

        let adapter = OsWorkspaceLinkAdapter::new(primary_home);
        let profile = AccountProfile::create_isolated(
            AccountId::new("work-acc").map_err(|e| e.to_string())?,
            "Work Acc".to_owned(),
            String::new(),
            "#6FD8EE".to_owned(),
            vault_root.to_string_lossy().into_owned(),
        );

        adapter
            .enable_shared_mode(&profile)
            .map_err(|e| e.to_string())?;
        assert!(adapter.is_shared_mode_enabled(&profile));
        // The link resolves to the primary folder contents.
        let via_link = vault_root
            .join(".gemini")
            .join("skills")
            .join("my skill.md");
        assert_eq!(
            fs::read_to_string(&via_link).map_err(|e| e.to_string())?,
            "keep me"
        );

        // Disabling removes only the links and recreates empty standalone folders.
        adapter
            .disable_shared_mode(&profile)
            .map_err(|e| e.to_string())?;
        assert!(!adapter.is_shared_mode_enabled(&profile));
        assert!(!via_link.exists());
        assert!(precious_skill.exists());

        // Re-enable is idempotent, and safe deletion still preserves the primary target.
        adapter
            .enable_shared_mode(&profile)
            .map_err(|e| e.to_string())?;
        adapter
            .delete_vault_directory_safely(&profile.root_path)
            .map_err(|e| e.to_string())?;
        assert!(!vault_root.exists());
        assert!(precious_skill.exists());
        Ok(())
    }

    #[test]
    fn telemetry_bridge_falls_back_to_local_json_and_validates_loopback_urls() -> Result<(), String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let json_path = tmp.path().join("usage-data.json");
        fs::write(
            &json_path,
            r#"{"meta":{"timestamp_bkk":"2026-11-25 14:30"},"maxplus":{"credit_usd":19.5}}"#,
        )
        .map_err(|e| e.to_string())?;

        // Use an unreachable loopback port so the adapter exercises its file fallback path
        let adapter = LocalTelemetryBridgeAdapter::new(
            "http://127.0.0.1:1".to_owned(),
            json_path,
            StdDuration::from_secs(10),
        );
        let snap = adapter
            .load_external_telemetry(true)
            .ok_or_else(|| "expected file fallback snapshot".to_owned())?;
        assert!(snap.connected);
        assert_eq!(snap.source_label, "usage-data.json");
        assert_eq!(snap.providers.len(), 1);

        // Reject external or shell-injected URLs
        assert!(open_local_url("https://example.com").is_err());
        assert!(open_local_url("http://127.0.0.1:8289/a&calc.exe").is_err());
        Ok(())
    }
}

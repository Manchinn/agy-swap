use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use agswitch_engine::{Engine, EngineDefaults, ProcessPort, TelemetryFeedPort, TranscriptPort};
use agswitch_integrations::{
    LocalTelemetryBridgeAdapter, OsProcessAdapter, OsTranscriptAdapter, OsWorkspaceLinkAdapter,
    SystemClockAdapter, default_antigravity_bin_path, default_data_dir, default_profiles_root_dir,
    primary_home_dir, sandbox,
};
use agswitch_primitives::{AccountTier, RuntimeOs};
use agswitch_spec::{
    AddAccountInput, CallerContext, CommandRequest, CommandResponse, DeleteAccountInput,
};
use agswitch_store::JsonFileStore;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "agswitch-cli",
    version,
    about = "Cross-platform Antigravity multi-account switcher CLI (macOS & Windows)"
)]
struct Cli {
    /// Emit machine-readable JSON output
    #[arg(long, global = true)]
    json: bool,

    /// Run in safe isolated sandbox mode (simulated processes, zero real Antigravity impact)
    #[arg(long, global = true)]
    sandbox: bool,

    /// Optional custom sandbox directory (implies --sandbox)
    #[arg(long, global = true)]
    sandbox_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Show active session, telemetry, and all configured accounts
    Status,
    /// List configured accounts
    List,
    /// Smart-switch to the next ready account (auto-skips 5h cooldowns)
    Next,
    /// Switch active Antigravity session to a specific account ID
    Switch { profile_id: String },
    /// Open or focus an account alongside currently running sessions
    Open { profile_id: String },
    /// Gracefully stop a running account instance and its orphan sidecars
    Stop { profile_id: String },
    /// Add a new isolated account vault
    Add {
        name: String,
        #[arg(long, default_value = "")]
        subtitle: String,
        #[arg(long)]
        tier: Option<String>,
        #[arg(long, default_value_t = true)]
        shared_chats: bool,
        #[arg(long, default_value_t = false)]
        open_sign_in: bool,
    },
    /// Toggle 5-hour quota cooldown for an account
    Cooldown { profile_id: String },
    /// Enable or disable shared chats/skills links for an isolated account
    Share {
        profile_id: String,
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: bool,
    },
    /// Remove or hide an account (pass --delete-disk --yes to remove local vault folder)
    Delete {
        profile_id: String,
        #[arg(long, default_value_t = false)]
        delete_disk: bool,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Restore hidden primary OS profile slot
    RestoreDefault,
    /// Show recent centralized engine audit entries
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Force-refresh external local telemetry bridge and print status
    Refresh,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let use_sandbox = cli.sandbox || cli.sandbox_dir.is_some();

    let engine = if use_sandbox {
        let base = cli.sandbox_dir.unwrap_or_else(sandbox::default_sandbox_dir);
        let paths = match sandbox::ensure_seeded(&base) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("sandbox init error: {err}");
                return ExitCode::FAILURE;
            }
        };
        let proc_adapter: Arc<dyn ProcessPort> =
            Arc::new(sandbox::SandboxProcessAdapter::new(paths.running_state_file));
        let tx_adapter: Arc<dyn TranscriptPort> = Arc::new(sandbox::SandboxTranscriptAdapter);
        let tel_adapter: Arc<dyn TelemetryFeedPort> = Arc::new(sandbox::SandboxTelemetryAdapter);
        Engine::new(
            EngineDefaults {
                profiles_root_dir: paths.profiles_root.to_string_lossy().into_owned(),
                antigravity_bin_path: "/sandbox/Antigravity".to_owned(),
                runtime_os: RuntimeOs::current(),
            },
            Arc::new(JsonFileStore::new(paths.data_dir)),
            proc_adapter,
            Arc::new(OsWorkspaceLinkAdapter::new(paths.fake_home)),
            tx_adapter,
            Arc::new(SystemClockAdapter),
        )
        .with_telemetry(tel_adapter)
    } else {
        let data_dir = default_data_dir();
        let profiles_root = default_profiles_root_dir(&data_dir);
        let primary_home = primary_home_dir();
        Engine::new(
            EngineDefaults {
                profiles_root_dir: profiles_root.to_string_lossy().into_owned(),
                antigravity_bin_path: default_antigravity_bin_path(),
                runtime_os: RuntimeOs::current(),
            },
            Arc::new(JsonFileStore::new(data_dir)),
            Arc::new(OsProcessAdapter::new()),
            Arc::new(OsWorkspaceLinkAdapter::new(primary_home.clone())),
            Arc::new(OsTranscriptAdapter::new(primary_home)),
            Arc::new(SystemClockAdapter),
        )
        .with_telemetry(Arc::new(LocalTelemetryBridgeAdapter::from_defaults()))
    };

    let (request, caller) = match cli.command {
        Commands::Status | Commands::List => {
            (CommandRequest::GetDashboard, CallerContext::local_interactive())
        }
        Commands::Next => (CommandRequest::SwitchNext, CallerContext::local_interactive()),
        Commands::Switch { profile_id } => (
            CommandRequest::SwitchToProfile { profile_id },
            CallerContext::local_interactive(),
        ),
        Commands::Open { profile_id } => (
            CommandRequest::OpenAlongside { profile_id },
            CallerContext::local_interactive(),
        ),
        Commands::Stop { profile_id } => (
            CommandRequest::StopProfile { profile_id },
            CallerContext::local_interactive(),
        ),
        Commands::Add {
            name,
            subtitle,
            tier,
            shared_chats,
            open_sign_in,
        } => {
            let tier_override = tier.as_deref().and_then(AccountTier::parse_opt);
            (
                CommandRequest::AddAccount(AddAccountInput {
                    display_name: name,
                    subtitle,
                    accent_hex: None,
                    tier_override,
                    share_chats_and_skills: shared_chats,
                    open_and_sign_in: open_sign_in,
                }),
                CallerContext::local_interactive(),
            )
        }
        Commands::Cooldown { profile_id } => (
            CommandRequest::ToggleCooldown { profile_id },
            CallerContext::local_interactive(),
        ),
        Commands::Share {
            profile_id,
            enabled,
        } => (
            CommandRequest::SetSharedChats {
                profile_id,
                enabled,
            },
            CallerContext::local_interactive(),
        ),
        Commands::Delete {
            profile_id,
            delete_disk,
            yes,
        } => (
            CommandRequest::DeleteAccount(DeleteAccountInput {
                profile_id,
                delete_disk_directory: delete_disk,
            }),
            if yes {
                CallerContext::local_confirmed()
            } else {
                CallerContext::local_interactive()
            },
        ),
        Commands::RestoreDefault => (
            CommandRequest::RestoreDefault,
            CallerContext::local_interactive(),
        ),
        Commands::Audit { limit } => (
            CommandRequest::ListAudit { limit },
            CallerContext::local_interactive(),
        ),
        Commands::Refresh => (
            CommandRequest::RefreshTelemetry,
            CallerContext::local_interactive(),
        ),
    };

    match engine.execute(request, caller).await {
        Ok(resp) => {
            if cli.json {
                match serde_json::to_string_pretty(&resp) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("json serialization error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                print_human(&resp);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            if cli.json {
                if let Ok(json) = serde_json::to_string_pretty(&err) {
                    eprintln!("{json}");
                } else {
                    eprintln!("{err}");
                }
            } else {
                eprintln!("error: {err}");
            }
            ExitCode::FAILURE
        }
    }
}

fn print_human(resp: &CommandResponse) {
    match resp {
        CommandResponse::Dashboard(d) => {
            let active = if d.active_names.is_empty() {
                "No window open".to_owned()
            } else {
                d.active_names.join(", ")
            };
            println!("Active: {active}");
            println!(
                "Binary: {} ({})",
                d.antigravity_bin_path,
                if d.antigravity_bin_exists {
                    "found"
                } else {
                    "MISSING"
                }
            );
            println!(
                "Next ready: {} | 24h: {} tok | Weekly (7d): {} tok ({} prompts) | Context: {:.0}%",
                d.next_account_name.as_deref().unwrap_or("none"),
                agswitch_domain::format_tokens(d.total_tokens_24h),
                agswitch_domain::format_tokens(d.total_tokens_7d),
                d.total_turns_7d,
                d.context_budget_percent,
            );
            println!(
                "Models (7d): Gemini {}x ({}) | Claude {}x ({}) | GPT {}x ({})",
                d.models_7d.gemini_calls_7d,
                agswitch_domain::format_tokens(d.models_7d.gemini_tokens_7d),
                d.models_7d.claude_calls_7d,
                agswitch_domain::format_tokens(d.models_7d.claude_tokens_7d),
                d.models_7d.gpt_calls_7d,
                agswitch_domain::format_tokens(d.models_7d.gpt_tokens_7d),
            );
            println!("\nAccounts ({}):", d.accounts.len());
            for a in &d.accounts {
                let state = if a.is_running {
                    "RUNNING"
                } else if a.is_on_cooldown {
                    "COOLDOWN"
                } else if !a.is_authenticated {
                    "UNAUTH"
                } else {
                    "READY"
                };
                let timer_txt = if a.reset_label.is_empty() {
                    "ready".to_owned()
                } else {
                    format!("reset {}", a.reset_label)
                };
                let quota_suffix = if let Some(sq) = &a.server_quota
                    && sq.ok
                {
                    format!(
                        " | Live Quota: G {:.0}%/{:.0}%w · CG {:.0}%/{:.0}%w",
                        a.models.gemini_remaining_5h_pct,
                        a.models.gemini_remaining_7d_pct,
                        a.models.claude_gpt_remaining_5h_pct,
                        a.models.claude_gpt_remaining_7d_pct,
                    )
                } else {
                    String::new()
                };
                println!(
                    "  - [{state:<8}] {:<18} ({}) | {} | {} | 5h: {}x ({timer_txt}) | 7d: {}x (G:{} C:{} GPT:{}){quota_suffix}",
                    a.display_name,
                    a.id,
                    a.tier.badge(),
                    if a.is_shared_chats { "shared" } else { "isolated" },
                    a.steps_5h,
                    a.steps_7d,
                    a.models.gemini_calls_7d,
                    a.models.claude_calls_7d,
                    a.models.gpt_calls_7d,
                );
            }
            if let Some(ext) = &d.external_telemetry
                && ext.connected
            {
                println!(
                    "\nExternal Telemetry ({} · {}):",
                    ext.source_label, ext.updated_at
                );
                for prov in &ext.providers {
                    println!(
                        "  - [{:<4}] {:<12} {} | {}",
                        prov.status_badge, prov.name, prov.headline, prov.subline
                    );
                }
            }
        }
        CommandResponse::Switched {
            target_name,
            closed_count,
            ..
        } => {
            println!("Switched active account to '{target_name}' (closed {closed_count} window(s))");
        }
        CommandResponse::OpenedAlongside {
            target_name,
            focused_existing,
            ..
        } => {
            if *focused_existing {
                println!("Focused running window for '{target_name}'");
            } else {
                println!("Opened '{target_name}' alongside active sessions");
            }
        }
        CommandResponse::Stopped {
            profile_id,
            closed_count,
        } => {
            println!("Stopped '{profile_id}' ({closed_count} window(s))");
        }
        CommandResponse::AccountSaved(p) => {
            println!("Saved account '{}' ({})", p.display_name, p.id);
        }
        CommandResponse::AccountRemoved {
            profile_id,
            hid_default,
            deleted_disk,
        } => {
            if *hid_default {
                println!("Hid primary profile '{profile_id}' (files untouched)");
            } else if *deleted_disk {
                println!("Removed '{profile_id}' and deleted local login vault");
            } else {
                println!("Removed '{profile_id}' from list (local vault kept)");
            }
        }
        CommandResponse::CooldownToggled {
            profile_id,
            is_on_cooldown,
        } => {
            println!(
                "Cooldown for '{profile_id}': {}",
                if *is_on_cooldown { "ACTIVE (5h)" } else { "CLEARED" }
            );
        }
        CommandResponse::SharedChatsUpdated {
            profile_id,
            enabled,
        } => {
            println!(
                "Shared chats for '{profile_id}': {}",
                if *enabled { "ENABLED" } else { "DISABLED" }
            );
        }
        CommandResponse::LoginReset { profile_id } => {
            println!("Reset login vault for '{profile_id}'");
        }
        CommandResponse::AuditLog(entries) => {
            if entries.is_empty() {
                println!("No audit entries recorded yet.");
                return;
            }
            for e in entries {
                let rule = e.rule.map_or("-", |r| r.code());
                println!(
                    "{} | {:<16} | {:<8?} | {:<2} | {}",
                    e.timestamp_utc.format("%H:%M:%S"),
                    e.command.as_str(),
                    e.outcome,
                    rule,
                    e.summary
                );
            }
        }
    }
}

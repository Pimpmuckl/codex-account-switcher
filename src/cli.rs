use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::{App, InteractiveExit, InteractiveMode};
use crate::env;
use crate::model::{
    AccountUsageView, AccountView, AutoStartUsageWindowsRunOutput,
    AutoStartUsageWindowsStatusOutput, RunningCodexProcess, UsageOutput,
};
use crate::process::format_process_table;
use crate::repository::SnapshotRepository;
use crate::secrets::LocalSecretStore;
use crate::usage::{usage_error_label, usage_error_requires_login};

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Switch Codex accounts by snapshotting live auth state"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Status {
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Save {
        #[arg(long)]
        json: bool,
    },
    Usage {
        account_id: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    Activate {
        account_id: Option<Uuid>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        force: bool,
    },
    Delete {
        account_id: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
    AutoStartUsageWindows {
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        run: bool,
        #[arg(long)]
        json: bool,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let env = env::detect()?;
    let repository = SnapshotRepository::new(
        &env.app_data_dir,
        LocalSecretStore::new(&env.app_data_dir.join("snapshots")),
    );
    let app = App::new(env, repository);
    match cli.command {
        None => run_interactive_app(&app),
        Some(Command::Status { json }) => {
            let status = app.status()?;
            if json {
                print_json(&status)?;
            } else {
                println!("Environment: {}", status.environment);
                println!("Codex root: {}", status.codex_root);
                match status.current_account {
                    Some(account) => println!("Current account: {}", account.email),
                    None => println!("Current account: not logged in"),
                }
                println!("Saved accounts: {}", status.saved_accounts);
                if !status.process_warnings.is_empty() {
                    print_process_summary("Codex processes", &status.process_warnings);
                }
            }
            Ok(())
        }
        Some(Command::List { json }) => {
            let list = app.list()?;
            if json {
                print_json(&list)?;
            } else if list.accounts.is_empty() {
                println!("No saved accounts in {}.", list.environment);
            } else {
                for account in list.accounts {
                    println!("{}", render_account_summary(&account));
                }
            }
            Ok(())
        }
        Some(Command::Save { json }) => {
            let output = app.save_current()?;
            if json {
                print_json(&output)?;
            } else {
                println!("Saved {} ({})", output.account.email, output.account.id);
            }
            Ok(())
        }
        Some(Command::Usage { account_id, json }) => {
            let output = app.usage(account_id)?;
            if json {
                print_json(&output)?;
            } else {
                print_usage_output(&output);
            }
            Ok(())
        }
        Some(Command::Activate {
            account_id,
            json,
            force,
        }) => {
            let mut showed_preflight = false;
            let output = match account_id {
                Some(account_id) => {
                    app.validate_activation_target(account_id)?;
                    let warnings = app.activation_preflight_warnings();
                    if !warnings.is_empty() {
                        if !force {
                            if !json {
                                print_process_summary("Codex processes", &warnings);
                            }
                            bail!(
                                "Codex appears to be running. Close those processes first or rerun `activate` with `--force`."
                            );
                        }
                        if !json {
                            showed_preflight = true;
                            print_process_summary("Codex processes", &warnings);
                        }
                    }
                    app.activate_with_running_policy(account_id, force)?
                }
                None => {
                    let _ = app.interactive(InteractiveMode::ActivateOnce, force)?;
                    return Ok(());
                }
            };
            if json {
                print_json(&output)?;
            } else {
                println!("Activated {} ({})", output.account.email, output.account.id);
                if !showed_preflight {
                    print_process_summary("Codex processes", &output.warnings);
                }
            }
            Ok(())
        }
        Some(Command::Delete { account_id, json }) => {
            let output = match account_id {
                Some(account_id) => app.delete(account_id)?,
                None => {
                    let _ = app.interactive(InteractiveMode::DeleteOnce, false)?;
                    return Ok(());
                }
            };
            if json {
                print_json(&output)?;
            } else {
                println!("Deleted saved snapshot {}", output.deleted_account_id);
            }
            Ok(())
        }
        Some(Command::AutoStartUsageWindows {
            enable,
            disable,
            run,
            json,
        }) => {
            let status = if enable {
                app.set_auto_start_usage_windows(true)?
            } else if disable {
                app.set_auto_start_usage_windows(false)?
            } else {
                app.auto_start_usage_windows_status()?
            };
            if run && !disable {
                let output = app.auto_start_usage_windows_once(false)?;
                if json {
                    print_json(&output)?;
                } else {
                    print_auto_start_usage_windows_run(&output);
                }
            } else if json {
                print_json(&status)?;
            } else {
                print_auto_start_usage_windows_status(&status);
            }
            Ok(())
        }
    }
}

fn run_interactive_app<S>(app: &App<S>) -> Result<()>
where
    S: crate::secrets::SecretStore,
{
    crate::app::spawn_auto_start_usage_windows_worker(app.env().clone());
    if !app.auto_start_usage_windows_status()?.enabled {
        spawn_saved_usage_cache_refresh(app.env().clone());
    }
    #[cfg(windows)]
    {
        loop {
            match app.interactive(InteractiveMode::Persistent, false)? {
                InteractiveExit::Quit => return Ok(()),
                InteractiveExit::SendToTray => {
                    crate::tray::hide_console_window();
                    match crate::tray::run(app)? {
                        crate::tray::TrayExit::ShowTui => crate::tray::show_console_window(),
                        crate::tray::TrayExit::Quit => return Ok(()),
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        match app.interactive(InteractiveMode::Persistent, false)? {
            InteractiveExit::Quit => Ok(()),
        }
    }
}

fn spawn_saved_usage_cache_refresh(env: env::AppEnv) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(move || {
        let _ = std::thread::Builder::new()
            .name("saved-usage-cache-refresh".to_owned())
            .spawn(move || {
                if let Err(error) = run_saved_usage_cache_refresh(env) {
                    eprintln!("saved usage refresh failed: {error:#}");
                }
            });
    });
}

fn run_saved_usage_cache_refresh(env: env::AppEnv) -> Result<()> {
    let repository = SnapshotRepository::new(
        &env.app_data_dir,
        LocalSecretStore::new(&env.app_data_dir.join("snapshots")),
    );
    let app = App::new(env, repository);
    app.refresh_saved_usage_cache()
}

fn print_json<T>(value: &T) -> Result<()>
where
    T: serde::Serialize,
{
    let json = serde_json::to_string_pretty(value).context("failed to encode JSON output")?;
    println!("{json}");
    Ok(())
}

fn print_process_summary(title: &str, processes: &[RunningCodexProcess]) {
    println!("{title}:");
    for line in format_process_table(processes) {
        println!("{line}");
    }
}

fn render_account_summary(account: &AccountView) -> String {
    let mut line = format!(
        "{} {}{}",
        account.id,
        account.email,
        if account.is_active { " [active]" } else { "" }
    );
    if account
        .usage_error
        .as_deref()
        .is_some_and(usage_error_requires_login)
    {
        line.push_str(&format!(
            " [{}]",
            usage_error_label(account.usage_error.as_deref().unwrap_or_default()).to_lowercase()
        ));
    } else if let Some(usage) = &account.usage
        && let Some(weekly) = &usage.weekly
    {
        if weekly.reset_at <= OffsetDateTime::now_utc() {
            line.push_str(" [weekly reset passed]");
        } else {
            line.push_str(&format!(
                " [weekly remaining: {}%, reset {}]",
                weekly.remaining_percent,
                weekly.reset_at.date()
            ));
        }
    } else if let Some(error) = &account.usage_error {
        line.push_str(&format!(" [{}]", usage_error_label(error).to_lowercase()));
    }
    line
}

fn print_usage_output(output: &UsageOutput) {
    println!("Environment: {}", output.environment);
    println!("Account: {}", output.account.email);
    if let Some(plan) = &output.account.plan_label {
        println!("Plan: {plan}");
    }
    print_usage_summary(&output.usage);
}

fn print_auto_start_usage_windows_status(output: &AutoStartUsageWindowsStatusOutput) {
    println!(
        "Auto-start usage windows: {}",
        if output.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("Poll interval: {}s", output.poll_seconds);
}

fn print_auto_start_usage_windows_run(output: &AutoStartUsageWindowsRunOutput) {
    println!(
        "Auto-start usage windows: {}",
        if output.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("Checked accounts: {}", output.checked_accounts);
    for account in &output.pinged_accounts {
        match &account.detail {
            Some(detail) => println!("{}: {} ({detail})", account.email, account.status),
            None => println!("{}: {}", account.email, account.status),
        }
    }
    for skipped in &output.skipped {
        println!("Skipped: {skipped}");
    }
}

fn print_usage_summary(usage: &AccountUsageView) {
    println!("Source: {}", format!("{:?}", usage.source).to_lowercase());
    println!("Fetched at: {}", usage.fetched_at);
    if let Some(five_hour) = &usage.five_hour {
        println!(
            "5h remaining: {}% (reset {})",
            five_hour.remaining_percent, five_hour.reset_at
        );
    }
    if let Some(weekly) = &usage.weekly {
        println!(
            "Weekly remaining: {}% (reset {})",
            weekly.remaining_percent, weekly.reset_at
        );
    }
    if let Some(credits) = &usage.credits {
        println!(
            "Credits: {} (has_credits={}, unlimited={})",
            credits.balance, credits.has_credits, credits.unlimited
        );
    }
}

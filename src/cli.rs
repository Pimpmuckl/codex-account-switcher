use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use uuid::Uuid;

use crate::app::{App, InteractiveMode};
use crate::env;
use crate::model::RunningCodexProcess;
use crate::process::format_process_table;
use crate::repository::SnapshotRepository;
use crate::secrets::KeyringSecretStore;

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
    Activate {
        account_id: Option<Uuid>,
        #[arg(long)]
        json: bool,
        #[arg(long, hide = true)]
        force: bool,
    },
    Delete {
        account_id: Option<Uuid>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let env = env::detect()?;
    let repository = SnapshotRepository::new(&env.app_data_dir, KeyringSecretStore::default());
    let app = App::new(env, repository);
    match cli.command {
        None => app.interactive(InteractiveMode::Persistent),
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
                    println!(
                        "{} {}{}",
                        account.id,
                        account.email,
                        if account.is_active { " [active]" } else { "" }
                    );
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
        Some(Command::Activate {
            account_id,
            json,
            force: _,
        }) => {
            let mut showed_preflight = false;
            let output = match account_id {
                Some(account_id) => {
                    let warnings = app.activation_preflight_warnings();
                    if !warnings.is_empty() && !json {
                        showed_preflight = true;
                        print_process_summary("Codex processes", &warnings);
                    }
                    app.activate(account_id)?
                }
                None => {
                    app.interactive(InteractiveMode::ActivateOnce)?;
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
                    app.interactive(InteractiveMode::DeleteOnce)?;
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
    }
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

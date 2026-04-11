use std::process::ExitCode;

fn main() -> ExitCode {
    match codex_account_switcher::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

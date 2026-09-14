//! Independent foreground observation UI; default orch CLI remains isolated.
use std::{env, path::PathBuf, process::ExitCode};
fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let mut root = None;
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("orch-tui [--root PATH]\nRead-only Fusion invocation observations, current directory by default.\n1/2/3 calls/harness/tasks; arrows and PageUp/PageDown navigate; Enter detail; Esc back; p project (Enter validates, Esc cancels); r refresh; y copy safe locator; q or Ctrl+C exit. Refresh every 2 seconds. Phase is last observed, not liveness. Requires TTY stdin and stdout.");
            return ExitCode::SUCCESS;
        } else if arg == "--root" && root.is_none() {
            let Some(path) = args.next() else {
                eprintln!("orch-tui: --root requires PATH");
                return ExitCode::from(2);
            };
            root = Some(PathBuf::from(path));
        } else {
            eprintln!("orch-tui: invalid arguments; use --help");
            return ExitCode::from(2);
        }
    }
    let root = match root.map(Ok).unwrap_or_else(env::current_dir) {
        Ok(root) => root,
        Err(_) => {
            eprintln!("orch-tui: current directory unavailable");
            return ExitCode::from(1);
        }
    };
    match orch_ui::app::run(&root) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "orch-tui: {}",
                orch_host::observation::safe_observation_text(&e)
            );
            ExitCode::from(1)
        }
    }
}

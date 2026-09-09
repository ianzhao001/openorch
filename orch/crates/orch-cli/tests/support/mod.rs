use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(dead_code)]
pub(crate) fn orch_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_orch"))
}

#[allow(dead_code)]
pub(crate) fn fixture_git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-C")
        .arg(root);
    command
}

#[allow(dead_code)]
pub(crate) fn configure_fixture_git_env(command: &mut Command, extras: &[(&str, &str)]) {
    command.env_remove("GIT_CONFIG_PARAMETERS");
    command.env("GIT_CONFIG_COUNT", (extras.len() + 1).to_string());
    command.env("GIT_CONFIG_KEY_0", "core.fsmonitor");
    command.env("GIT_CONFIG_VALUE_0", "false");

    for (index, (key, value)) in extras.iter().enumerate() {
        let slot = index + 1;
        command.env(format!("GIT_CONFIG_KEY_{slot}"), key);
        command.env(format!("GIT_CONFIG_VALUE_{slot}"), value);
    }
}

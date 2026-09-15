//! Standalone loopback-only observation WebUI.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(orch_ui::web::run_web_cli(&args));
}

use std::io::Write;

use clap::Parser;
use srtty::cli::Cli;

/// Whether `--json` is among the raw arguments (the parse failed, so `Cli` is not available).
fn wants_json() -> bool {
    std::env::args_os().any(|arg| arg == "--json")
}

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // Usage errors exit with 1, like every other usage error; help and version with 0.
            let code = if err.use_stderr() { 1 } else { 0 };
            if err.use_stderr() && wants_json() {
                // `--json` promises one JSON object on stdout, usage errors included.
                let rendered = err.render().to_string();
                let message = rendered.lines().next().unwrap_or_default();
                let message = message.strip_prefix("error: ").unwrap_or(message);
                let report = serde_json::json!({ "error": message, "exit_code": code });
                let _ = writeln!(std::io::stdout().lock(), "{report}");
            }
            let _ = err.print();
            std::process::exit(code);
        }
    };
    let json = cli.json;
    if let Err(err) = srtty::app::main(cli).await {
        let code = err.exit_code();
        if json {
            let report = serde_json::json!({ "error": err.to_string(), "exit_code": code });
            let _ = writeln!(std::io::stdout().lock(), "{report}");
        } else {
            eprintln!("{:?}", miette::Report::new(err));
        }
        std::process::exit(code);
    }
}

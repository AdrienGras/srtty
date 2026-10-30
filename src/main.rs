use clap::Parser;
use srtty::cli::Cli;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(err) = srtty::app::main(cli).await {
        let code = err.exit_code();
        eprintln!("{:?}", miette::Report::new(err));
        std::process::exit(code);
    }
}

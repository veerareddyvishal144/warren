//! `warren`: the command-line client for the Warren Sandbox API and Agent API.
//!
//! Exit codes: 0 success, 1 the API returned an error (code and message on
//! stderr), 2 wrong invocation.

mod run;

use clap::Parser;
use warren_cli::Cli;

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // Help and version print to stdout and exit 0; usage errors exit 2.
            let _ = e.print();
            std::process::exit(e.exit_code());
        }
    };
    let json = cli.json;
    let code = match run::run(cli).await {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("{}", failure.render(json));
            failure.exit_code()
        }
    };
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

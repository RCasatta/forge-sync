use std::ffi::OsString;

use forge_sync::cli::{self, Command};
use forge_sync::github::GithubClient;
use forge_sync::gitlab::GlabClient;

fn run() -> forge_sync::Result<()> {
    let command = cli::parse(std::env::args_os().skip(1).collect::<Vec<OsString>>())?;
    match command {
        Command::Github { repository, output } => {
            let mut client = GithubClient::new()?;
            forge_sync::sync::github(&repository, &output, &mut client)
        }
        Command::Gitlab {
            host,
            project,
            output,
        } => {
            let mut client = GlabClient::new(host.clone());
            forge_sync::sync::gitlab(&host, &project, &output, &mut client)
        }
        Command::Status { output } => {
            println!("{}", forge_sync::sync::status(&output)?);
            Ok(())
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("forge-sync: {error}");
        std::process::exit(1);
    }
}

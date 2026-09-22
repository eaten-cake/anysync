mod backend;
mod cli;
mod config;
mod sync;

use anyhow::Result;
use clap::Parser;
use cli::Cmd;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match cli::Cli::parse().command {
        Cmd::Init => config::init(),
        Cmd::Config {
            backend,
            url,
            root,
            username,
            password,
        } => config::remote_config(backend, url, root, username, password),
        Cmd::Pull => sync::pull(),
        Cmd::Push => sync::push(),
        Cmd::Status => sync::status(),
    }
}

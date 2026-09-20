mod cli;
mod config;
mod sync;
mod webdav;

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
            url,
            root,
            username,
            password,
        } => config::remote_config(url, root, username, password),
        Cmd::Pull => sync::pull(),
        Cmd::Push => sync::push(),
    }
}

mod dictionary;
mod gestured;
mod launcher;
mod popup;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "pareto", about = "Filesystem knowledge and dictionary panel")]
struct Cli {
    #[arg(long, global = true, default_value = "./data", value_name = "DIR")]
    dict_data: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Install this executable as the privileged launcher")]
    Install {
        #[arg(long, default_value = "/usr/bin")]
        dir: PathBuf,
    },
    #[command(about = "Run the gesture daemon and the user interface")]
    Run,
    #[command(name = "__ui", hide = true)]
    Ui,
    #[command(name = "__gestured", hide = true)]
    Gestured,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dict_data = cli.dict_data;
    let command = cli.command.unwrap_or(Command::Run);

    match command {
        Command::Install { dir } => launcher::install(&dir),
        Command::Run => launcher::run(&dict_data),
        Command::Ui => {
            launcher::drop_to_real_user()?;
            popup::run(&dict_data)
        }
        Command::Gestured => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(launcher::run_gestured())
        }
    }
}

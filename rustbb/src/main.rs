use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod builder;
mod codegen;
mod discovery;
mod transform;

#[derive(Parser)]
#[command(name = "rustbb")]
#[command(about = "Combine Rust CLI crates into a single multi-call binary (like gobusybox for Rust)")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Analyze a crate without building
    Analyze {
        /// Path to the crate
        #[arg(required = true)]
        crates: Vec<PathBuf>,
    },

    /// Build a multi-call binary
    Build {
        /// Paths to crates to include
        #[arg(required = true)]
        crates: Vec<PathBuf>,

        /// Output binary name
        #[arg(short, long, default_value = "rustbb_combined")]
        output: String,

        /// Build in release mode
        #[arg(long)]
        release: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Analyze { crates } => {
            for crate_path in crates {
                match discovery::analyze_crate(&crate_path) {
                    Ok(info) => {
                        println!("Crate: {}", info.name);
                        println!("  Path: {}", info.path.display());
                        println!("  Main: {}", info.main_path.display());
                        println!("  Strategy: {:?}", info.strategy);
                        println!("  Dependencies: {:?}", info.dependencies);
                        println!();
                    }
                    Err(e) => {
                        eprintln!("Error analyzing {:?}: {}", crate_path, e);
                    }
                }
            }
        }
        Commands::Build {
            crates,
            output,
            release,
        } => {
            builder::build(&crates, &output, release)?;
        }
    }

    Ok(())
}

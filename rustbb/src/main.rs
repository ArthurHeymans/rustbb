use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod builder;
mod codegen;
mod discovery;
mod fetch;
mod transform;

use fetch::{CrateSource, FetchedCrate};

#[derive(Parser)]
#[command(name = "rustbb")]
#[command(
    about = "Combine Rust CLI crates into a single multi-call binary (like gobusybox for Rust)"
)]
#[command(version)]
#[command(after_help = r#"CRATE SOURCES:
  ./path/to/crate       Local filesystem path
  crate_name            Crate from crates.io (latest version)
  crate_name@1.0        Crate from crates.io (specific version)
  github:user/repo      GitHub repository
  github:user/repo#tag  GitHub repository at specific tag/branch
  git:https://url.git   Any git repository

EXAMPLES:
  rustbb build ./my-cli -o mybox
  rustbb build ripgrep bat -o tools --release
  rustbb build github:sharkdp/fd github:sharkdp/bat -o finder
"#)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Analyze a crate without building
    Analyze {
        /// Crate sources (paths, crate names, or git URLs)
        #[arg(required = true)]
        sources: Vec<String>,
    },

    /// Build a multi-call binary
    Build {
        /// Crate sources (paths, crate names, or git URLs)
        #[arg(required = true)]
        sources: Vec<String>,

        /// Output binary name
        #[arg(short, long, default_value = "rustbb_combined")]
        output: String,

        /// Build in release mode
        #[arg(long)]
        release: bool,

        /// Cache directory for downloaded crates
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Analyze { sources } => {
            let cache_dir = std::env::temp_dir().join("rustbb-cache");
            std::fs::create_dir_all(&cache_dir)?;

            for source_str in sources {
                let source = CrateSource::parse(&source_str)?;
                println!("Source: {}", source.display_name());

                match fetch::fetch_crate(&source, &cache_dir) {
                    Ok(fetched) => match discovery::analyze_crate(&fetched.path) {
                        Ok(info) => {
                            println!("  Crate: {}", info.name);
                            println!("  Path: {}", info.path.display());
                            println!("  Main: {}", info.main_path.display());
                            println!("  Strategy: {:?}", info.strategy);
                            println!("  Dependencies: {:?}", info.dependencies);
                        }
                        Err(e) => {
                            eprintln!("  Error analyzing: {}", e);
                        }
                    },
                    Err(e) => {
                        eprintln!("  Error fetching: {}", e);
                    }
                }
                println!();
            }
        }
        Commands::Build {
            sources,
            output,
            release,
            cache_dir,
        } => {
            let cache_dir = cache_dir.unwrap_or_else(|| std::env::temp_dir().join("rustbb-cache"));
            std::fs::create_dir_all(&cache_dir)?;

            // Parse and fetch all crate sources
            println!("Fetching crates...");
            let mut fetched_crates: Vec<FetchedCrate> = Vec::new();

            for source_str in &sources {
                let source = CrateSource::parse(source_str)?;
                print!("  {} ... ", source.display_name());

                match fetch::fetch_crate(&source, &cache_dir) {
                    Ok(fetched) => {
                        println!("ok ({})", fetched.name);
                        fetched_crates.push(fetched);
                    }
                    Err(e) => {
                        println!("FAILED");
                        eprintln!("    Error: {}", e);
                    }
                }
            }

            if fetched_crates.is_empty() {
                anyhow::bail!("No crates could be fetched");
            }

            // Convert to paths for the builder
            let crate_paths: Vec<PathBuf> = fetched_crates.iter().map(|f| f.path.clone()).collect();

            // Build
            builder::build(&crate_paths, &output, release)?;
        }
    }

    Ok(())
}

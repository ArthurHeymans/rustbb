use clap::Parser;
use std::fs::File;
use std::io::{self, BufRead, BufReader};

#[derive(Parser)]
#[command(name = "wc")]
#[command(about = "Print newline, word, and byte counts")]
struct Args {
    /// Print the newline counts
    #[arg(short = 'l', long)]
    lines: bool,

    /// Print the word counts
    #[arg(short = 'w', long)]
    words: bool,

    /// Print the byte counts
    #[arg(short = 'c', long)]
    bytes: bool,

    /// Files to read (stdin if none)
    files: Vec<String>,
}

struct Counts {
    lines: usize,
    words: usize,
    bytes: usize,
}

fn main() {
    let mut args = Args::parse();

    // If no flags specified, show all
    if !args.lines && !args.words && !args.bytes {
        args.lines = true;
        args.words = true;
        args.bytes = true;
    }

    let mut total = Counts {
        lines: 0,
        words: 0,
        bytes: 0,
    };

    if args.files.is_empty() {
        // Read from stdin
        let stdin = io::stdin();
        let counts = count_reader(stdin.lock());
        print_counts(&counts, &args, None);
    } else {
        for filename in &args.files {
            match File::open(filename) {
                Ok(file) => {
                    let counts = count_reader(BufReader::new(file));
                    print_counts(&counts, &args, Some(filename));
                    total.lines += counts.lines;
                    total.words += counts.words;
                    total.bytes += counts.bytes;
                }
                Err(e) => eprintln!("wc: {}: {}", filename, e),
            }
        }

        if args.files.len() > 1 {
            print_counts(&total, &args, Some("total"));
        }
    }
}

fn count_reader<R: BufRead>(reader: R) -> Counts {
    let mut counts = Counts {
        lines: 0,
        words: 0,
        bytes: 0,
    };

    for line in reader.lines() {
        match line {
            Ok(line) => {
                counts.lines += 1;
                counts.words += line.split_whitespace().count();
                counts.bytes += line.len() + 1; // +1 for newline
            }
            Err(_) => break,
        }
    }

    counts
}

fn print_counts(counts: &Counts, args: &Args, filename: Option<&str>) {
    let mut parts = Vec::new();

    if args.lines {
        parts.push(format!("{:8}", counts.lines));
    }
    if args.words {
        parts.push(format!("{:8}", counts.words));
    }
    if args.bytes {
        parts.push(format!("{:8}", counts.bytes));
    }

    let output = parts.join("");
    match filename {
        Some(name) => println!("{} {}", output, name),
        None => println!("{}", output),
    }
}

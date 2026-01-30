use clap::Parser;
use std::fs::File;
use std::io::{self, BufRead, BufReader};

#[derive(Parser)]
#[command(name = "head")]
#[command(about = "Output the first part of files")]
struct Args {
    /// Number of lines to print
    #[arg(short = 'n', long, default_value = "10")]
    lines: usize,

    /// Files to read (stdin if none)
    files: Vec<String>,
}

fn main() {
    let args = Args::parse();

    if args.files.is_empty() {
        // Read from stdin
        let stdin = io::stdin();
        print_lines(stdin.lock(), args.lines);
    } else {
        for (i, filename) in args.files.iter().enumerate() {
            if args.files.len() > 1 {
                if i > 0 {
                    println!();
                }
                println!("==> {} <==", filename);
            }

            match File::open(filename) {
                Ok(file) => print_lines(BufReader::new(file), args.lines),
                Err(e) => eprintln!("head: {}: {}", filename, e),
            }
        }
    }
}

fn print_lines<R: BufRead>(reader: R, count: usize) {
    for line in reader.lines().take(count) {
        match line {
            Ok(line) => println!("{}", line),
            Err(e) => {
                eprintln!("head: error reading: {}", e);
                break;
            }
        }
    }
}

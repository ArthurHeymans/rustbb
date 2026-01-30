use std::fs;
use std::io::{self, Read};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        // Read from stdin
        let mut buffer = String::new();
        io::stdin().read_to_string(&mut buffer).unwrap();
        print!("{}", buffer);
    } else {
        // Read from files
        for filename in args {
            match fs::read_to_string(&filename) {
                Ok(contents) => print!("{}", contents),
                Err(e) => eprintln!("simple_cat: {}: {}", filename, e),
            }
        }
    }
}

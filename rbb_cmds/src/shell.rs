//! Deliberately not POSIX sh: quoting, escapes, comments and sequential commands.
//! No expansion, redirection, pipelines, job control or shell grammar.
use std::io::{self, BufRead, Write};
use std::process::Command;

pub fn split(line: &str) -> io::Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for c in line.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            started = true;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else {
            match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                '#' if !started => break,
                c if c.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                }
                _ => {
                    word.push(c);
                    started = true;
                }
            }
        }
    }
    if escaped || quote.is_some() {
        return Err(io::Error::other("unterminated quote or escape"));
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

fn execute(words: &[String]) -> io::Result<Option<i32>> {
    let Some(command) = words.first() else {
        return Ok(None);
    };
    match command.as_str() {
        "exit" => Ok(Some(
            words
                .get(1)
                .map(|v| v.parse())
                .transpose()
                .map_err(io::Error::other)?
                .unwrap_or(0),
        )),
        "cd" => {
            std::env::set_current_dir(words.get(1).map(String::as_str).unwrap_or("/"))?;
            Ok(None)
        }
        "export" => {
            for word in &words[1..] {
                let (key, value) = word
                    .split_once('=')
                    .ok_or_else(|| io::Error::other("export needs KEY=VALUE"))?;
                if key.is_empty() || key.contains('\0') || value.contains('\0') {
                    return Err(io::Error::other("invalid environment"));
                }
                std::env::set_var(key, value);
            }
            Ok(None)
        }
        _ => {
            let status = Command::new(command).args(&words[1..]).status()?;
            Ok(Some(status.code().unwrap_or(128)))
        }
    }
}

pub fn run(args: &[String]) -> io::Result<i32> {
    if args.first().is_some_and(|a| a == "-c") {
        if args.len() != 2 {
            return Err(io::Error::other("sh -c COMMAND"));
        }
        return Ok(execute(&split(&args[1])?)?.unwrap_or(0));
    }
    let interactive = args.is_empty();
    if args.len() > 1 {
        return Err(io::Error::other("sh [SCRIPT] | sh -c COMMAND"));
    }
    let mut input: Box<dyn BufRead> = if interactive {
        Box::new(io::BufReader::new(io::stdin()))
    } else {
        Box::new(io::BufReader::new(std::fs::File::open(&args[0])?))
    };
    let mut status = 0;
    loop {
        if interactive {
            print!("rbb# ");
            io::stdout().flush()?;
        }
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(status);
        }
        let words = match split(&line) {
            Ok(words) => words,
            Err(e) if interactive => {
                eprintln!("sh: {e}");
                continue;
            }
            Err(e) => return Err(e),
        };
        match execute(&words) {
            Ok(code) => {
                if let Some(code) = code {
                    status = code;
                }
                if words.first().is_some_and(|w| w == "exit") {
                    return Ok(status);
                }
            }
            Err(e) if interactive => {
                eprintln!("sh: {e}");
                status = 127;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quotes_and_escapes() {
        assert_eq!(
            split("echo 'a b' \"\" c\\ d # ignored").unwrap(),
            ["echo", "a b", "", "c d"]
        );
        assert!(split("echo '").is_err());
        assert!(split("echo \\").is_err());
    }
}

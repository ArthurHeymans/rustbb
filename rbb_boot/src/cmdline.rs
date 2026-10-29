use crate::invalid;
use std::collections::BTreeMap;
use std::io;

/// Linux-style key/value parameters, including double-quoted values. Last wins.
/// No shell expansion. Arguments after `--` belong to the real init.
#[derive(Debug, Default)]
pub struct Cmdline {
    pub values: BTreeMap<String, String>,
    pub init_args: Vec<String>,
}
impl Cmdline {
    pub fn parse(text: &str) -> io::Result<Self> {
        let mut result = Self::default();
        let mut quoted = false;
        let mut token = String::new();
        let mut tokens = Vec::new();
        for character in text.chars() {
            match character {
                '"' => quoted = !quoted,
                '\0' => return Err(invalid("NUL in kernel command line")),
                c if c.is_whitespace() && !quoted => {
                    if !token.is_empty() {
                        tokens.push(std::mem::take(&mut token));
                    }
                }
                c => token.push(c),
            }
        }
        if quoted {
            return Err(invalid("unterminated kernel command-line quote"));
        }
        if !token.is_empty() {
            tokens.push(token);
        }
        let mut init = false;
        for token in tokens {
            if init {
                result.init_args.push(token);
            } else if token == "--" {
                init = true;
            } else {
                let (key, value) = token.split_once('=').unwrap_or((&token, ""));
                result.values.insert(key.into(), value.into());
            }
        }
        Ok(result)
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
    pub fn contains(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_values_and_real_init_arguments() {
        let c = Cmdline::parse(
            "root=/dev/vda root=UUID=abc rootflags=\"ro,nodev\" quiet -- --verbose \"two words\"",
        )
        .unwrap();
        assert_eq!(c.get("root"), Some("UUID=abc"));
        assert_eq!(c.get("rootflags"), Some("ro,nodev"));
        assert!(c.contains("quiet"));
        assert_eq!(c.init_args, ["--verbose", "two words"]);
        assert!(Cmdline::parse("x=\"oops").is_err());
    }
}

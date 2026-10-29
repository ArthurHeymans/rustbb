//! Upstream's public entry owns its runtime/process. Use one exec-self to supply
//! real argv, POSIX mode and a supported input backend without rewriting source.
use brush_shell::args::CommandLineArgs;
use clap::{ArgAction, CommandFactory};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

// The -c/-- and +O preprocessing mirrors brush-shell 0.4's MIT-licensed
// entry.rs (https://github.com/reubeno/brush), copyright (c) 2024 reuben olinsky.
// Its MIT permission/copyright notice is retained in docs/third-party-notices.txt.
// The shared public parser does
// the actual flag/positional parsing; do not guess options from string matches.
fn options(name: &str, args: &[String]) -> Option<CommandLineArgs> {
    let mut scan: Vec<String> = std::iter::once(name.into())
        .chain(args.iter().cloned())
        .collect();
    for arg in &mut scan {
        if arg.starts_with("+O") {
            arg.insert_str(0, "--");
        }
    }
    if let Some(dd) = scan.iter().position(|s| s == "--") {
        let pending = dd.checked_sub(1).is_some_and(|i| {
            let Some(preceding) = scan[i].strip_prefix('-').and_then(|s| s.strip_suffix('c'))
            else {
                return false;
            };
            let command = CommandLineArgs::command();
            !preceding.starts_with('-')
                && preceding.chars().all(|ch| {
                    command.get_arguments().any(|a| {
                        a.get_short() == Some(ch)
                            && !matches!(a.get_action(), ArgAction::Set | ArgAction::Append)
                    })
                })
        });
        if pending {
            scan.remove(dd);
            if scan.get(dd).is_some_and(|s| s == "--") {
                scan.remove(dd);
                scan[dd - 1].push_str("--");
            }
        }
    }
    brush_core::builtins::try_parse_known::<CommandLineArgs>(scan)
        .ok()
        .map(|(args, _)| args)
}

pub fn run(args: &[String], sh_mode: bool) -> io::Result<i32> {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    let desired = if sh_mode { "sh" } else { "brush" };
    let parsed = options(desired, args);
    // Parse failures/help/version are left entirely to the upstream entry.
    let need_backend = parsed.as_ref().is_some_and(|p| p.input_backend.is_none());
    let need_sh = sh_mode && parsed.as_ref().is_some_and(|p| !p.sh_mode);
    if Path::new(&argv0).file_name() != Some(std::ffi::OsStr::new(desired))
        || need_backend
        || need_sh
    {
        let mut command = Command::new(std::env::current_exe()?);
        command.arg0(desired);
        if need_sh {
            command.arg("--sh");
        }
        if need_backend {
            command.args(["--input-backend", "minimal"]);
        }
        return Err(command.args(args).exec());
    }
    brush_shell::entry::run();
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn probe(args: &[&str]) -> CommandLineArgs {
        options("sh", &args.iter().map(|s| (*s).into()).collect::<Vec<_>>()).unwrap()
    }
    #[test]
    fn distinguish_backend_options_from_script_arguments() {
        assert!(probe(&["-c", "echo ok", "zero", "--input-backend"])
            .input_backend
            .is_none());
        assert!(
            probe(&["--no-config", "--input-backend=minimal", "-c", "echo ok"])
                .input_backend
                .is_some()
        );
        assert!(!probe(&["-c", "echo ok", "zero", "--sh"]).sh_mode);
        assert!(probe(&["--no-config", "--sh", "-c", "echo ok"]).sh_mode);
        assert!(probe(&["-ec", "--", "echo ok"]).input_backend.is_none());
        assert!(
            probe(&["-oc", "--input-backend", "minimal", "-c", "echo ok"])
                .input_backend
                .is_some()
        );
    }
}

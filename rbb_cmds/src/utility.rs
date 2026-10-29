//! Common uutils argv/localization initialization, including brush's POSIX printf.
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;

pub fn args(name: &str, args: &[String]) -> io::Result<std::vec::IntoIter<OsString>> {
    if std::env::args_os()
        .next()
        .as_deref()
        .and_then(|s| Path::new(s).file_name())
        != Some(OsStr::new(name))
    {
        uucore::set_utility_is_second_arg();
    }
    uucore::locale::setup_localization(if name == "[" { "test" } else { name })
        .map_err(io::Error::other)?;
    Ok(std::iter::once(OsString::from(name))
        .chain(args.iter().map(OsString::from))
        .collect::<Vec<_>>()
        .into_iter())
}

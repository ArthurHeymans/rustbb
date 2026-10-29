//! Direct uutils library dispatch: no rewriting, subprocess coreutils or native ACL/SELinux libraries.
use std::io;

pub fn run(name: &str, args: &[String]) -> io::Result<Option<i32>> {
    if !rbb_manifest::COREUTILS_EXTRA.contains(&name)
        && !["cat", "ls", "cp", "rm", "mkdir", "ln", "chmod", "readlink"].contains(&name)
    {
        return Ok(None);
    }
    let argv = crate::utility::args(name, args)?;
    let code = match name {
        "printf" => uu_printf::uumain(argv),
        "test" | "[" => uu_test::uumain(argv),
        "cat" => uu_cat::uumain(argv),
        "ls" => uu_ls::uumain(argv),
        "cp" => uu_cp::uumain(argv),
        "rm" => uu_rm::uumain(argv),
        "mkdir" => uu_mkdir::uumain(argv),
        "ln" => uu_ln::uumain(argv),
        "chmod" => uu_chmod::uumain(argv),
        "readlink" => uu_readlink::uumain(argv),
        "head" => uu_head::uumain(argv),
        "tail" => uu_tail::uumain(argv),
        "wc" => uu_wc::uumain(argv),
        "cut" => uu_cut::uumain(argv),
        "sort" => uu_sort::uumain(argv),
        "uniq" => uu_uniq::uumain(argv),
        "tee" => uu_tee::uumain(argv),
        "tr" => uu_tr::uumain(argv),
        "env" => uu_env::uumain(argv),
        "printenv" => uu_printenv::uumain(argv),
        "mktemp" => uu_mktemp::uumain(argv),
        "timeout" => uu_timeout::uumain(argv),
        "dd" => uu_dd::uumain(argv),
        "sync" => uu_sync::uumain(argv),
        "mknod" => uu_mknod::uumain(argv),
        "mkfifo" => uu_mkfifo::uumain(argv),
        "touch" => uu_touch::uumain(argv),
        "sha256sum" => uu_sha256sum::uumain(argv),
        _ => unreachable!("manifest/dispatch drift"),
    };
    Ok(Some(code))
}

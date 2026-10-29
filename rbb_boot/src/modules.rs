//! Dependency-aware module loading without libkmod or an external modprobe.
use crate::{device::kernel_release, invalid};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};

fn module_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .split(".ko")
        .next()
        .unwrap_or("")
        .replace('-', "_")
}
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

#[derive(Debug)]
pub struct ModuleIndex {
    pub deps: BTreeMap<String, Vec<String>>,
    pub names: BTreeMap<String, String>,
    aliases: Vec<(glob::Pattern, String)>,
}
impl ModuleIndex {
    pub fn parse(dependencies: &str, aliases: &str) -> io::Result<Self> {
        let mut result = Self {
            deps: BTreeMap::new(),
            names: BTreeMap::new(),
            aliases: Vec::new(),
        };
        for line in dependencies.lines().filter(|l| !l.trim().is_empty()) {
            let (module, deps) = line
                .split_once(':')
                .ok_or_else(|| invalid("invalid modules.dep line"))?;
            let module = module.trim();
            let deps: Vec<String> = deps.split_whitespace().map(String::from).collect();
            if !safe_path(module) || deps.iter().any(|d| !safe_path(d)) {
                return Err(invalid("unsafe modules.dep path"));
            }
            let name = module_name(module);
            if result.names.insert(name, module.into()).is_some()
                || result.deps.insert(module.into(), deps).is_some()
            {
                return Err(invalid("duplicate module name or path"));
            }
        }
        for line in aliases
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let words: Vec<_> = line.split_whitespace().collect();
            if let ["alias", pattern, name] = words.as_slice() {
                result.aliases.push((
                    glob::Pattern::new(pattern).map_err(io::Error::other)?,
                    name.replace('-', "_"),
                ));
            }
        }
        Ok(result)
    }

    pub fn resolve(&self, name: &str) -> io::Result<Vec<String>> {
        let normalized = name.replace('-', "_");
        if let Some(path) = self.names.get(&normalized) {
            return Ok(vec![path.clone()]);
        }
        let matches: BTreeSet<_> = self
            .aliases
            .iter()
            .filter(|(pattern, _)| pattern.matches(name))
            .filter_map(|(_, name)| self.names.get(name).cloned())
            .collect();
        if matches.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("module/alias not found: {name}"),
            ));
        }
        Ok(matches.into_iter().collect())
    }

    pub fn order(&self, paths: &[String]) -> io::Result<Vec<String>> {
        fn visit(
            index: &ModuleIndex,
            path: &str,
            active: &mut BTreeSet<String>,
            done: &mut BTreeSet<String>,
            order: &mut Vec<String>,
        ) -> io::Result<()> {
            if done.contains(path) {
                return Ok(());
            }
            if !active.insert(path.into()) {
                return Err(invalid(format!("module dependency cycle: {path}")));
            }
            let deps = index
                .deps
                .get(path)
                .ok_or_else(|| invalid(format!("unknown module dependency: {path}")))?;
            for dep in deps {
                visit(index, dep, active, done, order)?;
            }
            active.remove(path);
            done.insert(path.into());
            order.push(path.into());
            Ok(())
        }
        let mut result = Vec::new();
        let (mut active, mut done) = (BTreeSet::new(), BTreeSet::new());
        for path in paths {
            visit(self, path, &mut active, &mut done, &mut result)?;
        }
        Ok(result)
    }
}

pub fn insert(path: &Path, parameters: &str) -> io::Result<()> {
    let original = File::open(path)?;
    let suffix = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let file = if ["gz", "xz"].contains(&suffix) {
        let mut reader: Box<dyn Read> = if suffix == "gz" {
            Box::new(flate2::read::GzDecoder::new(original))
        } else {
            Box::new(lzma_rust2::XzReader::new(original, false))
        };
        let name = CString::new("rbb-module").unwrap();
        let fd =
            nix::sys::memfd::memfd_create(name.as_c_str(), nix::sys::memfd::MFdFlags::MFD_CLOEXEC)
                .map_err(io::Error::from)?;
        let mut file = File::from(fd);
        let bytes = io::copy(&mut reader.by_ref().take(64 * 1024 * 1024 + 1), &mut file)?;
        if bytes > 64 * 1024 * 1024 {
            return Err(invalid("decompressed kernel module exceeds 64MiB"));
        }
        file.flush()?;
        file
    } else if suffix == "ko" {
        original
    } else {
        return Err(invalid(
            "expected uncompressed .ko, .ko.gz or .ko.xz module",
        ));
    };
    let parameters = CString::new(parameters).map_err(io::Error::other)?;
    // SAFETY: valid owned fd, NUL-terminated parameters, no unsupported flags.
    if unsafe {
        libc::syscall(
            libc::SYS_finit_module,
            file.as_raw_fd(),
            parameters.as_ptr(),
            0,
        )
    } == -1
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(error);
        }
    }
    Ok(())
}

pub fn load_from(directory: &Path, name: &str, parameters: &str) -> io::Result<()> {
    let dependencies = fs::read_to_string(directory.join("modules.dep"))?;
    let aliases = fs::read_to_string(directory.join("modules.alias")).unwrap_or_default();
    if dependencies.len() > 16 * 1024 * 1024 || aliases.len() > 32 * 1024 * 1024 {
        return Err(invalid("module metadata exceeds limit"));
    }
    let index = ModuleIndex::parse(&dependencies, &aliases)?;
    let targets = index.resolve(name)?;
    let root = fs::canonicalize(directory)?;
    for module in index.order(&targets)? {
        let path = fs::canonicalize(directory.join(&module))?;
        if !path.starts_with(&root) {
            return Err(invalid("module symlink escapes module directory"));
        }
        insert(
            &path,
            if targets.contains(&module) {
                parameters
            } else {
                ""
            },
        )?;
    }
    Ok(())
}

pub fn load(name: &str, parameters: &str) -> io::Result<()> {
    load_from(
        &PathBuf::from("/lib/modules").join(kernel_release()?),
        name,
        parameters,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependency_order_and_aliases() {
        let index = ModuleIndex::parse(
            "kernel/base.ko:\nkernel/child.ko.xz: kernel/base.ko\n",
            "alias pci:v00001234* child\n",
        )
        .unwrap();
        assert_eq!(
            index
                .order(&index.resolve("pci:v00001234ABCD").unwrap())
                .unwrap(),
            ["kernel/base.ko", "kernel/child.ko.xz"]
        );
        assert!(ModuleIndex::parse("../bad.ko:\n", "").is_err());
        let cycle = ModuleIndex::parse("a.ko: b.ko\nb.ko: a.ko\n", "").unwrap();
        assert!(cycle.order(&["a.ko".into()]).is_err());
    }
}

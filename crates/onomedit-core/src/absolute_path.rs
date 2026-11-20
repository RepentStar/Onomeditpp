use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::journal::SEPARATOR;
use crate::native_move::{Identity, entry_identity, identity, is_redirect};

pub(crate) fn row_error(
    line: usize,
    old: &Path,
    new: &Path,
    error: impl std::fmt::Display,
) -> String {
    let display = |path: &Path| match path.to_str() {
        Some(path)
            if !path
                .contains(|ch: char| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}')) =>
        {
            format!("\"{path}\"")
        }
        _ => format!("{path:?}"),
    };
    format!(
        "第 {line} 行: {} -> {}: {error}",
        display(old),
        display(new)
    )
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn check_text(path: &Path) -> io::Result<&str> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("路径无法编码为 UTF-8"))?;
    if text.contains([
        '\0', '\r', '\n', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ]) {
        return Err(invalid("路径包含行分隔符或 NUL"));
    }
    if text.contains(SEPARATOR) {
        return Err(invalid(format!("路径包含日志分隔符 {SEPARATOR}")));
    }
    Ok(text)
}

pub(crate) fn check_name_segment(name: &str) -> io::Result<()> {
    check_text(Path::new(name))?;
    if name.contains(std::path::is_separator) {
        return Err(invalid("名称段包含路径分隔符"));
    }
    Ok(())
}

pub(crate) fn check_source_input(path: &Path) -> io::Result<()> {
    let _text = check_text(path)?;
    #[cfg(windows)]
    {
        let text = _text.replace('/', "\\");
        if text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
            return Err(invalid("路径使用了 Windows 设备或扩展命名空间"));
        }
        let rest = if matches!(text.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic()) {
            &text[2..]
        } else {
            &text
        };
        if rest.contains(':') {
            return Err(invalid("路径包含 Windows 备用数据流分隔符"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn check_windows_name(name: &str) -> io::Result<()> {
    if name.ends_with(['.', ' '])
        || crate::safe_name::is_windows_device_name(name)
        || name
            .contains(|ch| ch <= '\u{1f}' || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(invalid(format!("无效的 Windows 路径段 {name:?}")));
    }
    Ok(())
}

#[cfg(windows)]
fn windows_root(text: &str) -> io::Result<usize> {
    use std::path::Prefix;

    let normalized = text.replace('/', "\\");
    let Some(Component::Prefix(prefix)) = Path::new(&normalized).components().next() else {
        return Err(invalid("请填写完整的绝对路径和名称"));
    };
    match prefix.kind() {
        Prefix::Disk(_) => {}
        Prefix::UNC(server, share) => {
            for part in [server, share] {
                check_windows_name(part.to_str().expect("UTF-8 path"))?;
            }
        }
        _ => return Err(invalid("路径使用了 Windows 设备或扩展命名空间")),
    }
    let end = prefix.as_os_str().len();
    if normalized
        .as_bytes()
        .get(end)
        .is_some_and(|&byte| byte != b'\\')
    {
        return Err(invalid("请填写完整的绝对路径和名称"));
    }
    Ok(end)
}

pub(crate) fn target(path: &Path, final_name: bool) -> io::Result<PathBuf> {
    let text = check_text(path)?;
    #[cfg(windows)]
    let rest = &text[windows_root(text)?..];
    #[cfg(not(windows))]
    let rest = text;
    if !rest.starts_with(std::path::is_separator) || rest.ends_with(std::path::is_separator) {
        return Err(invalid("请填写完整的绝对路径和名称"));
    }
    let (parent, name) = rest
        .rsplit_once(std::path::is_separator)
        .expect("root separator");
    if name == "."
        || name == ".."
        || parent
            .split(std::path::is_separator)
            .any(|part| part == "..")
    {
        return Err(invalid("路径包含 .. 段或以 . 结尾"));
    }
    #[cfg(windows)]
    {
        if rest.contains(':') {
            return Err(invalid("路径包含 Windows 备用数据流分隔符"));
        }
        for part in parent
            .split(std::path::is_separator)
            .filter(|part| !matches!(*part, "" | "."))
        {
            check_windows_name(part)?;
        }
        if final_name {
            check_windows_name(name)?;
        }
    }
    #[cfg(not(windows))]
    let _ = final_name;
    Ok(path.components().collect())
}

pub(crate) fn lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            part => normalized.push(part.as_os_str()),
        }
    }
    normalized
}

fn absolute_source(path: &Path, cwd: &Path) -> io::Result<PathBuf> {
    check_text(path)?;
    // Preserve the path used for lookup until both the parent and the entry are checked.
    let raw = cwd.join(path);
    let text = check_text(&raw)?;
    #[cfg(windows)]
    {
        let rest = &text[windows_root(text)?..];
        for part in rest
            .split(std::path::is_separator)
            .filter(|part| !matches!(*part, "" | "." | ".."))
        {
            check_windows_name(part)?;
        }
    }
    let normalized = lexical(&raw);
    let Some(name) = normalized.file_name() else {
        return Err(invalid("根目录不能作为移动源"));
    };
    let text = text.trim_end_matches(std::path::is_separator);
    let (raw_parent, raw_name) = text
        .rsplit_once(std::path::is_separator)
        .ok_or_else(|| invalid("请填写完整的绝对路径和名称"))?;
    let raw_entry = Path::new(text);
    let normalized_parent = normalized.parent().expect("absolute parent");
    let same = if raw_entry.as_os_str() == normalized.as_os_str() {
        true
    } else if matches!(raw_name, "." | "..") {
        fs::metadata(raw_entry)?.is_dir()
            && identity(raw_entry, true)? == identity(&normalized, false)?
            && !is_redirect(&fs::symlink_metadata(&normalized)?)
    } else {
        // Retain the separator for POSIX and Windows drive roots.
        let raw_parent = Path::new(&text[..raw_parent.len() + 1]);
        raw_name == name
            && identity(raw_parent, true)? == identity(normalized_parent, true)?
            && identity(raw_entry, false)? == identity(&normalized, false)?
    };
    if !same {
        return Err(invalid("无法确认规范化前后的源目录项相同"));
    }
    target(&normalized, true)
}

pub(crate) fn key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = path.to_str() {
        return PathBuf::from(path.replace('/', "\\").to_lowercase());
    }
    path.to_owned()
}

pub(crate) fn physical(path: &Path) -> io::Result<PathBuf> {
    let resolved = fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        if let Some(path) = resolved.to_str() {
            if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
                return Ok(PathBuf::from(format!(r"\\{unc}")));
            }
            if let Some(disk) = path.strip_prefix(r"\\?\").filter(|disk| {
                matches!(disk.as_bytes(), [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic())
            }) {
                return Ok(PathBuf::from(disk));
            }
            return Err(invalid("无法将父目录转换为常规绝对路径"));
        }
    }
    Ok(resolved)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectoryIdentity {
    entry: Identity,
    directory: Identity,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Parent {
    pub resolved: PathBuf,
    pub existing: Vec<(PathBuf, DirectoryIdentity)>,
    pub missing: Vec<OsString>,
    aliases: HashSet<PathBuf>,
}

impl Parent {
    pub fn read(path: &Path) -> io::Result<Self> {
        let mut existing = Vec::new();
        let mut missing = Vec::new();
        let mut aliases = HashSet::new();
        let mut prefix = PathBuf::new();
        for part in path.components() {
            let previous = prefix.clone();
            prefix.push(part.as_os_str());
            if matches!(part, Component::Prefix(_)) {
                continue;
            }
            if !missing.is_empty() {
                missing.push(part.as_os_str().to_owned());
                continue;
            }
            let Some(entry) = entry_identity(&prefix)? else {
                if existing.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{prefix:?}"),
                    ));
                }
                missing.push(part.as_os_str().to_owned());
                continue;
            };
            let metadata = fs::symlink_metadata(&prefix)?;
            let (directory, is_directory) = if is_redirect(&metadata) {
                if !existing.is_empty() {
                    aliases.insert(physical(&previous)?.join(part.as_os_str()));
                }
                (identity(&prefix, true)?, fs::metadata(&prefix)?.is_dir())
            } else {
                (entry, metadata.is_dir())
            };
            if !is_directory {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{prefix:?}"),
                ));
            }
            existing.push((prefix.clone(), DirectoryIdentity { entry, directory }));
        }
        let (anchor, _) = existing
            .last()
            .ok_or_else(|| invalid("父路径必须为绝对目录"))?;
        let resolved = physical(anchor)?
            .iter()
            .chain(missing.iter().map(OsString::as_os_str))
            .collect();
        Ok(Self {
            resolved,
            existing,
            missing,
            aliases,
        })
    }

    pub fn device(&self) -> u64 {
        self.existing
            .last()
            .expect("parent root exists")
            .1
            .directory
            .device
    }

    pub fn child_key(&self, path: &Path) -> PathBuf {
        key(&self
            .resolved
            .join(path.file_name().expect("validated file name")))
    }

    pub fn ancestors<'a>(&'a self, lexical: &'a Path) -> impl Iterator<Item = PathBuf> + 'a {
        self.resolved
            .ancestors()
            .map(key)
            .chain(lexical.ancestors().map(key))
            .chain(
                self.aliases
                    .iter()
                    .flat_map(|path| path.ancestors().map(key)),
            )
    }
}

pub(crate) struct Source {
    pub path: PathBuf,
    pub parent: Parent,
    pub identity: Identity,
}

impl Source {
    pub fn read(path: &Path, cwd: &Path) -> io::Result<Self> {
        let path = absolute_source(path, cwd)?;
        let parent = Parent::read(path.parent().expect("absolute source"))?;
        let identity = identity(&path, false)?;
        Ok(Self {
            path,
            parent,
            identity,
        })
    }

    pub fn key(&self) -> PathBuf {
        self.parent.child_key(&self.path)
    }

    pub fn check_entry(&self, path: &Path) -> io::Result<()> {
        if entry_identity(path)? == Some(self.identity) {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "源目录项已变化或不存在: {path:?}"
        )))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn absolute_windows_paths_validate_drive_unc_and_unsafe_spellings() {
        for invalid in [
            r"C:relative.txt",
            r"\current-drive.txt",
            r"C:\",
            r"\\server\share",
            r"\\server\share\",
            r"\\?\C:\path\a.txt",
            r"\\.\C:\path\a.txt",
            r"C:\folder\a.txt:stream",
            r"C:\folder.\a.txt",
            r"C:\folder \a.txt",
            r"C:\CONIN$\a.txt",
            r"C:\COM¹\a.txt",
            r"C:\folder\..\a.txt",
            r"C:\folder\.",
        ] {
            assert!(target(Path::new(invalid), false).is_err(), "{invalid}");
        }
        assert_eq!(
            target(Path::new(r"C:/folder//./file.txt"), true).unwrap(),
            Path::new(r"C:\folder\file.txt")
        );
        assert_eq!(
            target(Path::new(r"\\server\share\folder\file.txt"), true).unwrap(),
            Path::new(r"\\server\share\folder\file.txt")
        );
        assert!(target(Path::new(r"C:\folder\NUL.txt"), false).is_ok());
        assert!(target(Path::new(r"C:\folder\NUL.txt"), true).is_err());
    }
}

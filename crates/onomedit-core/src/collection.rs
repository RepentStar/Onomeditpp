use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead};
use std::path::{Component, Path, PathBuf};

use glob::{MatchOptions, glob_with};

use crate::config::ExcludeOptions;
use crate::path::PathItem;

pub const SORT_BY_CHOICES: [&str; 6] = ["default", "name", "path", "mtime", "ctime", "size"];

pub fn read_stream_paths(reader: impl BufRead) -> io::Result<Vec<String>> {
    reader
        .lines()
        .map(|line| line.map(|value| value.trim().to_owned()))
        .filter(|line| line.as_ref().is_ok_and(|value| !value.is_empty()))
        .collect()
}

/// With `recursive`, insert `**` before the final glob segment unless already present.
/// Explicit file and directory paths are left unchanged.
pub fn collect_paths(raw_paths: &[String], recursive: bool) -> Vec<PathBuf> {
    let options = MatchOptions {
        case_sensitive: !cfg!(windows),
        require_literal_separator: false,
        require_literal_leading_dot: true,
    };
    let mut output = Vec::new();
    for raw in raw_paths {
        if has_magic(raw) {
            let pattern = if recursive
                && !Path::new(raw)
                    .components()
                    .any(|part| part.as_os_str() == "**")
            {
                let (parent, name) = raw
                    .rfind(std::path::is_separator)
                    .map_or(("", raw.as_str()), |index| raw.split_at(index + 1));
                format!("{parent}**/{name}")
            } else {
                raw.clone()
            };
            if let Ok(matches) = glob_with(&pattern, options) {
                output.extend(matches.filter_map(Result::ok).filter(|path| path.exists()));
            }
        } else {
            let path = PathBuf::from(raw);
            if path.exists() {
                output.push(path);
            }
        }
    }
    output
}

fn has_magic(path: &str) -> bool {
    path.contains(['*', '?', '['])
}

pub fn expand_subdirs(items: Vec<PathItem>, depth: i32) -> Vec<PathItem> {
    if depth <= 0 {
        return items;
    }
    let mut output = Vec::new();
    for item in items {
        if item.full().is_dir() {
            expand_directory(item.full(), 1, depth, &mut output);
        } else {
            output.push(item);
        }
    }
    output
}

fn expand_directory(directory: &Path, level: i32, depth: i32, output: &mut Vec<PathItem>) {
    if level > depth {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut directories = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            directories.push(path);
        } else {
            files.push(path);
        }
    }
    output.extend(directories.iter().cloned().map(PathItem::new));
    output.extend(files.into_iter().map(PathItem::new));
    for child in directories {
        expand_directory(&child, level + 1, depth, output);
    }
}

pub fn dedupe_items(items: Vec<PathItem>) -> Vec<PathItem> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(path_key(item.full())))
        .collect()
}

pub fn path_key(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    let key = normalized.to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

pub fn apply_excludes(items: Vec<PathItem>, exclude: &ExcludeOptions) -> Vec<PathItem> {
    items
        .into_iter()
        .filter(|item| {
            let path = item.full();
            !(exclude.files && path.is_file()
                || exclude.dirs && path.is_dir()
                || exclude.symlinks && path.is_symlink()
                || exclude.readonly && is_readonly(path)
                || exclude.hidden && is_hidden(path)
                || exclude.system && is_system(path))
        })
        .collect()
}

pub fn sort_items(mut items: Vec<PathItem>, sort_by: &str, reverse: bool) -> Vec<PathItem> {
    if sort_by == "default" || !SORT_BY_CHOICES.contains(&sort_by) {
        if reverse {
            items.reverse();
        }
        return items;
    }
    items.sort_by(|left, right| compare(left, right, sort_by));
    if reverse {
        items.reverse();
    }
    items
}

fn compare(left: &PathItem, right: &PathItem, sort_by: &str) -> Ordering {
    match sort_by {
        "name" => string_key(&left.name()).cmp(&string_key(&right.name())),
        "path" => path_key(left.full()).cmp(&path_key(right.full())),
        "mtime" => metadata_time(left.full(), true).total_cmp(&metadata_time(right.full(), true)),
        "ctime" => metadata_time(left.full(), false).total_cmp(&metadata_time(right.full(), false)),
        "size" => metadata_size(left.full()).cmp(&metadata_size(right.full())),
        _ => Ordering::Equal,
    }
}

fn string_key(value: &str) -> String {
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value.into()
    }
}

fn metadata_time(path: &Path, modified: bool) -> f64 {
    let Ok(metadata) = fs::metadata(path) else {
        return 0.0;
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (seconds, nanoseconds) = if modified {
            (metadata.mtime(), metadata.mtime_nsec())
        } else {
            (metadata.ctime(), metadata.ctime_nsec())
        };
        seconds as f64 + nanoseconds as f64 / 1_000_000_000.0
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const WINDOWS_TO_UNIX_EPOCH_TICKS: i128 = 116_444_736_000_000_000;
        let ticks = if modified {
            metadata.last_write_time()
        } else {
            metadata.creation_time()
        };
        (i128::from(ticks) - WINDOWS_TO_UNIX_EPOCH_TICKS) as f64 / 10_000_000.0
    }

    #[cfg(not(any(unix, windows)))]
    {
        use std::time::UNIX_EPOCH;
        let time = if modified {
            metadata.modified()
        } else {
            metadata.created()
        };
        time.ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map_or(0.0, |duration| duration.as_secs_f64())
    }
}

fn metadata_size(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

pub fn display_base(paths: &[String]) -> PathBuf {
    if paths.is_empty() {
        return PathBuf::new();
    }
    let absolute: Vec<PathBuf> = paths
        .iter()
        .map(|path| {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        })
        .collect();
    let mut common: Vec<_> = absolute[0].components().collect();
    for path in &absolute[1..] {
        let components: Vec<_> = path.components().collect();
        let length = common
            .iter()
            .zip(&components)
            .take_while(|(a, b)| a == b)
            .count();
        common.truncate(length);
    }
    let mut base: PathBuf = common
        .iter()
        .map(|component| component.as_os_str())
        .collect();
    if paths.len() == 1 {
        base.pop();
    }
    base
}

#[cfg(windows)]
fn attributes(path: &Path) -> u32 {
    use std::os::windows::fs::MetadataExt;
    fs::metadata(path)
        .map(|metadata| metadata.file_attributes())
        .unwrap_or(0)
}

#[cfg(windows)]
fn is_readonly(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().readonly())
}

#[cfg(unix)]
fn is_readonly(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    CString::new(path.as_os_str().as_bytes()).is_ok_and(|path| {
        // SAFETY: CString guarantees a valid NUL-terminated pointer for the duration of the call.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) != 0 }
    })
}

#[cfg(not(any(unix, windows)))]
fn is_readonly(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.permissions().readonly())
}

fn is_hidden(path: &Path) -> bool {
    #[cfg(windows)]
    return attributes(path) & 0x2 != 0;
    #[cfg(not(windows))]
    return path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'));
}

fn is_system(path: &Path) -> bool {
    #[cfg(windows)]
    return attributes(path) & 0x4 != 0;
    #[cfg(not(windows))]
    return false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_globs_match_reports_beyond_directory_expansion_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("test_files");
        let deep = (0..12).fold(root.clone(), |path, level| {
            path.join(format!("level{level}"))
        });
        fs::create_dir_all(&deep).unwrap();
        let expected = vec![root.join("report1.docx"), deep.join("report2.docx")];
        for path in expected
            .iter()
            .chain([deep.join("other.docx"), deep.join("report3.txt")].iter())
        {
            fs::write(path, "content").unwrap();
        }
        for pattern in ["report*.docx", "**/report*.docx"] {
            let paths = collect_paths(&[root.join(pattern).to_string_lossy().into_owned()], true);
            assert_eq!(paths.len(), expected.len());
            assert_eq!(
                paths.into_iter().collect::<HashSet<_>>(),
                HashSet::from_iter(expected.clone())
            );
        }
        assert_eq!(
            collect_paths(
                &[root.join("report*.docx").to_string_lossy().into_owned()],
                false
            ),
            vec![expected[0].clone()]
        );
    }

    #[test]
    fn recursive_relative_globs_support_wildcard_parents() {
        let dir = tempfile::tempdir_in(".").unwrap();
        let root = Path::new(dir.path().file_name().unwrap());
        fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        for name in [
            "a.txt",
            "sub/b.txt",
            "sub/deep/c.txt",
            "sub/deep/other.docx",
        ] {
            fs::write(dir.path().join(name), "content").unwrap();
        }
        for pattern in ["*.txt", "sub/*.txt", "s?b/[bc].txt"] {
            let paths = collect_paths(&[root.join(pattern).to_string_lossy().into_owned()], true);
            let names: HashSet<_> = paths
                .iter()
                .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            let expected = if pattern == "*.txt" {
                HashSet::from(["a.txt".into(), "b.txt".into(), "c.txt".into()])
            } else {
                HashSet::from(["b.txt".into(), "c.txt".into()])
            };
            assert_eq!(names, expected, "{pattern}");
        }
    }

    #[test]
    fn recursive_collection_preserves_explicit_paths_and_ignores_missing_matches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let sub = dir.path().join("sub");
        fs::write(&file, "content").unwrap();
        fs::create_dir(&sub).unwrap();
        let paths = vec![file, sub];
        let inputs: Vec<_> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(collect_paths(&inputs, true), paths);
        assert!(
            collect_paths(
                &[dir
                    .path()
                    .join("missing*.docx")
                    .to_string_lossy()
                    .into_owned()],
                true
            )
            .is_empty()
        );
    }

    #[test]
    fn expands_with_depth_and_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("b.txt"), "b").unwrap();
        let depth_one = expand_subdirs(vec![PathItem::new(dir.path())], 1);
        assert!(depth_one.iter().any(|item| item.name() == "a.txt"));
        assert!(depth_one.iter().any(|item| item.name() == "sub"));
        assert!(!depth_one.iter().any(|item| item.name() == "b.txt"));
        let duplicate = PathItem::new(dir.path().join("a.txt"));
        assert_eq!(dedupe_items(vec![duplicate.clone(), duplicate]).len(), 1);
    }
}

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Identity {
    pub device: u64,
    inode: u64,
    kind: u32,
}

pub(crate) fn is_redirect(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    metadata.is_symlink()
}

#[cfg(not(windows))]
fn metadata(path: &Path, follow: bool) -> io::Result<Metadata> {
    if follow {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
}

#[cfg(unix)]
pub(crate) fn identity(path: &Path, follow: bool) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = metadata(path, follow)?;
    Ok(Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
        kind: metadata.mode() & 0o170000,
    })
}

#[cfg(windows)]
pub(crate) fn identity(path: &Path, follow: bool) -> io::Result<Identity> {
    let mut options = OpenOptions::new();
    windows::flags(&mut options, follow);
    use std::os::windows::fs::OpenOptionsExt;
    options.access_mode(0);
    let info = windows::file_info(&options.open(path)?)?;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    Ok(Identity {
        device: info.dwVolumeSerialNumber.into(),
        inode: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        kind: info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT),
    })
}

#[cfg(not(any(windows, unix)))]
pub(crate) fn identity(path: &Path, follow: bool) -> io::Result<Identity> {
    let metadata = metadata(path, follow)?;
    Ok(Identity {
        device: 0,
        inode: metadata.len(),
        kind: u32::from(metadata.is_dir()),
    })
}

pub(crate) fn entry_identity(path: &Path) -> io::Result<Option<Identity>> {
    match identity(path, false) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        entry => entry.map(Some),
    }
}

pub(crate) fn check_log_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    #[cfg(unix)]
    let links = std::os::unix::fs::MetadataExt::nlink(&metadata);
    #[cfg(windows)]
    let links = windows::file_info(file)?.nNumberOfLinks;
    #[cfg(not(any(windows, unix)))]
    let links = 1;
    if !metadata.is_file() || is_redirect(&metadata) || links != 1 {
        return Err(io::Error::other("活动日志必须是独立的普通文件"));
    }
    Ok(())
}

pub(crate) fn log_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        windows::flags(options, false);
        options.share_mode(FILE_SHARE_READ);
    }
}

pub(crate) fn lock_log(_file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the borrowed file keeps the descriptor open throughout the call.
        if unsafe { libc::flock(_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) fn check_support() -> io::Result<()> {
    if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
        Ok(())
    } else {
        Err(unsupported())
    }
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "系统或文件系统不支持无覆盖重命名",
    )
}

pub(crate) fn occupied(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
    )
}

pub(crate) fn rename_no_replace(old: &Path, new: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let old = windows::wide_path(old)?;
        let new = windows::wide_path(new)?;
        // SAFETY: both buffers are NUL-terminated. Zero flags forbid replacement and copying.
        if unsafe { MoveFileExW(old.as_ptr(), new.as_ptr(), 0) } != 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let old = CString::new(old.as_os_str().as_bytes())?;
        let new = CString::new(new.as_os_str().as_bytes())?;
        // SAFETY: the CStrings own valid NUL-terminated path buffers for the entire call.
        let status = unsafe {
            #[cfg(target_os = "linux")]
            {
                libc::syscall(
                    libc::SYS_renameat2,
                    libc::AT_FDCWD,
                    old.as_ptr(),
                    libc::AT_FDCWD,
                    new.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            }
            #[cfg(target_os = "macos")]
            {
                libc::renamex_np(old.as_ptr(), new.as_ptr(), libc::RENAME_EXCL)
            }
        };
        if status == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS | libc::ENOTSUP | libc::EINVAL)
        ) {
            return Err(unsupported());
        }
        Err(error)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (old, new);
        Err(unsupported())
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    pub(super) fn flags(options: &mut OpenOptions, follow: bool) {
        let flags = FILE_FLAG_BACKUP_SEMANTICS
            | if follow {
                0
            } else {
                FILE_FLAG_OPEN_REPARSE_POINT
            };
        options.custom_flags(flags);
    }

    pub(super) fn file_info(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the file owns a live handle and info is valid writable storage.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info)
    }

    pub(super) fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let path = path
            .to_str()
            .ok_or_else(|| io::Error::other("路径无法编码为 UTF-8"))?;
        let path = if let Some(unc) = path.strip_prefix(r"\\") {
            format!(r"\\?\UNC\{unc}")
        } else {
            format!(r"\\?\{path}")
        };
        Ok(path.encode_utf16().chain([0]).collect())
    }
}

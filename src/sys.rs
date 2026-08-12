use std::fs::{DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io::Result;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

pub fn set_secure_dir_permissions(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(_path, Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn set_secure_file_permissions(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(_path, Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn set_permissions_mode(_path: &Path, _mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(_path, Permissions::from_mode(_mode))?;
    }
    Ok(())
}

pub fn set_open_options_mode(options: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        options.mode(_mode);
    }
    options
}

pub fn set_dir_builder_mode(builder: &mut DirBuilder, _mode: u32) -> &mut DirBuilder {
    #[cfg(unix)]
    {
        builder.mode(_mode);
    }
    builder
}

pub fn get_permissions_mode(permissions: &Permissions) -> u32 {
    #[cfg(unix)]
    {
        permissions.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        if permissions.readonly() {
            0o444
        } else {
            0o644
        }
    }
}

pub fn is_executable(metadata: &Metadata, _path: Option<&Path>) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        permissions_is_executable(&metadata.permissions())
    }
    #[cfg(not(unix))]
    {
        if let Some(p) = _path {
            if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                let ext_lower = ext.to_lowercase();
                matches!(ext_lower.as_str(), "exe" | "cmd" | "bat" | "ps1" | "com")
            } else {
                false
            }
        } else {
            true
        }
    }
}

pub fn permissions_is_executable(_permissions: &Permissions) -> bool {
    #[cfg(unix)]
    {
        _permissions.mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        !_permissions.readonly()
    }
}

pub fn flock_lock_ex(file: &File, non_blocking: bool) -> Result<bool> {
    #[cfg(unix)]
    {
        let flags = if non_blocking {
            libc::LOCK_EX | libc::LOCK_NB
        } else {
            libc::LOCK_EX
        };
        let ret = unsafe { libc::flock(file.as_raw_fd(), flags) };
        if ret == 0 {
            Ok(true)
        } else {
            let err = std::io::Error::last_os_error();
            if non_blocking
                && (err.kind() == std::io::ErrorKind::WouldBlock
                    || err.raw_os_error() == Some(libc::EWOULDBLOCK))
            {
                Ok(false)
            } else {
                Err(err)
            }
        }
    }
    #[cfg(windows)]
    {
        win_lock::flock_lock_ex(file, non_blocking)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, non_blocking);
        Ok(true)
    }
}

pub fn flock_unlock(file: &File) {
    #[cfg(unix)]
    {
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
    }
    #[cfg(windows)]
    {
        win_lock::flock_unlock(file);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
    }
}

#[cfg(windows)]
mod win_lock {
    use std::fs::File;
    use std::io::{Error, Result};
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct OVERLAPPED {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: usize,
    }

    extern "system" {
        fn LockFileEx(
            hFile: usize,
            dwFlags: u32,
            dwReserved: u32,
            nNumberOfBytesToLockLow: u32,
            nNumberOfBytesToLockHigh: u32,
            lpOverlapped: *mut OVERLAPPED,
        ) -> i32;

        fn UnlockFileEx(
            hFile: usize,
            dwReserved: u32,
            nNumberOfBytesToLockLow: u32,
            nNumberOfBytesToLockHigh: u32,
            lpOverlapped: *mut OVERLAPPED,
        ) -> i32;
    }

    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x00000002;
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x00000001;

    pub fn flock_lock_ex(file: &File, non_blocking: bool) -> Result<bool> {
        let handle = file.as_raw_handle() as usize;
        let mut overlapped = OVERLAPPED {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: 0,
        };
        let flags =
            LOCKFILE_EXCLUSIVE_LOCK | if non_blocking { LOCKFILE_FAIL_IMMEDIATELY } else { 0 };
        let res = unsafe { LockFileEx(handle, flags, 0, 1, 0, &mut overlapped) };
        if res != 0 {
            Ok(true)
        } else {
            let err = Error::last_os_error();
            // Try shared lock if exclusive lock failed due to read-only handle permissions (OS error 5)
            let shared_flags = flags & !LOCKFILE_EXCLUSIVE_LOCK;
            let shared_res = unsafe { LockFileEx(handle, shared_flags, 0, 1, 0, &mut overlapped) };
            if shared_res != 0 {
                return Ok(true);
            }
            if non_blocking {
                Ok(false)
            } else {
                Err(err)
            }
        }
    }

    pub fn flock_unlock(file: &File) {
        let handle = file.as_raw_handle() as usize;
        let mut overlapped = OVERLAPPED {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: 0,
        };
        unsafe {
            UnlockFileEx(handle, 0, 1, 0, &mut overlapped);
        }
    }
}

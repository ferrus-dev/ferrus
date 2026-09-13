//! Unix no-follow openat traversal and per-file publication.

use super::*;
use std::{
    ffi::{CStr, CString},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
};

fn name(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL"))
}

pub(super) fn identity(file: &File) -> io::Result<(u64, u128)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), u128::from(metadata.ino())))
}

pub(super) fn patch_name_key(name: &str) -> String {
    use caseless::Caseless;
    use unicode_normalization::UnicodeNormalization;

    // Absent targets have no inode. Conservatively reject canonical/case aliases
    // within a batch, including on volumes that permit both spellings.
    name.nfd().default_case_fold().nfd().collect()
}

pub(super) fn root(path: &Path) -> io::Result<File> {
    let root = CString::new("/").unwrap();
    // SAFETY: root is NUL-terminated; returned descriptor is owned below.
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };

    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: successful open returned an owned descriptor.
    let mut directory = unsafe { File::from_raw_fd(fd) };
    for component in path.components() {
        if let std::path::Component::Normal(part) = component {
            let part = CString::new(part.as_bytes()).map_err(|_| unsafe_file())?;
            directory = open(&directory, &part, true, false)?;
        } else if component != std::path::Component::RootDir {
            return Err(unsafe_file());
        }
    }

    Ok(directory)
}

fn open(parent: &File, name: &CStr, directory: bool, create: bool) -> io::Result<File> {
    let flags = libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if create {
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        }
        | if directory { libc::O_DIRECTORY } else { 0 };

    // SAFETY: parent and component remain live; O_EXCL never replaces an existing entry.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(
            if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                unsafe_file()
            } else {
                error
            },
        );
    }

    // SAFETY: successful openat returned an owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        regular(&file)?;
    }

    Ok(file)
}

pub(super) fn child(parent: &File, value: &str, directory: bool, create: bool) -> io::Result<File> {
    open(parent, &name(value)?, directory, create)
}

pub(super) fn regular(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(unsafe_file());
    }
    Ok(())
}

pub(super) fn publish(
    parent: &File,
    target: &str,
    temporary: &str,
    _: &File,
    create: bool,
) -> io::Result<()> {
    let target = name(target)?;
    let temporary = name(temporary)?;
    // SAFETY: both names are single components relative to a held directory.
    let status = unsafe {
        if create {
            libc::linkat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                target.as_ptr(),
                0,
            )
        } else {
            libc::renameat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                target.as_ptr(),
            )
        }
    };

    if status != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

pub(super) fn finish_publish(parent: &File, temporary: &str, create: bool) -> io::Result<()> {
    if create {
        delete(parent, temporary)?;
    }

    Ok(())
}

pub(super) fn delete(parent: &File, value: &str) -> io::Result<()> {
    let name = name(value)?;
    // SAFETY: unlinkat removes the directory entry itself and never follows a symlink.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn sync(directory: &File) -> io::Result<()> {
    directory.sync_all()
}

pub(super) fn children(directory: &File, limit: usize) -> io::Result<(Vec<String>, bool)> {
    struct Stream(*mut libc::DIR);

    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: Stream owns the fdopendir result.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    let fd = child(directory, ".", true, false)?.into_raw_fd();
    // SAFETY: fd ownership transfers only on success.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir did not consume fd on failure.
        drop(unsafe { File::from_raw_fd(fd) });
        return Err(error);
    }

    let stream = Stream(stream);
    let mut names = Vec::new();
    let mut unsupported_name = false;
    loop {
        errno::set_errno(errno::Errno(0));
        // SAFETY: stream remains live and is accessed synchronously.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = errno::errno().0;
            return if error == 0 {
                Ok((names, unsupported_name))
            } else {
                Err(io::Error::from_raw_os_error(error))
            };
        }

        // SAFETY: readdir returned a dirent containing a NUL-terminated name.
        let value = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if matches!(value.to_bytes(), b"." | b"..") {
            continue;
        }

        if names.len() == limit {
            return Ok((names, true));
        }

        // Non-UTF-8 names are explicitly counted but cannot become model paths.
        names.push(match value.to_str() {
            Ok(name) => name.to_owned(),
            Err(_) => {
                unsupported_name = true;
                "\0".into()
            }
        });
    }
}

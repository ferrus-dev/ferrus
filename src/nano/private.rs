//! Private journal storage. Reject symlinks and inherited/broad permissions on reopen.

use anyhow::{Result, ensure};
use std::{
    fs::{self, File},
    path::Path,
};

pub(crate) fn check(path: &Path, directory: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "Unexpected session storage file type"
    );

    imp::check(path, &metadata)
}

pub(crate) fn directory(path: &Path, exclusive: bool) -> Result<()> {
    match imp::directory(path) {
        Ok(()) => (),
        Err(error) if !exclusive && error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }

    check(path, true)
}

pub(crate) fn file(path: &Path, create: bool) -> Result<File> {
    if !create {
        check(path, false)?;
    }

    let file = imp::file(path, create, true)?;
    check(path, false)?;

    Ok(file)
}

/// Open a host input without write access, then validate the opened object before reading.
pub(crate) fn read_only_file(path: &Path) -> Result<File> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "Unexpected host input file type"
    );
    let file = imp::file(path, false, false)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Unexpected host input file type");
    imp::check_input(&file, &metadata)?;
    Ok(file)
}

pub(crate) use imp::{rename, sync_directory};

#[cfg(all(test, windows))]
pub(crate) fn check_input_handle(file: &File) -> Result<()> {
    imp::check_input(file, &file.metadata()?)
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::{
        fs::{DirBuilder, OpenOptions},
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    };

    pub(super) fn check(_path: &Path, metadata: &fs::Metadata) -> Result<()> {
        check_owner(metadata)
    }

    fn check_owner(metadata: &fs::Metadata) -> Result<()> {
        // SAFETY: geteuid has no preconditions and returns the process identity.
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0,
            "Session storage must be owner-only"
        );

        Ok(())
    }

    pub(super) fn directory(path: &Path) -> std::io::Result<()> {
        DirBuilder::new().mode(0o700).create(path)
    }

    pub(super) fn check_input(_file: &File, metadata: &fs::Metadata) -> Result<()> {
        check_owner(metadata)
    }

    pub(super) fn file(path: &Path, create: bool, writable: bool) -> std::io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(writable)
            .create_new(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }

    pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
        File::open(path)?.sync_all()
    }

    pub(crate) fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
        fs::rename(from, to)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::{
        os::windows::{
            ffi::OsStrExt,
            fs::MetadataExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr::{null, null_mut},
    };

    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW,
                ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
                SE_FILE_OBJECT,
            },
            DACL_SECURITY_INFORMATION, GetFileSecurityW, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING,
        },
    };

    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn descriptor() -> std::io::Result<Descriptor> {
        // Protected DACL: full access only for the object's owner, no inherited grants.
        descriptor_from_text("D:P(A;;FA;;;OW)")
    }

    fn descriptor_from_text(sddl: &str) -> std::io::Result<Descriptor> {
        let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut pointer = null_mut();

        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut pointer,
                null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Descriptor(pointer))
    }

    fn text(descriptor: PSECURITY_DESCRIPTOR) -> std::io::Result<Vec<u16>> {
        security_text(descriptor, DACL_SECURITY_INFORMATION)
    }

    fn security_text(
        descriptor: PSECURITY_DESCRIPTOR,
        information: u32,
    ) -> std::io::Result<Vec<u16>> {
        let mut pointer = null_mut();
        let mut length = 0;
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                1,
                information,
                &mut pointer,
                &mut length,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let value = unsafe { std::slice::from_raw_parts(pointer, length as usize).to_vec() };
        unsafe {
            LocalFree(pointer.cast());
        }

        Ok(value)
    }

    pub(super) fn check(path: &Path, _: &fs::Metadata) -> Result<()> {
        let path = wide(path);
        let mut length = 0;

        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut length,
            );
        }

        ensure!(length > 0 && length <= 65536, "Cannot inspect session DACL");
        let mut buffer = vec![0u32; (length as usize).div_ceil(4)];

        ensure!(
            unsafe {
                GetFileSecurityW(
                    path.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    buffer.as_mut_ptr().cast(),
                    length,
                    &mut length,
                )
            } != 0,
            "Cannot read session DACL"
        );

        ensure!(
            text(buffer.as_mut_ptr().cast())? == text(descriptor()?.0)?,
            "Session storage must have a protected owner-only DACL"
        );

        Ok(())
    }
    pub(super) fn directory(path: &Path) -> std::io::Result<()> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };

        if unsafe { CreateDirectoryW(wide(path).as_ptr(), &attributes) } == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(())
    }

    pub(super) fn check_input(file: &File, metadata: &fs::Metadata) -> Result<()> {
        ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "Host input cannot be a reparse point"
        );
        let descriptor = input_descriptor(file)?;
        check_input_dacl(descriptor.0)
    }

    fn input_descriptor(file: &File) -> Result<Descriptor> {
        let mut pointer = null_mut();
        // Inspect the opened handle so a path replacement cannot substitute another DACL.
        let result = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut pointer,
            )
        };
        if result != 0 {
            return Err(std::io::Error::from_raw_os_error(result as i32).into());
        }
        Ok(Descriptor(pointer))
    }

    fn input_owner(descriptor: PSECURITY_DESCRIPTOR) -> Result<String> {
        let owner = String::from_utf16(&security_text(descriptor, OWNER_SECURITY_INFORMATION)?)?;
        let owner = owner
            .trim_end_matches('\0')
            .strip_prefix("O:")
            .filter(|owner| !owner.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Host input owner is missing"))?;
        Ok(owner.to_owned())
    }

    fn check_input_dacl(descriptor: PSECURITY_DESCRIPTOR) -> Result<()> {
        let owner = input_owner(descriptor)?;
        let actual = text(descriptor)?;
        // SetSecurityInfo can add the descriptor's AI bookkeeping flag without
        // changing its ACEs. Require protection and one explicit owner grant in
        // either form; an inherited ACE (ID) or any extra grant still fails closed.
        for trustee in ["OW", owner.as_str()] {
            for rights in ["FA", "FR", "GR"] {
                for flags in ["P", "PAI"] {
                    let expected =
                        descriptor_from_text(&format!("D:{flags}(A;;{rights};;;{trustee})"))?;
                    if actual == text(expected.0)? {
                        return Ok(());
                    }
                }
            }
        }
        anyhow::bail!("Host input must have a protected owner-only DACL")
    }

    pub(super) fn file(path: &Path, create: bool, writable: bool) -> std::io::Result<File> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };

        let handle = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                GENERIC_READ | if writable { GENERIC_WRITE } else { 0 },
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                if create { &attributes } else { null() },
                if create { CREATE_NEW } else { OPEN_EXISTING },
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: CreateFileW returned a new owned handle; File closes it exactly once.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    pub(crate) fn sync_directory(_: &Path) -> std::io::Result<()> {
        Ok(())
    }

    pub(crate) fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
        if unsafe {
            MoveFileExW(
                wide(from).as_ptr(),
                wide(to).as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Read, Write};
        use windows_sys::Win32::Security::{PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW};

        const OWNER: &str = "S-1-5-21-100-200-300-1001";

        #[test]
        fn input_dacl_accepts_owner_account_and_owner_rights_only() {
            for trustee in ["OW", OWNER] {
                for rights in ["FA", "FR", "GR"] {
                    for flags in ["P", "PAI"] {
                        let sddl = format!("O:{OWNER}D:{flags}(A;;{rights};;;{trustee})");
                        let descriptor = descriptor_from_text(&sddl).unwrap();
                        check_input_dacl(descriptor.0)
                            .unwrap_or_else(|error| panic!("{sddl}: {error}"));
                    }
                }
            }
            for dacl in [
                "D:P(A;;FR;;;WD)".to_owned(),
                "D:P(A;;FR;;;S-1-5-21-100-200-300-1002)".to_owned(),
                format!("D:P(A;;FR;;;{OWNER})(A;;FR;;;WD)"),
                format!("D:(A;;FR;;;{OWNER})"),
                format!("D:P(A;ID;FR;;;{OWNER})"),
                "D:PAI(A;;FR;;;WD)".to_owned(),
                "D:PAI(A;;FR;;;S-1-5-21-100-200-300-1002)".to_owned(),
                format!("D:PAI(A;;FR;;;{OWNER})(A;;FR;;;WD)"),
                format!("D:AI(A;;FR;;;{OWNER})"),
                format!("D:PAI(A;ID;FR;;;{OWNER})"),
                "D:PAI".to_owned(),
            ] {
                let descriptor = descriptor_from_text(&format!("O:{OWNER}{dacl}")).unwrap();
                assert!(check_input_dacl(descriptor.0).is_err(), "{dacl}");
            }
        }

        #[test]
        fn provisioned_owner_read_grant_opens_without_write_access() {
            let directory = tempfile::TempDir::new().unwrap();
            let path = directory.path().join("credential");
            let mut file = super::super::file(&path, true).unwrap();
            file.write_all(b"fixture-token").unwrap();
            let owner = input_owner(input_descriptor(&file).unwrap().0).unwrap();
            drop(file);
            let descriptor = descriptor_from_text(&format!("D:P(A;;FR;;;{owner})")).unwrap();
            // Equivalent to removing inherited grants and granting the owner Read via icacls.
            assert_ne!(
                unsafe {
                    SetFileSecurityW(
                        wide(&path).as_ptr(),
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        descriptor.0,
                    )
                },
                0
            );
            let mut input = super::super::read_only_file(&path).unwrap();
            let mut bytes = Vec::new();
            input.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"fixture-token");
            assert!(input.write_all(b"overwrite").is_err());
        }
    }
}

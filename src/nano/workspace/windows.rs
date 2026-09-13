//! Windows handle-relative opens reject every reparse point before use.

use super::*;
use std::{
    fs::OpenOptions,
    mem::{offset_of, size_of},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
            FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
            FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile,
            NtSetInformationFile,
        },
    },
    Win32::{
        Foundation::{
            ERROR_NO_MORE_FILES, HANDLE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE,
            RtlNtStatusToDosError, STATUS_NOT_A_DIRECTORY, STATUS_REPARSE_POINT_ENCOUNTERED,
            UNICODE_STRING,
        },
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    },
};

pub(super) fn search_key(path: &str) -> Vec<u16> {
    use windows_sys::Wdk::System::SystemServices::RtlUpcaseUnicodeChar;

    path.encode_utf16()
        .map(|unit| {
            // SAFETY: this pure NT mapping accepts any UTF-16 code unit.
            unsafe { RtlUpcaseUnicodeChar(unit) }
        })
        .collect()
}

pub(super) fn identity(file: &File) -> io::Result<(u64, u128)> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the opened handle and output structure remain live for this call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.VolumeSerialNumber,
        u128::from_le_bytes(info.FileId.Identifier),
    ))
}

pub(super) fn root(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;

    let meta = file.metadata()?;
    if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(unsafe_file());
    }

    Ok(file)
}

pub(super) fn child(parent: &File, value: &str, directory: bool, create: bool) -> io::Result<File> {
    if directory && value == "." {
        return parent.try_clone();
    }

    open(
        parent,
        value,
        directory,
        create,
        if create { DELETE } else { 0 },
    )
}

pub(super) fn create_staged(parent: &File, value: &str, copy_security: bool) -> io::Result<File> {
    // New targets inherit their ACL. Only replacements need to copy security metadata.
    let access = DELETE
        | if copy_security {
            WRITE_DAC | WRITE_OWNER
        } else {
            0
        };
    open(parent, value, false, true, access)
}

fn open(
    parent: &File,
    value: &str,
    directory: bool,
    create: bool,
    extra_access: u32,
) -> io::Result<File> {
    let mut name: Vec<u16> = value.encode_utf16().collect();
    let length = u16::try_from(name.len() * 2).map_err(|_| unsafe_file())?;

    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_mut_ptr(),
    };

    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };

    let mut handle = std::ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: the held parent, name buffer and structs remain live for this synchronous call.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_GENERIC_READ | if create { FILE_GENERIC_WRITE } else { 0 } | extra_access,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            if create { FILE_CREATE } else { FILE_OPEN },
            FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | if directory {
                    FILE_DIRECTORY_FILE
                } else if create || extra_access & DELETE != 0 {
                    FILE_NON_DIRECTORY_FILE
                } else {
                    0
                },
            std::ptr::null(),
            0,
        )
    };

    if status < 0 {
        if matches!(
            status,
            STATUS_NOT_A_DIRECTORY | STATUS_REPARSE_POINT_ENCOUNTERED
        ) {
            return Err(unsafe_file());
        }

        // SAFETY: status conversion has no side effects.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }

    if handle.is_null() {
        return Err(io::Error::other("empty handle"));
    }

    // SAFETY: NtCreateFile returned a new owned handle.
    let file = unsafe { File::from_raw_handle(handle) };
    let meta = file.metadata()?;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(unsafe_file());
    }

    if !meta.is_dir() {
        regular(&file)?;
    }

    Ok(file)
}

pub(super) fn regular(file: &File) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file and output structure remain live.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }

    if !file.metadata()?.is_file()
        || info.nNumberOfLinks != 1
        || info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE) != 0
    {
        return Err(unsafe_file());
    }

    Ok(())
}
pub(super) fn publish(
    parent: &File,
    target: &str,
    _: &str,
    file: &File,
    create: bool,
) -> io::Result<()> {
    let name: Vec<u16> = target.encode_utf16().collect();
    let bytes = size_of::<FILE_RENAME_INFORMATION>() + name.len() * 2;

    let mut storage = vec![0_u64; bytes.div_ceil(8)];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: storage is aligned and sized for the header plus the full UTF-16 name.
    let status = unsafe {
        (*info).Anonymous.ReplaceIfExists = !create;
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );

        // Pass the held directory directly to NT. The Win32 rename wrapper can
        // reject RootDirectory rather than resolve this name relative to it.
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status_block,
            info.cast(),
            bytes as u32,
            FileRenameInformation,
        )
    };

    if status < 0 {
        // SAFETY: status conversion has no side effects.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }

    Ok(())
}

pub(super) fn finish_publish(_: &File, _: &str, _: bool) -> io::Result<()> {
    Ok(())
}

pub(super) fn delete(parent: &File, value: &str) -> io::Result<()> {
    let file = open(parent, value, false, false, DELETE)?;
    discard(&file)
}

pub(super) fn discard(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle and typed disposition structure remain live.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

pub(super) fn sync(_: &File) -> io::Result<()> {
    Ok(())
}

pub(super) fn children(directory: &File, limit: usize) -> io::Result<(Vec<String>, bool)> {
    let mut storage = vec![0_u64; 8192];
    let mut restart = true;
    let mut names = Vec::new();
    let mut unsupported_name = false;
    loop {
        // SAFETY: storage is aligned, writable and has the advertised 64 KiB size.
        if unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle(),
                if restart {
                    FileIdBothDirectoryRestartInfo
                } else {
                    FileIdBothDirectoryInfo
                },
                storage.as_mut_ptr().cast(),
                65536,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                Ok((names, unsupported_name))
            } else {
                Err(error)
            };
        }

        restart = false;

        let bytes = storage.as_ptr().cast::<u8>();
        let mut cursor = 0;
        loop {
            let fixed = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if cursor + fixed > 65536 {
                return Err(unsafe_file());
            }

            // SAFETY: fixed fields fit in the checked buffer; read_unaligned tolerates drivers.
            let info = unsafe { bytes.add(cursor).cast::<FILE_ID_BOTH_DIR_INFO>() };
            // SAFETY: fixed fields were bounds-checked above.
            let (next, length) = unsafe {
                (
                    std::ptr::addr_of!((*info).NextEntryOffset).read_unaligned() as usize,
                    std::ptr::addr_of!((*info).FileNameLength).read_unaligned() as usize,
                )
            };

            let start = cursor + fixed;
            if length == 0 || length % 2 != 0 || length > 65536 - start {
                return Err(unsafe_file());
            }

            let end = start + length;
            let wide: Vec<u16> = (start..end)
                .step_by(2)
                .map(|offset| {
                    // SAFETY: each code unit fits in the checked name region.
                    unsafe { bytes.add(offset).cast::<u16>().read_unaligned() }
                })
                .collect();

            if !matches!(wide.as_slice(), [46] | [46, 46]) {
                if names.len() == limit {
                    return Ok((names, true));
                }

                names.push(match String::from_utf16(&wide) {
                    Ok(name) => name,
                    Err(_) => {
                        unsupported_name = true;
                        "\0".into()
                    }
                });
            }

            if next == 0 {
                break;
            }

            if next < end - cursor || next >= 65536 - cursor {
                return Err(unsafe_file());
            }

            cursor += next;
        }
    }
}

pub(super) fn copy_security(source: &File, target: &File) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo},
            DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorControl,
            OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            SE_DACL_PROTECTED, UNPROTECTED_DACL_SECURITY_INFORMATION,
        },
    };

    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    let mut owner = std::ptr::null_mut();
    let mut group = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let information =
        DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION;

    // SAFETY: handles and out-pointers are valid for the duration of the call.
    let status = unsafe {
        GetSecurityInfo(
            source.as_raw_handle(),
            SE_FILE_OBJECT,
            information,
            &mut owner,
            &mut group,
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };

    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    let descriptor = Descriptor(descriptor);
    if owner.is_null() || group.is_null() {
        return Err(unsafe_file());
    }

    let mut control = 0;
    let mut revision = 0;
    // SAFETY: the descriptor owns every SID and ACL pointer until after SetSecurityInfo.
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let inheritance = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };

    // SAFETY: all SID/ACL pointers remain owned by descriptor. The new empty file
    // was opened with WRITE_DAC and WRITE_OWNER; failure precedes any content write.
    let status = unsafe {
        SetSecurityInfo(
            target.as_raw_handle(),
            SE_FILE_OBJECT,
            information | inheritance,
            owner,
            group,
            dacl,
            std::ptr::null(),
        )
    };

    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }

    Ok(())
}

//! Handle-relative filesystem operations; no model path is opened by an absolute join.

use std::{
    fs::{File, Permissions},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
#[cfg(unix)]
#[path = "unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

pub(super) struct Root(File);

pub(super) struct Parent {
    directory: File,
    name: String,
}

pub(super) struct Staged {
    directory: File,
    name: String,
    file: File,
    published: bool,
}

pub(super) struct CommitError {
    pub changed: bool,
    pub error: io::Error,
}

pub(super) fn unsafe_file() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "unsafe file")
}

pub(super) fn regular(file: &File) -> io::Result<()> {
    platform::regular(file)
}

pub(super) fn children(file: &File, limit: usize) -> io::Result<(Vec<String>, bool)> {
    platform::children(file, limit)
}

pub(super) fn search_key(path: &str) -> impl Ord + use<> {
    #[cfg(unix)]
    {
        path.to_owned()
    }
    #[cfg(windows)]
    {
        platform::search_key(path)
    }
}

pub(super) fn identity(file: &File) -> io::Result<(u64, u128)> {
    platform::identity(file)
}

impl Root {
    pub(super) fn new(path: &Path) -> io::Result<Self> {
        platform::root(path).map(Self)
    }

    pub(super) fn directory(&self) -> io::Result<File> {
        platform::child(&self.0, ".", true, false)
    }

    pub(super) fn parent(&self, path: &str) -> io::Result<Parent> {
        let mut parts = path.split('/').peekable();
        let mut directory = self.directory()?;
        while let Some(name) = parts.next() {
            if parts.peek().is_none() {
                return Ok(Parent {
                    directory,
                    name: name.into(),
                });
            }
            directory = platform::child(&directory, name, true, false)?;
        }

        Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"))
    }

    pub(super) fn open(&self, path: &str) -> io::Result<File> {
        self.parent(path)?.open()
    }
}

impl Parent {
    pub(super) fn open(&self) -> io::Result<File> {
        platform::child(&self.directory, &self.name, false, false)
    }

    pub(super) fn patch_key(&self) -> io::Result<impl Ord + use<>> {
        #[cfg(unix)]
        let name = platform::patch_name_key(&self.name);
        #[cfg(windows)]
        let name = platform::search_key(&self.name);
        Ok((identity(&self.directory)?, name))
    }

    pub(super) fn stage(&self, bytes: &[u8], mode: Option<&Permissions>) -> io::Result<Staged> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);

        let name = format!(
            ".nano-tmp-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );

        let directory = self.directory.try_clone()?;
        #[cfg(unix)]
        let file = platform::child(&directory, &name, false, true)?;
        #[cfg(windows)]
        let file = platform::create_staged(&directory, &name, mode.is_some())?;
        let mut staged = Staged {
            directory,
            name,
            file,
            published: false,
        };

        #[cfg(windows)]
        if mode.is_some() {
            platform::copy_security(&self.open()?, &staged.file)?;
        }

        staged.file.write_all(bytes)?;

        if let Some(mode) = mode {
            staged.file.set_permissions(mode.clone())?;
        }

        staged.file.sync_all()?;
        Ok(staged)
    }

    pub(super) fn publish(&self, mut staged: Staged, create: bool) -> Result<(), CommitError> {
        platform::publish(
            &self.directory,
            &self.name,
            &staged.name,
            &staged.file,
            create,
        )
        .map_err(|error| CommitError {
            changed: false,
            error,
        })?;

        staged.published = true;

        platform::finish_publish(&self.directory, &staged.name, create).map_err(|error| {
            CommitError {
                changed: true,
                error,
            }
        })?;

        platform::sync(&self.directory).map_err(|error| CommitError {
            changed: true,
            error,
        })
    }

    pub(super) fn delete(&self) -> Result<(), CommitError> {
        platform::delete(&self.directory, &self.name).map_err(|error| CommitError {
            changed: false,
            error,
        })?;

        platform::sync(&self.directory).map_err(|error| CommitError {
            changed: true,
            error,
        })
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        if !self.published {
            #[cfg(windows)]
            if let Ok(metadata) = self.file.metadata() {
                let mut permissions = metadata.permissions();
                // This changes only a Windows attribute on our owned temporary file.
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = self.file.set_permissions(permissions);
            }
            #[cfg(unix)]
            let _ = platform::delete(&self.directory, &self.name);
            #[cfg(windows)]
            let _ = platform::discard(&self.file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Read};

    #[test]
    fn search_keys_follow_platform_case_rules() {
        #[cfg(unix)]
        assert!(search_key("src/file") != search_key("SRC/FILE"));
        #[cfg(windows)]
        {
            assert!(search_key("src/file") == search_key("SRC/FILE"));
            assert!(search_key("caf\u{e9}/file") == search_key("CAF\u{c9}/FILE"));
            // NT upcasing preserves UTF-16 length; Unicode expansion is not a path alias.
            assert!(search_key("stra\u{df}e") != search_key("STRASSE"));
        }
    }

    #[test]
    fn create_publication_never_clobbers_an_existing_entry() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = Root::new(&directory.path().canonicalize().unwrap()).unwrap();
        let parent = root.parent("file").unwrap();
        let staged = parent.stage(b"agent", None).unwrap();
        fs::write(directory.path().join("file"), "human").unwrap();
        let error = parent.publish(staged, true).expect_err("target exists");
        assert!(!error.changed);
        assert_eq!(error.error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"human");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn publications_use_the_held_parent_for_create_replace_and_delete() {
        let directory = tempfile::TempDir::new().unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        let root = Root::new(&directory.path().canonicalize().unwrap()).unwrap();

        // Exercise short and variable-length UTF-16 rename buffers on Windows.
        for name in ["a", "longer-\u{e9}-\u{1f980}.txt"] {
            fs::write(directory.path().join(name), b"unrelated").unwrap();
            let parent = root.parent(&format!("nested/{name}")).unwrap();
            let target = directory.path().join("nested").join(name);
            for (bytes, create) in [(b"created".as_slice(), true), (b"replaced", false)] {
                let staged = parent.stage(bytes, None).unwrap();
                parent.publish(staged, create).unwrap_or_else(|error| {
                    panic!(
                        "publication failed (changed={}): {}",
                        error.changed, error.error
                    )
                });
                assert_eq!(fs::read(&target).unwrap(), bytes);
                assert_eq!(fs::read(directory.path().join(name)).unwrap(), b"unrelated");
                assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
            }
            parent.delete().unwrap_or_else(|error| {
                panic!(
                    "deletion failed (changed={}): {}",
                    error.changed, error.error
                )
            });
            assert!(!target.exists());
            assert_eq!(fs::read(directory.path().join(name)).unwrap(), b"unrelated");
            assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 0);
        }
    }

    #[test]
    fn enumeration_restarts_and_hardlink_aliases_are_rejected() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = Root::new(&directory.path().canonicalize().unwrap()).unwrap();
        fs::write(directory.path().join("file"), "body").unwrap();
        let file_identity = identity(&root.open("file").unwrap()).unwrap();
        assert_ne!(file_identity, identity(&root.directory().unwrap()).unwrap());
        for _ in 0..2 {
            assert_eq!(
                children(&root.directory().unwrap(), 10).unwrap(),
                (vec!["file".to_owned()], false)
            );
            let mut file = root.open("file").unwrap();
            assert_eq!(identity(&file).unwrap(), file_identity);
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            assert_eq!(text, "body");
        }
        fs::hard_link(
            directory.path().join("file"),
            directory.path().join("alias"),
        )
        .unwrap();
        assert!(root.open("alias").is_err());
        assert!(root.open("file").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_creates_need_only_inherited_modify_access() {
        use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt};
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::{
                    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                    SE_FILE_OBJECT, SetNamedSecurityInfoW,
                },
                DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
                PROTECTED_DACL_SECURITY_INFORMATION,
            },
            Storage::FileSystem::{WRITE_DAC, WRITE_OWNER},
        };

        let directory = tempfile::TempDir::new().unwrap();
        let path: Vec<u16> = directory
            .path()
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // Keep full control of the fixture directory, but give children only Modify.
        // An Owner Rights ACE also suppresses the owner's implicit WRITE_DAC access.
        let sddl: Vec<u16> = "D:P(A;;FA;;;OW)(A;OICIIO;0x001301bf;;;OW)"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        // SAFETY: the descriptor owns the ACL until SetNamedSecurityInfoW returns;
        // the path and all output pointers remain live. Only this temp fixture changes.
        unsafe {
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                ),
                0
            );
            let extracted =
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted);
            let status = if extracted != 0 && present != 0 && !dacl.is_null() {
                SetNamedSecurityInfoW(
                    path.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    dacl,
                    std::ptr::null(),
                )
            } else {
                u32::MAX
            };
            LocalFree(descriptor);
            assert_eq!(status, 0, "fixture DACL setup failed");
        }

        let probe = directory.path().join("ordinary.txt");
        fs::write(&probe, b"ordinary").expect("ordinary file creation with Modify access");
        for access in [WRITE_DAC, WRITE_OWNER] {
            let error = fs::OpenOptions::new()
                .access_mode(access)
                .open(&probe)
                .expect_err("fixture must deny security-management access");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        let root = Root::new(&directory.path().canonicalize().unwrap()).unwrap();
        let parent = root.parent("created.txt").unwrap();
        let staged = parent
            .stage(b"created", None)
            .expect("create needs no owner rights");
        parent
            .publish(staged, true)
            .unwrap_or_else(|error| panic!("create publication failed: {}", error.error));
        let target = directory.path().join("created.txt");
        assert_eq!(fs::read(&target).unwrap(), b"created");
        // Publication retains the inherited ACL instead of granting additional rights.
        for access in [WRITE_DAC, WRITE_OWNER] {
            assert_eq!(
                fs::OpenOptions::new()
                    .access_mode(access)
                    .open(&target)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        drop(parent.stage(b"discarded", None).unwrap());
        let mode = parent.open().unwrap().metadata().unwrap().permissions();
        assert_eq!(
            parent
                .stage(b"replacement", Some(&mode))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(&target).unwrap(), b"created");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn windows_updates_preserve_protected_owner_only_access() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("private.txt");
        crate::nano::private::file(&path, true)
            .unwrap()
            .write_all(b"old\n")
            .unwrap();
        crate::nano::private::read_only_file(&path).expect("original owner-only DACL");
        let root = Root::new(&directory.path().canonicalize().unwrap()).unwrap();
        let parent = root.parent("private.txt").unwrap();
        let mode = parent.open().unwrap().metadata().unwrap().permissions();
        let staged = parent.stage(b"new\n", Some(&mode)).unwrap();
        // Staging holds DELETE access for publication. Inspect its existing handle;
        // reopening through the private reader would deny that access via sharing.
        crate::nano::private::check_input_handle(&staged.file).expect("staged owner-only DACL");
        parent
            .publish(staged, false)
            .unwrap_or_else(|error| panic!("private publication failed: {}", error.error));
        // Validate the protected DACL from the opened handle, not just readonly.
        crate::nano::private::read_only_file(&path).expect("published owner-only DACL");
        assert_eq!(fs::read(path).unwrap(), b"new\n");
    }

    #[cfg(windows)]
    #[test]
    fn windows_junctions_cannot_redirect_reads_or_writes() {
        let directory = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        fs::write(outside.path().join("file"), "outside").unwrap();
        let link = directory.path().join("junction");
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "junction fixture setup failed");
        let root = Root::new(&directory.path().canonicalize().unwrap()).unwrap();
        assert!(root.open("junction/file").is_err());
        assert!(root.parent("junction/new").is_err());
        assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"outside");
        fs::remove_dir(link).unwrap();
    }
}

//! Read-only model workspace materialized from immutable Git blobs, without checkout filters.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) struct Source {
    pub path: PathBuf,
    pub owned: bool,
    pub omitted_links: usize,
    pub omitted_paths: usize,
}
impl Drop for Source {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// The caller holds a SQLite write transaction so new run reservations cannot
/// race the live-run check and removal of an existing inspection directory.
pub(super) fn reclaim(parent: &Path, tx: &rusqlite::Transaction<'_>) -> Result<usize> {
    let mut statement = tx.prepare(
        "SELECT id, pid FROM runs WHERE role='supervisor' AND status IN ('running','checking','reviewing')",
    )?;
    let mut live = std::collections::HashSet::new();
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<u32>>(1)?))
    })? {
        let (id, pid) = row?;
        if pid.is_some_and(crate::platform::pid_is_alive) {
            live.insert(id);
        }
    }
    let mut removed = 0;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str() else { continue };
        if !super::super::journal::valid_id(id) || live.contains(id) || !entry.file_type()?.is_dir()
        {
            continue;
        }
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::warn!(run_id = id, error = ?error, "failed to reclaim review source");
            }
        }
    }
    Ok(removed)
}

pub(super) fn snapshot(root: &Path, destination: &Path, tree: &str) -> Result<Source> {
    super::super::private::directory(destination, true)?;
    let mut source = Source {
        path: destination.canonicalize()?,
        owned: true,
        omitted_links: 0,
        omitted_paths: 0,
    };
    let mut list = Child(
        git(root)
            .args(["ls-tree", "-r", "-l", "-z", tree])
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut entries = Vec::new();
    list.0
        .stdout
        .take()
        .context("Missing tree listing")?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut entries)?;
    ensure!(
        entries.len() <= 8 * 1024 * 1024,
        "Reviewer tree listing exceeds quota"
    );
    ensure!(list.0.wait()?.success(), "Cannot inspect submitted tree");
    let mut batch = Child(
        git(root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut input = batch.0.stdin.take().context("Missing blob input")?;
    let mut output = BufReader::new(batch.0.stdout.take().context("Missing blob output")?);
    let mut total = 0u64;
    for (index, entry) in entries
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .enumerate()
    {
        ensure!(index < 20000, "Reviewer tree file quota exceeded");
        let separator = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .context("Invalid tree entry")?;
        let header = std::str::from_utf8(&entry[..separator])?;
        let path = &entry[separator + 1..];
        let fields: Vec<_> = header.split_whitespace().collect();
        // Never materialize symlinks or gitlinks into the inspection workspace.
        if fields
            .first()
            .is_some_and(|mode| *mode == "120000" || *mode == "160000")
        {
            source.omitted_links += 1;
            continue;
        }
        ensure!(
            fields.len() == 4 && fields[1] == "blob",
            "Unsupported tree entry"
        );
        let size: u64 = fields[3].parse()?;
        total = total.checked_add(size).context("Tree quota overflow")?;
        ensure!(
            size <= 8 * 1024 * 1024 && total <= 128 * 1024 * 1024,
            "Reviewer source exceeds quota"
        );
        #[cfg(unix)]
        let relative = {
            use std::os::unix::ffi::OsStrExt;
            Path::new(std::ffi::OsStr::from_bytes(path))
        };
        #[cfg(not(unix))]
        let relative = match std::str::from_utf8(path) {
            #[cfg(windows)]
            Ok(path) if !windows_path_supported(path) => {
                source.omitted_paths += 1;
                continue;
            }
            Ok(path) => Path::new(path),
            Err(_) => {
                source.omitted_paths += 1;
                continue;
            }
        };
        ensure!(
            !relative.is_absolute()
                && relative
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
            "Unsafe submitted path"
        );
        let target = source.path.join(relative);
        if target.is_dir() {
            source.omitted_paths += 1;
            continue;
        }
        let create = || -> Result<fs::File> {
            fs::create_dir_all(target.parent().context("Missing source parent")?)?;
            super::super::private::file(&target, true)
        };
        let mut file = match create() {
            Ok(file) => file,
            Err(error)
                if unsupported_name(&error)
                    || target
                        .ancestors()
                        .skip(1)
                        .take_while(|parent| *parent != source.path.as_path())
                        .any(Path::is_file) =>
            {
                source.omitted_paths += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        writeln!(input, "{}", fields[2])?;
        input.flush()?;
        let mut reply = String::new();
        output.read_line(&mut reply)?;
        ensure!(
            reply.trim() == format!("{} blob {}", fields[2], size),
            "Unexpected Git blob identity"
        );
        ensure!(
            std::io::copy(&mut output.by_ref().take(size), &mut file)? == size,
            "Incomplete submitted blob"
        );
        let mut separator = [0];
        output.read_exact(&mut separator)?;
        ensure!(separator == *b"\n", "Invalid blob boundary");
    }
    drop(input);
    ensure!(batch.0.wait()?.success(), "Cannot read submitted blobs");
    Ok(source)
}

fn unsupported_name(error: &anyhow::Error) -> bool {
    let Some(error) = error.downcast_ref::<std::io::Error>() else {
        return false;
    };
    if matches!(
        error.kind(),
        std::io::ErrorKind::AlreadyExists
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::IsADirectory
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EILSEQ)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            ERROR_BAD_PATHNAME, ERROR_DIRECTORY, ERROR_FILENAME_EXCED_RANGE, ERROR_INVALID_NAME,
        };
        error.raw_os_error().is_some_and(|code| {
            matches!(
                code as u32,
                ERROR_BAD_PATHNAME
                    | ERROR_DIRECTORY
                    | ERROR_FILENAME_EXCED_RANGE
                    | ERROR_INVALID_NAME
            )
        })
    }
}

#[cfg(any(windows, test))]
fn windows_path_supported(path: &str) -> bool {
    // Do not let CreateFileW reinterpret a Git name as a device or data stream.
    path.split('/').all(|component| {
        let lower = component.to_uppercase().to_ascii_lowercase();
        let stem = lower.split('.').next().unwrap_or_default();
        !component.is_empty()
            && !component.ends_with(['.', ' '])
            && !component
                .chars()
                .any(|c| c < ' ' || ":*?\"<>|\\".contains(c))
            && !matches!(stem, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
            && !["com", "lpt"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    suffix.chars().count() == 1 && "123456789\u{b9}\u{b2}\u{b3}".contains(suffix)
                })
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    #[test]
    fn reclaim_removes_only_abandoned_review_directories() {
        let root = tempfile::tempdir().unwrap();
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runs (id TEXT, role TEXT, status TEXT, pid INTEGER);")
            .unwrap();
        for (id, status, pid) in [
            ("live-review", "running", Some(std::process::id())),
            ("checking-review", "checking", Some(std::process::id())),
            ("human-review", "reviewing", Some(std::process::id())),
            ("finished-review", "completed", Some(std::process::id())),
            ("interrupted-review", "interrupted", None),
            ("dead-review", "running", None),
        ] {
            db.execute(
                "INSERT INTO runs VALUES (?1,'supervisor',?2,?3)",
                rusqlite::params![id, status, pid],
            )
            .unwrap();
            fs::create_dir(root.path().join(id)).unwrap();
            fs::write(root.path().join(id).join("source.rs"), "submitted source").unwrap();
        }
        fs::create_dir(root.path().join("orphan-review")).unwrap();
        fs::create_dir(root.path().join("not a run")).unwrap();
        fs::write(root.path().join("plain-file"), "leave alone").unwrap();
        #[cfg(unix)]
        let outside = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked-review")).unwrap();
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(reclaim(root.path(), &tx).unwrap(), 4);
        for id in [
            "live-review",
            "checking-review",
            "human-review",
            "not a run",
        ] {
            assert!(root.path().join(id).is_dir(), "{id}");
        }
        assert!(root.path().join("plain-file").is_file());
        #[cfg(unix)]
        assert!(root.path().join("linked-review").is_symlink() && outside.path().is_dir());
        assert_eq!(reclaim(root.path(), &tx).unwrap(), 0);
    }

    fn object(root: &Path, args: &[&str], bytes: &[u8]) -> String {
        let mut child = git(root)
            .args(["-c", "core.protectNTFS=false"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    #[test]
    fn windows_paths_exclude_devices_streams_and_unsupported_components() {
        for path in [
            "CON",
            "con.txt",
            "dir/AUX.rs",
            "NUL",
            "COM1",
            "LPT9.log",
            "COM\u{b2}",
            "CONIN$",
            "con\u{131}n$",
            "name:stream",
            "dir\\file",
            "trailing.",
            "trailing ",
            "name?",
            "line\nfeed",
        ] {
            assert!(!windows_path_supported(path), "{path:?}");
        }
        for path in [
            "src/lib.rs",
            "console.txt",
            "COM10",
            "dir/notes~old",
            "caf\u{e9}.txt",
        ] {
            assert!(windows_path_supported(path), "{path:?}");
        }
    }

    #[test]
    fn snapshot_omits_case_collisions_without_overwriting_the_first_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(
            git(root)
                .args(["init", "--object-format=sha1"])
                .status()
                .unwrap()
                .success()
        );
        let first = object(root, &["hash-object", "-w", "--stdin"], b"first\n");
        let second = object(root, &["hash-object", "-w", "--stdin"], b"second\n");
        let subtree = object(
            root,
            &["mktree", "-z"],
            format!("100644 blob {second}\tchild.txt\0").as_bytes(),
        );
        let entries = format!(
            "100644 blob {first}\tA.txt\0\
            100644 blob {first}\tREADME\0\
            100644 blob {second}\ta.txt\0\
            100644 blob {second}\tordinary.txt\0\
            040000 tree {subtree}\treadme\0"
        );
        let tree = object(root, &["mktree", "-z"], entries.as_bytes());
        let source = snapshot(root, &root.join("inspection"), &tree).unwrap();
        fs::write(source.path.join("CaseProbe"), "probe").unwrap();
        let case_insensitive = source.path.join("caseprobe").exists();
        assert_eq!(fs::read(source.path.join("A.txt")).unwrap(), b"first\n");
        assert_eq!(fs::read(source.path.join("README")).unwrap(), b"first\n");
        assert_eq!(
            fs::read(source.path.join("ordinary.txt")).unwrap(),
            b"second\n"
        );
        assert_eq!(source.omitted_paths, if case_insensitive { 2 } else { 0 });
        if !case_insensitive {
            assert_eq!(fs::read(source.path.join("a.txt")).unwrap(), b"second\n");
            assert_eq!(
                fs::read(source.path.join("readme/child.txt")).unwrap(),
                b"second\n"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_omits_windows_devices_and_stream_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(
            git(root)
                .args(["init", "--object-format=sha1"])
                .status()
                .unwrap()
                .success()
        );
        let blob = object(root, &["hash-object", "-w", "--stdin"], b"contents\n");
        let names = [
            "CON",
            "CON.txt",
            "file:stream.txt",
            "NUL",
            "trailing.",
            "ordinary.txt",
        ];
        let entries = names
            .iter()
            .map(|name| format!("100644 blob {blob}\t{name}\0"))
            .collect::<String>();
        let tree = object(root, &["mktree", "-z"], entries.as_bytes());
        let source = snapshot(root, &root.join("inspection"), &tree).unwrap();
        assert_eq!(source.omitted_paths, names.len() - 1);
        assert_eq!(
            fs::read(source.path.join("ordinary.txt")).unwrap(),
            b"contents\n"
        );
        assert!(!source.path.join("file").exists());
    }

    #[test]
    fn snapshot_preserves_non_utf8_paths_or_reports_host_restrictions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(
            git(root)
                .args(["init", "--object-format=sha1"])
                .status()
                .unwrap()
                .success()
        );
        let blob = object(
            root,
            &["hash-object", "-w", "--stdin"],
            b"frozen contents\n",
        );
        let mut entries = format!("100644 blob {blob}\tname-").into_bytes();
        entries.extend_from_slice(b"\xff\tfile.txt\0");
        entries.extend_from_slice(format!("100644 blob {blob}\tordinary.txt\0").as_bytes());
        let tree = object(root, &["mktree", "-z"], &entries);
        let source = snapshot(root, &root.join("inspection"), &tree).unwrap();
        assert_eq!(
            fs::read(source.path.join("ordinary.txt")).unwrap(),
            b"frozen contents\n"
        );
        #[cfg(unix)]
        {
            let path = source.path.join(OsStr::from_bytes(b"name-\xff\tfile.txt"));
            match fs::read(&path) {
                Ok(contents) => {
                    assert_eq!(contents, b"frozen contents\n");
                    assert_eq!(source.omitted_paths, 0);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::EILSEQ) =>
                {
                    assert_eq!(source.omitted_paths, 1);
                }
                Err(error) => panic!("unexpected native-path error: {error}"),
            }
        }
        #[cfg(not(unix))]
        assert_eq!(source.omitted_paths, 1);
        assert_eq!(source.omitted_links, 0);
    }
}

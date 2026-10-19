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
        let create = || -> Result<fs::File> {
            fs::create_dir_all(target.parent().context("Missing source parent")?)?;
            super::super::private::file(&target, true)
        };
        let mut file = match create() {
            Ok(file) => file,
            Err(error) if unsupported_name(&error) => {
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
    #[cfg(unix)]
    {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(libc::EILSEQ))
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    fn object(root: &Path, args: &[&str], bytes: &[u8]) -> String {
        let mut child = git(root)
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

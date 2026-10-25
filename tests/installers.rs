//! Exercise component installers against local release archives without network access.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![cfg(any(unix, windows))]

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const BINARIES: &[&str] = &["ferrus", "ferrus-nano"];

struct Fixture {
    temp: TempDir,
    assets: PathBuf,
    destination: PathBuf,
    home: PathBuf,
    downloads: PathBuf,
    target: String,
    archive_name: String,
    env: BTreeMap<&'static str, String>,
    #[cfg(windows)]
    powershell: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("ferrus installer ")
            .tempdir()
            .unwrap();
        let assets = temp.path().join("assets");
        let destination = temp.path().join("bin");
        let home = temp.path().join("home");
        let downloads = temp.path().join("downloads");
        fs::create_dir(&assets).unwrap();
        fs::create_dir(&home).unwrap();

        #[cfg(unix)]
        let target = {
            let machine = Command::new("uname").arg("-m").output().unwrap();
            assert_success(&machine);
            let machine = String::from_utf8(machine.stdout).unwrap();
            let arch = match machine.trim() {
                "aarch64" | "arm64" => "aarch64",
                "x86_64" | "amd64" => "x86_64",
                other => panic!("unsupported installer test architecture: {other}"),
            };
            let suffix = if cfg!(target_os = "macos") {
                "apple-darwin"
            } else {
                "unknown-linux-gnu"
            };
            format!("{arch}-{suffix}")
        };
        #[cfg(windows)]
        let target = "x86_64-pc-windows-msvc".to_string();
        let archive_extension = if cfg!(windows) { "zip" } else { "tar.gz" };
        let archive_name = format!("ferrus-{target}.{archive_extension}");

        let fixture = Self {
            temp,
            assets,
            destination,
            home,
            downloads,
            target,
            archive_name,
            env: BTreeMap::new(),
            #[cfg(windows)]
            powershell: ["pwsh.exe", "powershell.exe"]
                .into_iter()
                .find_map(find_command)
                .expect("PowerShell is required to test the Windows installer"),
        };
        fixture.prepare_runner();
        fixture.make_archive(BINARIES);
        fixture
    }

    #[cfg(unix)]
    fn prepare_runner(&self) {
        use std::os::unix::fs::PermissionsExt;

        let mock_bin = self.temp.path().join("mock bin");
        fs::create_dir(&mock_bin).unwrap();
        let curl = mock_bin.join("curl");
        fs::write(
            &curl,
            r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) output="$2"; shift 2 ;;
        -w) shift 2 ;;
        -sSL) shift ;;
        *) url="$1"; shift ;;
    esac
done
printf '%s\n' "$url" >> "$FERRUS_TEST_DOWNLOADS"
cp "$FERRUS_TEST_ASSETS/${url##*/}" "$output"
printf 200
"#,
        )
        .unwrap();
        fs::set_permissions(curl, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    fn prepare_runner(&self) {
        fs::write(
            self.temp.path().join("runner.ps1"),
            r#"$ErrorActionPreference = "Stop"
function Invoke-WebRequest {
    param($Uri, $OutFile, [switch]$UseBasicParsing)
    Add-Content $env:FERRUS_TEST_DOWNLOADS $Uri
    $asset = ([Uri]$Uri).Segments[-1]
    Copy-Item (Join-Path $env:FERRUS_TEST_ASSETS $asset) $OutFile
}
$installerParams = @{}
$options = ConvertFrom-Json $env:FERRUS_TEST_ARGS
foreach ($property in $options.PSObject.Properties) {
    $installerParams[$property.Name] = $property.Value
}
if ($env:FERRUS_TEST_SCRIPTBLOCK -eq "1") {
    $source = Get-Content -Raw $env:FERRUS_TEST_INSTALLER
    & ([scriptblock]::Create($source)) @installerParams
} else {
    & $env:FERRUS_TEST_INSTALLER @installerParams
}
"#,
        )
        .unwrap();
        fs::write(
            self.temp.path().join("pack.ps1"),
            r#"param($Source, $Destination)
$ErrorActionPreference = "Stop"
Compress-Archive -Path $Source -DestinationPath $Destination -Force
"#,
        )
        .unwrap();
    }

    fn archive(&self) -> PathBuf {
        self.assets.join(&self.archive_name)
    }

    fn checksum(&self) -> PathBuf {
        self.assets.join(format!("{}.sha256", self.archive_name))
    }

    fn binary_name(&self, name: &str) -> String {
        if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        }
    }

    fn make_archive(&self, binaries: &[&str]) {
        let staging = self.temp.path().join("staging");
        if staging.exists() {
            fs::remove_dir_all(&staging).unwrap();
        }

        let directory = format!("ferrus-{}", self.target);
        let package = staging.join(&directory);
        fs::create_dir_all(&package).unwrap();
        for binary in binaries {
            let path = package.join(self.binary_name(binary));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                fs::write(&path, format!("#!/bin/sh\necho '{binary} fixture'\n")).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            #[cfg(windows)]
            {
                // Use a real executable to avoid invalid-executable dialogs.
                // Its unsupported --version is harmless; probing is best-effort.
                let source = find_command("where.exe").expect("where.exe is required");
                fs::copy(source, path).unwrap();
            }
        }

        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("tar");
            command.arg("-czf").arg(self.archive());
            command.arg("-C").arg(&staging).arg(directory);
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new(&self.powershell);
            command.args(["-NoProfile", "-NonInteractive", "-File"]);
            command.arg(self.temp.path().join("pack.ps1"));
            command.arg("-Source").arg(package);
            command.arg("-Destination").arg(self.archive());
            command
        };
        assert_success(&self.execute(&mut command, None));
        let digest = Sha256::digest(fs::read(self.archive()).unwrap());
        let digest: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        fs::write(
            self.checksum(),
            format!("{digest}  {}\n", self.archive_name),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            let mut paths = vec![self.temp.path().join("mock bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            command.env("PATH", std::env::join_paths(paths).unwrap());
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new(&self.powershell);
            command.args(["-NoProfile", "-NonInteractive", "-File"]);
            command.arg(self.temp.path().join("runner.ps1"));
            command.env("PROCESSOR_ARCHITECTURE", "AMD64");
            command.env_remove("PROCESSOR_ARCHITEW6432");
            command.env("FERRUS_TEST_INSTALLER", Path::new(ROOT).join("install.ps1"));
            command
        };
        for name in [
            "FERRUS_INSTALL_COMPONENTS",
            "FERRUS_INSTALL_VERSION",
            "FERRUS_TEST_SCRIPTBLOCK",
        ] {
            command.env_remove(name);
        }

        command
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("FERRUS_INSTALL_DIR", &self.destination)
            .env("FERRUS_TEST_ASSETS", &self.assets)
            .env("FERRUS_TEST_DOWNLOADS", &self.downloads)
            .envs(&self.env);
        command
    }

    fn run(&self, components: Option<&str>) -> Output {
        self.run_options(components, false, false)
    }

    fn run_options(&self, components: Option<&str>, help: bool, unknown: bool) -> Output {
        let mut command = self.command();
        #[cfg(unix)]
        {
            command.arg(Path::new(ROOT).join("install.sh"));
            if let Some(components) = components {
                command.args(["--components", components]);
            }

            if help {
                command.arg("--help");
            }

            if unknown {
                command.arg("--unknown");
            }
        }
        #[cfg(windows)]
        {
            let mut options = serde_json::Map::new();
            if let Some(components) = components {
                options.insert("Components".into(), components.into());
            }

            if help {
                options.insert("Help".into(), true.into());
            }

            if unknown {
                options.insert("Unknown".into(), true.into());
            }

            command.env("FERRUS_TEST_ARGS", serde_json::to_string(&options).unwrap());
        }
        self.execute(&mut command, None)
    }

    fn execute(&self, command: &mut Command, input: Option<&[u8]>) -> Output {
        // Files keep verbose child output from filling a pipe before the timeout.
        let stdout = self.temp.path().join("command.stdout");
        let stderr = self.temp.path().join("command.stderr");
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap());
        let mut child = command.spawn().unwrap_or_else(|error| {
            panic!("failed to spawn {command:?}: {error}");
        });
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }

            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "command timed out: {command:?}\n{}\n{}",
                    fs::read_to_string(stdout).unwrap(),
                    fs::read_to_string(stderr).unwrap()
                );
            }

            thread::sleep(Duration::from_millis(20));
        };
        Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        }
    }

    fn assert_installed(&self, expected: &[&str]) {
        let actual: BTreeSet<_> = if self.destination.exists() {
            fs::read_dir(&self.destination)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect()
        } else {
            BTreeSet::new()
        };
        let expected: BTreeSet<OsString> = expected
            .iter()
            .map(|name| self.binary_name(name).into())
            .collect();
        assert_eq!(actual, expected);
    }

    fn retain(&self, name: &str, content: &[u8]) -> PathBuf {
        fs::create_dir_all(&self.destination).unwrap();
        let path = self.destination.join(self.binary_name(name));
        fs::write(&path, content).unwrap();
        path
    }
}

#[cfg(windows)]
fn find_command(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        output.status,
        output_text(output)
    );
}

#[test]
fn default_installs_both_from_shared_archive() {
    let fixture = Fixture::new();
    let output = fixture.run(None);
    assert_success(&output);
    fixture.assert_installed(BINARIES);
    let text = output_text(&output);
    assert!(text.contains("installed ferrus to"));
    assert!(text.contains("installed ferrus-nano to"));
    assert_eq!(
        fs::read_to_string(&fixture.downloads)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn ferrus_only_retains_existing_nano() {
    let fixture = Fixture::new();
    let retained = fixture.retain("ferrus-nano", b"existing nano");
    let output = fixture.run(Some("ferrus"));
    assert_success(&output);
    assert_eq!(fs::read(retained).unwrap(), b"existing nano");
    assert!(output_text(&output).contains("installed ferrus to"));
    assert!(!output_text(&output).contains("installed ferrus-nano to"));
}

#[test]
fn nano_only_retains_existing_ferrus() {
    let fixture = Fixture::new();
    let retained = fixture.retain("ferrus", b"existing ferrus");
    let output = fixture.run(Some("nano"));
    assert_success(&output);
    assert_eq!(fs::read(retained).unwrap(), b"existing ferrus");
    assert!(output_text(&output).contains("installed ferrus-nano to"));
    assert!(!output_text(&output).contains("installed ferrus to"));
    assert!(!fixture.home.join(".ferrus/projects").exists());
}

#[test]
fn nano_only_does_not_require_ferrus_in_archive() {
    let fixture = Fixture::new();
    fixture.make_archive(&["ferrus-nano"]);
    assert_success(&fixture.run(Some("nano")));
    fixture.assert_installed(&["ferrus-nano"]);
}

#[test]
fn environment_selects_nano() {
    let mut fixture = Fixture::new();
    fixture
        .env
        .insert("FERRUS_INSTALL_COMPONENTS", "nano".into());
    assert_success(&fixture.run(None));
    fixture.assert_installed(&["ferrus-nano"]);
}

#[test]
fn parameter_overrides_environment() {
    let mut fixture = Fixture::new();
    fixture
        .env
        .insert("FERRUS_INSTALL_COMPONENTS", "invalid".into());
    assert_success(&fixture.run(Some("ferrus")));
    fixture.assert_installed(&["ferrus"]);
}

#[test]
fn all_supports_legacy_release_and_retains_nano() {
    let fixture = Fixture::new();
    fixture.make_archive(&["ferrus"]);
    let retained = fixture.retain("ferrus-nano", b"existing nano");
    let output = fixture.run(Some("all"));
    assert_success(&output);
    assert_eq!(fs::read(retained).unwrap(), b"existing nano");
    let text = output_text(&output);
    assert!(text.contains("does not include ferrus-nano"));
    assert!(text.contains("may be incompatible"));
    assert!(!text.contains("installed ferrus-nano to"));
}

#[test]
fn missing_selected_binary_preserves_installation() {
    let fixture = Fixture::new();
    fixture.make_archive(&["ferrus"]);
    let retained = fixture.retain("ferrus", b"existing ferrus");
    let output = fixture.run(Some("nano"));
    assert!(!output.status.success());
    assert!(output_text(&output).contains("expected binary entry"));
    assert_eq!(fs::read(retained).unwrap(), b"existing ferrus");
    fixture.assert_installed(&["ferrus"]);
}

#[test]
fn all_requires_ferrus_before_installing_nano() {
    let fixture = Fixture::new();
    fixture.make_archive(&["ferrus-nano"]);
    assert!(!fixture.run(Some("all")).status.success());
    assert!(!fixture.destination.exists());
}

#[test]
fn checksum_mismatch_preserves_installation() {
    let fixture = Fixture::new();
    fs::write(fixture.checksum(), "0".repeat(64)).unwrap();
    let output = fixture.run(Some("nano"));
    assert!(!output.status.success());
    assert!(output_text(&output).contains("checksum verification failed"));
    assert!(!fixture.destination.exists());
}

#[test]
fn invalid_parameter_fails_before_download() {
    let fixture = Fixture::new();
    assert!(!fixture.run(Some("invalid")).status.success());
    assert!(!fixture.downloads.exists());
    assert!(!fixture.destination.exists());
}

#[test]
fn invalid_environment_fails_before_download() {
    let mut fixture = Fixture::new();
    fixture
        .env
        .insert("FERRUS_INSTALL_COMPONENTS", "invalid".into());
    assert!(!fixture.run(None).status.success());
    assert!(!fixture.downloads.exists());
}

#[test]
fn unknown_parameter_fails_before_download() {
    let fixture = Fixture::new();
    assert!(!fixture.run_options(None, false, true).status.success());
    assert!(!fixture.downloads.exists());
}

#[test]
fn help_does_not_download() {
    let fixture = Fixture::new();
    let output = fixture.run_options(None, true, false);
    assert_success(&output);
    assert!(output_text(&output).contains("all|ferrus|nano"));
    assert!(!fixture.downloads.exists());
}

#[cfg(unix)]
#[test]
fn pipe_invocation_selects_nano() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command.args(["-s", "--", "--components", "nano"]);
    let script = fs::read(Path::new(ROOT).join("install.sh")).unwrap();
    assert_success(&fixture.execute(&mut command, Some(&script)));
    fixture.assert_installed(&["ferrus-nano"]);
}

#[cfg(unix)]
#[test]
fn equals_parameter() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command.arg(Path::new(ROOT).join("install.sh"));
    command.arg("--components=nano");
    assert_success(&fixture.execute(&mut command, None));
    fixture.assert_installed(&["ferrus-nano"]);
}

#[cfg(unix)]
#[test]
fn missing_parameter_value_fails_before_download() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command.arg(Path::new(ROOT).join("install.sh"));
    command.arg("--components");
    assert!(!fixture.execute(&mut command, None).status.success());
    assert!(!fixture.downloads.exists());
}

#[cfg(windows)]
#[test]
fn scriptblock_installs_nano() {
    let mut fixture = Fixture::new();
    fixture.env.insert("FERRUS_TEST_SCRIPTBLOCK", "1".into());
    assert_success(&fixture.run(Some("nano")));
    fixture.assert_installed(&["ferrus-nano"]);
}

#[cfg(windows)]
#[test]
fn scriptblock_supports_legacy_release() {
    let mut fixture = Fixture::new();
    fixture.env.insert("FERRUS_TEST_SCRIPTBLOCK", "1".into());
    fixture.make_archive(&["ferrus"]);
    assert_success(&fixture.run(Some("all")));
    fixture.assert_installed(&["ferrus"]);
}

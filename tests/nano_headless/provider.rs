//! Tests for provider.

use super::*;

#[cfg(unix)]
#[test]
fn native_registration_provisions_default_local_provider_once() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    let home = f.data.join("home");
    fs::create_dir(&home).unwrap();
    let settings = home.join(".ferrus/nano.toml");
    let before = fs::read(f.root.join("ferrus.toml")).unwrap();

    let missing_model = ferrus(&f.root)
        .args(["register", "--executor", "nano"])
        .env_remove("FERRUS_NANO_CONFIG")
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(!missing_model.status.success());
    assert!(String::from_utf8_lossy(&missing_model.stderr).contains("--executor-model"));
    assert_eq!(fs::read(f.root.join("ferrus.toml")).unwrap(), before);
    assert!(!settings.exists());

    let registered = ferrus(&f.root)
        .args([
            "register",
            "--executor",
            "nano",
            "--executor-model",
            "local/model",
        ])
        .env_remove("FERRUS_NANO_CONFIG")
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    let original = fs::read(&settings).unwrap();
    assert!(String::from_utf8_lossy(&original).contains("local/model"));
    private::check(&settings, false).unwrap();
    private::check(settings.parent().unwrap(), true).unwrap();
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["executor"]["model"].as_str(), Some(""));

    let changed_settings = "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'updated/model'\n";
    fs::write(&settings, changed_settings).unwrap();
    success(
        ferrus(&f.root)
            .args(["register", "--executor", "nano"])
            .env_remove("FERRUS_NANO_CONFIG")
            .env("HOME", &home),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["executor"]["model"].as_str(), Some(""));

    // Existing Ferrus homes may predate Nano and need not have private directory mode.
    fs::set_permissions(
        settings.parent().unwrap(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    success(
        ferrus(&f.root)
            .args([
                "register",
                "--executor",
                "nano",
                "--executor-model",
                "another-model",
            ])
            .env_remove("FERRUS_NANO_CONFIG")
            .env("HOME", &home),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(
        config["hq"]["executor"]["model"].as_str(),
        Some("another-model")
    );
    success(
        ferrus(&f.root)
            .args(["register", "--executor", "nano"])
            .env_remove("FERRUS_NANO_CONFIG")
            .env("HOME", &home),
    );
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
    assert_eq!(config["hq"]["executor"]["model"].as_str(), Some(""));
    assert_eq!(fs::read_to_string(&settings).unwrap(), changed_settings);
}

#[cfg(unix)]
#[test]
fn registering_both_native_roles_shares_the_bootstrap_model_but_preserves_overrides() {
    for supervisor_model in [None, Some("supervisor-model")] {
        let f = Fixture::new();
        let home = f.data.join("home");
        fs::create_dir(&home).unwrap();
        let mut command = ferrus(&f.root);
        command.args([
            "register",
            "--supervisor",
            "nano",
            "--executor",
            "nano",
            "--executor-model",
            "executor-model",
        ]);
        if let Some(model) = supervisor_model {
            command.args(["--supervisor-model", model]);
        }

        success(command.env_remove("FERRUS_NANO_CONFIG").env("HOME", &home));
        let settings: toml::Value =
            toml::from_str(&fs::read_to_string(home.join(".ferrus/nano.toml")).unwrap()).unwrap();
        assert_eq!(
            settings["model"].as_str(),
            Some(supervisor_model.unwrap_or("executor-model"))
        );
        let config: toml::Value =
            toml::from_str(&fs::read_to_string(f.root.join("ferrus.toml")).unwrap()).unwrap();
        assert_eq!(config["hq"]["supervisor"]["agent"].as_str(), Some("nano"));
        assert_eq!(config["hq"]["supervisor"]["model"].as_str(), Some(""));
        assert_eq!(
            config["hq"]["executor"]["model"].as_str(),
            Some(if supervisor_model.is_some() {
                "executor-model"
            } else {
                ""
            })
        );
    }
}

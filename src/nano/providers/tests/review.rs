//! Tests for review.

use super::*;

#[test]
fn provider_inputs_open_read_only_and_reject_unsafe_paths() {
    let directory = TempDir::new().unwrap();
    let key = directory.path().join("key");
    private::file(&key, true)
        .unwrap()
        .write_all(b"fixture-token")
        .unwrap();
    let settings = directory.path().join("nano.toml");
    let mut cfg = config("http://127.0.0.1:1234/v1");
    cfg.api_key_file = Some(key.clone());
    private::file(&settings, true)
        .unwrap()
        .write_all(b"base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\n")
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&key, &settings] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400)).unwrap();
        }
    }
    #[cfg(windows)]
    for path in [&key, &settings] {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    assert_eq!(Config::load(&settings).unwrap().model, "fixture-model");
    assert_eq!(
        cfg.authorization().unwrap().unwrap(),
        "Bearer fixture-token"
    );
    assert!(
        private::read_only_file(&key)
            .unwrap()
            .write_all(b"overwrite")
            .is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let link = directory.path().join("link");
        symlink(&key, &link).unwrap();
        cfg.api_key_file = Some(link.clone());
        assert!(cfg.authorization().is_err());
        assert!(Config::load(&link).is_err());
        cfg.api_key_file = Some(key.clone());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o444)).unwrap();
        assert!(cfg.authorization().is_err());
    }
    assert!(Config::load(directory.path()).is_err());
    assert_eq!(std::fs::read(&key).unwrap(), b"fixture-token");
    #[cfg(windows)]
    for path in [&key, &settings] {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

#[test]
fn host_configuration_rejects_inline_secrets_and_private_file_errors_without_echoing_them() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("nano.toml");
    private::file(&path, true).unwrap().write_all(b"base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\napi_key = 'inline-secret'\n").unwrap();
    let error = Config::load(&path).unwrap_err();
    assert!(!format!("{error:?}").contains("inline-secret"));
    std::fs::write(
        &path,
        "base_url = 'http://127.0.0.1:1234/v1'\nmodel = 'fixture-model'\n",
    )
    .unwrap();
    assert!(
        Config::load(&path)
            .unwrap()
            .authorization()
            .unwrap()
            .is_none()
    );
    let key = directory.path().join("key");
    private::file(&key, true)
        .unwrap()
        .write_all(b"bad\nsecret")
        .unwrap();
    let mut cfg = config("http://127.0.0.1:1234/v1");
    cfg.api_key_file = Some(key);
    assert!(!format!("{:?}", cfg.authorization().unwrap_err()).contains("secret"));
}

#[tokio::test]
async fn rejected_requests_keep_http_status_without_echoing_server_details() {
    for (status, kind, reason) in [
        (
            400,
            ProviderErrorKind::Unsupported,
            EndReason::ProviderProtocol,
        ),
        (
            404,
            ProviderErrorKind::Unsupported,
            EndReason::ProviderProtocol,
        ),
        (
            422,
            ProviderErrorKind::Unsupported,
            EndReason::ProviderProtocol,
        ),
        (
            401,
            ProviderErrorKind::Authentication,
            EndReason::ProviderFailed,
        ),
    ] {
        let (url, server) = server(vec![Reply::failure(status, "secret-code")]).await;
        let directory = TempDir::new().unwrap();
        let mut engine = engine(OpenAi::new(config(&url)).unwrap(), &directory);
        let end = engine
            .run(
                SessionCommand::Start {
                    input: "Use lookup".into(),
                },
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(end.reason, reason);
        assert_eq!(end.budget.model_turns, 1);
        assert_eq!(end.budget.retries, 0);
        assert_eq!(engine.tools.calls, 0);
        let record = engine
            .host
            .records
            .iter()
            .find(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
            .unwrap();
        assert!(
            matches!(&record.event, SessionEvent::ModelFailed { error: Some(actual), diagnostic: Some(ProviderDiagnostic::Http {status: actual_status}), .. } if actual == &kind && *actual_status == status)
        );
        let serialized = serde_json::to_string(record).unwrap();
        assert!(!serialized.contains("secret-code"));
        assert!(!serialized.contains("secret-server-detail"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn explicit_reasoning_rejection_never_falls_back_or_retries() {
    for status in [400, 422] {
        let (url, server) = server(vec![Reply::failure(status, "unsupported")]).await;
        let mut cfg = config(&url);
        cfg.reasoning_effort = Some(ReasoningEffort::Max);
        let directory = TempDir::new().unwrap();
        let mut engine = engine(OpenAi::new(cfg).unwrap(), &directory);
        let end = engine
            .run(
                SessionCommand::Start {
                    input: "Use lookup".into(),
                },
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(end.reason, EndReason::ProviderProtocol);
        assert_eq!(end.budget.model_turns, 1);
        assert_eq!(end.budget.retries, 0);
        assert_eq!(engine.tools.calls, 0);
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].1["reasoning_effort"], "max");
        assert_eq!(requests[0].1["stream_options"]["include_usage"], true);
    }
}

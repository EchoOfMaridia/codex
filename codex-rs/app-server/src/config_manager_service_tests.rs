use super::*;
use anyhow::Result;
use codex_app_server_protocol::AppConfig;
use codex_app_server_protocol::AppToolApproval;
use codex_app_server_protocol::AppsConfig;
use codex_app_server_protocol::AskForApproval;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::test_support::CloudConfigBundleFixture;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[test]
fn toml_value_to_item_handles_nested_config_tables() {
    let config = r#"
[mcp_servers.docs]
command = "docs-server"

[mcp_servers.docs.http_headers]
X-Doc = "42"
"#;

    let value: TomlValue = toml::from_str(config).expect("parse config example");
    let item = toml_value_to_item(&value).expect("convert to toml_edit item");

    let root = item.as_table().expect("root table");
    assert!(!root.is_implicit(), "root table should be explicit");

    let mcp_servers = root
        .get("mcp_servers")
        .and_then(TomlItem::as_table)
        .expect("mcp_servers table");
    assert!(
        !mcp_servers.is_implicit(),
        "mcp_servers table should be explicit"
    );

    let docs = mcp_servers
        .get("docs")
        .and_then(TomlItem::as_table)
        .expect("docs table");
    assert_eq!(
        docs.get("command")
            .and_then(TomlItem::as_value)
            .and_then(toml_edit::Value::as_str),
        Some("docs-server")
    );

    let http_headers = docs
        .get("http_headers")
        .and_then(TomlItem::as_table)
        .expect("http_headers table");
    assert_eq!(
        http_headers
            .get("X-Doc")
            .and_then(TomlItem::as_value)
            .and_then(toml_edit::Value::as_str),
        Some("42")
    );
}

#[tokio::test]
async fn write_value_preserves_comments_and_order() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let original = r#"# Codex user configuration
model = "gpt-5.2"
approval_policy = "on-request"

[notice]
# Preserve this comment
hide_full_access_warning = true

[features]
unified_exec = true
"#;
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), original)?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "features.personality".to_string(),
            value: serde_json::json!(true),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write succeeds");

    let updated = std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"# Codex user configuration
model = "gpt-5.2"
approval_policy = "on-request"

[notice]
# Preserve this comment
hide_full_access_warning = true

[features]
unified_exec = true
personality = true
"#;
    assert_eq!(updated, expected);
    Ok(())
}

#[tokio::test]
async fn clear_missing_nested_config_is_noop() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&path, "")?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let response = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "features.personality".to_string(),
            value: serde_json::Value::Null,
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("clear missing config succeeds");

    assert_eq!(response.status, WriteStatus::Ok);
    assert_eq!(response.overridden_metadata, None);
    assert_eq!(std::fs::read_to_string(&path)?, "");
    Ok(())
}

#[tokio::test]
async fn write_value_rejects_legacy_profile_selector() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&path, "model = \"gpt-main\"\n")?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "profile".to_string(),
            value: serde_json::json!("work"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect_err("legacy profile selector write should fail");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigValidationError)
    );
    assert!(
        error
            .to_string()
            .contains("`profile` is a legacy config selector"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path)?, "model = \"gpt-main\"\n");
    Ok(())
}

#[tokio::test]
async fn write_value_rejects_legacy_profile_table() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&path, "")?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "profiles.work.model".to_string(),
            value: serde_json::json!("gpt-work"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect_err("legacy profile table write should fail");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigValidationError)
    );
    assert!(
        error
            .to_string()
            .contains("`profiles` contains legacy config profile tables"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path)?, "");
    Ok(())
}

#[tokio::test]
async fn batch_write_rejects_legacy_profile_selector() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&path, "model = \"gpt-main\"\n")?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let error = service
        .batch_write(ConfigBatchWriteParams {
            edits: vec![
                codex_app_server_protocol::ConfigEdit {
                    key_path: "model".to_string(),
                    value: serde_json::json!("gpt-work"),
                    merge_strategy: MergeStrategy::Replace,
                },
                codex_app_server_protocol::ConfigEdit {
                    key_path: "profile".to_string(),
                    value: serde_json::json!("work"),
                    merge_strategy: MergeStrategy::Replace,
                },
            ],
            file_path: Some(path.display().to_string()),
            expected_version: None,
            reload_user_config: false,
        })
        .await
        .expect_err("legacy profile selector batch write should fail");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigValidationError)
    );
    assert!(
        error
            .to_string()
            .contains("`profile` is a legacy config selector"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path)?, "model = \"gpt-main\"\n");
    Ok(())
}

#[tokio::test]
async fn write_value_supports_nested_app_paths() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "")?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "apps".to_string(),
            value: serde_json::json!({
                "app1": {
                    "enabled": false,
                },
            }),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write apps succeeds");

    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "apps.app1.default_tools_approval_mode".to_string(),
            value: serde_json::json!("prompt"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write apps.app1.default_tools_approval_mode succeeds");

    let read = service
        .read(ConfigReadParams {
            include_layers: false,
            cwd: None,
        })
        .await
        .expect("config read succeeds");

    assert_eq!(
        read.config.apps,
        Some(AppsConfig {
            default: None,
            apps: std::collections::HashMap::from([(
                "app1".to_string(),
                AppConfig {
                    enabled: false,
                    approvals_reviewer: None,
                    destructive_enabled: None,
                    open_world_enabled: None,
                    default_tools_approval_mode: Some(AppToolApproval::Prompt),
                    default_tools_enabled: None,
                    tools: None,
                },
            )]),
        })
    );

    Ok(())
}

#[tokio::test]
async fn write_value_supports_custom_mcp_server_default_tool_approval_mode() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        "[mcp_servers.docs]\ncommand = \"docs-server\"\n",
    )?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "mcp_servers.docs.default_tools_approval_mode".to_string(),
            value: serde_json::json!("approve"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write mcp server default_tools_approval_mode succeeds");

    let contents = std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE))?;
    assert!(contents.contains("default_tools_approval_mode = \"approve\""));

    let read = service
        .read(ConfigReadParams {
            include_layers: false,
            cwd: None,
        })
        .await
        .expect("config read succeeds");

    assert_eq!(
        read.config
            .additional
            .get("mcp_servers")
            .and_then(|servers| servers.get("docs"))
            .and_then(|docs| docs.get("default_tools_approval_mode")),
        Some(&serde_json::json!("approve"))
    );

    Ok(())
}

#[tokio::test]
async fn read_includes_origins_and_layers() {
    let tmp = tempdir().expect("tempdir");
    let user_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&user_path, "model = \"user\"").unwrap();
    let user_file = AbsolutePathBuf::try_from(user_path.clone()).expect("user file");

    let managed_path = tmp.path().join("managed_config.toml");
    std::fs::write(&managed_path, "approval_policy = \"never\"").unwrap();
    let managed_file = AbsolutePathBuf::try_from(managed_path.clone()).expect("managed file");

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        LoaderOverrides::with_managed_config_path_for_tests(managed_path.clone()),
        CloudConfigBundleLoader::default(),
    );

    let response = service
        .read(ConfigReadParams {
            include_layers: true,
            cwd: None,
        })
        .await
        .expect("response");

    assert_eq!(response.config.approval_policy, Some(AskForApproval::Never));

    assert_eq!(
        response
            .origins
            .get("approval_policy")
            .expect("origin")
            .name,
        ConfigLayerSource::LegacyManagedConfigTomlFromFile {
            file: managed_file.clone()
        },
    );
    let layers = response.layers.expect("layers present");
    // Local macOS machines can surface an MDM-managed config layer at the
    // top of the stack; ignore it so this test stays focused on file/user/system ordering.
    let layers = if matches!(
        layers.first().map(|layer| &layer.name),
        Some(ConfigLayerSource::LegacyManagedConfigTomlFromMdm)
    ) {
        &layers[1..]
    } else {
        layers.as_slice()
    };
    assert_eq!(layers.len(), 3, "expected three layers");
    assert_eq!(
        layers.first().unwrap().name,
        ConfigLayerSource::LegacyManagedConfigTomlFromFile {
            file: managed_file.clone()
        }
    );
    assert_eq!(
        layers.get(1).unwrap().name,
        ConfigLayerSource::User {
            file: user_file.clone(),
            profile: None,
        }
    );
    assert!(matches!(
        layers.get(2).unwrap().name,
        ConfigLayerSource::System { .. }
    ));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn write_value_succeeds_when_managed_preferences_expand_home_directory_paths() -> Result<()> {
    use base64::Engine;

    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "model = \"user\"\n")?;

    let mut loader_overrides =
        LoaderOverrides::with_managed_config_path_for_tests(tmp.path().join("managed_config.toml"));
    loader_overrides.managed_preferences_base64 = Some(
        base64::prelude::BASE64_STANDARD.encode(
            r#"
sandbox_mode = "workspace-write"
[sandbox_workspace_write]
writable_roots = ["~/code"]
"#
            .as_bytes(),
        ),
    );

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        loader_overrides,
        CloudConfigBundleLoader::default(),
    );

    let response = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "model".to_string(),
            value: serde_json::json!("updated"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write succeeds");

    assert_eq!(response.status, WriteStatus::Ok);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read config"),
        "model = \"updated\"\n"
    );

    Ok(())
}

#[tokio::test]
async fn write_value_reports_override() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        "approval_policy = \"on-request\"",
    )
    .unwrap();

    let managed_path = tmp.path().join("managed_config.toml");
    std::fs::write(&managed_path, "approval_policy = \"never\"").unwrap();
    let managed_file = AbsolutePathBuf::try_from(managed_path.clone()).expect("managed file");

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        LoaderOverrides::with_managed_config_path_for_tests(managed_path.clone()),
        CloudConfigBundleLoader::default(),
    );

    let result = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "approval_policy".to_string(),
            value: serde_json::json!("never"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("result");

    let read_after = service
        .read(ConfigReadParams {
            include_layers: true,
            cwd: None,
        })
        .await
        .expect("read");
    assert_eq!(
        read_after.config.approval_policy,
        Some(AskForApproval::Never)
    );
    assert_eq!(
        read_after
            .origins
            .get("approval_policy")
            .expect("origin")
            .name,
        ConfigLayerSource::LegacyManagedConfigTomlFromFile {
            file: managed_file.clone()
        }
    );
    assert_eq!(result.status, WriteStatus::Ok);
    assert!(result.overridden_metadata.is_none());
}

#[tokio::test]
async fn version_conflict_rejected() {
    let tmp = tempdir().expect("tempdir");
    let user_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&user_path, "model = \"user\"").unwrap();

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "model".to_string(),
            value: serde_json::json!("gpt-5.2"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: Some("sha256:bogus".to_string()),
        })
        .await
        .expect_err("should fail");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigVersionConflict)
    );
}

#[tokio::test]
async fn write_value_defaults_to_user_config_path() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "").unwrap();

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: None,
            key_path: "model".to_string(),
            value: serde_json::json!("gpt-new"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write succeeds");

    let contents = std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read config");
    assert!(
        contents.contains("model = \"gpt-new\""),
        "config.toml should be updated even when file_path is omitted"
    );
}

#[tokio::test]
async fn write_value_defaults_to_selected_user_config_path() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "model = \"gpt-main\"").unwrap();
    let selected_path = tmp.path().join("work.config.toml");
    std::fs::write(&selected_path, "").unwrap();

    let mut loader_overrides =
        LoaderOverrides::with_managed_config_path_for_tests(tmp.path().join("managed_config.toml"));
    loader_overrides.user_config_path =
        Some(AbsolutePathBuf::from_absolute_path(&selected_path).expect("selected config path"));
    loader_overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));
    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        loader_overrides,
        CloudConfigBundleLoader::default(),
    );
    service
        .write_value(ConfigValueWriteParams {
            file_path: None,
            key_path: "model".to_string(),
            value: serde_json::json!("gpt-work"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("write succeeds");

    assert_eq!(
        std::fs::read_to_string(&selected_path).expect("read selected config"),
        "model = \"gpt-work\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read main config"),
        "model = \"gpt-main\""
    );
}

#[tokio::test]
async fn load_default_config_preserves_selected_user_config_path_after_load_error() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "model = \"gpt-main\"").unwrap();
    let selected_path = tmp.path().join("work.config.toml");
    std::fs::write(&selected_path, "not valid toml").unwrap();
    let selected_file =
        AbsolutePathBuf::from_absolute_path(&selected_path).expect("selected config path");

    let mut loader_overrides =
        LoaderOverrides::with_managed_config_path_for_tests(tmp.path().join("managed_config.toml"));
    loader_overrides.user_config_path = Some(selected_file.clone());
    loader_overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));
    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        loader_overrides,
        CloudConfigBundleLoader::default(),
    );

    service
        .load_latest_config(/*fallback_cwd*/ None)
        .await
        .expect_err("selected config should fail to load");
    let config = service
        .load_default_config()
        .await
        .expect("default config loads after selected config error");

    assert_eq!(
        config.config_layer_stack.get_user_config_file(),
        Some(&selected_file)
    );
}

#[tokio::test]
async fn invalid_user_value_rejected_even_if_overridden_by_managed() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "model = \"user\"").unwrap();

    let managed_path = tmp.path().join("managed_config.toml");
    std::fs::write(&managed_path, "approval_policy = \"never\"").unwrap();

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        LoaderOverrides::with_managed_config_path_for_tests(managed_path.clone()),
        CloudConfigBundleLoader::default(),
    );

    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "approval_policy".to_string(),
            value: serde_json::json!("bogus"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect_err("should fail validation");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigValidationError)
    );

    let contents = std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents.trim(), "model = \"user\"");
}

#[tokio::test]
async fn reserved_builtin_provider_override_rejected() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "model = \"user\"\n").unwrap();

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "model_providers.openai.name".to_string(),
            value: serde_json::json!("OpenAI Override"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect_err("should reject reserved provider override");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigValidationError)
    );
    assert!(error.to_string().contains("reserved built-in provider IDs"));
    assert!(error.to_string().contains("`openai`"));

    let contents = std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, "model = \"user\"\n");
}

#[tokio::test]
async fn write_value_rejects_feature_requirement_conflict() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "").unwrap();

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        LoaderOverrides::without_managed_config_for_tests(),
        CloudConfigBundleFixture::loader_with_enterprise_requirement(
            r#"
[features]
personality = true
"#,
        ),
    );

    let error = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "features.personality".to_string(),
            value: serde_json::json!(false),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect_err("conflicting feature write should fail");

    assert_eq!(
        error.write_error_code(),
        Some(ConfigWriteErrorCode::ConfigValidationError)
    );
    assert!(
        error
            .to_string()
            .contains("invalid value for `features`: `features.personality=false`"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).unwrap(),
        ""
    );
}

#[tokio::test]
async fn read_reports_managed_overrides_user_and_session_flags() {
    let tmp = tempdir().expect("tempdir");
    let user_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&user_path, "model = \"user\"").unwrap();
    let user_file = AbsolutePathBuf::try_from(user_path.clone()).expect("user file");

    let managed_path = tmp.path().join("managed_config.toml");
    std::fs::write(&managed_path, "model = \"system\"").unwrap();
    let managed_file = AbsolutePathBuf::try_from(managed_path.clone()).expect("managed file");

    let cli_overrides = vec![(
        "model".to_string(),
        TomlValue::String("session".to_string()),
    )];

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        cli_overrides,
        LoaderOverrides::with_managed_config_path_for_tests(managed_path.clone()),
        CloudConfigBundleLoader::default(),
    );

    let response = service
        .read(ConfigReadParams {
            include_layers: true,
            cwd: None,
        })
        .await
        .expect("response");

    assert_eq!(response.config.model.as_deref(), Some("system"));
    assert_eq!(
        response.origins.get("model").expect("origin").name,
        ConfigLayerSource::LegacyManagedConfigTomlFromFile {
            file: managed_file.clone()
        },
    );
    let layers = response.layers.expect("layers");
    // Local macOS machines can surface an MDM-managed config layer at the
    // top of the stack; ignore it so this test stays focused on file/session/user ordering.
    let layers = if matches!(
        layers.first().map(|layer| &layer.name),
        Some(ConfigLayerSource::LegacyManagedConfigTomlFromMdm)
    ) {
        &layers[1..]
    } else {
        layers.as_slice()
    };
    assert_eq!(
        layers.first().unwrap().name,
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file: managed_file }
    );
    assert_eq!(layers.get(1).unwrap().name, ConfigLayerSource::SessionFlags);
    assert_eq!(
        layers.get(2).unwrap().name,
        ConfigLayerSource::User {
            file: user_file,
            profile: None
        }
    );
}

#[tokio::test]
async fn write_value_reports_managed_override() {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), "").unwrap();

    let managed_path = tmp.path().join("managed_config.toml");
    std::fs::write(&managed_path, "approval_policy = \"never\"").unwrap();
    let managed_file = AbsolutePathBuf::try_from(managed_path.clone()).expect("managed file");

    let service = ConfigManager::new_for_tests(
        tmp.path().to_path_buf(),
        vec![],
        LoaderOverrides::with_managed_config_path_for_tests(managed_path.clone()),
        CloudConfigBundleLoader::default(),
    );

    let result = service
        .write_value(ConfigValueWriteParams {
            file_path: Some(tmp.path().join(CONFIG_TOML_FILE).display().to_string()),
            key_path: "approval_policy".to_string(),
            value: serde_json::json!("on-request"),
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("result");

    assert_eq!(result.status, WriteStatus::OkOverridden);
    let overridden = result.overridden_metadata.expect("overridden metadata");
    assert_eq!(
        overridden.overriding_layer.name,
        ConfigLayerSource::LegacyManagedConfigTomlFromFile { file: managed_file }
    );
    assert_eq!(overridden.effective_value, serde_json::json!("never"));
}

#[tokio::test]
async fn upsert_merges_tables_replace_overwrites() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    let base = r#"[mcp_servers.linear]
bearer_token_env_var = "TOKEN"
name = "linear"
url = "https://linear.example"

[mcp_servers.linear.env_http_headers]
existing = "keep"

[mcp_servers.linear.http_headers]
alpha = "a"
"#;

    let overlay = serde_json::json!({
        "bearer_token_env_var": "NEW_TOKEN",
        "http_headers": {
            "alpha": "updated",
            "beta": "b"
        },
        "name": "linear",
        "url": "https://linear.example"
    });

    std::fs::write(&path, base)?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "mcp_servers.linear".to_string(),
            value: overlay.clone(),
            merge_strategy: MergeStrategy::Upsert,
            expected_version: None,
        })
        .await
        .expect("upsert succeeds");

    let upserted: TomlValue = toml::from_str(&std::fs::read_to_string(&path)?)?;
    let expected_upsert: TomlValue = toml::from_str(
        r#"[mcp_servers.linear]
bearer_token_env_var = "NEW_TOKEN"
name = "linear"
url = "https://linear.example"

[mcp_servers.linear.env_http_headers]
existing = "keep"

[mcp_servers.linear.http_headers]
alpha = "updated"
beta = "b"
"#,
    )?;
    assert_eq!(upserted, expected_upsert);

    std::fs::write(&path, base)?;

    service
        .write_value(ConfigValueWriteParams {
            file_path: Some(path.display().to_string()),
            key_path: "mcp_servers.linear".to_string(),
            value: overlay,
            merge_strategy: MergeStrategy::Replace,
            expected_version: None,
        })
        .await
        .expect("replace succeeds");

    let replaced: TomlValue = toml::from_str(&std::fs::read_to_string(&path)?)?;
    let expected_replace: TomlValue = toml::from_str(
        r#"[mcp_servers.linear]
bearer_token_env_var = "NEW_TOKEN"
name = "linear"
url = "https://linear.example"

[mcp_servers.linear.http_headers]
alpha = "updated"
beta = "b"
"#,
    )?;
    assert_eq!(replaced, expected_replace);

    Ok(())
}

#[tokio::test]
async fn batch_write_preserves_existing_mcp_server_with_subtable() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    let original = r#"model = "MiniMax-M3"

[mcp_servers.minimax]
command = "uvx"
args = ["minimax-coding-plan-mcp", "-y"]
startup_timeout_sec = 30.0
tool_timeout_sec = 60.0

[mcp_servers.minimax.env]
MINIMAX_API_HOST = "https://api.minimax.io"
MINIMAX_API_KEY = "sk-cp-test"
"#;
    std::fs::write(&path, original)?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    let response = service
        .batch_write(ConfigBatchWriteParams {
            edits: vec![
                codex_app_server_protocol::ConfigEdit {
                    key_path: "model".to_string(),
                    value: serde_json::json!("MiniMax-M3"),
                    merge_strategy: MergeStrategy::Replace,
                },
                codex_app_server_protocol::ConfigEdit {
                    key_path: "model_reasoning_effort".to_string(),
                    value: serde_json::json!("medium"),
                    merge_strategy: MergeStrategy::Replace,
                },
            ],
            file_path: Some(path.display().to_string()),
            expected_version: None,
            reload_user_config: false,
        })
        .await
        .expect("batch write succeeds for unchanged model");

    assert_eq!(response.status, WriteStatus::Ok);

    let updated = std::fs::read_to_string(&path)?;
    let expected = r#"model = "MiniMax-M3"
model_reasoning_effort = "medium"

[mcp_servers.minimax]
command = "uvx"
args = ["minimax-coding-plan-mcp", "-y"]
startup_timeout_sec = 30.0
tool_timeout_sec = 60.0

[mcp_servers.minimax.env]
MINIMAX_API_HOST = "https://api.minimax.io"
MINIMAX_API_KEY = "sk-cp-test"
"#;
    assert_eq!(updated, expected);
    Ok(())
}

#[tokio::test]
async fn batch_write_upsert_through_nested_path_preserves_mcp_server() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    let original = r#"model = "MiniMax-M3"

[mcp_servers.minimax]
command = "uvx"
args = ["minimax-coding-plan-mcp", "-y"]
startup_timeout_sec = 30.0
tool_timeout_sec = 60.0

[mcp_servers.minimax.env]
MINIMAX_API_HOST = "https://api.minimax.io"
MINIMAX_API_KEY = "sk-cp-test"
"#;
    std::fs::write(&path, original)?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    service
        .batch_write(ConfigBatchWriteParams {
            edits: vec![codex_app_server_protocol::ConfigEdit {
                key_path: "hooks.state".to_string(),
                value: serde_json::json!({
                    "my-hook": { "enabled": true }
                }),
                merge_strategy: MergeStrategy::Upsert,
            }],
            file_path: Some(path.display().to_string()),
            expected_version: None,
            reload_user_config: false,
        })
        .await
        .expect("hooks upsert preserves mcp_servers");

    let updated = std::fs::read_to_string(&path)?;
    assert!(
        updated.contains("[mcp_servers.minimax]"),
        "minimax table missing after hooks upsert: {updated}"
    );
    assert!(
        updated.contains("command = \"uvx\""),
        "command missing after hooks upsert: {updated}"
    );
    Ok(())
}

#[tokio::test]
async fn batch_write_trust_toggle_preserves_mcp_server() -> Result<()> {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join(CONFIG_TOML_FILE);
    let original = r#"model = "MiniMax-M3"

[mcp_servers.minimax]
command = "uvx"
args = ["minimax-coding-plan-mcp", "-y"]
startup_timeout_sec = 30.0
tool_timeout_sec = 60.0

[mcp_servers.minimax.env]
MINIMAX_API_HOST = "https://api.minimax.io"
MINIMAX_API_KEY = "sk-cp-test"
"#;
    std::fs::write(&path, original)?;

    let service = ConfigManager::without_managed_config_for_tests(tmp.path().to_path_buf());
    // The TUI trust toggle sends a quoted project key path: projects."/path".trust_level
    service
        .batch_write(ConfigBatchWriteParams {
            edits: vec![codex_app_server_protocol::ConfigEdit {
                key_path: "projects.\"/work/proj\".trust_level".to_string(),
                value: serde_json::json!("trusted"),
                merge_strategy: MergeStrategy::Replace,
            }],
            file_path: Some(path.display().to_string()),
            expected_version: None,
            reload_user_config: false,
        })
        .await
        .expect("trust toggle preserves mcp_servers");

    let updated = std::fs::read_to_string(&path)?;
    assert!(
        updated.contains("[mcp_servers.minimax]"),
        "minimax table missing after trust toggle: {updated}"
    );
    assert!(
        updated.contains("command = \"uvx\""),
        "command missing after trust toggle: {updated}"
    );
    Ok(())
}

#[tokio::test]
async fn apply_merge_rejects_nested_write_into_scalar_parent() {
    // Regression guard: the parents-traversal loop must not silently overwrite a
    // scalar/array value with an empty table when a caller attempts to set a
    // nested key whose intermediate parent is not a table. Returning an error
    // preserves sibling data such as `[mcp_servers.minimax]`.
    let mut root: TomlValue = toml::from_str(
        r#"model = "MiniMax-M3"

[mcp_servers.minimax]
command = "uvx"
"#,
    )
    .expect("parse");

    // Two parents so the second iteration hits the `_ =>` arm of the
    // parents-traversal match.  "model" is a scalar, so the second step
    // cannot reasonably replace it with an empty table.
    let segments = vec![
        "model".to_string(),
        "nested".to_string(),
        "deep".to_string(),
    ];
    let value = TomlValue::String("oops".to_string());
    let err = apply_merge(&mut root, &segments, Some(&value), MergeStrategy::Replace)
        .expect_err("writing into a scalar parent must fail");
    let MergeError::Validation(message) = err;
    assert!(
        message.contains("`nested`"),
        "error should mention the conflicting parent segment: {message}"
    );

    let mcp_servers = root
        .get("mcp_servers")
        .and_then(TomlValue::as_table)
        .expect("mcp_servers table preserved");
    let minimax = mcp_servers
        .get("minimax")
        .and_then(TomlValue::as_table)
        .expect("minimax table preserved");
    assert_eq!(
        minimax.get("command").and_then(TomlValue::as_str),
        Some("uvx"),
        "minimax.command must not be clobbered by the failed merge"
    );
}

#[tokio::test]
async fn batch_write_with_profile_v2_overlay_preserves_mcp_server() -> Result<()> {
    // Regression test for "Invalid configuration: invalid transport in `mcp_servers.minimax`".
    //
    // When the TUI is started with `-p <name>`, the active user layer is the
    // profile-v2 overlay (e.g. `minimax-m3.config.toml`), not the base
    // `config.toml`. The profile is meant to layer overrides on top of the
    // base; it does not have to be a complete, valid `ConfigToml` on its
    // own. In particular, the user's profile only sets per-tool approval
    // overrides such as `[mcp_servers.minimax.tools.web_search]` and never
    // re-declares the `command`/`url` from the base.
    //
    // Before the fix, `apply_edits` validated the *raw* user layer, so the
    // profile overlay (which has no `command`/`url`) failed
    // `McpServerConfig::try_from` with "invalid transport" — blocking
    // *every* config write (trust toggle, model change, skill enable, etc.)
    // for any user running the TUI under `-p`. The fix validates the merged
    // effective config, which is the right place to catch transport errors
    // because that is what the user actually experiences.
    use codex_config::CONFIG_TOML_FILE;
    use codex_config::LoaderOverrides;

    let tmp = tempdir().expect("tempdir");
    let codex_home = tmp.path().to_path_buf();

    // Write the user's main config.toml (base layer) and the profile-v2 (overlay)
    let base_config =
        std::fs::read_to_string("/tmp/user_config_backup.toml").expect("read user config");
    std::fs::write(codex_home.join(CONFIG_TOML_FILE), &base_config)?;

    let profile_config =
        std::fs::read_to_string("/home/cage/.codex/minimax-m3.config.toml").expect("read profile");
    std::fs::write(codex_home.join("minimax-m3.config.toml"), &profile_config)?;

    // Point the loader at the profile
    let mut loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    loader_overrides.user_config_path = Some(
        codex_utils_absolute_path::AbsolutePathBuf::resolve_path_against_base(
            "minimax-m3.config.toml",
            &codex_home,
        ),
    );
    loader_overrides.user_config_profile = Some("minimax-m3".parse().expect("profile-v2 name"));

    let service = ConfigManager::new_for_tests(
        codex_home.clone(),
        Vec::new(),
        loader_overrides,
        CloudConfigBundleLoader::default(),
    );

    // This is what the TUI does for a trust toggle
    let result = service
        .batch_write(codex_app_server_protocol::ConfigBatchWriteParams {
            edits: vec![codex_app_server_protocol::ConfigEdit {
                key_path: r#"projects."/home/cage/Desktop/Workspaces/codex-fork".trust_level"#
                    .to_string(),
                value: serde_json::json!("trusted"),
                merge_strategy: MergeStrategy::Replace,
            }],
            file_path: None,
            expected_version: None,
            reload_user_config: true,
        })
        .await;

    eprintln!("RESULT: {result:#?}");
    assert!(
        result.is_ok(),
        "trust toggle must succeed even when the active user layer is a profile-v2 overlay whose mcp_servers only carry tool overrides; got {result:#?}"
    );
    Ok(())
}

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::rollout_path;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server::in_process;
use codex_app_server::in_process::InProcessStartArgs;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::FeedbackUploadParams;
use codex_app_server_protocol::FeedbackUploadResponse;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadResumeParams;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_exec_server::EnvironmentManager;
use codex_feedback::CodexFeedback;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::McpToolCallEndEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_rollout::append_rollout_item_to_path;
use codex_rollout::state_db::reconcile_rollout;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use sentry::Envelope;
use sentry::Transport;
use serde_json::json;
use tempfile::TempDir;

const PROJECT_ID: &str = "appgprj_6a239a76a4b08191986eb3712f427df9";
const VERSION_ID: &str =
    "appgprj_6a239a76a4b08191986eb3712f427df9~appgver_a24820415a508191a28bd3a66b59b3f9";
const DEPLOYMENT_ID: &str = "appgdep_6a1f3df81a0c81919722b3bd1ec5c657";

#[derive(Default)]
struct CapturingTransport {
    event_tags: Mutex<Vec<BTreeMap<String, String>>>,
}

impl CapturingTransport {
    fn take_canonical_tags(&self) -> BTreeMap<String, String> {
        let tags = self
            .event_tags
            .lock()
            .expect("capture transport mutex should not be poisoned")
            .remove(0);
        tags.into_iter()
            .filter(|(key, _)| {
                matches!(key.as_str(), "project_id" | "version_id" | "deployment_id")
            })
            .collect()
    }
}

impl Transport for CapturingTransport {
    fn send_envelope(&self, envelope: Envelope) {
        let tags = envelope
            .event()
            .expect("feedback envelope should contain an event")
            .tags
            .clone();
        self.event_tags
            .lock()
            .expect("capture transport mutex should not be poisoned")
            .push(tags);
    }
}

#[tokio::test]
async fn feedback_upload_projects_cold_and_live_history_into_sentry_tags() -> Result<()> {
    let responses_server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &responses_server.uri(),
        &BTreeMap::new(),
        /*auto_compact_limit*/ 8_192,
        Some(false),
        "mock_provider",
        "compact",
    )?;
    let filename_ts = "2026-06-17T12-00-00";
    let timestamp = "2026-06-17T19:00:00Z";
    let thread_id = create_fake_rollout(
        codex_home.path(),
        filename_ts,
        timestamp,
        "deploy the site",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let rollout_path = rollout_path(codex_home.path(), filename_ts, &thread_id);
    append_appgen_call(&rollout_path).await?;

    let state_db =
        StateRuntime::init(codex_home.path().to_path_buf(), "mock_provider".into()).await?;
    reconcile_rollout(
        Some(&state_db),
        &rollout_path,
        "mock_provider",
        /*builder*/ None,
        &[],
        /*archived_only*/ None,
        /*new_thread_memory_mode*/ None,
    )
    .await;

    let loader_overrides = LoaderOverrides::without_managed_config_for_tests();
    let config = Arc::new(
        ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .loader_overrides(loader_overrides.clone())
            .build()
            .await?,
    );
    let transport = Arc::new(CapturingTransport::default());
    let feedback = CodexFeedback::new_with_transport(Arc::new(Arc::clone(&transport)));
    let client = start_client(config, loader_overrides, feedback, Arc::clone(&state_db)).await?;

    let expected = BTreeMap::from([
        ("deployment_id".to_string(), DEPLOYMENT_ID.to_string()),
        ("project_id".to_string(), PROJECT_ID.to_string()),
        ("version_id".to_string(), VERSION_ID.to_string()),
    ]);
    upload_feedback(&client, 1, Some(&thread_id), None).await?;
    assert_eq!(transport.take_canonical_tags(), expected);

    client
        .request(ClientRequest::ThreadResume {
            request_id: RequestId::Integer(2),
            params: ThreadResumeParams {
                thread_id: thread_id.clone(),
                ..Default::default()
            },
        })
        .await?
        .expect("thread/resume should succeed");
    upload_feedback(&client, 3, Some(&thread_id), None).await?;
    assert_eq!(transport.take_canonical_tags(), expected);

    let caller_project_id = "appgprj_ffffffffffffffffffffffffffffffff";
    upload_feedback(
        &client,
        4,
        Some(&thread_id),
        Some(BTreeMap::from([(
            "project_id".to_string(),
            caller_project_id.to_string(),
        )])),
    )
    .await?;
    assert_eq!(
        transport.take_canonical_tags(),
        BTreeMap::from([("project_id".to_string(), caller_project_id.to_string())])
    );

    upload_feedback(&client, 5, /*thread_id*/ None, None).await?;
    assert_eq!(transport.take_canonical_tags(), BTreeMap::new());

    client.shutdown().await?;
    Ok(())
}

async fn start_client(
    config: Arc<Config>,
    loader_overrides: LoaderOverrides,
    feedback: CodexFeedback,
    state_db: Arc<StateRuntime>,
) -> Result<in_process::InProcessClientHandle> {
    Ok(in_process::start(InProcessStartArgs {
        arg0_paths: Arg0DispatchPaths::default(),
        config,
        cli_overrides: Vec::new(),
        loader_overrides,
        strict_config: false,
        cloud_config_bundle: CloudConfigBundleLoader::default(),
        thread_config_loader: Arc::new(codex_config::NoopThreadConfigLoader),
        feedback,
        log_db: None,
        state_db: Some(state_db),
        environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
        config_warnings: Vec::new(),
        session_source: SessionSource::VSCode,
        enable_codex_api_key_env: false,
        initialize: InitializeParams {
            client_info: ClientInfo {
                name: "codex-app-server-tests".to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            capabilities: Some(InitializeCapabilities {
                experimental_api: true,
                ..Default::default()
            }),
        },
        channel_capacity: in_process::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
    })
    .await?)
}

async fn upload_feedback(
    client: &in_process::InProcessClientHandle,
    request_id: i64,
    thread_id: Option<&str>,
    tags: Option<BTreeMap<String, String>>,
) -> Result<FeedbackUploadResponse> {
    let result = client
        .request(ClientRequest::FeedbackUpload {
            request_id: RequestId::Integer(request_id),
            params: FeedbackUploadParams {
                classification: "bug".to_string(),
                reason: Some("Sites feedback correlation E2E".to_string()),
                thread_id: thread_id.map(str::to_string),
                include_logs: false,
                extra_log_files: None,
                tags,
            },
        })
        .await?
        .expect("feedback/upload should succeed");
    Ok(serde_json::from_value(result)?)
}

async fn append_appgen_call(path: &std::path::Path) -> Result<()> {
    let payload = EventMsg::McpToolCallEnd(McpToolCallEndEvent {
        call_id: "sites-call-1".to_string(),
        invocation: McpInvocation {
            server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
            tool: "site_creator_publish".to_string(),
            arguments: Some(json!({
                "project_id": PROJECT_ID,
                "version_id": VERSION_ID,
                "deployment_id": DEPLOYMENT_ID,
            })),
        },
        mcp_app_resource_uri: None,
        plugin_id: None,
        duration: Duration::from_millis(8),
        result: Ok(CallToolResult {
            content: Vec::new(),
            structured_content: None,
            is_error: Some(false),
            meta: None,
        }),
    });
    append_rollout_item_to_path(path, &RolloutItem::EventMsg(payload)).await?;
    Ok(())
}

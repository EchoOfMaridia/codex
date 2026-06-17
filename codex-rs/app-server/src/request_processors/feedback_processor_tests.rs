use super::*;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::McpToolCallEndEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;

#[test]
fn appgen_feedback_tags_collect_latest_ids_from_persisted_calls() {
    let project_id = "appgprj_6a239a76a4b08191986eb3712f427df9";
    let version_id =
        "appgprj_6a239a76a4b08191986eb3712f427df9~appgver_a24820415a508191a28bd3a66b59b3f9";
    let deployment_id = "appgdep_6a1f3df81a0c81919722b3bd1ec5c657";
    let items = vec![
        legacy_mcp_call(
            "call-1",
            json!({
                "project_id": project_id,
                "version_id": version_id,
            }),
        ),
        mcp_call(
            codex_mcp::CODEX_APPS_MCP_SERVER_NAME,
            "call-2",
            json!({
                "project_id": project_id,
                "deployment_id": deployment_id,
            }),
            CallOutcome::ToolError,
        ),
        mcp_call(
            codex_mcp::CODEX_APPS_MCP_SERVER_NAME,
            "call-3",
            json!({
                "project_id": "appgprj_ffffffffffffffffffffffffffffffff",
            }),
            CallOutcome::LocalFailure,
        ),
    ];

    assert_eq!(
        appgen_feedback_tags(&items),
        BTreeMap::from([
            ("deployment_id".to_string(), deployment_id.to_string()),
            ("project_id".to_string(), project_id.to_string()),
            ("version_id".to_string(), version_id.to_string()),
        ])
    );
}

#[test]
fn appgen_feedback_tags_clear_ids_when_the_project_changes() {
    let project_id = "appgprj_6a239a76a4b08191986eb3712f427df9";
    let items = vec![
        legacy_mcp_call(
            "call-1",
            json!({
                "project_id": "appgprj_00000000000000000000000000000000",
                "version_id": "appgprj_00000000000000000000000000000000~appgver_a24820415a508191a28bd3a66b59b3f9",
            }),
        ),
        legacy_mcp_call(
            "call-2",
            json!({
                "project_id": project_id,
                "version_id": "appgprj_00000000000000000000000000000000~appgver_a24820415a508191a28bd3a66b59b3f9",
                "deployment_id": "appgdep_6a1f3df81a0c81919722b3bd1ec5c657",
            }),
        ),
    ];

    assert_eq!(
        appgen_feedback_tags(&items),
        BTreeMap::from([
            (
                "deployment_id".to_string(),
                "appgdep_6a1f3df81a0c81919722b3bd1ec5c657".to_string(),
            ),
            ("project_id".to_string(), project_id.to_string()),
        ])
    );
}

#[test]
fn appgen_feedback_tags_keep_projectless_calls_as_standalone_context() {
    let deployment_id = "appgdep_6a1f3df81a0c81919722b3bd1ec5c657";
    let items = vec![
        legacy_mcp_call(
            "call-1",
            json!({
                "project_id": "appgprj_00000000000000000000000000000000",
                "version_id": "appgver_a24820415a508191a28bd3a66b59b3f9",
            }),
        ),
        legacy_mcp_call(
            "call-2",
            json!({
                "deployment_id": deployment_id,
            }),
        ),
    ];

    assert_eq!(
        appgen_feedback_tags(&items),
        BTreeMap::from([("deployment_id".to_string(), deployment_id.to_string())])
    );
}

#[test]
fn appgen_feedback_tags_ignore_untrusted_shapes_and_values() {
    let items = vec![
        legacy_mcp_call(
            "call-1",
            json!({
                "project_id": {"nested": "appgprj_6a239a76a4b08191986eb3712f427df9"},
                "version_id": "version-4",
                "deployment_id": "appgdep_not-hex",
            }),
        ),
        mcp_call(
            "custom_server",
            "call-3",
            json!({
                "project_id": "appgprj_6a239a76a4b08191986eb3712f427df9",
            }),
            CallOutcome::Success,
        ),
        legacy_mcp_call(
            "call-2",
            json!({
                "nested": {
                    "project_id": "appgprj_6a239a76a4b08191986eb3712f427df9",
                },
                "project_id": format!("appgprj_{}", "a".repeat(65)),
            }),
        ),
    ];

    assert_eq!(appgen_feedback_tags(&items), BTreeMap::new());
}

#[test]
fn appgen_feedback_tags_exclude_rolled_back_tool_calls() {
    let mut items = turn_with_mcp_call(json!({
        "project_id": "appgprj_6a239a76a4b08191986eb3712f427df9",
    }));
    items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        ThreadRolledBackEvent { num_turns: 1 },
    )));

    assert_eq!(appgen_feedback_tags(&items), BTreeMap::new());
}

fn turn_with_mcp_call(arguments: serde_json::Value) -> Vec<RolloutItem> {
    vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            trace_id: None,
            started_at: Some(1),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })),
        RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "deploy the site".to_string(),
            ..Default::default()
        })),
        legacy_mcp_call("call-1", arguments),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            completed_at: Some(2),
            duration_ms: Some(1),
            time_to_first_token_ms: None,
        })),
    ]
}

fn legacy_mcp_call(call_id: &str, arguments: serde_json::Value) -> RolloutItem {
    mcp_call(
        codex_mcp::CODEX_APPS_MCP_SERVER_NAME,
        call_id,
        arguments,
        CallOutcome::Success,
    )
}

#[derive(Clone, Copy)]
enum CallOutcome {
    Success,
    ToolError,
    LocalFailure,
}

fn mcp_call(
    server: &str,
    call_id: &str,
    arguments: serde_json::Value,
    outcome: CallOutcome,
) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::McpToolCallEnd(McpToolCallEndEvent {
        call_id: call_id.to_string(),
        invocation: McpInvocation {
            server: server.to_string(),
            tool: "sites_tool".to_string(),
            arguments: Some(arguments),
        },
        mcp_app_resource_uri: None,
        plugin_id: None,
        duration: Duration::ZERO,
        result: match outcome {
            CallOutcome::Success | CallOutcome::ToolError => Ok(CallToolResult {
                content: Vec::new(),
                structured_content: None,
                is_error: matches!(outcome, CallOutcome::ToolError).then_some(true),
                meta: None,
            }),
            CallOutcome::LocalFailure => Err("sites tool was rejected locally".to_string()),
        },
    }))
}

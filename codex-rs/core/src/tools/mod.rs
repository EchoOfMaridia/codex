pub(crate) mod code_mode;
pub(crate) mod context;
pub(crate) mod events;
pub(crate) mod handlers;
pub(crate) mod hook_names;
pub(crate) mod hosted_spec;
pub(crate) mod lifecycle;
pub(crate) mod network_approval;
pub(crate) mod orchestrator;
pub(crate) mod parallel;
pub(crate) mod registry;
pub(crate) mod router;
pub(crate) mod runtimes;
pub(crate) mod sandboxing;
pub(crate) mod spec_plan;
pub(crate) mod tool_dispatch_trace;

use std::borrow::Cow;

use codex_protocol::ToolName;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_output_truncation::truncate_text;
pub use router::ToolRouter;

// Telemetry preview limits: keep log events smaller than model budgets.
pub(crate) const TELEMETRY_PREVIEW_MAX_BYTES: usize = 2 * 1024; // 2 KiB
pub(crate) const TELEMETRY_PREVIEW_MAX_LINES: usize = 64;
pub(crate) const TELEMETRY_PREVIEW_TRUNCATION_NOTICE: &str =
    "[... telemetry preview truncated ...]";

pub(crate) const MCP_TOOL_NAME_DELIMITER: &str = "__";

/// Legacy boundaries such as hook payloads, telemetry tags, and Responses tool
/// names still require a single flattened string. Keep comparisons and sorting
/// on `ToolName` itself; use this only when crossing those boundaries.
pub(crate) fn flat_tool_name(tool_name: &ToolName) -> Cow<'_, str> {
    match tool_name.namespace.as_deref() {
        Some(namespace) => {
            let mut name = String::with_capacity(namespace.len() + tool_name.name.len());
            name.push_str(namespace);
            name.push_str(&tool_name.name);
            Cow::Owned(name)
        }
        None => Cow::Borrowed(tool_name.name.as_str()),
    }
}

/// Serialize a `ToolName` into the single-string wire form used by Codex's
/// Responses-API function-tool definitions (and the function-call items the
/// model returns). Joins the namespace and the bare name with `__` so that
/// `mcp__minimax.web_search` round-trips as `mcp__minimax__web_search`. Used
/// to give the model enough context to identify which MCP server owns a
/// flattened namespace tool, and to let the dispatcher recover the original
/// `ToolName` when the function call comes back.
pub(crate) fn join_responses_tool_name(tool_name: &ToolName) -> String {
    match tool_name.namespace.as_deref() {
        Some(namespace) => {
            let namespace = namespace.trim_end_matches('_');
            let name = tool_name.name.trim_start_matches('_');
            format!("{namespace}{MCP_TOOL_NAME_DELIMITER}{name}")
        }
        None => tool_name.name.clone(),
    }
}

/// Inverse of [`join_responses_tool_name`] for the legacy `mcp__<server>__<tool>`
/// wire format used by flattened namespace tools. Splits the wire name on the
/// LAST `__` separator so that the `mcp__` prefix and any later `__` inside
/// the server name stay attached to the namespace portion
/// (e.g. `mcp__minimax__web_search` parses to namespace=`mcp__minimax`,
/// name=`web_search`, matching the `ToolName` the McpHandler was registered
/// with). Returns `None` for plain un-namespaced tool names that contain no
/// `__` separator.
pub(crate) fn split_responses_tool_name(wire_name: &str) -> Option<ToolName> {
    // `rsplit_once` returns the split *before* the last occurrence, so the
    // namespace keeps its leading `mcp__` plus any inner `__` segments (e.g.
    // `mcp__codex_apps__github__add_issue` parses to namespace=
    // `mcp__codex_apps__github`, name=`add_issue`).
    let (namespace, name) = wire_name.rsplit_once(MCP_TOOL_NAME_DELIMITER)?;
    if namespace.is_empty() || name.is_empty() {
        return None;
    }
    Some(ToolName::namespaced(
        namespace.to_string(),
        name.to_string(),
    ))
}
pub(crate) fn tool_user_shell_type(
    user_shell: &crate::shell::Shell,
) -> codex_tools::ToolUserShellType {
    match user_shell.shell_type {
        crate::shell::ShellType::Zsh => codex_tools::ToolUserShellType::Zsh,
        crate::shell::ShellType::Bash => codex_tools::ToolUserShellType::Bash,
        crate::shell::ShellType::PowerShell => codex_tools::ToolUserShellType::PowerShell,
        crate::shell::ShellType::Sh => codex_tools::ToolUserShellType::Sh,
        crate::shell::ShellType::Cmd => codex_tools::ToolUserShellType::Cmd,
    }
}

/// Format the combined exec output for sending back to the model.
/// Includes exit code and duration metadata; truncates large bodies safely.
pub fn format_exec_output_for_model(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
) -> String {
    // round to 1 decimal place
    let duration_seconds = ((exec_output.duration.as_secs_f32()) * 10.0).round() / 10.0;

    let content = build_content_with_timeout(exec_output);

    let total_lines = content.lines().count();

    let formatted_output = truncate_text(&content, truncation_policy);

    let mut sections = Vec::new();

    sections.push(format!("Exit code: {}", exec_output.exit_code));
    sections.push(format!("Wall time: {duration_seconds} seconds"));
    if total_lines != formatted_output.lines().count() {
        sections.push(format!("Total output lines: {total_lines}"));
    }

    sections.push("Output:".to_string());
    sections.push(formatted_output);

    sections.join("\n")
}

pub fn format_exec_output_str(
    exec_output: &ExecToolCallOutput,
    truncation_policy: TruncationPolicy,
) -> String {
    let content = build_content_with_timeout(exec_output);

    // Truncate for model consumption before serialization.
    formatted_truncate_text(&content, truncation_policy)
}

/// Extracts exec output content and prepends a timeout message if the command timed out.
fn build_content_with_timeout(exec_output: &ExecToolCallOutput) -> String {
    if exec_output.timed_out {
        format!(
            "command timed out after {} milliseconds\n{}",
            exec_output.duration.as_millis(),
            exec_output.aggregated_output.text
        )
    } else {
        exec_output.aggregated_output.text.clone()
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

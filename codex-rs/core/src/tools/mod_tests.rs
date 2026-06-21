use super::*;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;

/// Documents the lossy behavior of `split_responses_tool_name` when the
/// tool name itself contains `__` followed by a fragment that looks like
/// a namespace segment (e.g. AWS-MCP's `aws___<name>` pattern).
///
/// The function uses `rsplit_once("__")`, which finds the *last* `__` in
/// the wire name. For names that include an internal `__`, the split lands
/// inside the tool name rather than at the namespace/name boundary, and
/// the returned `ToolName` does not match the one that was registered.
///
/// This test pins the lossy behavior so the bug is visible in CI, and
/// so future refactors don't silently change it without updating the
/// fallback contract. The dispatcher does not rely on this function for
/// registered tools anymore — see `ToolRegistry::resolve_wire_name`.
#[test]
fn split_responses_tool_name_is_lossy_for_aws_mcp_wire_name() {
    let wire = "mcp__aws_mcp__aws___search_documentation";
    let parsed = split_responses_tool_name(wire).expect("wire name contains `__`");

    // The lossy result: rsplit_once picks the `__` inside `aws___`,
    // producing a trailing-underscore namespace and a stripped name.
    assert_eq!(
        parsed,
        ToolName::namespaced("mcp__aws_mcp__aws_", "search_documentation"),
        "split_responses_tool_name is expected to be lossy for names with internal `__`; \
         the dispatcher must use ToolRegistry::resolve_wire_name for registered tools",
    );

    // Sanity: the registered ToolName is different from the parsed one.
    let registered = ToolName::namespaced("mcp__aws_mcp", "aws___search_documentation");
    assert_ne!(
        parsed, registered,
        "parser must disagree with the registered name for AWS-MCP"
    );
}

/// Round-trip property for names whose `name` component does NOT contain
/// `__`. These must round-trip cleanly because that is the common case
/// the existing `rsplit_once` parser was designed for.
#[test]
fn join_then_split_roundtrips_simple_names() {
    let cases = [
        ToolName::namespaced("mcp__minimax", "web_search"),
        ToolName::namespaced("mcp__codex_apps__github", "add_issue"),
        ToolName::namespaced("mcp__playwright", "browser_navigate"),
    ];
    for tool_name in cases {
        let wire = join_responses_tool_name(&tool_name);
        let parsed = split_responses_tool_name(&wire)
            .unwrap_or_else(|| panic!("round-trip must succeed for {wire}"));
        assert_eq!(
            parsed, tool_name,
            "round-trip mismatch for {tool_name:?} via {wire}"
        );
    }
}

/// Round-trip property for names whose `name` component DOES contain `__`.
/// These are intentionally lossy today; this test documents that.
#[test]
fn join_then_split_is_lossy_when_name_contains_double_underscore() {
    let tool_name = ToolName::namespaced("mcp__aws_mcp", "aws___search_documentation");
    let wire = join_responses_tool_name(&tool_name);
    assert_eq!(wire, "mcp__aws_mcp__aws___search_documentation");
    let parsed = split_responses_tool_name(&wire).expect("wire name contains `__`");
    assert_ne!(
        parsed, tool_name,
        "this documents the lossy round-trip; the fix lives in ToolRegistry::resolve_wire_name",
    );
}

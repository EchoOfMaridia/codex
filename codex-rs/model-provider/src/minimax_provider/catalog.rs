//! Static model catalog for the MiniMax provider.
//!
//! MiniMax exposes a multimodal M-series of foundation models. The flagship
//! M3 model and the high-throughput M2.7 / M2.5 lines all accept text and
//! image inputs; M3 additionally accepts video. The MiniMax MCP image tool
//! (`mcp__minimax__understand_image`) is server-side and works regardless of
//! the chat model, so we must also let text+image payloads flow back through
//! the function-call output path.
//!
//! Every model in this catalog declares `input_modalities = [Text, Image]`.
//! This catalog is used as the default for users who do not configure
//! `model_catalog_json`; users who do configure a custom catalog retain
//! full control of their model list.

use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::WebSearchToolType;

const MINIMAX_M3_CONTEXT_WINDOW: i64 = 512_000;
const MINIMAX_M2_7_CONTEXT_WINDOW: i64 = 200_000;
const MINIMAX_M2_5_CONTEXT_WINDOW: i64 = 100_000;

/// Default input modalities for MiniMax models.
const MINIMAX_DEFAULT_INPUT_MODALITIES: &[InputModality] =
    &[InputModality::Text, InputModality::Image];

/// Persona used for every MiniMax model in the static catalog. We deliberately
/// do not ship a bespoke base prompt here; the user persona lives in the
/// runtime model instructions file configured via the user's `config.toml`.
const MINIMAX_BASE_INSTRUCTIONS: &str = "";

pub(crate) fn static_model_catalog() -> ModelsResponse {
    ModelsResponse {
        models: vec![
            minimax_model(
                "MiniMax-M3",
                "MiniMax M3",
                /*priority*/ 0,
                MINIMAX_M3_CONTEXT_WINDOW,
                ReasoningEffort::High,
                MINIMAX_DEFAULT_INPUT_MODALITIES,
            ),
            minimax_model(
                "MiniMax-M3-highspeed",
                "MiniMax M3 (High Speed)",
                /*priority*/ 1,
                MINIMAX_M3_CONTEXT_WINDOW,
                ReasoningEffort::High,
                MINIMAX_DEFAULT_INPUT_MODALITIES,
            ),
            minimax_model(
                "MiniMax-M2.7",
                "MiniMax M2.7",
                /*priority*/ 2,
                MINIMAX_M2_7_CONTEXT_WINDOW,
                ReasoningEffort::High,
                MINIMAX_DEFAULT_INPUT_MODALITIES,
            ),
            minimax_model(
                "MiniMax-M2.7-highspeed",
                "MiniMax M2.7 (High Speed)",
                /*priority*/ 3,
                MINIMAX_M2_7_CONTEXT_WINDOW,
                ReasoningEffort::High,
                MINIMAX_DEFAULT_INPUT_MODALITIES,
            ),
            minimax_model(
                "MiniMax-M2.5",
                "MiniMax M2.5",
                /*priority*/ 4,
                MINIMAX_M2_5_CONTEXT_WINDOW,
                ReasoningEffort::Medium,
                MINIMAX_DEFAULT_INPUT_MODALITIES,
            ),
            minimax_model(
                "MiniMax-M2.5-highspeed",
                "MiniMax M2.5 (High Speed)",
                /*priority*/ 5,
                MINIMAX_M2_5_CONTEXT_WINDOW,
                ReasoningEffort::Medium,
                MINIMAX_DEFAULT_INPUT_MODALITIES,
            ),
        ],
    }
}

fn minimax_model(
    slug: &str,
    display_name: &str,
    priority: i32,
    context_window: i64,
    default_reasoning_level: ReasoningEffort,
    input_modalities: &[InputModality],
) -> ModelInfo {
    ModelInfo {
        slug: slug.to_string(),
        display_name: display_name.to_string(),
        description: Some(format!(
            "MiniMax model - {}k context window",
            context_window / 1000
        )),
        default_reasoning_level: Some(default_reasoning_level),
        supported_reasoning_levels: vec![
            reasoning_effort_preset(ReasoningEffort::Low),
            reasoning_effort_preset(ReasoningEffort::Medium),
            reasoning_effort_preset(ReasoningEffort::High),
        ],
        shell_type: ConfigShellToolType::ShellCommand,
        visibility: ModelVisibility::List,
        supported_in_api: true,
        priority,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        availability_nux: None,
        upgrade: None,
        base_instructions: MINIMAX_BASE_INSTRUCTIONS.to_string(),
        model_messages: None,
        supports_reasoning_summaries: false,
        default_reasoning_summary: ReasoningSummary::None,
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: None,
        web_search_tool_type: WebSearchToolType::Text,
        truncation_policy: TruncationPolicyConfig::tokens(/*limit*/ 10_000),
        supports_parallel_tool_calls: true,
        supports_image_detail_original: false,
        context_window: Some(context_window),
        max_context_window: Some(context_window),
        auto_compact_token_limit: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: input_modalities.to_vec(),
        used_fallback_model_metadata: false,
        supports_search_tool: false,
        use_responses_lite: false,
        auto_review_model_override: None,
        tool_mode: None,
        multi_agent_version: None,
    }
}

fn reasoning_effort_preset(effort: ReasoningEffort) -> ReasoningEffortPreset {
    let description: String = match &effort {
        ReasoningEffort::None => "No reasoning",
        ReasoningEffort::Minimal => "Minimal reasoning",
        ReasoningEffort::Low => "Fast responses with lighter reasoning",
        ReasoningEffort::Medium => "Balances speed and reasoning depth for everyday tasks",
        ReasoningEffort::High => "Greater reasoning depth for complex problems",
        ReasoningEffort::XHigh => "Extra high reasoning depth for complex problems",
        ReasoningEffort::Custom(_) => "Model-specific reasoning effort",
    }
    .to_string();
    ReasoningEffortPreset { effort, description }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn catalog_contains_six_models() {
        let catalog = static_model_catalog();
        assert_eq!(catalog.models.len(), 6);
    }

    #[test]
    fn m3_is_default_model() {
        let catalog = static_model_catalog();
        let default_model = catalog
            .models
            .iter()
            .find(|model| model.slug == "MiniMax-M3")
            .expect("Catalog should include MiniMax-M3");
        assert_eq!(default_model.priority, 0);
    }

    #[test]
    fn m3_has_512k_context_window() {
        let catalog = static_model_catalog();
        let m3_model = catalog
            .models
            .iter()
            .find(|model| model.slug == "MiniMax-M3")
            .expect("Catalog should include MiniMax-M3");
        assert_eq!(m3_model.context_window, Some(512_000));
    }

    #[test]
    fn every_minimax_model_supports_image_input() {
        let catalog = static_model_catalog();
        assert!(!catalog.models.is_empty(), "catalog should not be empty");
        for model in &catalog.models {
            assert!(
                model.input_modalities.contains(&InputModality::Image),
                "MiniMax model {} must declare Image input modality                  (server-side MCP image tools rely on this)",
                model.slug,
            );
            assert!(
                model.input_modalities.contains(&InputModality::Text),
                "MiniMax model {} must declare Text input modality",
                model.slug,
            );
        }
    }

    #[test]
    fn m2_7_has_200k_context_window() {
        let catalog = static_model_catalog();
        let m2_7_model = catalog
            .models
            .iter()
            .find(|model| model.slug == "MiniMax-M2.7")
            .expect("Catalog should include MiniMax-M2.7");
        assert_eq!(m2_7_model.context_window, Some(200_000));
    }

    #[test]
    fn m2_5_has_100k_context_window() {
        let catalog = static_model_catalog();
        let m2_5_model = catalog
            .models
            .iter()
            .find(|model| model.slug == "MiniMax-M2.5")
            .expect("Catalog should include MiniMax-M2.5");
        assert_eq!(m2_5_model.context_window, Some(100_000));
    }

    #[test]
    fn highspeed_models_have_expected_priorities() {
        let catalog = static_model_catalog();
        let highspeed_models: Vec<_> = catalog
            .models
            .iter()
            .filter(|model| model.slug.contains("highspeed"))
            .collect();
        assert_eq!(highspeed_models.len(), 3);
        let priorities: Vec<i32> = highspeed_models.iter().map(|m| m.priority).collect();
        assert!(priorities.contains(&1), "M3-highspeed should be priority 1, got {priorities:?}");
        assert!(priorities.contains(&3), "M2.7-highspeed should be priority 3, got {priorities:?}");
        assert!(priorities.contains(&5), "M2.5-highspeed should be priority 5, got {priorities:?}");
    }
}

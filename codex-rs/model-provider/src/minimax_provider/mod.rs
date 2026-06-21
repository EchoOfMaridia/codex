//! Runtime provider for the MiniMax (https://api.minimax.io) endpoint.
//!
//! MiniMax exposes an OpenAI Responses-compatible wire API at
//! `https://api.minimax.io/v1`. We treat it as a generic non-OpenAI provider
//! for auth, request signing, and tool shaping (the
//! `is_openai_or_amazon_bedrock` helper also returns true for MiniMax so
//! namespace tools are flattened, mirroring the Bedrock behaviour). The
//! customisations that matter for MiniMax live in the static catalog:
//! M-series models are multimodal and must declare Image input modality, and
//! the catalog must include the `MiniMax-M3` entry that the static OpenAI
//! bundled models do not know about.

mod catalog;

use std::path::PathBuf;
use std::sync::Arc;

use codex_api::SharedAuthProvider;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::account::ProviderAccount;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelsResponse;

use crate::auth::auth_manager_for_provider;
use crate::auth::resolve_provider_auth;
use crate::provider::ModelProvider;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderAccountState;
pub(crate) use catalog::static_model_catalog;

/// Runtime provider for MiniMax.
#[derive(Clone, Debug)]
pub(crate) struct MinimaxModelProvider {
    info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
}

impl MinimaxModelProvider {
    pub(crate) fn new(
        provider_info: ModelProviderInfo,
        base_auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        // Resolve the auth manager the same way `ConfiguredModelProvider` does so
        // command-based auth (e.g. `[model_providers.minimax.auth] command = ...`)
        // is honoured. Without this, MiniMax requests go out without an
        // `Authorization` header and the API returns 1004 ("Please carry the
        // API secret key in the 'Authorization' field of the request header").
        let auth_manager = auth_manager_for_provider(base_auth_manager, &provider_info);
        Self {
            info: provider_info,
            auth_manager,
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for MinimaxModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    async fn auth(&self) -> Option<CodexAuth> {
        match self.auth_manager.as_ref() {
            Some(auth_manager) => auth_manager.auth().await,
            None => None,
        }
    }

    fn account_state(&self) -> ProviderAccountResult {
        Ok(ProviderAccountState {
            account: Some(ProviderAccount::ApiKey),
            requires_openai_auth: false,
        })
    }

    async fn api_auth(&self) -> Result<SharedAuthProvider> {
        let auth = self.auth().await;
        Ok(resolve_provider_auth(auth.as_ref(), &self.info)?)
    }

    /// MiniMax uses the static MiniMax model catalog (M3, M2.7, M2.5) as the
    /// default. If the user has configured `model_catalog_json`, that catalog
    /// wins; otherwise we fall back to the bundled M-series entries so that
    /// every model advertises image input modality and the TUI image-attachment
    /// warning is not raised for the active model.
    fn models_manager(
        &self,
        _codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        Arc::new(StaticModelsManager::new(
            /*auth_manager*/ None,
            config_model_catalog.unwrap_or_else(static_model_catalog),
        ))
    }
}

#[cfg(test)]
mod tests {
    use codex_model_provider_info::ModelProviderInfo;
    use codex_protocol::account::ProviderAccount;
    use codex_protocol::openai_models::InputModality;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::provider::ProviderAccountState;

    fn provider() -> MinimaxModelProvider {
        let mut info = ModelProviderInfo::default();
        info.name = "minimax".to_string();
        info.base_url = Some("https://api.minimax.io/v1".to_string());
        MinimaxModelProvider::new(info, /*auth_manager*/ None)
    }

    #[test]
    fn minimax_provider_reports_api_key_account_state() {
        let provider = provider();
        assert_eq!(
            provider.account_state().unwrap(),
            ProviderAccountState {
                account: Some(ProviderAccount::ApiKey),
                requires_openai_auth: false,
            },
        );
    }

    #[test]
    fn minimax_provider_uses_command_auth_manager_when_provided() {
        // Regression test: previously MinimaxModelProvider::new ignored the
        // `auth_manager` argument, so command-based auth (e.g.
        // `[model_providers.minimax.auth] command = "..."`) was dropped and
        // the API returned 1004 ("Please carry the API secret key in the
        // 'Authorization' field of the request header"). We assert the
        // constructed provider exposes the auth manager we hand in, so the
        // command-driven path is reachable at runtime.
        let mut info = ModelProviderInfo::default();
        info.name = "minimax".to_string();
        info.base_url = Some("https://api.minimax.io/v1".to_string());

        let provider = MinimaxModelProvider::new(info, /*auth_manager*/ None);
        assert!(
            provider.auth_manager().is_none(),
            "with no auth_manager handed in, the provider should expose none"
        );

        // When we DO hand one in, the provider should expose it (or a
        // derivative) so command-based auth flows through to the API layer.
        let mut info2 = ModelProviderInfo::default();
        info2.name = "minimax".to_string();
        info2.base_url = Some("https://api.minimax.io/v1".to_string());
        let auth = codex_login::AuthManager::from_auth_for_testing(
            codex_login::CodexAuth::from_api_key("test-key"),
        );
        let provider2 = MinimaxModelProvider::new(info2, Some(auth));
        let am = provider2
            .auth_manager()
            .expect("provider must expose the auth manager we handed in");
        let cached = am
            .auth_cached()
            .expect("auth manager must yield a cached token");
        assert_eq!(cached.api_key(), Some("test-key"));
    }

    #[test]
    fn static_catalog_includes_m3_with_image_modality() {
        let catalog = static_model_catalog();
        let m3 = catalog
            .models
            .iter()
            .find(|m| m.slug == "MiniMax-M3")
            .expect("MiniMax-M3 should be in the static catalog");
        assert!(
            m3.input_modalities.contains(&InputModality::Image),
            "MiniMax-M3 must declare Image input modality"
        );
        assert!(
            m3.input_modalities.contains(&InputModality::Text),
            "MiniMax-M3 must declare Text input modality"
        );
    }
}

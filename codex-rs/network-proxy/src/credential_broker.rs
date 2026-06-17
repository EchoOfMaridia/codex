use crate::policy::normalize_host;
use rama_http::HeaderMap;
use rama_http::HeaderValue;
use rama_http::header::AUTHORIZATION;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

const GH_HOST_ENV_VAR: &str = "GH_HOST";
const GITHUB_TOKEN_PREFIXES: &[&str] = &["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
const GITHUB_TOKEN_MIN_LEN: usize = 40;
const OPENAI_API_KEY_MIN_LEN: usize = 51;
const GITHUB_CLOUD_TOKEN_ENV_VARS: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN"];
const GITHUB_ENTERPRISE_TOKEN_ENV_VARS: &[&str] =
    &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"];
const OPENAI_API_KEY_ENV_VARS: &[&str] = &["OPENAI_API_KEY"];
pub const CREDENTIAL_BROKER_ACTIVE_ENV_KEY: &str = "CODEX_NETWORK_PROXY_CREDENTIAL_BROKER_ACTIVE";
pub(crate) const BROKERED_CREDENTIALS_ENV_KEY: &str = "CODEX_NETWORK_PROXY_BROKERED_CREDENTIALS";

#[derive(Clone)]
pub(crate) struct CredentialBroker {
    state: Arc<RwLock<CredentialBrokerState>>,
}

#[derive(Default)]
struct CredentialBrokerState {
    enabled: bool,
    next_credential_id: usize,
    credentials: Vec<CredentialRecord>,
}

struct CredentialRecord {
    env_var: String,
    kind: CredentialKind,
    host_binding: CredentialHostBinding,
    real_value: String,
    dummy_value: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CredentialKind {
    GitHub,
    OpenAiApiKey,
}

#[derive(Clone, PartialEq, Eq)]
enum CredentialHostBinding {
    GitHubCloud,
    ExactHost(String),
    OpenAiApi,
}

type HostBindingResolver = fn(&HashMap<String, String>) -> Option<CredentialHostBinding>;

struct CredentialSource {
    env_vars: &'static [&'static str],
    kind: CredentialKind,
    host_binding: HostBindingResolver,
}

const CREDENTIAL_SOURCES: &[CredentialSource] = &[
    CredentialSource {
        env_vars: GITHUB_CLOUD_TOKEN_ENV_VARS,
        kind: CredentialKind::GitHub,
        host_binding: github_cloud_binding,
    },
    CredentialSource {
        env_vars: GITHUB_ENTERPRISE_TOKEN_ENV_VARS,
        kind: CredentialKind::GitHub,
        host_binding: github_enterprise_binding,
    },
    CredentialSource {
        env_vars: OPENAI_API_KEY_ENV_VARS,
        kind: CredentialKind::OpenAiApiKey,
        host_binding: openai_api_binding,
    },
];

impl CredentialBroker {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            state: Arc::new(RwLock::new(CredentialBrokerState {
                enabled,
                ..CredentialBrokerState::default()
            })),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        let mut state = self.write_state();
        state.enabled = enabled;
        if !enabled {
            state.credentials.clear();
            state.next_credential_id = 0;
        }
    }

    pub(crate) fn virtualize_child_env(&self, env: &mut HashMap<String, String>) {
        let mut state = self.write_state();
        if !state.enabled {
            env.remove(CREDENTIAL_BROKER_ACTIVE_ENV_KEY);
            env.remove(BROKERED_CREDENTIALS_ENV_KEY);
            return;
        }
        env.insert(
            CREDENTIAL_BROKER_ACTIVE_ENV_KEY.to_string(),
            "1".to_string(),
        );

        for source in CREDENTIAL_SOURCES {
            if let Some(host_binding) = (source.host_binding)(env) {
                for env_var in source.env_vars {
                    virtualize_env_var(env, &mut state, env_var, source.kind, host_binding.clone());
                }
            }
        }
        update_brokered_credentials_marker(&state, env);
    }

    pub(crate) fn host_requires_mitm(&self, host: &str) -> bool {
        let normalized_host = normalize_host(host);
        let state = self.read_state();
        state.enabled
            && state
                .credentials
                .iter()
                .any(|credential| credential.matches_host(&normalized_host))
    }

    pub(crate) fn inject_request_headers(&self, host: &str, headers: &mut HeaderMap) {
        let normalized_host = normalize_host(host);
        let state = self.read_state();
        if !state.enabled {
            return;
        }

        let matching_credentials = state
            .credentials
            .iter()
            .filter(|credential| credential.matches_host(&normalized_host))
            .collect::<Vec<_>>();
        let Some(credential) = select_credential(headers, &matching_credentials) else {
            return;
        };
        let Some(header_value) = credential.kind.request_header_value(&credential.real_value)
        else {
            return;
        };
        credential.kind.insert_request_header(headers, header_value);
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, CredentialBrokerState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, CredentialBrokerState> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn virtualize_env_var(
    env: &mut HashMap<String, String>,
    state: &mut CredentialBrokerState,
    env_var: &str,
    kind: CredentialKind,
    host_binding: CredentialHostBinding,
) {
    let Some(real_value) = brokerable_credential_value(env, state, env_var, kind) else {
        return;
    };

    let dummy_value = state.register(env_var, kind, host_binding, real_value);
    env.insert(env_var.to_string(), dummy_value);
}

fn brokerable_credential_value<'a>(
    env: &'a HashMap<String, String>,
    state: &CredentialBrokerState,
    env_var: &str,
    kind: CredentialKind,
) -> Option<&'a str> {
    let real_value = env.get(env_var)?.trim();
    (!real_value.is_empty()
        && !state.is_dummy_value(real_value)
        && kind.request_header_value(real_value).is_some())
    .then_some(real_value)
}

impl CredentialBrokerState {
    fn register(
        &mut self,
        env_var: &str,
        kind: CredentialKind,
        host_binding: CredentialHostBinding,
        real_value: &str,
    ) -> String {
        if let Some(existing) = self.credentials.iter().find(|credential| {
            credential.env_var == env_var
                && credential.kind == kind
                && credential.host_binding == host_binding
                && credential.real_value == real_value
        }) {
            return existing.dummy_value.clone();
        }

        let dummy_value = kind.dummy_value(self.next_credential_id, real_value);
        self.next_credential_id += 1;
        self.credentials.push(CredentialRecord {
            env_var: env_var.to_string(),
            kind,
            host_binding,
            real_value: real_value.to_string(),
            dummy_value: dummy_value.clone(),
        });
        dummy_value
    }

    fn is_dummy_value(&self, value: &str) -> bool {
        self.credentials
            .iter()
            .any(|credential| credential.dummy_value == value)
    }
}

impl CredentialRecord {
    fn matches_host(&self, host: &str) -> bool {
        self.host_binding.matches_host(host)
    }
}

impl CredentialKind {
    fn dummy_value(self, credential_id: usize, real_value: &str) -> String {
        match self {
            Self::GitHub => shaped_dummy_value(
                real_value,
                github_token_prefix(real_value),
                GITHUB_TOKEN_MIN_LEN,
                "github",
                credential_id,
            ),
            Self::OpenAiApiKey => shaped_dummy_value(
                real_value,
                openai_api_key_prefix(real_value),
                OPENAI_API_KEY_MIN_LEN,
                "openai",
                credential_id,
            ),
        }
    }

    fn request_header(self, headers: &HeaderMap) -> Option<&HeaderValue> {
        match self {
            Self::GitHub | Self::OpenAiApiKey => headers.get(AUTHORIZATION),
        }
    }

    fn request_header_value(self, value: &str) -> Option<HeaderValue> {
        match self {
            Self::GitHub | Self::OpenAiApiKey => {
                HeaderValue::from_str(&format!("Bearer {value}")).ok()
            }
        }
    }

    fn insert_request_header(self, headers: &mut HeaderMap, value: HeaderValue) {
        match self {
            Self::GitHub | Self::OpenAiApiKey => {
                headers.insert(AUTHORIZATION, value);
            }
        }
    }
}

impl CredentialHostBinding {
    fn matches_host(&self, host: &str) -> bool {
        match self {
            Self::GitHubCloud => github_cloud_host(host),
            Self::ExactHost(expected_host) => host == expected_host,
            Self::OpenAiApi => host == "api.openai.com",
        }
    }
}

fn github_cloud_host(host: &str) -> bool {
    matches!(host, "api.github.com" | "github.com") || host.ends_with(".ghe.com")
}

fn github_token_prefix(value: &str) -> &str {
    GITHUB_TOKEN_PREFIXES
        .iter()
        .copied()
        .find(|prefix| value.starts_with(prefix))
        .unwrap_or("ghp_")
}

fn openai_api_key_prefix(value: &str) -> &str {
    let Some(suffix) = value.strip_prefix("sk-") else {
        return "sk-";
    };
    suffix
        .find('-')
        .map_or("sk-", |separator| &value[..separator + 4])
}

fn shaped_dummy_value(
    real_value: &str,
    prefix: &str,
    minimum_len: usize,
    seed: &str,
    credential_id: usize,
) -> String {
    let target_len = real_value.len().max(minimum_len).max(prefix.len() + 16);
    let digest = Sha256::digest(format!("{seed}:{credential_id}").as_bytes());
    let mut dummy = String::with_capacity(target_len);
    dummy.push_str(prefix);
    for index in prefix.len()..target_len {
        let offset = index - prefix.len();
        let entropy = digest[offset % digest.len()].wrapping_add(offset as u8);
        let character = match real_value.as_bytes().get(index).copied() {
            Some(template) if !template.is_ascii_alphanumeric() => template,
            Some(template) if template.is_ascii_digit() => b'0' + entropy % 10,
            Some(template) if template.is_ascii_uppercase() => b'A' + entropy % 26,
            _ => b'a' + entropy % 26,
        };
        dummy.push(char::from(character));
    }
    dummy
}

fn github_cloud_binding(_: &HashMap<String, String>) -> Option<CredentialHostBinding> {
    Some(CredentialHostBinding::GitHubCloud)
}

fn github_enterprise_binding(env: &HashMap<String, String>) -> Option<CredentialHostBinding> {
    github_host_hint(env)
        .filter(|host| !github_cloud_host(host))
        .map(CredentialHostBinding::ExactHost)
}

fn openai_api_binding(_: &HashMap<String, String>) -> Option<CredentialHostBinding> {
    Some(CredentialHostBinding::OpenAiApi)
}

fn github_host_hint(env: &HashMap<String, String>) -> Option<String> {
    env.get(GH_HOST_ENV_VAR)
        .map(String::as_str)
        .map(normalize_host)
        .filter(|host| !host.is_empty())
}

fn select_credential<'a>(
    headers: &HeaderMap,
    matching_credentials: &[&'a CredentialRecord],
) -> Option<&'a CredentialRecord> {
    let dummy_matches = matching_credentials
        .iter()
        .copied()
        .filter(|credential| {
            credential
                .kind
                .request_header(headers)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains(&credential.dummy_value))
        })
        .collect::<Vec<_>>();
    match dummy_matches.as_slice() {
        [credential] => return Some(*credential),
        [] => {}
        [_, _, ..] => return None,
    }

    let credential = missing_dummy_fallback(matching_credentials)?;
    credential
        .kind
        .request_header(headers)
        .is_none()
        .then_some(credential)
}

fn missing_dummy_fallback<'a>(
    matching_credentials: &[&'a CredentialRecord],
) -> Option<&'a CredentialRecord> {
    let credential = *matching_credentials.first()?;
    matching_credentials
        .iter()
        .all(|candidate| {
            candidate.kind == credential.kind && candidate.real_value == credential.real_value
        })
        .then_some(credential)
}

fn update_brokered_credentials_marker(
    state: &CredentialBrokerState,
    env: &mut HashMap<String, String>,
) {
    let brokered = credential_broker_env_keys()
        .filter_map(|key| {
            let value = env.get(key)?;
            state.is_dummy_value(value).then_some((key, value.as_str()))
        })
        .collect::<Vec<_>>();
    match serde_json::to_string(&brokered) {
        Ok(marker) => {
            env.insert(BROKERED_CREDENTIALS_ENV_KEY.to_string(), marker);
        }
        Err(_) => {
            env.remove(BROKERED_CREDENTIALS_ENV_KEY);
        }
    }
}

pub fn strip_brokered_credentials(env: &mut HashMap<String, String>) {
    let brokered = env
        .remove(BROKERED_CREDENTIALS_ENV_KEY)
        .and_then(|marker| serde_json::from_str::<Vec<(String, String)>>(&marker).ok())
        .unwrap_or_default();
    for (key, dummy_value) in brokered {
        if credential_broker_env_keys().any(|candidate| candidate == key.as_str())
            && env.get(&key) == Some(&dummy_value)
        {
            env.remove(&key);
        }
    }
}

fn credential_broker_env_keys() -> impl Iterator<Item = &'static str> {
    std::iter::once(GH_HOST_ENV_VAR).chain(
        CREDENTIAL_SOURCES
            .iter()
            .flat_map(|source| source.env_vars.iter().copied()),
    )
}

/// Returns supported credential keys only for an environment with an active broker.
pub fn brokered_credential_env_keys(
    env: &HashMap<String, String>,
) -> impl Iterator<Item = &'static str> {
    let active = env
        .get(CREDENTIAL_BROKER_ACTIVE_ENV_KEY)
        .is_some_and(|value| value == "1");
    credential_broker_env_keys().filter(move |_| active)
}

#[cfg(test)]
#[path = "credential_broker_tests.rs"]
mod tests;

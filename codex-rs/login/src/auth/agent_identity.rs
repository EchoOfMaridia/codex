use std::future::Future;
use std::sync::Arc;

use codex_agent_identity::AgentIdentityKey;
use codex_agent_identity::decode_agent_identity_jwt;
use codex_agent_identity::fetch_agent_identity_jwks;
use codex_agent_identity::is_retryable_registration_error;
use codex_agent_identity::public_key_ssh_from_private_key_pkcs8_base64;
use codex_agent_identity::register_agent_task;
use codex_protocol::account::PlanType as AccountPlanType;
use thiserror::Error;

use crate::default_client::build_reqwest_client;

use super::storage::AgentIdentityAuthRecord;

pub(super) const MAX_AGENT_IDENTITY_BOOTSTRAP_ATTEMPTS: usize = 3;

#[derive(Debug, Error)]
#[error("retryable agent identity registration failure: {message}")]
pub(super) struct RetryableAgentIdentityRegistrationError {
    message: String,
}

impl RetryableAgentIdentityRegistrationError {
    pub(super) fn new(message: String) -> Self {
        Self { message }
    }
}

#[derive(Clone, Debug)]
pub struct AgentIdentityAuth {
    record: Arc<AgentIdentityAuthRecord>,
}

impl AgentIdentityAuth {
    pub async fn from_record(
        mut record: AgentIdentityAuthRecord,
        agent_identity_authapi_base_url: &str,
    ) -> std::io::Result<Self> {
        public_key_ssh_from_private_key_pkcs8_base64(&record.agent_private_key)
            .map_err(std::io::Error::other)?;
        if record
            .task_id
            .as_deref()
            .is_none_or(|task_id| task_id.trim().is_empty())
        {
            record.task_id = Some(
                register_task_for_record_with_retries(&record, agent_identity_authapi_base_url)
                    .await?,
            );
        }
        Ok(Self {
            record: Arc::new(record),
        })
    }

    pub async fn from_jwt(
        jwt: &str,
        chatgpt_base_url: &str,
        agent_identity_authapi_base_url: &str,
    ) -> std::io::Result<Self> {
        let record = verified_record_from_jwt(jwt, chatgpt_base_url).await?;
        Self::from_record(record, agent_identity_authapi_base_url).await
    }

    #[cfg(test)]
    fn from_initialized_record(mut record: AgentIdentityAuthRecord, run_task_id: String) -> Self {
        record.task_id = Some(run_task_id);
        Self {
            record: Arc::new(record),
        }
    }

    pub fn record(&self) -> &AgentIdentityAuthRecord {
        self.record.as_ref()
    }

    pub fn run_task_id(&self) -> &str {
        match self.record.task_id.as_deref() {
            Some(task_id) => task_id,
            None => unreachable!("AgentIdentityAuth should only be constructed with a task_id"),
        }
    }

    pub fn account_id(&self) -> &str {
        &self.record.account_id
    }

    pub fn chatgpt_user_id(&self) -> &str {
        &self.record.chatgpt_user_id
    }

    pub fn email(&self) -> &str {
        &self.record.email
    }

    pub fn plan_type(&self) -> AccountPlanType {
        self.record.plan_type
    }

    pub fn is_fedramp_account(&self) -> bool {
        self.record.chatgpt_account_is_fedramp
    }
}

pub(super) async fn verified_record_from_jwt(
    jwt: &str,
    chatgpt_base_url: &str,
) -> std::io::Result<AgentIdentityAuthRecord> {
    AgentIdentityAuthRecord::from_agent_identity_jwt(jwt)?;
    let jwks = fetch_agent_identity_jwks(&build_reqwest_client(), chatgpt_base_url)
        .await
        .map_err(std::io::Error::other)?;
    let claims = decode_agent_identity_jwt(jwt, Some(&jwks)).map_err(std::io::Error::other)?;
    Ok(claims.into())
}

pub(super) fn is_retryable_io_registration_error(err: &std::io::Error) -> bool {
    err.get_ref().is_some_and(
        <dyn std::error::Error + std::marker::Send + std::marker::Sync + 'static>::is::<
            RetryableAgentIdentityRegistrationError,
        >,
    )
}

pub(super) async fn retry_registration<T, F, Fut>(mut operation: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    let mut attempt = 1;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err)
                if attempt < MAX_AGENT_IDENTITY_BOOTSTRAP_ATTEMPTS
                    && is_retryable_io_registration_error(&err) =>
            {
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_AGENT_IDENTITY_BOOTSTRAP_ATTEMPTS,
                    error = %err,
                    "agent identity registration attempt failed; retrying"
                );
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

async fn register_task_for_record_with_retries(
    record: &AgentIdentityAuthRecord,
    agent_identity_authapi_base_url: &str,
) -> std::io::Result<String> {
    retry_registration(|| async {
        register_task_for_record(record, agent_identity_authapi_base_url).await
    })
    .await
}

async fn register_task_for_record(
    record: &AgentIdentityAuthRecord,
    agent_identity_authapi_base_url: &str,
) -> std::io::Result<String> {
    register_agent_task(
        &build_reqwest_client(),
        agent_identity_authapi_base_url,
        key_for_record(record),
    )
    .await
    .map_err(|err| {
        if is_retryable_registration_error(&err) {
            std::io::Error::other(RetryableAgentIdentityRegistrationError::new(
                err.to_string(),
            ))
        } else {
            std::io::Error::other(err)
        }
    })
}

fn key_for_record(record: &AgentIdentityAuthRecord) -> AgentIdentityKey<'_> {
    AgentIdentityKey {
        agent_runtime_id: &record.agent_runtime_id,
        private_key_pkcs8_base64: &record.agent_private_key,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use codex_agent_identity::generate_agent_key_material;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::*;

    fn agent_identity_record(private_key: String) -> AgentIdentityAuthRecord {
        AgentIdentityAuthRecord {
            agent_runtime_id: "agent-runtime-1".to_string(),
            agent_private_key: private_key,
            account_id: "account-1".to_string(),
            chatgpt_user_id: "user-1".to_string(),
            email: "agent@example.com".to_string(),
            plan_type: AccountPlanType::Plus,
            chatgpt_account_is_fedramp: false,
            task_id: None,
        }
    }

    fn agent_identity_record_with_generated_key() -> AgentIdentityAuthRecord {
        let key_material = generate_agent_key_material().expect("generate key material");
        agent_identity_record(key_material.private_key_pkcs8_base64)
    }

    #[tokio::test]
    async fn from_record_registers_task() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agent/agent-runtime-1/task/register"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "task_id": "task-run-1",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let auth = AgentIdentityAuth::from_record(
            agent_identity_record_with_generated_key(),
            &server.uri(),
        )
        .await?;

        assert_eq!(auth.run_task_id(), "task-run-1");
        let requests = server
            .received_requests()
            .await
            .expect("failed to fetch task registration request");
        let request_body = requests[0]
            .body_json::<serde_json::Value>()
            .expect("task registration request should be JSON");
        let request_body = request_body
            .as_object()
            .expect("request body should be object");
        assert!(request_body.get("timestamp").is_some());
        assert!(request_body.get("signature").is_some());
        assert_eq!(request_body.len(), 2);
        Ok(())
    }

    #[test]
    fn run_task_is_shared_across_clones() {
        let auth = AgentIdentityAuth::from_initialized_record(
            agent_identity_record_with_generated_key(),
            "task-run-1".to_string(),
        );
        let cloned = auth.clone();

        assert!(Arc::ptr_eq(&auth.record, &cloned.record));
        assert_eq!(cloned.run_task_id(), "task-run-1");
    }

    #[tokio::test]
    async fn from_record_retries_transient_registration() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        let request_count = Arc::new(AtomicUsize::new(0));
        let response_count = Arc::clone(&request_count);
        Mock::given(method("POST"))
            .and(path("/v1/agent/agent-runtime-1/task/register"))
            .respond_with(move |_request: &wiremock::Request| {
                if response_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "task_id": "task-run-1",
                    }))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let auth = AgentIdentityAuth::from_record(
            agent_identity_record_with_generated_key(),
            &server.uri(),
        )
        .await?;

        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert_eq!(auth.run_task_id(), "task-run-1");
        Ok(())
    }
}

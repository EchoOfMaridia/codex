use super::*;
#[cfg(target_os = "windows")]
use codex_feedback::WINDOWS_SANDBOX_LOG_ATTACHMENT_FILENAME;

const MAX_FEEDBACK_TREE_THREADS: usize = 8;
const APPGEN_FEEDBACK_ID_FIELDS: [(&str, &str); 3] = [
    ("project_id", "appgprj_"),
    ("deployment_id", "appgdep_"),
    ("version_id", "appgver_"),
];

#[derive(Clone)]
pub(crate) struct FeedbackRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    config: Arc<Config>,
    feedback: CodexFeedback,
    log_db: Option<LogDbLayer>,
    state_db: Option<StateDbHandle>,
}

impl FeedbackRequestProcessor {
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        config: Arc<Config>,
        feedback: CodexFeedback,
        log_db: Option<LogDbLayer>,
        state_db: Option<StateDbHandle>,
    ) -> Self {
        Self {
            auth_manager,
            thread_manager,
            config,
            feedback,
            log_db,
            state_db,
        }
    }

    pub(crate) async fn feedback_upload(
        &self,
        params: FeedbackUploadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.upload_feedback_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    async fn upload_feedback_response(
        &self,
        params: FeedbackUploadParams,
    ) -> Result<FeedbackUploadResponse, JSONRPCErrorError> {
        if !self.config.feedback_enabled {
            return Err(invalid_request(
                "sending feedback is disabled by configuration",
            ));
        }

        let FeedbackUploadParams {
            classification,
            reason,
            thread_id,
            include_logs,
            extra_log_files,
            tags,
        } = params;
        let mut upload_tags = tags.unwrap_or_default();

        let conversation_id = match thread_id.as_deref() {
            Some(thread_id) => match ThreadId::from_string(thread_id) {
                Ok(conversation_id) => Some(conversation_id),
                Err(err) => return Err(invalid_request(format!("invalid thread id: {err}"))),
            },
            None => None,
        };

        if let Some(conversation_id) = conversation_id
            && !APPGEN_FEEDBACK_ID_FIELDS
                .iter()
                .any(|(field, _)| upload_tags.contains_key(*field))
        {
            let history_items = match self.thread_manager.get_thread(conversation_id).await {
                Ok(conversation) => {
                    match conversation.load_history(/*include_archived*/ true).await {
                        Ok(history) => Some(history.items),
                        Err(err) => {
                            warn!(
                                "failed to load live thread history for feedback tags for thread_id={conversation_id}: {err}"
                            );
                            None
                        }
                    }
                }
                Err(live_err) => {
                    match self
                        .resolve_rollout_path(conversation_id, self.state_db.as_ref())
                        .await
                    {
                        Some(path) => {
                            match codex_core::RolloutRecorder::load_rollout_items(&path).await {
                                Ok((items, _, _)) => Some(items),
                                Err(err) => {
                                    warn!(
                                        "failed to load stored thread history for feedback tags for thread_id={conversation_id}: {err}"
                                    );
                                    None
                                }
                            }
                        }
                        None => {
                            warn!(
                                "failed to resolve thread history for feedback tags for thread_id={conversation_id}: {live_err}"
                            );
                            None
                        }
                    }
                }
            };
            if let Some(history_items) = history_items {
                upload_tags.extend(appgen_feedback_tags(&history_items));
            }
        }

        if let Some(chatgpt_user_id) = self
            .auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_chatgpt_user_id())
        {
            tracing::info!(target: "feedback_tags", chatgpt_user_id);
        }
        if let Some(account_id) = self
            .auth_manager
            .auth_cached()
            .and_then(|auth| auth.get_account_id())
        {
            tracing::info!(target: "feedback_tags", account_id);
        }
        let snapshot = self.feedback.snapshot(conversation_id);
        let thread_id = snapshot.thread_id.clone();
        let (feedback_thread_ids, sqlite_feedback_logs, state_db_ctx) = if include_logs {
            if let Some(log_db) = self.log_db.as_ref() {
                log_db.flush().await;
            }
            let state_db_ctx = self.state_db.clone();
            let feedback_thread_ids = match conversation_id {
                Some(conversation_id) => match self
                    .thread_manager
                    .list_agent_subtree_thread_ids(conversation_id)
                    .await
                {
                    Ok(thread_ids) => thread_ids,
                    Err(err) => {
                        warn!(
                            "failed to list feedback subtree for thread_id={conversation_id}: {err}"
                        );
                        let mut thread_ids = vec![conversation_id];
                        if let Some(state_db_ctx) = state_db_ctx.as_ref() {
                            for status in [
                                codex_state::DirectionalThreadSpawnEdgeStatus::Open,
                                codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
                            ] {
                                match state_db_ctx
                                    .list_thread_spawn_descendants_with_status(
                                        conversation_id,
                                        status,
                                    )
                                    .await
                                {
                                    Ok(descendant_ids) => thread_ids.extend(descendant_ids),
                                    Err(err) => warn!(
                                        "failed to list persisted feedback subtree for thread_id={conversation_id}: {err}"
                                    ),
                                }
                            }
                        }
                        thread_ids
                    }
                },
                None => Vec::new(),
            };
            let mut feedback_thread_ids = feedback_thread_ids;
            let original_len = feedback_thread_ids.len();
            if let Some(conversation_id) = conversation_id {
                let mut descendant_thread_ids = feedback_thread_ids
                    .into_iter()
                    .filter(|thread_id| *thread_id != conversation_id)
                    .collect::<Vec<_>>();
                // Thread ids are UUIDv7, so lexicographic order tracks creation time.
                descendant_thread_ids.sort_unstable_by_key(ToString::to_string);
                if original_len > MAX_FEEDBACK_TREE_THREADS {
                    let keep_descendants = MAX_FEEDBACK_TREE_THREADS.saturating_sub(1);
                    let split_index = descendant_thread_ids.len().saturating_sub(keep_descendants);
                    descendant_thread_ids = descendant_thread_ids.split_off(split_index);
                    warn!(
                        "feedback log upload for thread_id={conversation_id:?} truncated from {original_len} threads to root plus {keep_descendants} most recent descendants"
                    );
                }
                feedback_thread_ids = Vec::with_capacity(descendant_thread_ids.len() + 1);
                feedback_thread_ids.push(conversation_id);
                feedback_thread_ids.extend(descendant_thread_ids);
            }
            let sqlite_feedback_logs = if let Some(state_db_ctx) = state_db_ctx.as_ref()
                && !feedback_thread_ids.is_empty()
            {
                let thread_id_texts = feedback_thread_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                let thread_id_refs = thread_id_texts
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                match state_db_ctx
                    .query_feedback_logs_for_threads(&thread_id_refs)
                    .await
                {
                    Ok(logs) if logs.is_empty() => None,
                    Ok(logs) => Some(logs),
                    Err(err) => {
                        let thread_ids = thread_id_texts.join(", ");
                        warn!(
                            "failed to query feedback logs from sqlite for thread_ids=[{thread_ids}]: {err}"
                        );
                        None
                    }
                }
            } else {
                None
            };
            (feedback_thread_ids, sqlite_feedback_logs, state_db_ctx)
        } else {
            (Vec::new(), None, None)
        };

        let mut attachment_paths = Vec::new();
        let mut seen_attachment_paths = HashSet::new();
        if include_logs {
            for feedback_thread_id in &feedback_thread_ids {
                let Some(rollout_path) = self
                    .resolve_rollout_path(*feedback_thread_id, state_db_ctx.as_ref())
                    .await
                else {
                    continue;
                };
                if seen_attachment_paths.insert(rollout_path.clone()) {
                    attachment_paths.push(FeedbackAttachmentPath {
                        path: rollout_path,
                        attachment_filename_override: None,
                    });
                }
            }
            if let Some(conversation_id) = conversation_id
                && let Ok(conversation) = self.thread_manager.get_thread(conversation_id).await
                && let Some(guardian_rollout_path) =
                    conversation.guardian_trunk_rollout_path().await
                && seen_attachment_paths.insert(guardian_rollout_path.clone())
            {
                attachment_paths.push(FeedbackAttachmentPath {
                    path: guardian_rollout_path,
                    attachment_filename_override: Some(auto_review_rollout_filename(
                        conversation_id,
                    )),
                });
            }
            if let Some(sandbox_log_attachment) =
                windows_sandbox_log_attachment(&self.config.codex_home)
                && seen_attachment_paths.insert(sandbox_log_attachment.path.clone())
            {
                attachment_paths.push(sandbox_log_attachment);
            }
        }
        if let Some(extra_log_files) = extra_log_files {
            for extra_log_file in extra_log_files {
                if seen_attachment_paths.insert(extra_log_file.clone()) {
                    attachment_paths.push(FeedbackAttachmentPath {
                        path: extra_log_file,
                        attachment_filename_override: None,
                    });
                }
            }
        }

        let mut extra_attachments = Vec::new();
        if include_logs
            && let Some(doctor_report) =
                super::feedback_doctor_report::doctor_feedback_report(&self.config).await
        {
            extra_attachments.push(doctor_report.attachment);
            for (key, value) in doctor_report.tags {
                upload_tags.entry(key).or_insert(value);
            }
        }

        let session_source = self.thread_manager.session_source();

        let upload_result = tokio::task::spawn_blocking(move || {
            let tags = (!upload_tags.is_empty()).then_some(&upload_tags);
            snapshot.upload_feedback(FeedbackUploadOptions {
                classification: &classification,
                reason: reason.as_deref(),
                tags,
                include_logs,
                extra_attachments: &extra_attachments,
                extra_attachment_paths: &attachment_paths,
                session_source: Some(session_source),
                logs_override: sqlite_feedback_logs,
            })
        })
        .await;

        let upload_result = match upload_result {
            Ok(result) => result,
            Err(join_err) => {
                return Err(internal_error(format!(
                    "failed to upload feedback: {join_err}"
                )));
            }
        };

        upload_result.map_err(|err| internal_error(format!("failed to upload feedback: {err}")))?;
        Ok(FeedbackUploadResponse { thread_id })
    }

    async fn resolve_rollout_path(
        &self,
        conversation_id: ThreadId,
        state_db_ctx: Option<&StateDbHandle>,
    ) -> Option<PathBuf> {
        if let Ok(conversation) = self.thread_manager.get_thread(conversation_id).await
            && let Some(rollout_path) = conversation.rollout_path()
        {
            return Some(rollout_path);
        }

        let state_db_ctx = state_db_ctx?;
        state_db_ctx
            .find_rollout_path_by_id(conversation_id, /*archived_only*/ None)
            .await
            .unwrap_or_else(|err| {
                warn!("failed to resolve rollout path for thread_id={conversation_id}: {err}");
                None
            })
    }
}

fn appgen_feedback_tags(items: &[RolloutItem]) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    let mut context_project_id = None;
    // Reuse the app-server history projection so rollback and legacy MCP events have the same
    // semantics here as they do in thread/read and thread/turns/list.
    let turns = codex_app_server_protocol::build_turns_from_rollout_items(items);
    for item in turns.into_iter().flat_map(|turn| turn.items) {
        let ThreadItem::McpToolCall {
            server,
            arguments,
            result: Some(_),
            ..
        } = item
        else {
            continue;
        };
        if server != codex_mcp::CODEX_APPS_MCP_SERVER_NAME {
            continue;
        }
        // Keep returned MCP error results: plugin-service logs their arguments too, and those
        // downstream failures are often exactly what feedback needs to correlate. Calls rejected
        // locally have no result and therefore no matching plugin-service log.
        let Some(arguments) = arguments.as_object() else {
            continue;
        };
        let mut call_tags = BTreeMap::new();
        for (field, prefix) in APPGEN_FEEDBACK_ID_FIELDS {
            let Some(value) = arguments
                .get(field)
                .and_then(serde_json::Value::as_str)
                .filter(|value| valid_appgen_id(field, prefix, value))
            else {
                continue;
            };
            call_tags.insert(field.to_string(), value.to_string());
        }

        let explicit_project_id = call_tags.get("project_id").cloned();
        let version_project_id = call_tags
            .get("version_id")
            .and_then(|version_id| split_compound_appgen_version_id(version_id))
            .map(|(project_id, _)| project_id.to_string());
        if explicit_project_id.is_some()
            && version_project_id.is_some()
            && explicit_project_id != version_project_id
        {
            call_tags.remove("version_id");
        }
        let next_project_id = explicit_project_id.or(version_project_id);
        if let Some(next_project_id) = next_project_id {
            if context_project_id
                .as_ref()
                .is_some_and(|project_id| project_id != &next_project_id)
                || context_project_id.is_none() && !tags.is_empty()
            {
                tags.clear();
            }
            context_project_id = Some(next_project_id);
        } else if !call_tags.is_empty() {
            // Without a project identity, this call cannot safely extend the previous project's
            // tuple. Keep only this call's exact OLogs keys as a standalone correlation context.
            tags.clear();
            context_project_id = None;
        }
        tags.extend(call_tags);
    }
    tags
}

fn valid_appgen_id(field: &str, prefix: &str, value: &str) -> bool {
    if field == "version_id" && split_compound_appgen_version_id(value).is_some() {
        return true;
    }
    valid_prefixed_hex_id(value, prefix)
}

fn split_compound_appgen_version_id(value: &str) -> Option<(&str, &str)> {
    let (project_id, version_id) = value.split_once('~')?;
    (valid_prefixed_hex_id(project_id, "appgprj_") && valid_prefixed_hex_id(version_id, "appgver_"))
        .then_some((project_id, version_id))
}

fn valid_prefixed_hex_id(value: &str, prefix: &str) -> bool {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return false;
    };
    !suffix.is_empty() && suffix.len() <= 64 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn auto_review_rollout_filename(thread_id: ThreadId) -> String {
    format!("auto-review-rollout-{thread_id}.jsonl")
}

#[cfg(target_os = "windows")]
fn windows_sandbox_log_attachment(codex_home: &Path) -> Option<FeedbackAttachmentPath> {
    let sandbox_log_path = codex_windows_sandbox::current_log_file_path_for_codex_home(codex_home);
    sandbox_log_path
        .is_file()
        .then_some(FeedbackAttachmentPath {
            path: sandbox_log_path,
            attachment_filename_override: Some(WINDOWS_SANDBOX_LOG_ATTACHMENT_FILENAME.to_string()),
        })
}

#[cfg(not(target_os = "windows"))]
fn windows_sandbox_log_attachment(_codex_home: &Path) -> Option<FeedbackAttachmentPath> {
    None
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn windows_sandbox_log_attachment_uses_current_log() {
        let codex_home = tempfile::tempdir().expect("create tempdir");
        let sandbox_dir = codex_windows_sandbox::sandbox_dir(codex_home.path());
        std::fs::create_dir_all(&sandbox_dir).expect("create sandbox dir");
        let sandbox_log_path =
            codex_windows_sandbox::current_log_file_path_for_codex_home(codex_home.path());
        std::fs::write(&sandbox_log_path, "sandbox log").expect("write sandbox log");

        let attachment = windows_sandbox_log_attachment(codex_home.path())
            .map(|attachment| (attachment.path, attachment.attachment_filename_override));

        assert_eq!(
            attachment,
            Some((
                sandbox_log_path,
                Some(WINDOWS_SANDBOX_LOG_ATTACHMENT_FILENAME.to_string())
            ))
        );
    }
}

#[cfg(test)]
#[path = "feedback_processor_tests.rs"]
mod feedback_tests;

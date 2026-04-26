use crate::config::AppConfig;
use crate::error::ServiceError;
use crate::models::{
    ConnectionStatus, ReceivedEvent, TenantCredential, TenantSummary, mask_secret,
};
use crate::poller;
use crate::storage::TenantStore;
use crate::wechat_api;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, watch};
use tracing::{info, warn};

pub struct PollWorkerHandle {
    pub stop_tx: watch::Sender<bool>,
    pub join_handle: tokio::task::JoinHandle<()>,
}

pub struct LoginPollHandle {
    pub stop_tx: watch::Sender<bool>,
    pub join_handle: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LoginSession {
    pub qrcode: String,
    pub base_url: String,
    pub auto_start: bool,
    pub status: String,
    pub confirmed: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

pub struct TenantContext {
    pub tenant_id: String,
    pub credential: RwLock<TenantCredential>,
    pub worker: Mutex<Option<PollWorkerHandle>>,
    pub login_worker: Mutex<Option<LoginPollHandle>>,
    pub login_session: RwLock<Option<LoginSession>>,
    pub events: Mutex<VecDeque<ReceivedEvent>>,
    pub latest_context_tokens: Mutex<HashMap<String, String>>,
    pub connection_status: RwLock<ConnectionStatus>,
}

impl TenantContext {
    pub fn new(tenant_id: String, credential: TenantCredential) -> Self {
        let running = credential.is_enabled();
        Self {
            tenant_id,
            credential: RwLock::new(credential),
            worker: Mutex::new(None),
            login_worker: Mutex::new(None),
            login_session: RwLock::new(None),
            events: Mutex::new(VecDeque::new()),
            latest_context_tokens: Mutex::new(HashMap::new()),
            connection_status: RwLock::new(ConnectionStatus {
                running,
                ..ConnectionStatus::default()
            }),
        }
    }

    pub async fn update_credential(&self, credential: TenantCredential) {
        let mut guard = self.credential.write().await;
        *guard = credential;
        drop(guard);
        self.refresh_runtime_flags().await;
    }

    pub async fn push_event(&self, event: ReceivedEvent, retention: usize) {
        {
            let mut events = self.events.lock().await;
            events.push_back(event);
            while events.len() > retention {
                let _ = events.pop_front();
            }
        }

        let now = Utc::now();
        let running = self.credential.read().await.is_enabled();
        let mut status = self.connection_status.write().await;
        if !status.connected {
            status.connected_at = Some(now);
        }
        status.running = running;
        status.connected = true;
        status.last_event_at = Some(now);
        status.last_heartbeat_at = Some(now);
        status.last_error = None;
        status.last_error_at = None;
        status.last_disconnect_reason = None;
    }

    pub async fn list_events(&self, limit: usize) -> Vec<ReceivedEvent> {
        let events = self.events.lock().await;
        let take_size = limit.min(events.len());
        events
            .iter()
            .rev()
            .take(take_size)
            .cloned()
            .collect::<Vec<_>>()
    }

    pub async fn set_context_token(&self, user_id: &str, context_token: &str) {
        let user_id = user_id.trim();
        let context_token = context_token.trim();
        if user_id.is_empty() || context_token.is_empty() {
            return;
        }

        let mut guard = self.latest_context_tokens.lock().await;
        guard.insert(user_id.to_string(), context_token.to_string());
    }

    pub async fn get_context_token(&self, user_id: &str) -> Option<String> {
        let guard = self.latest_context_tokens.lock().await;
        guard.get(user_id).cloned()
    }

    pub async fn summary(&self) -> TenantSummary {
        let credential = self.credential.read().await.clone();
        let connection = self.connection_status.read().await.clone();
        let enabled = credential.is_enabled();
        let logged_in = credential.is_logged_in();
        TenantSummary {
            tenant_id: self.tenant_id.clone(),
            logged_in,
            bot_token_masked: credential.bot_token.as_deref().map(mask_secret),
            api_base_url: credential.api_base_url,
            account_id: credential.account_id,
            user_id: credential.user_id,
            lowcode_ws_base_url: credential.lowcode_ws_base_url,
            lowcode_forward_enabled: credential.lowcode_forward_enabled,
            enabled,
            assistant_name: credential.assistant_name,
            command_actions_configured: credential
                .command_actions
                .as_ref()
                .map(|items| !items.is_empty())
                .unwrap_or(false),
            connection,
        }
    }

    pub async fn refresh_runtime_flags(&self) {
        let credential = self.credential.read().await.clone();
        let mut status = self.connection_status.write().await;
        status.running = credential.is_enabled();
        if !credential.is_enabled() || !credential.is_logged_in() {
            status.connected = false;
            status.last_disconnect_at = Some(Utc::now());
            status.last_disconnect_reason = Some(if credential.is_enabled() {
                "tenant_not_logged_in".to_string()
            } else {
                "tenant_disabled".to_string()
            });
        }
    }

    pub async fn mark_connecting(&self) {
        let mut status = self.connection_status.write().await;
        status.running = true;
        status.connected = false;
    }

    pub async fn mark_connected(&self) {
        let now = Utc::now();
        let mut status = self.connection_status.write().await;
        status.running = true;
        status.connected = true;
        status.connected_at = Some(now);
        status.last_heartbeat_at = Some(now);
        status.last_error = None;
        status.last_error_at = None;
        status.last_disconnect_reason = None;
    }

    pub async fn mark_heartbeat(&self) {
        let mut status = self.connection_status.write().await;
        status.last_heartbeat_at = Some(Utc::now());
    }

    pub async fn set_last_error(&self, err: impl ToString) {
        let mut status = self.connection_status.write().await;
        status.last_error = Some(err.to_string());
        status.last_error_at = Some(Utc::now());
    }

    pub async fn mark_reconnect_pending(&self, reason: impl ToString) {
        let mut status = self.connection_status.write().await;
        status.running = true;
        status.connected = false;
        status.last_disconnect_at = Some(Utc::now());
        status.last_disconnect_reason = Some(reason.to_string());
        status.reconnect_count = status.reconnect_count.saturating_add(1);
    }

    pub async fn mark_disabled(&self) {
        let mut status = self.connection_status.write().await;
        status.running = false;
        status.connected = false;
        status.last_disconnect_at = Some(Utc::now());
        status.last_disconnect_reason = Some("tenant_disabled".to_string());
    }

    pub async fn prune_finished_worker(&self) {
        let finished = {
            let guard = self.worker.lock().await;
            guard
                .as_ref()
                .map(|handle| handle.join_handle.is_finished())
                .unwrap_or(false)
        };

        if finished {
            let handle_opt = {
                let mut guard = self.worker.lock().await;
                guard.take()
            };
            if let Some(handle) = handle_opt {
                let _ = handle.join_handle.await;
            }
            self.refresh_runtime_flags().await;
        }
    }

    pub async fn prune_finished_login_worker(&self) {
        let finished = {
            let guard = self.login_worker.lock().await;
            guard
                .as_ref()
                .map(|handle| handle.join_handle.is_finished())
                .unwrap_or(false)
        };

        if finished {
            let handle_opt = {
                let mut guard = self.login_worker.lock().await;
                guard.take()
            };
            if let Some(handle) = handle_opt {
                let _ = handle.join_handle.await;
            }
        }
    }

    pub async fn set_login_session(&self, session: LoginSession) {
        let mut guard = self.login_session.write().await;
        *guard = Some(session);
    }

    pub async fn get_login_session(&self) -> Option<LoginSession> {
        self.login_session.read().await.clone()
    }

    pub async fn update_login_session<F>(&self, qrcode: &str, update: F) -> Option<LoginSession>
    where
        F: FnOnce(&mut LoginSession),
    {
        let mut guard = self.login_session.write().await;
        let session = guard.as_mut()?;
        if session.qrcode != qrcode {
            return None;
        }
        update(session);
        session.updated_at = Utc::now();
        Some(session.clone())
    }

    pub async fn clear_login_session(&self) {
        let mut guard = self.login_session.write().await;
        *guard = None;
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub http_client: reqwest::Client,
    pub tenant_store: TenantStore,
    pub tenants: DashMap<String, Arc<TenantContext>>,
}

impl AppState {
    pub async fn new(config: AppConfig) -> Result<Self, ServiceError> {
        let timeout =
            std::time::Duration::from_secs(config.runtime.poll_timeout_seconds.max(5) + 10);
        let http_client = reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(timeout)
            .build()
            .expect("failed to create reqwest client");

        let tenant_store = TenantStore::new(&config.database).await?;

        Ok(Self {
            tenant_store,
            config,
            http_client,
            tenants: DashMap::new(),
        })
    }

    pub fn get_tenant(&self, tenant_id: &str) -> Option<Arc<TenantContext>> {
        self.tenants
            .get(tenant_id)
            .map(|entry| entry.value().clone())
    }

    pub async fn list_tenants(&self) -> Vec<TenantSummary> {
        let mut out = Vec::with_capacity(self.tenants.len());
        for item in &self.tenants {
            let mut summary = item.value().summary().await;
            if summary.assistant_name.is_none() {
                summary.assistant_name = Some(self.config.runtime.assistant_name.clone());
            }
            out.push(summary);
        }
        out.sort_by(|a, b| a.tenant_id.cmp(&b.tenant_id));
        out
    }

    pub async fn load_tenants_from_storage(&self) -> Result<usize, ServiceError> {
        let tenants = self.tenant_store.list_tenants().await?;
        let mut loaded = 0usize;
        for item in tenants {
            let mut credential = item.credential;
            if credential.ensure_outbound_token() {
                self.tenant_store
                    .upsert_tenant(&item.tenant_id, &credential)
                    .await?;
            }
            let context = Arc::new(TenantContext::new(item.tenant_id.clone(), credential));
            self.tenants.insert(item.tenant_id, context);
            loaded += 1;
        }
        Ok(loaded)
    }

    pub async fn upsert_tenant(
        &self,
        tenant_id: &str,
        credential: TenantCredential,
    ) -> Result<(Arc<TenantContext>, bool), ServiceError> {
        self.tenant_store
            .upsert_tenant(tenant_id, &credential)
            .await?;

        if let Some(existing) = self.get_tenant(tenant_id) {
            existing.update_credential(credential).await;
            Ok((existing, false))
        } else {
            let context = Arc::new(TenantContext::new(tenant_id.to_string(), credential));
            self.tenants.insert(tenant_id.to_string(), context.clone());
            Ok((context, true))
        }
    }

    pub async fn remove_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Option<Arc<TenantContext>>, ServiceError> {
        let Some(tenant) = self.get_tenant(tenant_id) else {
            return Ok(None);
        };
        self.tenant_store.update_enabled(tenant_id, false).await?;
        let mut credential = tenant.credential.read().await.clone();
        credential.enabled = Some(false);
        tenant.update_credential(credential).await;
        tenant.mark_disabled().await;
        Ok(Some(tenant))
    }

    pub async fn set_tenant_enabled(
        &self,
        tenant_id: &str,
        enabled: bool,
    ) -> Result<Arc<TenantContext>, ServiceError> {
        let tenant = self
            .get_tenant(tenant_id)
            .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;
        self.tenant_store.update_enabled(tenant_id, enabled).await?;
        let mut credential = tenant.credential.read().await.clone();
        credential.enabled = Some(enabled);
        tenant.update_credential(credential).await;
        if !enabled {
            tenant.mark_disabled().await;
        }
        Ok(tenant)
    }

    pub async fn update_sync_buf(
        &self,
        tenant_id: &str,
        sync_buf: Option<&str>,
    ) -> Result<(), ServiceError> {
        self.tenant_store
            .update_sync_buf(tenant_id, sync_buf)
            .await?;
        if let Some(tenant) = self.get_tenant(tenant_id) {
            let mut credential = tenant.credential.read().await.clone();
            credential.sync_buf = sync_buf.map(ToString::to_string);
            tenant.update_credential(credential).await;
        }
        Ok(())
    }

    pub async fn update_login_result(
        &self,
        tenant_id: &str,
        bot_token: &str,
        api_base_url: &str,
        account_id: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<Arc<TenantContext>, ServiceError> {
        let tenant = self
            .get_tenant(tenant_id)
            .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;
        let mut credential = tenant.credential.read().await.clone();
        credential.bot_token = Some(bot_token.to_string());
        credential.api_base_url = Some(api_base_url.to_string());
        credential.account_id = account_id.map(ToString::to_string);
        credential.user_id = user_id.map(ToString::to_string);
        credential.sync_buf = None;
        self.tenant_store
            .upsert_tenant(tenant_id, &credential)
            .await?;
        tenant.update_credential(credential).await;
        Ok(tenant)
    }

    pub async fn start_login_worker(
        self: &Arc<Self>,
        tenant: Arc<TenantContext>,
        qrcode: String,
        base_url: String,
        auto_start: bool,
    ) -> Result<(), ServiceError> {
        tenant.prune_finished_login_worker().await;
        let previous_handle = {
            let mut guard = tenant.login_worker.lock().await;
            guard.take()
        };

        let now = Utc::now();
        tenant
            .set_login_session(LoginSession {
                qrcode: qrcode.clone(),
                base_url: base_url.clone(),
                auto_start,
                status: "pending".to_string(),
                confirmed: false,
                created_at: now,
                updated_at: now,
                completed_at: None,
                last_error: None,
            })
            .await;

        if let Some(handle) = previous_handle {
            let tenant_id = tenant.tenant_id.clone();
            let _ = handle.stop_tx.send(true);
            tokio::spawn(async move {
                let _ = handle.join_handle.await;
                info!(
                    tenant_id = %tenant_id,
                    "旧微信扫码后台轮询已后台回收"
                );
            });
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let state_clone = self.clone();
        let tenant_clone = tenant.clone();
        let qrcode_clone = qrcode.clone();
        let join_handle = tokio::spawn(async move {
            run_login_poll_worker(state_clone, tenant_clone, qrcode_clone, base_url, stop_rx).await;
        });

        let mut guard = tenant.login_worker.lock().await;
        *guard = Some(LoginPollHandle {
            stop_tx,
            join_handle,
        });
        drop(guard);

        info!(
            tenant_id = %tenant.tenant_id,
            qrcode = %wechat_api::mask_identifier(&qrcode),
            auto_start,
            "已启动微信扫码后台轮询"
        );
        Ok(())
    }

    pub async fn stop_login_worker(&self, tenant: &Arc<TenantContext>) -> bool {
        let handle_opt = {
            let mut guard = tenant.login_worker.lock().await;
            guard.take()
        };

        if let Some(handle) = handle_opt {
            let _ = handle.stop_tx.send(true);
            let _ = handle.join_handle.await;
            true
        } else {
            false
        }
    }

    pub async fn start_tenant_worker(
        self: &Arc<Self>,
        tenant: Arc<TenantContext>,
    ) -> Result<bool, ServiceError> {
        tenant.prune_finished_worker().await;

        let credential = tenant.credential.read().await.clone();
        if !credential.is_enabled() {
            return Err(ServiceError::BadRequest("租户已停用".to_string()));
        }
        if !credential.is_logged_in() {
            return Err(ServiceError::BadRequest("租户尚未扫码登录".to_string()));
        }

        let mut guard = tenant.worker.lock().await;
        if guard.is_some() {
            return Ok(false);
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let tenant_clone = tenant.clone();
        let app_state_clone = self.clone();
        let join_handle = tokio::spawn(async move {
            poller::run_tenant_poll_worker(app_state_clone, tenant_clone, stop_rx).await;
        });

        *guard = Some(PollWorkerHandle {
            stop_tx,
            join_handle,
        });
        drop(guard);

        tenant.mark_connecting().await;
        Ok(true)
    }

    pub async fn stop_tenant_worker(&self, tenant: &Arc<TenantContext>) -> bool {
        let handle_opt = {
            let mut guard = tenant.worker.lock().await;
            guard.take()
        };

        if let Some(handle) = handle_opt {
            let _ = handle.stop_tx.send(true);
            let _ = handle.join_handle.await;
            tenant.refresh_runtime_flags().await;
            true
        } else {
            false
        }
    }
}

const LOGIN_POLL_TIMEOUT_SECS: u64 = 300;

fn is_terminal_login_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "confirmed" | "expired" | "timeout" | "canceled" | "cancelled" | "rejected" | "failed"
    )
}

async fn run_login_poll_worker(
    state: Arc<AppState>,
    tenant: Arc<TenantContext>,
    qrcode: String,
    base_url: String,
    mut stop_rx: watch::Receiver<bool>,
) {
    let poll_interval = Duration::from_secs(state.config.runtime.poll_retry_seconds.max(1));
    let deadline = Instant::now() + Duration::from_secs(LOGIN_POLL_TIMEOUT_SECS);

    loop {
        if *stop_rx.borrow() {
            info!(
                tenant_id = %tenant.tenant_id,
                qrcode = %wechat_api::mask_identifier(&qrcode),
                "微信扫码后台轮询已停止"
            );
            return;
        }

        match wechat_api::poll_login_status(&state, &tenant, &qrcode, Some(base_url.as_str())).await
        {
            Ok(response) => {
                let status = response.status.clone();
                let session = tenant
                    .update_login_session(&qrcode, |session| {
                        session.status = status.clone();
                        session.last_error = None;
                        if status == "confirmed" {
                            session.confirmed = true;
                            session.completed_at = Some(Utc::now());
                        } else if is_terminal_login_status(&status) {
                            session.completed_at = Some(Utc::now());
                        }
                    })
                    .await;

                let Some(session) = session else {
                    info!(
                        tenant_id = %tenant.tenant_id,
                        qrcode = %wechat_api::mask_identifier(&qrcode),
                        "微信扫码后台轮询发现会话已被替换，结束旧轮询"
                    );
                    return;
                };

                info!(
                    tenant_id = %tenant.tenant_id,
                    qrcode = %wechat_api::mask_identifier(&qrcode),
                    status = %session.status,
                    confirmed = session.confirmed,
                    "微信扫码后台轮询状态已更新"
                );

                if status == "confirmed" {
                    let bot_token = match response.bot_token.as_deref() {
                        Some(value) if !value.trim().is_empty() => value,
                        _ => {
                            let err = "扫码确认成功但缺少 bot_token".to_string();
                            warn!(
                                tenant_id = %tenant.tenant_id,
                                qrcode = %wechat_api::mask_identifier(&qrcode),
                                error = %err,
                                "微信扫码后台轮询确认失败"
                            );
                            let _ = tenant
                                .update_login_session(&qrcode, |session| {
                                    session.status = "error".to_string();
                                    session.last_error = Some(err.clone());
                                    session.completed_at = Some(Utc::now());
                                })
                                .await;
                            return;
                        }
                    };

                    let login_base_url = response.baseurl.as_deref().unwrap_or(base_url.as_str());
                    match state
                        .update_login_result(
                            &tenant.tenant_id,
                            bot_token,
                            login_base_url,
                            response.ilink_bot_id.as_deref(),
                            response.ilink_user_id.as_deref(),
                        )
                        .await
                    {
                        Ok(updated_tenant) => {
                            let auto_start = updated_tenant
                                .get_login_session()
                                .await
                                .filter(|session| session.qrcode == qrcode)
                                .map(|session| session.auto_start)
                                .unwrap_or(true);
                            if auto_start && updated_tenant.credential.read().await.is_enabled() {
                                match state.start_tenant_worker(updated_tenant.clone()).await {
                                    Ok(true) => info!(
                                        tenant_id = %tenant.tenant_id,
                                        account_id = ?response.ilink_bot_id,
                                        user_id = ?response.ilink_user_id,
                                        auto_start,
                                        "微信扫码后台轮询确认成功并已启动租户轮询"
                                    ),
                                    Ok(false) => info!(
                                        tenant_id = %tenant.tenant_id,
                                        account_id = ?response.ilink_bot_id,
                                        user_id = ?response.ilink_user_id,
                                        auto_start,
                                        "微信扫码后台轮询确认成功，租户轮询已在运行"
                                    ),
                                    Err(err) => warn!(
                                        tenant_id = %tenant.tenant_id,
                                        account_id = ?response.ilink_bot_id,
                                        user_id = ?response.ilink_user_id,
                                        auto_start,
                                        error = %err,
                                        "微信扫码后台轮询确认成功，但启动租户轮询失败"
                                    ),
                                }
                            } else {
                                info!(
                                    tenant_id = %tenant.tenant_id,
                                    account_id = ?response.ilink_bot_id,
                                    user_id = ?response.ilink_user_id,
                                    auto_start,
                                    "微信扫码后台轮询确认成功并已写入租户凭据"
                                );
                            }
                        }
                        Err(err) => {
                            warn!(
                                tenant_id = %tenant.tenant_id,
                                qrcode = %wechat_api::mask_identifier(&qrcode),
                                error = %err,
                                "微信扫码后台轮询写入租户凭据失败"
                            );
                            let _ = tenant
                                .update_login_session(&qrcode, |session| {
                                    session.status = "error".to_string();
                                    session.last_error = Some(err.to_string());
                                    session.completed_at = Some(Utc::now());
                                })
                                .await;
                        }
                    }
                    return;
                }

                if is_terminal_login_status(&status) {
                    info!(
                        tenant_id = %tenant.tenant_id,
                        qrcode = %wechat_api::mask_identifier(&qrcode),
                        status = %status,
                        "微信扫码后台轮询已结束"
                    );
                    return;
                }
            }
            Err(err) => {
                warn!(
                    tenant_id = %tenant.tenant_id,
                    qrcode = %wechat_api::mask_identifier(&qrcode),
                    error = %err,
                    "微信扫码后台轮询请求失败"
                );
                let _ = tenant
                    .update_login_session(&qrcode, |session| {
                        session.last_error = Some(err.to_string());
                    })
                    .await;
            }
        }

        if Instant::now() >= deadline {
            let _ = tenant
                .update_login_session(&qrcode, |session| {
                    session.status = "timeout".to_string();
                    session.last_error = Some(format!(
                        "二维码状态轮询超过 {} 秒仍未完成",
                        LOGIN_POLL_TIMEOUT_SECS
                    ));
                    session.completed_at = Some(Utc::now());
                })
                .await;
            warn!(
                tenant_id = %tenant.tenant_id,
                qrcode = %wechat_api::mask_identifier(&qrcode),
                timeout_seconds = LOGIN_POLL_TIMEOUT_SECS,
                "微信扫码后台轮询超时"
            );
            return;
        }

        let sleep = tokio::time::sleep(poll_interval);
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    info!(
                        tenant_id = %tenant.tenant_id,
                        qrcode = %wechat_api::mask_identifier(&qrcode),
                        "微信扫码后台轮询收到停止信号"
                    );
                    return;
                }
            }
        }
    }
}

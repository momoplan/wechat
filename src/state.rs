use crate::config::AppConfig;
use crate::error::ServiceError;
use crate::models::{
    ConnectionStatus, ReceivedEvent, TenantCredential, TenantSummary, mask_secret,
};
use crate::poller;
use crate::storage::TenantStore;
use chrono::Utc;
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, watch};

pub struct PollWorkerHandle {
    pub stop_tx: watch::Sender<bool>,
    pub join_handle: tokio::task::JoinHandle<()>,
}

pub struct TenantContext {
    pub tenant_id: String,
    pub credential: RwLock<TenantCredential>,
    pub worker: Mutex<Option<PollWorkerHandle>>,
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
            out.push(item.value().summary().await);
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
        if !self.tenants.contains_key(tenant_id) {
            return Ok(None);
        }

        self.tenant_store.delete_tenant(tenant_id).await?;
        Ok(self.tenants.remove(tenant_id).map(|(_, value)| value))
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

use crate::config::DatabaseConfig;
use crate::models::TenantCredential;
use anyhow::{Context, Result};
use sqlx::Row;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PersistedTenant {
    pub tenant_id: String,
    pub credential: TenantCredential,
}

#[derive(Debug, Clone)]
pub struct TenantStore {
    pool: MySqlPool,
}

impl TenantStore {
    pub async fn new(config: &DatabaseConfig) -> Result<Self> {
        let pool = MySqlPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(Duration::from_secs(config.connect_timeout))
            .idle_timeout(Duration::from_secs(config.idle_timeout))
            .connect(&config.url)
            .await
            .with_context(|| format!("连接 MySQL 失败: {}", config.url))?;

        let store = Self { pool };
        store.init_schema().await?;
        Ok(store)
    }

    pub async fn list_tenants(&self) -> Result<Vec<PersistedTenant>> {
        let rows = sqlx::query(
            "SELECT tenant_id, bot_token, api_base_url, account_id, user_id, sync_buf, lowcode_ws_base_url, lowcode_ws_token, lowcode_forward_enabled, enabled FROM wechat_tenant_credentials ORDER BY tenant_id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("查询 wechat_tenant_credentials 失败")?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(PersistedTenant {
                tenant_id: row.get("tenant_id"),
                credential: TenantCredential {
                    bot_token: row.get("bot_token"),
                    api_base_url: row.get("api_base_url"),
                    account_id: row.get("account_id"),
                    user_id: row.get("user_id"),
                    sync_buf: row.get("sync_buf"),
                    lowcode_ws_base_url: row.get("lowcode_ws_base_url"),
                    lowcode_ws_token: row.get("lowcode_ws_token"),
                    lowcode_forward_enabled: row.get("lowcode_forward_enabled"),
                    enabled: row.get("enabled"),
                },
            });
        }
        Ok(out)
    }

    pub async fn upsert_tenant(
        &self,
        tenant_id: &str,
        credential: &TenantCredential,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO wechat_tenant_credentials (
              tenant_id, bot_token, api_base_url, account_id, user_id, sync_buf, lowcode_ws_base_url, lowcode_ws_token, lowcode_forward_enabled, enabled
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON DUPLICATE KEY UPDATE
              bot_token = VALUES(bot_token),
              api_base_url = VALUES(api_base_url),
              account_id = VALUES(account_id),
              user_id = VALUES(user_id),
              sync_buf = VALUES(sync_buf),
              lowcode_ws_base_url = VALUES(lowcode_ws_base_url),
              lowcode_ws_token = VALUES(lowcode_ws_token),
              lowcode_forward_enabled = VALUES(lowcode_forward_enabled),
              enabled = VALUES(enabled),
              updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(tenant_id)
        .bind(credential.bot_token.as_deref())
        .bind(credential.api_base_url.as_deref())
        .bind(credential.account_id.as_deref())
        .bind(credential.user_id.as_deref())
        .bind(credential.sync_buf.as_deref())
        .bind(credential.lowcode_ws_base_url.as_deref())
        .bind(credential.lowcode_ws_token.as_deref())
        .bind(credential.lowcode_forward_enabled)
        .bind(credential.enabled.unwrap_or(true))
        .execute(&self.pool)
        .await
        .context("写入 wechat_tenant_credentials 失败")?;
        Ok(())
    }

    pub async fn delete_tenant(&self, tenant_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM wechat_tenant_credentials WHERE tenant_id = ?")
            .bind(tenant_id)
            .execute(&self.pool)
            .await
            .context("删除 wechat_tenant_credentials 失败")?;
        Ok(())
    }

    pub async fn update_enabled(&self, tenant_id: &str, enabled: bool) -> Result<()> {
        sqlx::query(
            "UPDATE wechat_tenant_credentials SET enabled = ?, updated_at = CURRENT_TIMESTAMP WHERE tenant_id = ?",
        )
        .bind(enabled)
        .bind(tenant_id)
        .execute(&self.pool)
        .await
        .context("更新 wechat_tenant_credentials.enabled 失败")?;
        Ok(())
    }

    pub async fn update_sync_buf(&self, tenant_id: &str, sync_buf: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE wechat_tenant_credentials SET sync_buf = ?, updated_at = CURRENT_TIMESTAMP WHERE tenant_id = ?",
        )
        .bind(sync_buf)
        .bind(tenant_id)
        .execute(&self.pool)
        .await
        .context("更新 wechat_tenant_credentials.sync_buf 失败")?;
        Ok(())
    }

    async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS wechat_tenant_credentials (
              tenant_id VARCHAR(128) PRIMARY KEY,
              bot_token VARCHAR(255) NULL,
              api_base_url VARCHAR(512) NULL,
              account_id VARCHAR(255) NULL,
              user_id VARCHAR(255) NULL,
              sync_buf TEXT NULL,
              lowcode_ws_base_url VARCHAR(512) NULL,
              lowcode_ws_token VARCHAR(255) NULL,
              lowcode_forward_enabled TINYINT(1) NULL,
              enabled TINYINT(1) NOT NULL DEFAULT 1,
              updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
            "#,
        )
        .execute(&self.pool)
        .await
        .context("初始化 wechat_tenant_credentials 表失败")?;

        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN bot_token VARCHAR(255) NULL",
            "bot_token",
        )
        .await?;
        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN api_base_url VARCHAR(512) NULL",
            "api_base_url",
        )
        .await?;
        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN account_id VARCHAR(255) NULL",
            "account_id",
        )
        .await?;
        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN user_id VARCHAR(255) NULL",
            "user_id",
        )
        .await?;
        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN sync_buf TEXT NULL",
            "sync_buf",
        )
        .await?;
        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN lowcode_ws_base_url VARCHAR(512) NULL",
            "lowcode_ws_base_url",
        )
        .await?;
        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN lowcode_ws_token VARCHAR(255) NULL",
            "lowcode_ws_token",
        )
        .await?;
        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN lowcode_forward_enabled TINYINT(1) NULL",
            "lowcode_forward_enabled",
        )
        .await?;
        self.add_column_if_missing(
            "ALTER TABLE wechat_tenant_credentials ADD COLUMN enabled TINYINT(1) NOT NULL DEFAULT 1",
            "enabled",
        )
        .await?;
        Ok(())
    }

    async fn add_column_if_missing(&self, ddl: &str, column_name: &str) -> Result<()> {
        match sqlx::query(ddl).execute(&self.pool).await {
            Ok(_) => Ok(()),
            Err(err) => {
                let message = err.to_string();
                if message.contains("Duplicate column name")
                    || message.contains("already exists")
                    || message.contains(&format!("'{}'", column_name))
                {
                    Ok(())
                } else {
                    Err(err).with_context(|| {
                        format!("升级 wechat_tenant_credentials 列失败: {column_name}")
                    })
                }
            }
        }
    }
}

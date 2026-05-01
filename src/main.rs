mod channel_binding_client;
mod channel_session_client;
mod config;
mod error;
mod logging;
mod models;
mod poller;
mod project_file_client;
mod routes;
mod state;
mod storage;
mod wechat_api;

use actix_web::{App, HttpServer, web};
use config::AppConfig;
use logging::init_logging;
use state::AppState;
use std::io::{Error, ErrorKind};
use std::sync::Arc;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let config_path =
        parse_config_path_arg().map_err(|err| Error::new(ErrorKind::InvalidInput, err))?;
    let app_config = if let Some(path) = config_path {
        AppConfig::from_file(&path)
            .map_err(|err| Error::other(format!("加载配置文件失败({path}): {err}")))?
    } else {
        match AppConfig::load_default() {
            Ok(config) => {
                println!("✓ 配置文件加载成功");
                config
            }
            Err(err) => {
                println!("⚠ 配置文件加载失败，使用默认配置: {err}");
                AppConfig::default()
            }
        }
    };

    init_logging(&app_config.logging)
        .map_err(|err| std::io::Error::other(format!("初始化日志失败: {err}")))?;

    let state = Arc::new(
        AppState::new(app_config.clone())
            .await
            .map_err(|err| std::io::Error::other(format!("初始化应用状态失败: {err}")))?,
    );
    let loaded = state
        .load_tenants_from_storage()
        .await
        .map_err(|err| std::io::Error::other(format!("加载租户配置失败: {err}")))?;
    info!(loaded_tenants = loaded, "已从数据库加载租户配置");

    let tenants = state
        .tenants
        .iter()
        .map(|entry| entry.value().clone())
        .collect::<Vec<_>>();
    let mut auto_started = 0usize;
    for tenant in tenants {
        let credential = tenant.credential.read().await.clone();
        if !credential.is_enabled() || !credential.is_logged_in() {
            continue;
        }
        match state.start_tenant_worker(tenant.clone()).await {
            Ok(true) => {
                auto_started += 1;
                info!(tenant_id = %tenant.tenant_id, "启动时自动恢复微信轮询 worker");
            }
            Ok(false) => {}
            Err(err) => {
                warn!(
                    tenant_id = %tenant.tenant_id,
                    error = %err,
                    "启动时自动恢复微信轮询 worker 失败"
                );
            }
        }
    }
    info!(
        auto_started_tenants = auto_started,
        "启动时自动恢复租户轮询完成"
    );

    let management_bind = format!(
        "{}:{}",
        app_config.server.bind_address, app_config.server.management_port
    );
    let api_bind = format!(
        "{}:{}",
        app_config.server.bind_address, app_config.server.api_port
    );

    info!(management_addr = %management_bind, "管理服务启动");
    info!(api_addr = %api_bind, "业务 API 服务启动");

    let management_state = state.clone();
    let management_server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(management_state.clone()))
            .service(routes::management::scope())
    })
    .bind(&management_bind)?
    .run();

    let api_state = state.clone();
    let api_server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(api_state.clone()))
            .service(routes::api::scope())
    })
    .bind(&api_bind)?
    .run();

    tokio::try_join!(management_server, api_server)?;

    Ok(())
}

fn parse_config_path_arg() -> Result<Option<String>, String> {
    let mut args = std::env::args().skip(1);
    let mut config_path: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--config 需要一个文件路径参数".to_string())?;
                if config_path.is_some() {
                    return Err("重复传入 --config 参数".to_string());
                }
                config_path = Some(value);
            }
            "-h" | "--help" => {
                println!("Usage: wechat [--config <config.toml|yaml|yml|json>]");
                std::process::exit(0);
            }
            other => {
                return Err(format!("未知参数: {other}"));
            }
        }
    }

    Ok(config_path)
}

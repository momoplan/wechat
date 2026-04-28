use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub wechat: WechatConfig,
    pub runtime: RuntimeConfig,
    #[serde(default, alias = "storage")]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub lowcode_ws: LowcodeWsConfig,
    #[serde(default)]
    pub channel_gateway: ChannelGatewayConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    #[serde(default = "default_management_port")]
    pub management_port: u16,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WechatConfig {
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_event_retention")]
    pub event_retention_per_tenant: usize,
    #[serde(default = "default_poll_retry_seconds")]
    pub poll_retry_seconds: u64,
    #[serde(default = "default_poll_timeout_seconds")]
    pub poll_timeout_seconds: u64,
    #[serde(default = "default_max_inline_image_bytes")]
    pub max_inline_image_bytes: usize,
    #[serde(default = "default_assistant_name")]
    pub assistant_name: String,
    #[serde(default = "default_command_actions")]
    pub command_actions: Vec<CommandActionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandActionConfig {
    #[serde(default, alias = "command")]
    pub text: String,
    #[serde(default)]
    pub action: String,
    #[serde(default, alias = "agentConfigId", alias = "agentId")]
    pub agent_config_id: Option<String>,
    #[serde(default, alias = "sessionId", alias = "targetSessionId")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_database_url")]
    pub url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: default_database_url(),
            max_connections: default_max_connections(),
            connect_timeout: default_connect_timeout(),
            idle_timeout: default_idle_timeout(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LowcodeWsConfig {
    #[serde(default = "default_lowcode_ws_base_url")]
    pub base_url: String,
    #[serde(default = "default_forward_enabled")]
    pub forward_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelGatewayConfig {
    #[serde(default)]
    pub inbound_token: Option<String>,
}

impl Default for LowcodeWsConfig {
    fn default() -> Self {
        Self {
            base_url: default_lowcode_ws_base_url(),
            forward_enabled: default_forward_enabled(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_dir")]
    pub log_dir: String,
}

fn default_bind_address() -> String {
    "0.0.0.0".to_string()
}

fn default_management_port() -> u16 {
    3141
}

fn default_api_port() -> u16 {
    3140
}

fn default_base_url() -> String {
    "https://ilinkai.weixin.qq.com".to_string()
}

fn default_event_retention() -> usize {
    200
}

fn default_poll_retry_seconds() -> u64 {
    3
}

fn default_poll_timeout_seconds() -> u64 {
    35
}

fn default_max_inline_image_bytes() -> usize {
    5 * 1024 * 1024
}

fn default_assistant_name() -> String {
    "小百".to_string()
}

fn default_command_actions() -> Vec<CommandActionConfig> {
    vec![
        CommandActionConfig {
            text: "新话题".to_string(),
            action: "new".to_string(),
            agent_config_id: None,
            session_id: None,
            service: None,
            method: None,
            params: None,
        },
        CommandActionConfig {
            text: "结束当前会话".to_string(),
            action: "abort".to_string(),
            agent_config_id: None,
            session_id: None,
            service: None,
            method: None,
            params: None,
        },
        CommandActionConfig {
            text: "命令列表".to_string(),
            action: "list_commands".to_string(),
            agent_config_id: None,
            session_id: None,
            service: None,
            method: None,
            params: None,
        },
        CommandActionConfig {
            text: "列会话".to_string(),
            action: "list_sessions".to_string(),
            agent_config_id: None,
            session_id: None,
            service: None,
            method: None,
            params: None,
        },
    ]
}

fn default_database_url() -> String {
    "mysql://root:password@127.0.0.1:3306/wechat?charset=utf8mb4".to_string()
}

fn default_max_connections() -> u32 {
    20
}

fn default_connect_timeout() -> u64 {
    5
}

fn default_idle_timeout() -> u64 {
    60
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_dir() -> String {
    "logs".to_string()
}

fn default_lowcode_ws_base_url() -> String {
    "http://127.0.0.1:4012".to_string()
}

fn default_forward_enabled() -> bool {
    false
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind_address: default_bind_address(),
                management_port: default_management_port(),
                api_port: default_api_port(),
            },
            wechat: WechatConfig {
                base_url: default_base_url(),
            },
            runtime: RuntimeConfig {
                event_retention_per_tenant: default_event_retention(),
                poll_retry_seconds: default_poll_retry_seconds(),
                poll_timeout_seconds: default_poll_timeout_seconds(),
                max_inline_image_bytes: default_max_inline_image_bytes(),
                assistant_name: default_assistant_name(),
                command_actions: default_command_actions(),
            },
            database: DatabaseConfig::default(),
            lowcode_ws: LowcodeWsConfig::default(),
            channel_gateway: ChannelGatewayConfig {
                inbound_token: None,
            },
            logging: LoggingConfig {
                level: default_log_level(),
                log_dir: default_log_dir(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommandActionConfig, RuntimeConfig, default_assistant_name, default_command_actions,
    };

    #[test]
    fn runtime_defaults_include_default_command_actions() {
        let runtime = RuntimeConfig {
            event_retention_per_tenant: 200,
            poll_retry_seconds: 3,
            poll_timeout_seconds: 35,
            max_inline_image_bytes: 1024,
            assistant_name: default_assistant_name(),
            command_actions: default_command_actions(),
        };

        assert_eq!(runtime.assistant_name, "小百");
        assert_eq!(
            runtime.command_actions,
            vec![
                CommandActionConfig {
                    text: "新话题".to_string(),
                    action: "new".to_string(),
                    agent_config_id: None,
                    session_id: None,
                    service: None,
                    method: None,
                    params: None,
                },
                CommandActionConfig {
                    text: "结束当前会话".to_string(),
                    action: "abort".to_string(),
                    agent_config_id: None,
                    session_id: None,
                    service: None,
                    method: None,
                    params: None,
                },
                CommandActionConfig {
                    text: "命令列表".to_string(),
                    action: "list_commands".to_string(),
                    agent_config_id: None,
                    session_id: None,
                    service: None,
                    method: None,
                    params: None,
                },
                CommandActionConfig {
                    text: "列会话".to_string(),
                    action: "list_sessions".to_string(),
                    agent_config_id: None,
                    session_id: None,
                    service: None,
                    method: None,
                    params: None,
                }
            ]
        );
    }
}

impl AppConfig {
    pub fn from_file<P: AsRef<Path>>(config_path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let settings = config::Config::builder()
            .add_source(config::File::with_name(
                config_path.as_ref().to_str().ok_or("invalid config path")?,
            ))
            .add_source(config::Environment::with_prefix("WECHAT").separator("__"))
            .build()?;

        let app_config: AppConfig = settings.try_deserialize()?;
        Ok(app_config)
    }

    pub fn load_default() -> Result<Self, Box<dyn std::error::Error>> {
        let config_files = ["config.toml", "config.yaml", "config.yml", "config.json"];

        for config_file in &config_files {
            if Path::new(config_file).exists() {
                return Self::from_file(config_file);
            }
        }

        Err("No configuration file found. Expected: config.toml/yaml/yml/json".into())
    }
}

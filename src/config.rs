//! 配置文件（TOML）：mode 和 model-router 节点（README: Model Router）
//! 文件不存在时全部用默认值，即 mode = "none"，行为和没有配置文件时完全一样。

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_CONFIG: &str = "vg-mirror.toml";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    // 不启用任何功能，model 原样透传
    #[default]
    None,
    // 先调分类服务判断难度，再路由到 small / medium / frontier
    ModelRouter,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default, rename = "model-router", alias = "model_router")]
    pub model_router: Option<RouterConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterConfig {
    // 三档模型的 model-id
    pub small: String,
    pub medium: String,
    pub frontier: String,

    // true：把对话上下文一起发给分类服务；false：只发最后一条 user 消息
    #[serde(default = "yes")]
    pub include_context: bool,
    // 发给分类服务的文本上限（字符数，超出时保留尾部）。分类服务上限约 32k~48k tokens
    #[serde(default = "default_max_state_chars")]
    pub max_state_chars: usize,
    // difficulty_level == cheap 且 prefer_cheap_model.noul 大于该值才走 small
    #[serde(default = "default_cheap_threshold")]
    pub cheap_threshold: f64,

    #[serde(default = "default_classifier_url")]
    pub classifier_url: String,
    #[serde(default = "default_classifier_model")]
    pub classifier_model: String,
    // 分类服务超时，超时或失败时走 frontier
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    // 训练数据日志（JSONL），每个请求一行；不填则不记录
    #[serde(default = "default_log_path")]
    pub log_path: Option<PathBuf>,
}

fn yes() -> bool {
    true
}
fn default_max_state_chars() -> usize {
    24_000
}
fn default_cheap_threshold() -> f64 {
    0.75
}
fn default_classifier_url() -> String {
    "https://api.typesafe.ai/v1/systemone".into()
}
fn default_classifier_model() -> String {
    "jev-latest".into()
}
fn default_timeout_ms() -> u64 {
    5_000
}
fn default_log_path() -> Option<PathBuf> {
    Some("vg-mirror-router.jsonl".into())
}

impl Config {
    // 显式指定的路径必须存在；默认路径（./vg-mirror.toml）不存在时用默认配置
    pub fn load(path: Option<&Path>) -> Result<(Self, Option<PathBuf>), String> {
        let (path, explicit) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => (PathBuf::from(DEFAULT_CONFIG), false),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => return Ok((Self::default(), None)),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        let cfg: Self = toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        if cfg.mode == Mode::ModelRouter && cfg.model_router.is_none() {
            return Err(format!("{}: mode = \"model-router\" requires a [model-router] section", path.display()));
        }
        Ok((cfg, Some(path)))
    }
}

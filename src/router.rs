//! Model Router（mode = "model-router"）：
//! 先把请求交给分类服务（typesafe systemone）判断难度，再选 small / medium / frontier 模型。
//! 一轮对话只分类一次：最后一条消息是 user 时算新的一轮，之后工具调用的后续请求沿用这一轮的结果。
//! （README: Model Router）

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::config::RouterConfig;

// 会话缓存最多保留多少个会话的决策
const MAX_SESSIONS: usize = 1024;
// 上下文里单条工具输出 / 工具参数最多保留的字符数
const MAX_TOOL_OUTPUT_CHARS: usize = 1_000;
const MAX_TOOL_ARGS_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Small,
    Medium,
    Frontier,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Small => "small",
            Tier::Medium => "medium",
            Tier::Frontier => "frontier",
        }
    }
}

// 分类服务的结果
#[derive(Debug, Clone)]
pub struct Verdict {
    // 实际发给分类服务的文本
    pub state: String,
    // 分类服务返回的 answers 原样保存（写进训练数据日志）
    pub answers: Value,
    // 分类服务实际用的模型版本，如 jev-1.13.0
    pub classifier_model: String,
    pub choice: String,
    pub confidence: Option<f64>,
    pub score: Option<f64>,
    pub noul: Option<f64>,
    pub latency: Duration,
}

// 这个请求的路由是怎么来的
#[derive(Debug, Clone)]
pub enum Source {
    // 本次请求调了分类服务
    Classified(Verdict),
    // 同一轮里的后续请求，沿用 #req_id 的决策
    Cached { from_req: u64 },
    // 分类服务失败 / 超时，退回 frontier
    Fallback(String),
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub tier: Tier,
    pub model: String,
    pub session: Option<String>,
    pub new_turn: bool,
    pub source: Source,
}

impl Decision {
    // 请求日志里的一行：route ▸ small → gpt-5.6-luna  (classified 0.41s: cheap 0.93, score 0.30, noul 0.91)
    pub fn summary(&self) -> String {
        let how = match &self.source {
            Source::Classified(v) => format!(
                "classified {:.2}s: {} {}, score {}, noul {}",
                v.latency.as_secs_f64(),
                v.choice,
                fmt_opt(v.confidence),
                fmt_opt(v.score),
                fmt_opt(v.noul),
            ),
            Source::Cached { from_req } => format!("same turn as #{from_req}"),
            Source::Fallback(e) => format!("⚠ classifier failed, fallback: {e}"),
        };
        format!("    route  ▸ {} → {}  ({how})", self.tier.as_str(), self.model)
    }
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map_or("n/a".into(), |v| format!("{v:.2}"))
}

struct Cached {
    req_id: u64,
    tier: Tier,
}

pub struct Router {
    cfg: RouterConfig,
    client: reqwest::Client,
    api_key: String,
    // 会话 → 这一轮的决策；order 用来淘汰最早的会话
    sessions: Mutex<(HashMap<String, Cached>, VecDeque<String>)>,
}

impl Router {
    pub fn new(cfg: RouterConfig, client: reqwest::Client, api_key: String) -> Self {
        Self {
            cfg,
            client,
            api_key,
            sessions: Mutex::new((HashMap::new(), VecDeque::new())),
        }
    }

    fn model_for(&self, tier: Tier) -> &str {
        match tier {
            Tier::Small => &self.cfg.small,
            Tier::Medium => &self.cfg.medium,
            Tier::Frontier => &self.cfg.frontier,
        }
    }

    // 决定这个请求用哪个模型。req 是 codex 发来的 Responses 请求，messages 是转换后的 Chat messages
    pub async fn route(&self, req_id: u64, req: &Value, headers: &HeaderMap, messages: &[Value]) -> Decision {
        let session = session_key(req, headers);
        let new_turn = messages.last().and_then(|m| m.get("role")).and_then(Value::as_str) == Some("user");

        // 同一轮的后续请求：沿用缓存（代理重启后缓存为空，就重新分类）
        if !new_turn
            && let Some(s) = &session
            && let Some(c) = self.sessions.lock().unwrap().0.get(s)
        {
            return Decision {
                tier: c.tier,
                model: self.model_for(c.tier).to_string(),
                session,
                new_turn,
                source: Source::Cached { from_req: c.req_id },
            };
        }

        let state = build_state(messages, self.cfg.include_context, self.cfg.max_state_chars);
        let (tier, source) = match self.classify(&state).await {
            Ok(v) => (self.decide(&v), Source::Classified(v)),
            Err(e) => (Tier::Frontier, Source::Fallback(e)),
        };

        // 分类失败不缓存，下一个请求再试
        if let Some(s) = &session
            && !matches!(source, Source::Fallback(_))
        {
            let mut guard = self.sessions.lock().unwrap();
            let (map, order) = &mut *guard;
            if map.insert(s.clone(), Cached { req_id, tier }).is_none() {
                order.push_back(s.clone());
                while order.len() > MAX_SESSIONS {
                    if let Some(old) = order.pop_front() {
                        map.remove(&old);
                    }
                }
            }
        }

        Decision {
            tier,
            model: self.model_for(tier).to_string(),
            session,
            new_turn,
            source,
        }
    }

    // 路由规则（router_design.md）：
    //   cheap 且 noul > 阈值 → small；cheap 但 noul 不够高、或 medium → medium；其他 → frontier
    fn decide(&self, v: &Verdict) -> Tier {
        match v.choice.as_str() {
            "cheap" if v.noul.is_some_and(|n| n > self.cfg.cheap_threshold) => Tier::Small,
            "cheap" | "medium" => Tier::Medium,
            _ => Tier::Frontier,
        }
    }

    // 调分类服务；文本超出分类服务的 token 上限（400 max_tokens_exceeded）时，只保留后一半重试一次
    async fn classify(&self, state: &str) -> Result<Verdict, String> {
        match self.classify_once(state).await {
            Err(e) if e.contains("max_tokens_exceeded") => {
                let half = tail_chars(state, state.chars().count() / 2);
                self.classify_once(&half).await
            }
            r => r,
        }
    }

    async fn classify_once(&self, state: &str) -> Result<Verdict, String> {
        let started = Instant::now();
        let body = json!({
            "model": self.cfg.classifier_model,
            "state": state,
            "questions": questions(),
        });
        let resp = self
            .client
            .post(&self.cfg.classifier_url)
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_millis(self.cfg.timeout_ms))
            .json(&body)
            .send()
            .await
            .map_err(|e| if e.is_timeout() { format!("timeout after {}ms", self.cfg.timeout_ms) } else { e.to_string() })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", truncate(&text, 300)));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("invalid JSON ({e}): {}", truncate(&text, 300)))?;
        let answers = v.get("answers").cloned().unwrap_or(Value::Null);
        let choice = answers
            .pointer("/difficulty_level/choice")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("no difficulty_level.choice in response: {}", truncate(&text, 300)))?
            .to_string();
        Ok(Verdict {
            state: state.to_string(),
            classifier_model: v.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
            choice,
            confidence: answers.pointer("/difficulty_level/confidence").and_then(Value::as_f64),
            score: answers.pointer("/difficulty_score/score").and_then(Value::as_f64),
            noul: answers.pointer("/prefer_cheap_model/noul").and_then(Value::as_f64),
            answers,
            latency: started.elapsed(),
        })
    }
}

// 分类服务的问题定义（router_design.md 第 3 步）
pub fn questions() -> Value {
    json!({
        "difficulty_level": {
            "type": "choice",
            "instructions": "Classify the overall difficulty of this AI request for model routing",
            "criteria": {
                "cheap": "Simple factual questions, short summaries, basic rewriting, or easy classification. Can be handled by a small/cheap model.",
                "medium": "Moderate reasoning, code explanation, medium-length context, or multi-step but not highly complex tasks.",
                "expensive": "Complex reasoning, advanced code optimization, debugging concurrency issues, long context, or tasks requiring strong intelligence."
            }
        },
        "difficulty_score": {
            "type": "score",
            "instructions": "Rate the difficulty from 0 (easiest) to 4 (hardest)",
            "criteria": [
                "Very easy - simple facts or short tasks",
                "Easy - light reasoning or short code changes",
                "Medium - solid reasoning or multi-step work",
                "Hard - complex analysis, optimization, or debugging",
                "Very hard - deep reasoning, concurrency, or advanced engineering"
            ]
        },
        "prefer_cheap_model": {
            "type": "noul",
            "instructions": "Is it safe and high-quality enough to route this request to a cheap/small model?"
        }
    })
}

// 会话标识：codex 的 prompt_cache_key（即 conversation id），没有就看 session_id / conversation_id header
fn session_key(req: &Value, headers: &HeaderMap) -> Option<String> {
    if let Some(k) = req.get("prompt_cache_key").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return Some(k.to_string());
    }
    ["session_id", "conversation_id"]
        .iter()
        .find_map(|h| headers.get(*h)?.to_str().ok().filter(|s| !s.is_empty()).map(str::to_string))
}

// codex 自动插入的 user 消息（环境信息、AGENTS.md 等），不算用户的真实请求
fn is_injected(text: &str) -> bool {
    let t = text.trim_start();
    ["<environment_context>", "<user_instructions>", "# AGENTS.md instructions", "<INSTRUCTIONS>"]
        .iter()
        .any(|p| t.starts_with(p))
}

fn content_text(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image_url") => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// 发给分类服务的文本。
// include_context = false：只有最后一条真实的 user 消息；
// include_context = true：再加上前面的对话（system 消息不带，工具输出截短），超长时从最早的开始丢。
pub fn build_state(messages: &[Value], include_context: bool, max_chars: usize) -> String {
    let role = |m: &Value| m.get("role").and_then(Value::as_str).unwrap_or("").to_string();
    let current = messages
        .iter()
        .rposition(|m| role(m) == "user" && !is_injected(&content_text(m)));
    let current_text = current.map(|i| content_text(&messages[i])).unwrap_or_default();
    if !include_context {
        return tail_chars(&current_text, max_chars);
    }

    let mut lines = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if Some(i) == current {
            continue;
        }
        let text = content_text(m);
        match role(m).as_str() {
            "user" if !is_injected(&text) => lines.push(format!("user: {text}")),
            "assistant" => {
                if !text.is_empty() {
                    lines.push(format!("assistant: {text}"));
                }
                for tc in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let name = tc.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
                    let args = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("");
                    lines.push(format!("assistant tool call: {name}({})", truncate(args, MAX_TOOL_ARGS_CHARS)));
                }
            }
            "tool" => lines.push(format!("tool output: {}", truncate(&text, MAX_TOOL_OUTPUT_CHARS))),
            _ => {}
        }
    }

    let current_block = format!("[Current request]\n{current_text}");
    if lines.is_empty() {
        return tail_chars(&current_block, max_chars);
    }
    let header = "[Conversation context]\n";
    let budget = max_chars.saturating_sub(current_block.chars().count() + header.len() + 2);
    if budget == 0 {
        return tail_chars(&current_block, max_chars);
    }
    let context = tail_chars(&lines.join("\n"), budget);
    format!("{header}{context}\n\n{current_block}")
}

// 保留最后 n 个字符
fn tail_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    s.chars().skip(total - n).collect()
}

// 保留前 n 个字符，超出的部分用 … 表示
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs() -> Vec<Value> {
        vec![
            json!({"role": "system", "content": "you are codex"}),
            json!({"role": "user", "content": "<environment_context>cwd=/x</environment_context>"}),
            json!({"role": "user", "content": "fix the bug"}),
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{\"command\":[\"ls\"]}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "a.rs\nb.rs"}),
            json!({"role": "user", "content": "now make it thread-safe"}),
        ]
    }

    #[test]
    fn state_without_context_is_last_user_message() {
        assert_eq!(build_state(&msgs(), false, 1000), "now make it thread-safe");
    }

    #[test]
    fn state_with_context_skips_system_and_injected() {
        let s = build_state(&msgs(), true, 1000);
        assert!(!s.contains("you are codex"));
        assert!(!s.contains("environment_context"));
        assert!(s.contains("user: fix the bug"));
        assert!(s.contains("assistant tool call: shell("));
        assert!(s.contains("tool output: a.rs"));
        assert!(s.ends_with("[Current request]\nnow make it thread-safe"));
    }

    #[test]
    fn state_is_truncated_from_the_front() {
        let s = build_state(&msgs(), true, 60);
        assert!(s.chars().count() <= 60);
        assert!(s.ends_with("now make it thread-safe"));
    }

    #[test]
    fn decide_follows_design_rules() {
        let cfg: RouterConfig = toml::from_str("small='s'\nmedium='m'\nfrontier='f'").unwrap();
        let r = Router::new(cfg, reqwest::Client::new(), String::new());
        let v = |choice: &str, noul: f64| Verdict {
            state: String::new(),
            answers: Value::Null,
            classifier_model: String::new(),
            choice: choice.into(),
            confidence: None,
            score: None,
            noul: Some(noul),
            latency: Duration::ZERO,
        };
        assert_eq!(r.decide(&v("cheap", 0.9)), Tier::Small);
        assert_eq!(r.decide(&v("cheap", 0.75)), Tier::Medium);
        assert_eq!(r.decide(&v("medium", 0.1)), Tier::Medium);
        assert_eq!(r.decide(&v("expensive", 0.9)), Tier::Frontier);
    }
}

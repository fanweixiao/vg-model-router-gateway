//! Model Router（mode = "model-router"）：
//! 先把请求交给分类服务（typesafe systemone）判断难度，再选 small / medium / frontier 模型。
//! 一轮对话只分类一次：input 最后一个 item 是 user 消息时算新的一轮，之后工具调用的后续请求沿用这一轮的结果。
//! （README: Model Router）

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, header};
use tracing::{debug, info, warn};
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
    // decide() 选这一档的依据，只有真的分类过才有（缓存和降级各有自己的说法）
    pub rule: Option<String>,
    pub session: Option<String>,
    pub new_turn: bool,
    pub source: Source,
}

impl Decision {
    // 请求日志里的一行：route ▸ small → gpt-5.6-luna  (classified by jev-1.13.0 in 0.41s; cheap, noul 0.91 > 0.75)
    pub fn summary(&self) -> String {
        let how = match &self.source {
            Source::Classified(v) => format!(
                "classified by {} in {:.2}s",
                if v.classifier_model.is_empty() { "classifier" } else { &v.classifier_model },
                v.latency.as_secs_f64(),
            ),
            Source::Cached { from_req } => format!("same turn as #{from_req}"),
            Source::Fallback(e) => format!("⚠ classifier failed, fallback: {e}"),
        };
        let why = self.rule.as_deref().map_or(String::new(), |r| format!("; {r}"));
        format!("    route  ▸ {} → {}  ({how}{why})", self.tier.as_str(), self.model)
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
    // 会话 → 这一轮的决策；order 用来淘汰最早的会话
    sessions: Mutex<(HashMap<String, Cached>, VecDeque<String>)>,
}

impl Router {
    pub fn new(cfg: RouterConfig, client: reqwest::Client) -> Self {
        Self {
            cfg,
            client,
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

    // 决定这个请求用哪个模型。req 是 codex 发来的 Responses 请求，
    // auth 是这次请求发给上游 /v1/responses 的 Authorization，分类服务用同一个
    pub async fn route(&self, req_id: u64, req: &Value, headers: &HeaderMap, auth: &HeaderValue) -> Decision {
        let session = session_key(req, headers);
        let input = input_items(req);
        let new_turn = input.last().is_some_and(|it| role(it) == "user");

        // 同一轮的后续请求：沿用缓存（代理重启后缓存为空，就重新分类）
        if !new_turn
            && let Some(s) = &session
            && let Some(c) = self.sessions.lock().unwrap().0.get(s)
        {
            return Decision {
                tier: c.tier,
                model: self.model_for(c.tier).to_string(),
                rule: None,
                session,
                new_turn,
                source: Source::Cached { from_req: c.req_id },
            };
        }

        let state = build_state(&input, self.cfg.include_context, self.cfg.max_state_chars);
        let (tier, rule, source) = match self.classify(req_id, &state, auth).await {
            Ok(v) => {
                let (tier, rule) = self.decide(&v);
                (tier, Some(rule), Source::Classified(v))
            }
            Err(e) => (Tier::Frontier, None, Source::Fallback(e)),
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
            rule,
            session,
            new_turn,
            source,
        }
    }

    // 路由规则（router_design.md）：
    //   cheap 且 noul > 阈值 → small；cheap 但 noul 不够高、或 medium → medium；其他 → frontier
    // 第二个返回值是写进日志的依据：choice 和 tier 对不上时（cheap 却走了 medium）不用翻文档才能看懂
    fn decide(&self, v: &Verdict) -> (Tier, String) {
        let th = self.cfg.cheap_threshold;
        match v.choice.as_str() {
            "cheap" if v.noul.is_some_and(|n| n > th) => (Tier::Small, format!("cheap, noul {} > {th}", fmt_opt(v.noul))),
            "cheap" => (Tier::Medium, format!("cheap but noul {} ≤ {th}", fmt_opt(v.noul))),
            "medium" => (Tier::Medium, "medium".into()),
            "expensive" => (Tier::Frontier, "expensive".into()),
            other => (Tier::Frontier, format!("unknown choice {other:?}")),
        }
    }

    // 调分类服务；文本超出分类服务的 token 上限（400 max_tokens_exceeded）时，只保留后一半重试一次。
    // 重试时第一次的失败也会打出来，不会被这里的 match 吞掉
    async fn classify(&self, req_id: u64, state: &str, auth: &HeaderValue) -> Result<Verdict, String> {
        match self.classify_once(req_id, 1, state, auth).await {
            Err(e) if e.contains("max_tokens_exceeded") => {
                let half = tail_chars(state, state.chars().count() / 2);
                self.classify_once(req_id, 2, &half, auth).await
            }
            r => r,
        }
    }

    // 分类服务的每一次调用都在这里打印结果：成功是 INFO（模型版本、耗时、token、全部 answers），
    // 失败是 WARN（耗时 + HTTP 状态和响应体）。路由决策只依赖这一个外部服务，任何一次调用都不能不留痕迹。
    // 完整的原始响应体在 RUST_LOG=vg_model_router=debug 下打印。
    async fn classify_once(&self, req_id: u64, attempt: u32, state: &str, auth: &HeaderValue) -> Result<Verdict, String> {
        let started = Instant::now();
        let out = self.classify_call(req_id, state, auth).await;
        let retry = if attempt > 1 { format!(", attempt {attempt}") } else { String::new() };
        match &out {
            Ok((v, usage)) => info!(
                "#{req_id} ⇢ {}  [{:.2}s, {usage}{retry}]   answer ▸ {}",
                if v.classifier_model.is_empty() { &self.cfg.classifier_model } else { &v.classifier_model },
                v.latency.as_secs_f64(),
                answers_digest(v),
            ),
            Err(e) => warn!(
                "#{req_id} ⇢ {}  [{:.2}s{retry}]   answer ▸ ✖ {e}",
                self.cfg.classifier_model,
                started.elapsed().as_secs_f64(),
            ),
        }
        out.map(|(v, _)| v)
    }

    // 真正发请求的部分。返回 Verdict 和 usage 的单行摘要，打日志交给 classify_once
    async fn classify_call(&self, req_id: u64, state: &str, auth: &HeaderValue) -> Result<(Verdict, String), String> {
        let started = Instant::now();
        let body = json!({
            "model": self.cfg.classifier_model,
            "state": state,
            "questions": questions(),
        });
        let resp = self
            .client
            .post(&self.cfg.classifier_url)
            .header(header::AUTHORIZATION, auth.clone())
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
        debug!("#{req_id} classifier response body: {text}");
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("invalid JSON ({e}): {}", truncate(&text, 300)))?;
        let answers = v.get("answers").cloned().unwrap_or(Value::Null);
        let choice = answers
            .pointer("/difficulty_level/choice")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("no difficulty_level.choice in response: {}", truncate(&text, 300)))?
            .to_string();
        let usage = match v.get("usage") {
            Some(u) => format!(
                "in {} / out {}",
                u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
            ),
            None => "usage n/a".to_string(),
        };
        let verdict = Verdict {
            state: state.to_string(),
            classifier_model: v.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
            choice,
            confidence: answers.pointer("/difficulty_level/confidence").and_then(Value::as_f64),
            score: answers.pointer("/difficulty_score/score").and_then(Value::as_f64),
            noul: answers.pointer("/prefer_cheap_model/noul").and_then(Value::as_f64),
            answers,
            latency: started.elapsed(),
        };
        Ok((verdict, usage))
    }
}

// 分类服务响应的摘要，跟在 ⇢ 行后面：各档概率 + score + noul（legend 太长，只在 debug 的原始响应里看）。
// 不重复 choice——它在下面 route 行的依据里（“cheap but noul …”），这里省下的宽度留给概率
fn answers_digest(v: &Verdict) -> String {
    let probs = v
        .answers
        .pointer("/difficulty_level/probabilities")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, p)| format!("{} {:.2}", abbrev(k), p.as_f64().unwrap_or(0.0)))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "-".into());
    format!(
        "(conf {} | {probs}), scr {} (conf {}), noul {}",
        fmt_opt(v.confidence),
        fmt_opt(v.score),
        fmt_opt(v.answers.pointer("/difficulty_score/confidence").and_then(Value::as_f64)),
        fmt_opt(v.noul),
    )
}

// 档位名缩写，只为了这一行放得下；认不出的原样打，不猜
fn abbrev(choice: &str) -> &str {
    match choice {
        "cheap" => "chp",
        "medium" => "med",
        "expensive" => "exp",
        other => other,
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

// 这一轮真正的用户提问：input 里最后一条 user message，跳过 codex 注入的那些。
// build_state 挑的是同一条（include_context = false 时 state 就是它），所以日志里
// 打出来的就是分类服务看到的问题本身
pub fn last_user_text(req: &Value) -> Option<String> {
    let input = input_items(req);
    let i = input.iter().rposition(|it| role(it) == "user" && !is_injected(&content_text(it)))?;
    Some(content_text(&input[i]))
}

// codex 自动插入的 user 消息（环境信息、AGENTS.md 等），不算用户的真实请求
fn is_injected(text: &str) -> bool {
    let t = text.trim_start();
    ["<environment_context>", "<user_instructions>", "# AGENTS.md instructions", "<INSTRUCTIONS>"]
        .iter()
        .any(|p| t.starts_with(p))
}

// input 可以是字符串（等于一条 user 消息）或 item 数组
fn input_items(req: &Value) -> Vec<Value> {
    match req.get("input") {
        Some(Value::String(s)) => vec![json!({ "type": "message", "role": "user", "content": s })],
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

// message item 的 role；其他类型的 item 返回 ""
fn role(it: &Value) -> &str {
    let is_message = it.get("type").and_then(Value::as_str).is_none_or(|t| t == "message");
    if !is_message {
        return "";
    }
    it.get("role").and_then(Value::as_str).unwrap_or("")
}

// message 的文本：input_text / output_text 拼起来，图片和文件用占位符
fn content_text(it: &Value) -> String {
    match it.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("refusal") => p.get("refusal").and_then(Value::as_str).map(str::to_string),
                Some("input_image") => Some("[image]".into()),
                Some("input_file") => Some("[file]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// 工具输出：字符串，或内容片段数组
fn output_text(it: &Value) -> String {
    match it.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

// 发给分类服务的文本。
// include_context = false：只有最后一条真实的 user 消息；
// include_context = true：再加上前面的对话（system / developer 消息和 reasoning 不带，工具输出截短），
// 超长时从最早的开始丢。
pub fn build_state(input: &[Value], include_context: bool, max_chars: usize) -> String {
    let current = input.iter().rposition(|it| role(it) == "user" && !is_injected(&content_text(it)));
    let current_text = current.map(|i| content_text(&input[i])).unwrap_or_default();
    if !include_context {
        return tail_chars(&current_text, max_chars);
    }

    let field = |it: &Value, k: &str| it.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut lines = Vec::new();
    for (i, it) in input.iter().enumerate() {
        if Some(i) == current {
            continue;
        }
        match (role(it), it.get("type").and_then(Value::as_str).unwrap_or("")) {
            ("user", _) => {
                let text = content_text(it);
                if !is_injected(&text) {
                    lines.push(format!("user: {text}"));
                }
            }
            ("assistant", _) => {
                let text = content_text(it);
                if !text.is_empty() {
                    lines.push(format!("assistant: {text}"));
                }
            }
            (_, "function_call") => lines.push(format!(
                "assistant tool call: {}({})",
                field(it, "name"),
                truncate(&field(it, "arguments"), MAX_TOOL_ARGS_CHARS)
            )),
            (_, "custom_tool_call") => lines.push(format!(
                "assistant tool call: {}({})",
                field(it, "name"),
                truncate(&field(it, "input"), MAX_TOOL_ARGS_CHARS)
            )),
            (_, "local_shell_call") => {
                let cmd = it.pointer("/action/command").map(Value::to_string).unwrap_or_default();
                lines.push(format!("assistant tool call: local_shell({})", truncate(&cmd, MAX_TOOL_ARGS_CHARS)));
            }
            (_, "function_call_output" | "custom_tool_call_output" | "local_shell_call_output") => {
                lines.push(format!("tool output: {}", truncate(&output_text(it), MAX_TOOL_OUTPUT_CHARS)))
            }
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
        let text = |role: &str, ty: &str, t: &str| json!({"type": "message", "role": role, "content": [{"type": ty, "text": t}]});
        vec![
            text("developer", "input_text", "you are codex"),
            text("user", "input_text", "<environment_context>cwd=/x</environment_context>"),
            text("user", "input_text", "fix the bug"),
            json!({"type": "reasoning", "summary": [], "encrypted_content": "xxx"}),
            json!({"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"command\":[\"ls\"]}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "a.rs\nb.rs"}),
            text("assistant", "output_text", "found it"),
            text("user", "input_text", "now make it thread-safe"),
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
        assert!(s.contains("assistant: found it"));
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
        let r = Router::new(cfg, reqwest::Client::new());
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
        assert_eq!(r.decide(&v("cheap", 0.9)), (Tier::Small, "cheap, noul 0.90 > 0.75".into()));
        assert_eq!(r.decide(&v("cheap", 0.75)), (Tier::Medium, "cheap but noul 0.75 ≤ 0.75".into()));
        assert_eq!(r.decide(&v("medium", 0.1)), (Tier::Medium, "medium".into()));
        assert_eq!(r.decide(&v("expensive", 0.9)), (Tier::Frontier, "expensive".into()));
        assert_eq!(r.decide(&v("weird", 0.9)).0, Tier::Frontier);
    }
}

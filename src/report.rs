//! 日志格式化：响应的 stop 值 + usage，以及请求里的 tools 信息
//! （README: Reading the logs）

use std::fmt::Write;
use std::time::Duration;

use serde_json::Value;
use tracing::{info, warn};

// 一次响应要打印的内容（stream / non-stream 共用）
pub struct Report<'a> {
    pub req_id: u64,
    pub stream: bool,
    pub elapsed: Duration,
    pub model: &'a str,
    // 上游原始的 finish_reason（None = 从没收到过）——排查异常的核心字段
    pub finish_reason: Option<&'a str>,
    // 上游在 choice 上额外带的 stop 相关字段（stop_reason、native_finish_reason…）
    pub extra_stop: &'a [(String, Value)],
    // 实际发给 codex 的状态（completed / incomplete / failed）
    pub status: &'a str,
    pub usage: Option<Usage>,
    // 异常事件（没收到 [DONE]、客户端断开等），有 note 时整条日志升级为 WARN
    pub notes: &'a [String],
}

pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cached: Option<u64>,
    pub reasoning: Option<u64>,
}

impl Usage {
    // 从 Chat Completions 的 usage 里取出要打印的 4 个数
    pub fn from_chat(u: Option<&Value>) -> Option<Self> {
        let u = u.filter(|u| u.is_object())?;
        let n = |p: &str| u.pointer(p).and_then(Value::as_u64);
        Some(Self {
            input: n("/prompt_tokens").unwrap_or(0),
            output: n("/completion_tokens").unwrap_or(0),
            cached: n("/prompt_tokens_details/cached_tokens"),
            reasoning: n("/completion_tokens_details/reasoning_tokens"),
        })
    }
}

// 打印响应日志（README: Reading the logs → Response line）：
//   #3 ◀ response  [stream, 8.42s, model=..]
//       stop   ▸ finish_reason = "tool_calls"  →  status = completed
//       usage  ▸ inp: 8,029, cd-inp: 6,144 (76.5%), opt: 212 (reasoning: 64)
// finish_reason 不是 stop / tool_calls，或有 note 时，用 WARN 级别
pub fn log(r: &Report) {
    let mut s = String::new();
    let mode = if r.stream { "stream" } else { "non-stream" };
    let _ = writeln!(
        s,
        "#{} ◀ response  [{mode}, {:.2}s, model={}]",
        r.req_id,
        r.elapsed.as_secs_f64(),
        r.model
    );

    // --- stop ---
    let fr = match r.finish_reason {
        Some(v) => format!("{v:?}"),
        None => "<MISSING> (upstream never sent finish_reason)".to_string(),
    };
    let _ = write!(s, "    stop   ▸ finish_reason = {fr}  →  status = {}", r.status);
    for (k, v) in r.extra_stop {
        let _ = write!(s, "  | {k} = {v}");
    }
    let normal_stop = matches!(r.finish_reason, Some("stop" | "tool_calls"));
    s.push('\n');

    // --- usage：一行显示，不打印 raw ---
    match &r.usage {
        None => s.push_str("    usage  ▸ <none> (upstream returned no usage)"),
        Some(u) => {
            let cached = match u.cached {
                Some(c) => {
                    let pct = if u.input > 0 { c as f64 * 100.0 / u.input as f64 } else { 0.0 };
                    format!("{} ({pct:.1}%)", fmt_num(c))
                }
                None => "n/a".into(),
            };
            let reasoning = u.reasoning.map_or("n/a".into(), fmt_num);
            let _ = write!(
                s,
                "    usage  ▸ inp: {}, cd-inp: {cached}, opt: {} (reasoning: {reasoning})",
                fmt_num(u.input),
                fmt_num(u.output)
            );
        }
    }

    for note in r.notes {
        let _ = write!(s, "\n    note   ▸ {note}");
    }

    if normal_stop && r.notes.is_empty() {
        info!("{s}");
    } else {
        warn!("{s}");
    }
}

// 数字加千分位：8029 → 8,029
pub fn fmt_num(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// 请求里和 tools 相关的信息（README: Reading the logs → Request line）：
// - declared：声明的工具及参数名、转换方式
// - dropped：发不了给上游的工具
// - parallel_tool_calls（按要求不打印 tool_choice）
// - in input：历史里的工具调用 / 输出数量，调用和输出对不上时标 ⚠
pub fn tools_summary(req: &Value, dropped: &[String]) -> String {
    let mut s = String::new();

    let tools = req.get("tools").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let declared: Vec<String> = tools
        .iter()
        .filter_map(|t| {
            let ty = t.get("type").and_then(Value::as_str).unwrap_or("?");
            let name = t.get("name").and_then(Value::as_str).unwrap_or("?");
            match ty {
                "function" => {
                    let params: Vec<&str> = t
                        .pointer("/parameters/properties")
                        .and_then(Value::as_object)
                        .map(|p| p.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    Some(format!("{name}({}) [function]", params.join(", ")))
                }
                "custom" => Some(format!("{name}(input) [custom→function]")),
                _ => None,
            }
        })
        .collect();
    let _ = write!(s, "    tools  ▸ declared ({}): ", declared.len());
    s.push_str(if declared.is_empty() { "-" } else { "" });
    s.push_str(&declared.join(", "));
    if !dropped.is_empty() {
        let _ = write!(s, "\n             dropped ({}): {}", dropped.len(), dropped.join(", "));
    }
    let parallel = req
        .get("parallel_tool_calls")
        .filter(|v| !v.is_null())
        .map_or("-".to_string(), Value::to_string);
    let _ = write!(s, "\n             parallel_tool_calls={parallel}");

    // 统计 input 历史里回放的工具调用和输出，按 call_id 配对
    let items = req.get("input").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut calls: Vec<(String, String)> = Vec::new(); // (call_id, name)
    let mut outputs: Vec<String> = Vec::new();
    for it in items {
        let call_id = it.get("call_id").and_then(Value::as_str).unwrap_or("").to_string();
        match it.get("type").and_then(Value::as_str).unwrap_or("") {
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                let name = it.get("name").and_then(Value::as_str).unwrap_or("local_shell");
                calls.push((call_id, name.to_string()));
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => outputs.push(call_id),
            _ => {}
        }
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (_, name) in &calls {
        match counts.iter_mut().find(|(n, _)| n == name) {
            Some(c) => c.1 += 1,
            None => counts.push((name.clone(), 1)),
        }
    }
    let by_name: Vec<String> = counts.iter().map(|(n, c)| format!("{n} ×{c}")).collect();
    let _ = write!(s, "\n             in input: {} calls", calls.len());
    if !by_name.is_empty() {
        let _ = write!(s, " ({})", by_name.join(", "));
    }
    let _ = write!(s, ", {} outputs", outputs.len());

    let no_output: Vec<&str> = calls
        .iter()
        .filter(|(id, _)| !outputs.contains(id))
        .map(|(id, _)| id.as_str())
        .collect();
    let no_call: Vec<&str> = outputs
        .iter()
        .filter(|id| !calls.iter().any(|(c, _)| c == *id))
        .map(String::as_str)
        .collect();
    if !no_output.is_empty() {
        let _ = write!(s, "\n             ⚠ calls without output: {}", no_output.join(", "));
    }
    if !no_call.is_empty() {
        let _ = write!(s, "\n             ⚠ outputs without call: {}", no_call.join(", "));
    }
    s
}

// 排查用：按类型统计 input 里的每个 item（message 再按 role 细分），
// 标出 convert.rs 不认识、被静默丢掉的类型，确认 history 里有没有其他形式的工具调用
pub fn input_items_summary(req: &Value) -> String {
    const CONVERTED: &[&str] = &[
        "message",
        "function_call",
        "custom_tool_call",
        "function_call_output",
        "custom_tool_call_output",
    ];
    let items = req.get("input").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut counts: Vec<(String, usize)> = Vec::new();
    for it in items {
        let ty = it
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(if it.get("role").is_some() { "message" } else { "<no type>" });
        let key = match (ty, it.get("role").and_then(Value::as_str)) {
            ("message", Some(role)) => format!("message:{role}"),
            _ => ty.to_string(),
        };
        match counts.iter_mut().find(|(k, _)| *k == key) {
            Some(c) => c.1 += 1,
            None => counts.push((key, 1)),
        }
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(k, n)| {
            let base = k.split(':').next().unwrap_or(k);
            let tag = if base == "reasoning" {
                " [dropped]"
            } else if CONVERTED.contains(&base) {
                ""
            } else {
                " [⚠ skipped: unknown type]"
            };
            format!("{k} ×{n}{tag}")
        })
        .collect();
    format!("    items  ▸ {}", if parts.is_empty() { "-".into() } else { parts.join(", ") })
}

// 排查用：检查实际发给上游的 chat body 里还有没有任何工具痕迹
// （tools / tool_choice / functions 字段，带 tool_calls 的 assistant 消息，role=tool/function 的消息）
pub fn upstream_tool_traces(body: &Value) -> String {
    let mut found = Vec::new();
    for key in ["tools", "tool_choice", "parallel_tool_calls", "functions", "function_call"] {
        if body.get(key).is_some() {
            found.push(format!("field `{key}`"));
        }
    }
    let msgs = body.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let with_calls = msgs.iter().filter(|m| m.get("tool_calls").is_some()).count();
    let tool_msgs = msgs
        .iter()
        .filter(|m| matches!(m.get("role").and_then(Value::as_str), Some("tool" | "function")))
        .count();
    if with_calls > 0 {
        found.push(format!("{with_calls} assistant messages with tool_calls"));
    }
    if tool_msgs > 0 {
        found.push(format!("{tool_msgs} tool-role messages"));
    }
    format!(
        "    sent   ▸ tool traces in upstream body: {}",
        if found.is_empty() { "none".into() } else { found.join(", ") }
    )
}

// 有些后端在 choice 上额外带的 stop 相关字段，一起打印在 stop 行上
pub fn extra_stop_fields(choice: &Value) -> Vec<(String, Value)> {
    ["stop_reason", "native_finish_reason", "matched_stop"]
        .iter()
        .filter_map(|k| {
            choice
                .get(*k)
                .filter(|v| !v.is_null())
                .map(|v| (k.to_string(), v.clone()))
        })
        .collect()
}

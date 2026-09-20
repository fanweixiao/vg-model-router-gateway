//! 日志格式化：响应的 status + usage，以及请求里的 tools 和 input 信息
//! （README: Reading the logs）

use std::fmt::Write;
use std::io::IsTerminal;
use std::time::Duration;

use serde_json::Value;
use tracing::{Event, Level, Subscriber, info, warn};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

// 8 种预设颜色（256 色码），按请求编号轮换。请求是并发的，一个请求的 ▶ / ⇢ / ◀ 之间
// 会插进别的请求的行，同一个 #N 同色就能一眼串起来
const REQ_COLORS: [u8; 8] = [39, 208, 141, 36, 205, 178, 45, 167];

// 日志格式：<时间> <级别> <消息>，和 tracing-subscriber 的默认格式一致，
// 只多做一件事——消息以 #<编号> 开头时整条（含续行）按编号上色
pub struct ReqColor {
    ansi: bool,
}

impl ReqColor {
    // 输出不是终端（重定向进文件或管道）或设了 NO_COLOR 时，不写任何转义序列
    pub fn new() -> Self {
        Self { ansi: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }
}

impl Default for ReqColor {
    fn default() -> Self {
        Self::new()
    }
}

impl<S, N> FormatEvent<S, N> for ReqColor
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> std::fmt::Result {
        SystemTime.format_time(&mut writer)?;
        let (level, color) = match *event.metadata().level() {
            Level::TRACE => ("TRACE", 35),
            Level::DEBUG => ("DEBUG", 34),
            Level::INFO => (" INFO", 32),
            Level::WARN => (" WARN", 33),
            Level::ERROR => ("ERROR", 31),
        };
        match self.ansi {
            true => write!(writer, " \x1b[{color}m{level}\x1b[0m ")?,
            false => write!(writer, " {level} ")?,
        }

        // 先把消息格式化出来，才能看到开头的 #<编号>
        let mut msg = String::new();
        ctx.format_fields(Writer::new(&mut msg), event)?;
        match self.ansi.then(|| req_color(&msg)).flatten() {
            // 颜色一直持续到 reset，多行消息的续行（instr ▸、route ▸…）也是同色
            Some(c) => writeln!(writer, "\x1b[38;5;{c}m{msg}\x1b[0m"),
            None => writeln!(writer, "{msg}"),
        }
    }
}

// 消息开头的 #<编号> 决定颜色；没有编号（启动信息等）不上色
fn req_color(msg: &str) -> Option<u8> {
    let digits: String = msg.strip_prefix('#')?.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u64 = digits.parse().ok()?;
    Some(REQ_COLORS[(n % REQ_COLORS.len() as u64) as usize])
}

// 一次响应要打印的内容（stream / non-stream 共用），都从上游的 response 对象里取
pub struct Report {
    pub req_id: u64,
    pub stream: bool,
    pub elapsed: Duration,
    // 上游返回的 model（没有就用请求里的），日志里同时显示返回给 codex 的带前缀版本
    pub model: String,
    // completed / incomplete / failed…（None = 上游没给最终的 response 对象）
    pub status: Option<String>,
    // incomplete_details.reason 或 error.message
    pub reason: Option<String>,
    pub output: String,
    pub usage: Option<Usage>,
    // 异常事件（流中断、客户端断开等），有 note 时整条日志升级为 WARN
    pub notes: Vec<String>,
}

impl Report {
    pub fn new(req_id: u64, stream: bool, elapsed: Duration, model: &str, resp: Option<&Value>, notes: Vec<String>) -> Self {
        let get = |p: &str| resp.and_then(|r| r.pointer(p)).and_then(Value::as_str).map(str::to_string);
        Self {
            req_id,
            stream,
            elapsed,
            model: get("/model").unwrap_or_else(|| model.to_string()),
            status: get("/status"),
            reason: get("/incomplete_details/reason").or_else(|| get("/error/message")),
            output: output_summary(resp.and_then(|r| r.get("output"))),
            usage: Usage::from_response(resp.and_then(|r| r.get("usage"))),
            notes,
        }
    }
}

pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cached: Option<u64>,
    pub reasoning: Option<u64>,
}

impl Usage {
    // 从 Responses 的 usage 里取出要打印的 4 个数
    pub fn from_response(u: Option<&Value>) -> Option<Self> {
        let u = u.filter(|u| u.is_object())?;
        let n = |p: &str| u.pointer(p).and_then(Value::as_u64);
        Some(Self {
            input: n("/input_tokens").unwrap_or(0),
            output: n("/output_tokens").unwrap_or(0),
            cached: n("/input_tokens_details/cached_tokens"),
            reasoning: n("/output_tokens_details/reasoning_tokens"),
        })
    }
}

// 输出 item 按类型计数：message ×1, function_call ×2
fn output_summary(output: Option<&Value>) -> String {
    let items = output.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut counts: Vec<(String, usize)> = Vec::new();
    for it in items {
        let ty = it.get("type").and_then(Value::as_str).unwrap_or("?");
        match counts.iter_mut().find(|(k, _)| k == ty) {
            Some(c) => c.1 += 1,
            None => counts.push((ty.to_string(), 1)),
        }
    }
    if counts.is_empty() {
        return "-".into();
    }
    counts.iter().map(|(k, n)| format!("{k} ×{n}")).collect::<Vec<_>>().join(", ")
}

// 打印响应日志（README: Reading the logs → Response line）。
// 正常结束（status = completed 且没有 note）只打一行：
//   #3 ◀ response  [stream, 8.42s, model=gpt-5 → viv-gpt-5]
// 出问题时升级为 WARN，并补上 stop / output / usage / note 明细：
//       stop   ▸ status = incomplete  | reason = max_output_tokens
//       output ▸ reasoning ×1, function_call ×1
//       usage  ▸ inp: 8,029, cd-inp: 6,144 (76.5%), opt: 212 (reasoning: 64)
pub fn log(r: &Report) {
    let mut s = String::new();
    let mode = if r.stream { "stream" } else { "non-stream" };
    let mut shown = serde_json::json!({ "model": r.model });
    crate::stream::add_model_prefix(&mut shown);
    let _ = write!(
        s,
        "#{} ◀ response  [{mode}, {:.2}s, model={} → {}]",
        r.req_id,
        r.elapsed.as_secs_f64(),
        r.model,
        shown["model"].as_str().unwrap_or("")
    );

    // 正常结束就到此为止，明细不打印（usage 仍然写进训练日志 JSONL）
    if r.status.as_deref() == Some("completed") && r.notes.is_empty() {
        info!("{s}");
        return;
    }

    // --- stop ---
    s.push('\n');
    let status = r.status.as_deref().unwrap_or("<MISSING> (no final response object)");
    let _ = write!(s, "    stop   ▸ status = {status}");
    if let Some(reason) = &r.reason {
        let _ = write!(s, "  | reason = {reason}");
    }
    let _ = write!(s, "\n    output ▸ {}\n", r.output);

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

    for note in &r.notes {
        let _ = write!(s, "\n    note   ▸ {note}");
    }

    warn!("{s}");
}

// 数字加千分位：8029 → 8,029
pub fn fmt_num(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// 这一轮用户问的问题（README: Reading the logs → Request line）。
// 只有这一条：不打 instructions（system prompt 每个请求都一样）、不打历史消息、不打工具输出。
// codex 会用同样的 model 和差不多的概要行发出用途完全不同的请求（主对话、compact、起标题…），
// 看这一行就知道这个请求到底在问什么，所以每个请求都打。
pub fn user_message_summary(req: &Value) -> String {
    const HEAD: usize = 200;
    let Some(text) = crate::router::last_user_text(req) else {
        // 这一轮没有用户提问：compact、起标题这类请求，或者 input 里只剩 codex 注入的消息
        return "    ask    ▸ <no user message>".into();
    };
    // 折叠所有空白，提问可能是多行的，日志里只占一行
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = flat.chars().count();
    let head: String = flat.chars().take(HEAD).collect();
    let more = if n > HEAD { "…" } else { "" };
    format!("    ask    ▸ ({} chars) {head}{more}", fmt_num(text.chars().count() as u64))
}

// 请求里和 tools 相关的信息（README: Reading the logs → Request line）：
// - declared：声明的工具及参数名、类型
// - parallel_tool_calls（按要求不打印 tool_choice）
// - in input：历史里的工具调用 / 输出数量，调用和输出对不上时标 ⚠
pub fn tools_summary(req: &Value) -> String {
    let mut s = String::new();

    let tools = req.get("tools").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let declared: Vec<String> = tools
        .iter()
        .map(|t| {
            let ty = t.get("type").and_then(Value::as_str).unwrap_or("?");
            let name = t.get("name").and_then(Value::as_str).unwrap_or("?");
            match ty {
                "function" => {
                    let params: Vec<&str> = t
                        .pointer("/parameters/properties")
                        .and_then(Value::as_object)
                        .map(|p| p.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    format!("{name}({}) [function]", params.join(", "))
                }
                "custom" => format!("{name} [custom]"),
                other if name == "?" => format!("[{other}]"),
                other => format!("{name} [{other}]"),
            }
        })
        .collect();
    let _ = write!(s, "    tools  ▸ declared ({}): ", declared.len());
    s.push_str(if declared.is_empty() { "-" } else { "" });
    s.push_str(&declared.join(", "));
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

// 排查用：按类型统计 input 里的每个 item（message 再按 role 细分）
pub fn input_items_summary(req: &Value) -> String {
    if req.get("input").is_some_and(Value::is_string) {
        return "    items  ▸ input is a string (one user message)".into();
    }
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
        .map(|(k, n)| format!("{k} ×{n}"))
        .collect();
    format!("    items  ▸ {}", if parts.is_empty() { "-".into() } else { parts.join(", ") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_color_cycles_every_eight_requests() {
        assert_eq!(req_color("#1 ▶ POST /v1/responses  [stream]"), Some(REQ_COLORS[1]));
        assert_ne!(req_color("#2 ⇢ route"), req_color("#1 ▶ POST"));
        assert_eq!(req_color("#9 ⇢ route"), req_color("#1 ▶ POST"));
        // 没有编号的（启动信息）和不是数字的都不上色
        assert_eq!(req_color("vg-model-router listening on http://…"), None);
        assert_eq!(req_color("#nope"), None);
    }
}

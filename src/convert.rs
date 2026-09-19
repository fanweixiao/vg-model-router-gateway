//! Responses API <-> Chat Completions API 的格式转换（非流式部分）
//! （README: Conversion details）

use std::collections::HashSet;

use serde_json::{Map, Value, json};
use tracing::{debug, warn};

pub struct ChatRequest {
    pub body: Value,
    // custom（自由文本）工具的名字，如 codex 的 apply_patch。
    // 发给上游时包装成 {"input": string} 的 function，回来时再还原成 custom_tool_call。
    pub custom_tools: HashSet<String>,
    // 无法发给上游的工具（如 web_search），日志里显示为 dropped
    pub dropped_tools: Vec<String>,
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 请求：Responses -> Chat Completions（README: Conversion details → Request）
// ---------------------------------------------------------------------------

pub fn to_chat_request(req: &Value, stream: bool) -> ChatRequest {
    let mut messages: Vec<Value> = Vec::new();

    // instructions → system 消息
    if let Some(ins) = req.get("instructions").and_then(Value::as_str)
        && !ins.is_empty()
    {
        messages.push(json!({ "role": "system", "content": ins }));
    }

    // input（字符串或 item 数组）→ messages
    match req.get("input") {
        Some(Value::String(s)) => messages.push(json!({ "role": "user", "content": s })),
        Some(Value::Array(items)) => {
            for item in items {
                convert_input_item(item, &mut messages);
            }
        }
        _ => {}
    }

    let mut body = Map::new();
    if let Some(m) = req.get("model") {
        body.insert("model".into(), m.clone());
    }
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), Value::Bool(stream));
    // 流式时要求上游在最后一个 chunk 带上 usage，否则日志拿不到 usage
    if stream {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }

    // 采样参数 / max_output_tokens / reasoning.effort / text.format 的字段映射
    for key in ["temperature", "top_p", "user"] {
        if let Some(v) = req.get(key).filter(|v| !v.is_null()) {
            body.insert(key.into(), v.clone());
        }
    }
    if let Some(v) = req.get("max_output_tokens").filter(|v| !v.is_null()) {
        body.insert("max_tokens".into(), v.clone());
    }
    if let Some(effort) = req.pointer("/reasoning/effort").filter(|v| !v.is_null()) {
        body.insert("reasoning_effort".into(), effort.clone());
    }
    if let Some(fmt) = req.pointer("/text/format")
        && fmt.get("type").and_then(Value::as_str) == Some("json_schema")
    {
        body.insert(
            "response_format".into(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": fmt.get("name").cloned().unwrap_or(json!("output")),
                    "schema": fmt.get("schema").cloned().unwrap_or(json!({})),
                    "strict": fmt.get("strict").cloned().unwrap_or(json!(false)),
                }
            }),
        );
    }

    // tools 转换：function 直接转；custom 包装成带 input 参数的 function；其他类型丢弃
    let mut custom_tools = HashSet::new();
    let mut dropped_tools = Vec::new();
    let mut tools = Vec::new();
    for tool in req.get("tools").and_then(Value::as_array).into_iter().flatten() {
        let ty = tool.get("type").and_then(Value::as_str).unwrap_or("");
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        match ty {
            "function" => {
                let mut f = Map::new();
                f.insert("name".into(), json!(name));
                if let Some(d) = tool.get("description") {
                    f.insert("description".into(), d.clone());
                }
                f.insert(
                    "parameters".into(),
                    tool.get("parameters")
                        .cloned()
                        .unwrap_or(json!({ "type": "object", "properties": {} })),
                );
                if let Some(s) = tool.get("strict").filter(|v| !v.is_null()) {
                    f.insert("strict".into(), s.clone());
                }
                tools.push(json!({ "type": "function", "function": f }));
            }
            "custom" => {
                // 把工具的语法定义拼进描述里，让模型知道 input 该怎么写
                let mut desc = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(def) = tool.pointer("/format/definition").and_then(Value::as_str) {
                    let syntax = tool
                        .pointer("/format/syntax")
                        .and_then(Value::as_str)
                        .unwrap_or("grammar");
                    desc.push_str(&format!(
                        "\n\nThe `input` argument must be raw text following this {syntax} grammar:\n{def}"
                    ));
                }
                custom_tools.insert(name.to_string());
                tools.push(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": desc,
                        "parameters": {
                            "type": "object",
                            "properties": { "input": { "type": "string", "description": "Raw tool input" } },
                            "required": ["input"],
                        }
                    }
                }));
            }
            other => dropped_tools.push(if name.is_empty() {
                other.to_string()
            } else {
                format!("{other}:{name}")
            }),
        }
    }
    // tool_choice / parallel_tool_calls 只在有工具时才发，否则上游可能报错
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        if let Some(tc) = req.get("tool_choice").filter(|v| !v.is_null()) {
            body.insert("tool_choice".into(), convert_tool_choice(tc));
        }
        if let Some(p) = req.get("parallel_tool_calls").filter(|v| !v.is_null()) {
            body.insert("parallel_tool_calls".into(), p.clone());
        }
    }

    ChatRequest {
        body: Value::Object(body),
        custom_tools,
        dropped_tools,
    }
}

// Responses 的 {"type":"function","name":..} → Chat 的 {"type":"function","function":{"name":..}}
fn convert_tool_choice(tc: &Value) -> Value {
    match tc {
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
            json!({ "type": "function", "function": { "name": o.get("name").cloned().unwrap_or(Value::Null) } })
        }
        Value::Object(_) => json!("auto"),
        other => other.clone(),
    }
}

// 单个 input item → Chat 消息：
// - developer/system → system
// - function_call / custom_tool_call → assistant 的 tool_calls
// - *_output → tool 消息
fn convert_input_item(item: &Value, messages: &mut Vec<Value>) {
    let ty = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(if item.get("role").is_some() { "message" } else { "" });
    let str_field = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_string();

    match ty {
        "message" => {
            let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
                "developer" | "system" => "system",
                "assistant" => "assistant",
                _ => "user",
            };
            let content = convert_message_content(item.get("content"), role);
            messages.push(json!({ "role": role, "content": content }));
        }
        "function_call" => {
            let mut args = str_field("arguments");
            if args.trim().is_empty() {
                args = "{}".into();
            }
            push_tool_call(messages, &str_field("call_id"), &str_field("name"), args);
        }
        "custom_tool_call" => {
            let args = json!({ "input": str_field("input") }).to_string();
            push_tool_call(messages, &str_field("call_id"), &str_field("name"), args);
        }
        "function_call_output" | "custom_tool_call_output" => {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": str_field("call_id"),
                "content": tool_output_to_string(item.get("output")),
            }));
        }
        // 之前轮次的 reasoning（通常是加密的）无法回放给 Chat 接口，直接丢弃（README: Limitations）
        "reasoning" => {}
        other => debug!(item_type = other, "skipping unsupported input item"),
    }
}

// Chat 要求 tool_calls 挂在 assistant 消息上：
// 连续的多个调用（以及前面紧挨着的 assistant 文本）合并成一条消息
fn push_tool_call(messages: &mut Vec<Value>, call_id: &str, name: &str, args: String) {
    let tc = json!({
        "id": call_id,
        "type": "function",
        "function": { "name": name, "arguments": args },
    });
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("assistant")
        && let Some(obj) = last.as_object_mut()
    {
        if let Some(arr) = obj
            .entry("tool_calls")
            .or_insert_with(|| json!([]))
            .as_array_mut()
        {
            arr.push(tc);
        }
        return;
    }
    messages.push(json!({ "role": "assistant", "content": null, "tool_calls": [tc] }));
}

// 消息内容：文本片段拼成字符串；user 消息带图片时保留数组形式（图片只支持 URL / data URL）
fn convert_message_content(content: Option<&Value>, role: &str) -> Value {
    let parts = match content {
        Some(Value::String(s)) => return json!(s),
        Some(Value::Array(parts)) => parts,
        _ => return json!(""),
    };

    let mut out = Vec::new();
    let mut has_image = false;
    for p in parts {
        match p.get("type").and_then(Value::as_str).unwrap_or("") {
            "input_text" | "output_text" | "text" => {
                out.push(json!({ "type": "text", "text": p.get("text").and_then(Value::as_str).unwrap_or("") }))
            }
            "refusal" => {
                out.push(json!({ "type": "text", "text": p.get("refusal").and_then(Value::as_str).unwrap_or("") }))
            }
            "input_image" => {
                let url = p
                    .get("image_url")
                    .and_then(|u| u.as_str().map(str::to_string).or_else(|| u.get("url")?.as_str().map(str::to_string)))
                    .unwrap_or_default();
                if url.is_empty() {
                    warn!("input_image without image_url (file_id is not supported), dropped");
                    continue;
                }
                has_image = true;
                let mut image = json!({ "url": url });
                if let Some(d) = p.get("detail").filter(|v| !v.is_null()) {
                    image["detail"] = d.clone();
                }
                out.push(json!({ "type": "image_url", "image_url": image }));
            }
            other => debug!(part_type = other, "skipping unsupported content part"),
        }
    }

    if has_image && role == "user" {
        return Value::Array(out);
    }
    let text: Vec<&str> = out
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect();
    json!(text.join("\n"))
}

// 工具输出统一转成字符串，作为 tool 消息的 content
fn tool_output_to_string(output: Option<&Value>) -> String {
    match output {
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

// ---------------------------------------------------------------------------
// 响应：Chat Completions -> Responses（README: Conversion details → Response）
// ---------------------------------------------------------------------------

// finish_reason → (Responses status, incomplete_details.reason)
// （README: How finish_reason maps to what Codex receives）
pub fn map_finish_reason(fr: Option<&str>) -> (&'static str, Option<&'static str>) {
    match fr {
        Some("length") => ("incomplete", Some("max_output_tokens")),
        Some("content_filter") => ("incomplete", Some("content_filter")),
        _ => ("completed", None),
    }
}

// Chat 的 usage（prompt/completion_tokens）→ Responses 的 usage（input/output_tokens），返回给 codex
pub fn convert_usage(u: Option<&Value>) -> Value {
    let Some(u) = u.filter(|u| u.is_object()) else {
        return Value::Null;
    };
    let n = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let input = n("/prompt_tokens");
    let output = n("/completion_tokens");
    let total = u.get("total_tokens").and_then(Value::as_u64).unwrap_or(input + output);
    json!({
        "input_tokens": input,
        "input_tokens_details": { "cached_tokens": n("/prompt_tokens_details/cached_tokens") },
        "output_tokens": output,
        "output_tokens_details": { "reasoning_tokens": n("/completion_tokens_details/reasoning_tokens") },
        "total_tokens": total,
    })
}

// 以下几个函数构造 Responses 格式的对象：response 本体、message、reasoning、工具调用
pub fn response_object(
    id: &str,
    created_at: i64,
    model: &str,
    status: &str,
    incomplete_reason: Option<&str>,
    output: Vec<Value>,
    usage: Value,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": status,
        "model": model,
        "output": output,
        "usage": usage,
        "incomplete_details": incomplete_reason.map(|r| json!({ "reason": r })),
        "error": null,
    })
}

pub fn message_item(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
    })
}

pub fn reasoning_item(id: &str, text: &str) -> Value {
    json!({
        "id": id,
        "type": "reasoning",
        "summary": [{ "type": "summary_text", "text": text }],
    })
}

pub fn tool_call_item(id: &str, call_id: &str, name: &str, args: &str, custom: bool) -> Value {
    if custom {
        // custom 工具：拆开 {"input": "..."} 还原成 custom_tool_call；模型没按格式给就用原始字符串
        let input = serde_json::from_str::<Value>(args)
            .ok()
            .and_then(|v| v.get("input")?.as_str().map(str::to_string))
            .unwrap_or_else(|| args.to_string());
        json!({
            "id": id,
            "type": "custom_tool_call",
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "input": input,
        })
    } else {
        json!({
            "id": id,
            "type": "function_call",
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "arguments": args,
        })
    }
}

// 上游的思考内容：reasoning_content 或 reasoning 字段，codex 里显示为 reasoning summary
pub fn reasoning_text(v: &Value) -> Option<&str> {
    v.get("reasoning_content")
        .or_else(|| v.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

// stream=false：整个 chat completion 转成一个 Responses 对象（只用第一个 choice）
pub fn chat_to_response(chat: &Value, resp_id: &str, fallback_model: &str, custom: &HashSet<String>) -> Value {
    let choice = chat.pointer("/choices/0").cloned().unwrap_or(Value::Null);
    let msg = choice.get("message").cloned().unwrap_or(Value::Null);
    let mut output = Vec::new();

    if let Some(r) = reasoning_text(&msg) {
        output.push(reasoning_item(&new_id("rs"), r));
    }
    if let Some(text) = msg.get("content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        output.push(message_item(&new_id("msg"), text));
    }
    for tc in msg.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
        let name = tc.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
        let args = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("");
        let call_id = tc
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| new_id("call"));
        output.push(tool_call_item(&new_id("fc"), &call_id, name, args, custom.contains(name)));
    }

    let (status, reason) = map_finish_reason(choice.get("finish_reason").and_then(Value::as_str));
    let model = chat.get("model").and_then(Value::as_str).unwrap_or(fallback_model);
    let created = chat.get("created").and_then(Value::as_i64).unwrap_or_else(now_secs);
    response_object(resp_id, created, model, status, reason, output, convert_usage(chat.get("usage")))
}

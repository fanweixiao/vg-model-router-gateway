//! 导出 SFT 数据用：把训练数据日志里的 Responses 格式（instructions + input/output items + tools）
//! 转成 LoRA 微调工具通用的 Chat 格式（messages + tools）。代理本身不做格式转换，请求原样透传。
//! （README: Model Router → Training data）

use serde_json::{Map, Value, json};

// instructions + items → Chat messages。
// with_reasoning = true 时，reasoning item 里的明文（summary / content）挂到后面那条 assistant 消息的 reasoning_content 上
pub fn to_chat_messages(instructions: Option<&str>, items: &[Value], with_reasoning: bool) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(ins) = instructions.filter(|s| !s.is_empty()) {
        messages.push(json!({ "role": "system", "content": ins }));
    }
    let mut reasoning: Option<String> = None;
    for item in items {
        convert_item(item, &mut messages, with_reasoning.then_some(&mut reasoning));
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            continue;
        }
        // 这个 item 产生（或 tool_call 合并进）的 assistant 消息，带上前面的 reasoning
        if let Some(m) = messages.last_mut().filter(|m| m["role"] == "assistant")
            && let Some(r) = reasoning.take()
        {
            m["reasoning_content"] = json!(r);
        }
    }
    messages
}

// Responses tools → Chat tools：function 直接转；custom（如 apply_patch）包装成带 input 参数的 function
pub fn to_chat_tools(req_tools: &[Value]) -> Vec<Value> {
    // tools 转换：function 直接转；custom 包装成带 input 参数的 function；其他类型丢弃
    let mut tools = Vec::new();
    for tool in req_tools {
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
            // web_search 等内置工具没有 Chat 格式的对应物，丢弃
            _ => {}
        }
    }
    tools
}

// 单个 item → Chat 消息：
// - developer/system → system
// - function_call / custom_tool_call → assistant 的 tool_calls
// - *_output → tool 消息
// - reasoning → 只在导出 reasoning 时取出明文
fn convert_item(item: &Value, messages: &mut Vec<Value>, reasoning: Option<&mut Option<String>>) {
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
        "reasoning" => {
            if let Some(slot) = reasoning {
                let text = reasoning_text(item);
                if !text.is_empty() {
                    slot.get_or_insert_with(String::new).push_str(&text);
                }
            }
        }
        _ => {}
    }
}

// reasoning item 的明文：优先 content（完整思考），没有再用 summary；加密的 encrypted_content 取不到
fn reasoning_text(item: &Value) -> String {
    let join = |k: &str| {
        item.get(k)
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    };
    let content = join("content");
    if content.is_empty() { join("summary") } else { content }
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
                    continue;
                }
                has_image = true;
                let mut image = json!({ "url": url });
                if let Some(d) = p.get("detail").filter(|v| !v.is_null()) {
                    image["detail"] = d.clone();
                }
                out.push(json!({ "type": "image_url", "image_url": image }));
            }
            _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_become_chat_messages() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
            json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "think"}]}),
            json!({"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"}),
            json!({"type": "custom_tool_call", "call_id": "c2", "name": "apply_patch", "input": "*** Begin"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
        ];
        let m = to_chat_messages(Some("sys"), &items, true);
        assert_eq!(m[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(m[1], json!({"role": "user", "content": "hi"}));
        assert_eq!(m[2]["reasoning_content"], "think");
        assert_eq!(m[2]["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(m[2]["tool_calls"][1]["function"]["arguments"], "{\"input\":\"*** Begin\"}");
        assert_eq!(m[3]["role"], "tool");
        assert!(to_chat_messages(None, &items, false)[1].get("reasoning_content").is_none());
    }
}

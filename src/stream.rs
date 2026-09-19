//! stream=true：把上游 Chat Completions 的 SSE chunk 实时翻译成 Responses API 的 SSE 事件，
//! 结束时打印 stop 值和 usage（README: Conversion details → Response、Reading the logs）

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::time::Instant;

use axum::response::sse::Event;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::convert::{
    convert_usage, map_finish_reason, message_item, new_id, now_secs, reasoning_item, reasoning_text,
    response_object, tool_call_item,
};
use crate::report;
use crate::trainlog::{self, Pending};

pub type EventTx = mpsc::Sender<Result<Event, Infallible>>;

// 输出里的一个 item：文本消息 / 思考内容 / 工具调用
enum Kind {
    Message,
    Reasoning,
    Tool {
        call_id: String,
        name: String,
        custom: bool,
        added: bool,
    },
}

struct Item {
    id: String,
    kind: Kind,
    // 累积的增量内容（文本 / 思考 / 工具参数）
    buf: String,
    // 发出 response.output_item.done 后的最终 item，用于最后的 response 对象
    done: Option<Value>,
}

// 处理一行 SSE 后的结果：继续 / 收到 [DONE] / 上游报错
enum Line {
    Continue,
    Done,
    Error(String),
}

pub struct Translator {
    tx: EventTx,
    seq: u64,
    req_id: u64,
    started: Instant,
    resp_id: String,
    created_at: i64,
    model: String,
    custom_tools: HashSet<String>,

    // 输出 item 列表，以及当前正在写入的文本 / 思考 item、工具调用 index → item 的映射
    items: Vec<Item>,
    open_text: Option<usize>,
    open_reasoning: Option<usize>,
    tool_slots: HashMap<u64, usize>,
    last_tool: Option<usize>,

    // 以下用于最后的日志：stop 值、usage、是否收到 [DONE]、异常 note
    finish_reason: Option<String>,
    extra_stop: Vec<(String, Value)>,
    usage: Option<Value>,
    got_done: bool,
    chunks: u64,
    client_gone: bool,
    notes: Vec<String>,

    // model-router 模式下的训练数据记录，结束时补上完整输出后写入
    pending: Option<Pending>,
}

impl Translator {
    pub fn new(
        tx: EventTx,
        req_id: u64,
        started: Instant,
        model: String,
        custom_tools: HashSet<String>,
        pending: Option<Pending>,
    ) -> Self {
        Self {
            tx,
            seq: 0,
            req_id,
            started,
            resp_id: new_id("resp"),
            created_at: now_secs(),
            model,
            custom_tools,
            items: Vec::new(),
            open_text: None,
            open_reasoning: None,
            tool_slots: HashMap::new(),
            last_tool: None,
            finish_reason: None,
            extra_stop: Vec::new(),
            usage: None,
            got_done: false,
            chunks: 0,
            client_gone: false,
            notes: Vec::new(),
            pending,
        }
    }

    // 主循环：先发 response.created / in_progress，再逐行读上游 SSE，最后 finish
    pub async fn run(mut self, upstream: reqwest::Response) {
        let snapshot = response_object(&self.resp_id, self.created_at, &self.model, "in_progress", None, vec![], Value::Null);
        self.emit("response.created", json!({ "response": snapshot.clone() })).await;
        self.emit("response.in_progress", json!({ "response": snapshot })).await;

        let mut body = upstream.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut error: Option<String> = None;

        'outer: while let Some(chunk) = body.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    error = Some(format!("upstream stream read error: {e}"));
                    break;
                }
            };
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                match self.process_line(&String::from_utf8_lossy(&raw)).await {
                    Line::Continue => {}
                    Line::Done => break 'outer,
                    Line::Error(e) => {
                        error = Some(e);
                        break 'outer;
                    }
                }
                if self.client_gone {
                    break 'outer;
                }
            }
        }
        // 最后一行可能没有换行符
        if error.is_none() && !self.got_done && !self.client_gone && !buf.is_empty() {
            let rest = String::from_utf8_lossy(&buf).to_string();
            if let Line::Error(e) = self.process_line(&rest).await {
                error = Some(e);
            }
        }

        self.finish(error).await;
    }

    // 解析一行 "data: ..."：[DONE] / error 事件 / 普通 chunk
    async fn process_line(&mut self, line: &str) -> Line {
        let line = line.trim_end_matches(['\r', '\n']);
        let Some(data) = line.strip_prefix("data:") else {
            return Line::Continue;
        };
        let data = data.trim_start();
        if data == "[DONE]" {
            self.got_done = true;
            return Line::Done;
        }
        match serde_json::from_str::<Value>(data) {
            Ok(v) => {
                if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                    return Line::Error(format!("upstream sent error event: {err}"));
                }
                self.on_chunk(&v).await;
            }
            Err(e) => warn!(req = self.req_id, "unparseable upstream SSE data ({e}): {data}"),
        }
        Line::Continue
    }

    // 处理一个 chunk：记下 usage，把 delta 分发给思考 / 文本 / 工具调用，记录 finish_reason
    async fn on_chunk(&mut self, v: &Value) {
        self.chunks += 1;
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        if let Some(m) = v.get("model").and_then(Value::as_str).filter(|m| !m.is_empty()) {
            self.model = m.to_string();
        }
        let Some(choices) = v.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(r) = reasoning_text(delta) {
                self.on_reasoning(r).await;
            }
            if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                self.on_text(t).await;
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tcs {
                    self.on_tool_delta(tc).await;
                }
            }
            // stop 值：记录 finish_reason；中途变化也记一条 note，方便排查
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                debug!(req = self.req_id, chunk = self.chunks, finish_reason = fr, "finish_reason received");
                if let Some(prev) = &self.finish_reason
                    && prev != fr
                {
                    self.notes.push(format!("finish_reason changed mid-stream: {prev:?} -> {fr:?}"));
                }
                self.finish_reason = Some(fr.to_string());
            }
            for (k, val) in report::extra_stop_fields(choice) {
                match self.extra_stop.iter_mut().find(|(ek, _)| *ek == k) {
                    Some(slot) => slot.1 = val,
                    None => self.extra_stop.push((k, val)),
                }
            }
        }
    }

    fn push_item(&mut self, kind: Kind, prefix: &str) -> usize {
        self.items.push(Item {
            id: new_id(prefix),
            kind,
            buf: String::new(),
            done: None,
        });
        self.items.len() - 1
    }

    // 思考内容增量 → reasoning item + reasoning_summary_text.delta 事件
    async fn on_reasoning(&mut self, delta: &str) {
        let idx = match self.open_reasoning {
            Some(i) => i,
            None => {
                self.close_text().await;
                let i = self.push_item(Kind::Reasoning, "rs");
                self.open_reasoning = Some(i);
                let id = self.items[i].id.clone();
                self.emit(
                    "response.output_item.added",
                    json!({ "output_index": i, "item": { "id": id, "type": "reasoning", "summary": [] } }),
                )
                .await;
                self.emit(
                    "response.reasoning_summary_part.added",
                    json!({ "item_id": id, "output_index": i, "summary_index": 0, "part": { "type": "summary_text", "text": "" } }),
                )
                .await;
                i
            }
        };
        self.items[idx].buf.push_str(delta);
        let id = self.items[idx].id.clone();
        self.emit(
            "response.reasoning_summary_text.delta",
            json!({ "item_id": id, "output_index": idx, "summary_index": 0, "delta": delta }),
        )
        .await;
    }

    // 文本增量 → message item + output_text.delta 事件
    async fn on_text(&mut self, delta: &str) {
        let idx = match self.open_text {
            Some(i) => i,
            None => {
                self.close_reasoning().await;
                let i = self.push_item(Kind::Message, "msg");
                self.open_text = Some(i);
                let id = self.items[i].id.clone();
                self.emit(
                    "response.output_item.added",
                    json!({ "output_index": i, "item": {
                        "id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": []
                    } }),
                )
                .await;
                self.emit(
                    "response.content_part.added",
                    json!({ "item_id": id, "output_index": i, "content_index": 0,
                            "part": { "type": "output_text", "text": "", "annotations": [] } }),
                )
                .await;
                i
            }
        };
        self.items[idx].buf.push_str(delta);
        let id = self.items[idx].id.clone();
        self.emit(
            "response.output_text.delta",
            json!({ "item_id": id, "output_index": idx, "content_index": 0, "delta": delta, "logprobs": [] }),
        )
        .await;
    }

    // 工具调用增量：按 index 归到同一个调用，拼接参数；
    // function 工具发 function_call_arguments.delta，custom 工具等结束时一次性给出 input
    async fn on_tool_delta(&mut self, tc: &Value) {
        let key = tc.get("index").and_then(Value::as_u64);
        let id = tc.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
        let name = tc.pointer("/function/name").and_then(Value::as_str).filter(|s| !s.is_empty());
        let args = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("");

        let existing = match key {
            Some(k) => self.tool_slots.get(&k).copied(),
            None => self.last_tool,
        };
        // 同一个 index 沿用同一个调用，除非上游在这个 index 上换了新的 call id
        let slot = match existing {
            Some(i) if id.is_none_or(|id| matches!(&self.items[i].kind, Kind::Tool { call_id, .. } if call_id == id)) => i,
            _ => {
                self.close_reasoning().await;
                self.close_text().await;
                let kind = Kind::Tool {
                    call_id: id.map(str::to_string).unwrap_or_else(|| new_id("call")),
                    name: String::new(),
                    custom: false,
                    added: false,
                };
                let i = self.push_item(kind, "fc");
                if let Some(k) = key {
                    self.tool_slots.insert(k, i);
                }
                i
            }
        };
        self.last_tool = Some(slot);

        let custom_names = &self.custom_tools;
        let item = &mut self.items[slot];
        item.buf.push_str(args);
        let Kind::Tool { call_id, name: cur_name, custom, added } = &mut item.kind else {
            return;
        };
        if let Some(n) = name
            && cur_name.is_empty()
        {
            *cur_name = n.to_string();
            *custom = custom_names.contains(n);
        }
        let need_added = !*added && !cur_name.is_empty();
        if need_added {
            *added = true;
        }
        let (item_id, call_id, cur_name, custom, added) =
            (item.id.clone(), call_id.clone(), cur_name.clone(), *custom, *added);

        if need_added {
            self.emit_tool_added(slot, &item_id, &call_id, &cur_name, custom).await;
        }
        if added && !custom && !args.is_empty() {
            self.emit(
                "response.function_call_arguments.delta",
                json!({ "item_id": item_id, "output_index": slot, "delta": args }),
            )
            .await;
        }
    }

    async fn emit_tool_added(&mut self, idx: usize, item_id: &str, call_id: &str, name: &str, custom: bool) {
        let item = if custom {
            json!({ "id": item_id, "type": "custom_tool_call", "status": "in_progress",
                    "call_id": call_id, "name": name, "input": "" })
        } else {
            json!({ "id": item_id, "type": "function_call", "status": "in_progress",
                    "call_id": call_id, "name": name, "arguments": "" })
        };
        self.emit("response.output_item.added", json!({ "output_index": idx, "item": item })).await;
    }

    // 以下 close_* 结束对应的 item，发出 *.done 和 output_item.done 事件
    async fn close_text(&mut self) {
        let Some(i) = self.open_text.take() else { return };
        let (id, text) = (self.items[i].id.clone(), self.items[i].buf.clone());
        self.emit(
            "response.output_text.done",
            json!({ "item_id": id, "output_index": i, "content_index": 0, "text": text, "logprobs": [] }),
        )
        .await;
        self.emit(
            "response.content_part.done",
            json!({ "item_id": id, "output_index": i, "content_index": 0,
                    "part": { "type": "output_text", "text": text, "annotations": [] } }),
        )
        .await;
        let item = message_item(&id, &text);
        self.items[i].done = Some(item.clone());
        self.emit("response.output_item.done", json!({ "output_index": i, "item": item })).await;
    }

    async fn close_reasoning(&mut self) {
        let Some(i) = self.open_reasoning.take() else { return };
        let (id, text) = (self.items[i].id.clone(), self.items[i].buf.clone());
        self.emit(
            "response.reasoning_summary_text.done",
            json!({ "item_id": id, "output_index": i, "summary_index": 0, "text": text }),
        )
        .await;
        self.emit(
            "response.reasoning_summary_part.done",
            json!({ "item_id": id, "output_index": i, "summary_index": 0,
                    "part": { "type": "summary_text", "text": text } }),
        )
        .await;
        let item = reasoning_item(&id, &text);
        self.items[i].done = Some(item.clone());
        self.emit("response.output_item.done", json!({ "output_index": i, "item": item })).await;
    }

    async fn close_tools(&mut self) {
        for i in 0..self.items.len() {
            let it = &self.items[i];
            let Kind::Tool { call_id, name, custom, added } = &it.kind else { continue };
            if it.done.is_some() {
                continue;
            }
            let (id, call_id, name, custom, added, args) =
                (it.id.clone(), call_id.clone(), name.clone(), *custom, *added, it.buf.clone());
            if name.is_empty() {
                self.notes.push(format!("tool call {call_id} never received a function name"));
            }
            if !added {
                self.emit_tool_added(i, &id, &call_id, &name, custom).await;
            }
            if !custom {
                self.emit(
                    "response.function_call_arguments.done",
                    json!({ "item_id": id, "output_index": i, "arguments": args }),
                )
                .await;
            }
            let item = tool_call_item(&id, &call_id, &name, &args, custom);
            self.items[i].done = Some(item.clone());
            self.emit("response.output_item.done", json!({ "output_index": i, "item": item })).await;
        }
    }

    // 结束：关闭所有 item，按 finish_reason 决定发 completed / incomplete / failed，
    // 然后打印 stop + usage 日志（README: How finish_reason maps to what Codex receives）
    async fn finish(mut self, mut error: Option<String>) {
        self.close_reasoning().await;
        self.close_text().await;
        self.close_tools().await;

        // 既没 finish_reason 也没 [DONE]：上游中途断了，发 response.failed
        if error.is_none() && !self.client_gone && !self.got_done && self.finish_reason.is_none() {
            error = Some("upstream stream ended without finish_reason and without [DONE]".into());
        }
        if !self.got_done && !self.client_gone {
            self.notes.push(format!("upstream stream closed without [DONE] (after {} chunks)", self.chunks));
        }

        let output: Vec<Value> = self.items.iter().filter_map(|i| i.done.clone()).collect();
        let usage = convert_usage(self.usage.as_ref());

        let (event, status) = match &error {
            Some(err) => {
                self.notes.push(format!("ERROR: {err}"));
                let mut resp = response_object(&self.resp_id, self.created_at, &self.model, "failed", None, output, usage);
                resp["error"] = json!({ "code": "upstream_error", "message": err });
                ("response.failed", resp)
            }
            None => {
                let (status, reason) = map_finish_reason(self.finish_reason.as_deref());
                let resp = response_object(&self.resp_id, self.created_at, &self.model, status, reason, output, usage);
                let ev = if status == "incomplete" { "response.incomplete" } else { "response.completed" };
                (ev, resp)
            }
        };
        let status_str = status["status"].as_str().unwrap_or("?").to_string();

        if self.client_gone {
            self.notes.push("client disconnected before the response finished".into());
        } else {
            self.emit(event, json!({ "response": status })).await;
        }

        if let Some(p) = self.pending.take() {
            let err = error.as_deref().or(self.client_gone.then_some("client disconnected"));
            p.finish(&self.model, self.chat_message(), self.finish_reason.as_deref(), self.usage.as_ref(), err);
        }

        report::log(&report::Report {
            req_id: self.req_id,
            stream: true,
            elapsed: self.started.elapsed(),
            model: &self.model,
            finish_reason: self.finish_reason.as_deref(),
            extra_stop: &self.extra_stop,
            status: &format!("{status_str} (sent {event})"),
            usage: report::Usage::from_chat(self.usage.as_ref()),
            notes: &self.notes,
        });
    }

    // 把流式输出拼回 Chat 格式的 assistant 消息（写训练数据日志用）
    fn chat_message(&self) -> serde_json::Value {
        let (mut text, mut reasoning, mut tool_calls) = (String::new(), String::new(), Vec::new());
        for it in &self.items {
            match &it.kind {
                Kind::Message => text.push_str(&it.buf),
                Kind::Reasoning => reasoning.push_str(&it.buf),
                Kind::Tool { call_id, name, .. } => tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": it.buf },
                })),
            }
        }
        trainlog::assistant_message(
            (!text.is_empty()).then_some(text),
            (!reasoning.is_empty()).then_some(reasoning),
            tool_calls,
        )
    }

    // 发一个 Responses SSE 事件给 codex（带 type 和递增的 sequence_number）
    async fn emit(&mut self, ty: &str, mut data: Value) {
        if self.client_gone {
            return;
        }
        data["type"] = json!(ty);
        data["sequence_number"] = json!(self.seq);
        self.seq += 1;
        let ev = Event::default().event(ty).data(data.to_string());
        if self.tx.send(Ok(ev)).await.is_err() {
            self.client_gone = true;
        }
    }
}

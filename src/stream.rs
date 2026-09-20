//! stream=true：把上游 /v1/responses 的 SSE 原样转发给 codex，只改写 data 里的 model（加 viv- 前缀）；
//! 同时记下最终的 response 对象，结束时打印 status 和 usage（README: Reading the logs）

use std::convert::Infallible;
use std::time::Instant;

use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::debug;

use crate::report;
use crate::trainlog::Pending;

pub type ByteTx = mpsc::Sender<Result<Bytes, Infallible>>;

// 返回给 codex 的 model-id 都加上这个前缀（README: Model id prefix）
pub const MODEL_PREFIX: &str = "viv-";

// 给响应对象的 model 加前缀：非流式是顶层 model，流式事件是 response.model。已经带前缀的不重复加。
// 返回是否改动过
pub fn add_model_prefix(v: &mut Value) -> bool {
    fn prefix(m: Option<&mut Value>) -> bool {
        if let Some(m) = m
            && let Some(s) = m.as_str()
            && !s.is_empty()
            && !s.starts_with(MODEL_PREFIX)
        {
            *m = Value::String(format!("{MODEL_PREFIX}{s}"));
            return true;
        }
        false
    }
    let top = prefix(v.get_mut("model"));
    prefix(v.pointer_mut("/response/model")) | top
}

// 改写一行 SSE：data 行里的 JSON 有 model 要加前缀时才重新序列化，其他行（event: / id: / 空行 / [DONE]
// / 不带 model 的 delta 事件）原样返回。第二个返回值是解析出的事件（改写前），用来记录最终结果
fn rewrite_line(line: &str) -> (String, Option<Value>) {
    let body = line.trim_end_matches(['\r', '\n']);
    let Some(data) = body.strip_prefix("data:") else {
        return (line.to_string(), None);
    };
    let Ok(event) = serde_json::from_str::<Value>(data.trim_start()) else {
        return (line.to_string(), None);
    };
    let mut out = event.clone();
    if !add_model_prefix(&mut out) {
        return (line.to_string(), Some(event));
    }
    let eol = &line[body.len()..];
    (format!("data: {out}{eol}"), Some(event))
}

pub struct Relay {
    tx: ByteTx,
    req_id: u64,
    started: Instant,
    // 请求里的 model（路由后的），上游没给最终 response 时日志里用它
    model: String,
    // response.completed / incomplete / failed 里的 response 对象（未加前缀）
    final_resp: Option<Value>,
    // 上游的 error 事件
    error: Option<String>,
    events: u64,
    client_gone: bool,
    // model-router 模式下的训练数据记录，结束时补上完整输出后写入
    pending: Option<Pending>,
}

impl Relay {
    pub fn new(tx: ByteTx, req_id: u64, started: Instant, model: String, pending: Option<Pending>) -> Self {
        Self {
            tx,
            req_id,
            started,
            model,
            final_resp: None,
            error: None,
            events: 0,
            client_gone: false,
            pending,
        }
    }

    // 主循环：按行读上游 SSE，改写后转发；每个网络 chunk 里完整的行攒成一次发送
    pub async fn run(mut self, upstream: reqwest::Response) {
        let mut body = upstream.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut read_error = None;

        while let Some(chunk) = body.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    read_error = Some(format!("upstream stream read error: {e}"));
                    break;
                }
            };
            buf.extend_from_slice(&bytes);
            let mut out = String::new();
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                out.push_str(&self.process_line(&String::from_utf8_lossy(&raw)));
            }
            if !out.is_empty() && !self.send(out).await {
                break;
            }
        }
        // 最后一行没有换行符时也转发出去
        if !buf.is_empty() && !self.client_gone {
            let out = self.process_line(&String::from_utf8_lossy(&buf));
            self.send(out).await;
        }
        self.finish(read_error);
    }

    fn process_line(&mut self, line: &str) -> String {
        let (out, event) = rewrite_line(line);
        if let Some(ev) = event {
            self.events += 1;
            let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
            debug!("#{} upstream event: {ty}", self.req_id);
            match ty {
                "response.completed" | "response.incomplete" | "response.failed" => {
                    self.final_resp = ev.get("response").cloned();
                }
                "error" => {
                    self.error = Some(
                        ev.get("message")
                            .or_else(|| ev.pointer("/error/message"))
                            .and_then(Value::as_str)
                            .map_or_else(|| ev.to_string(), str::to_string),
                    );
                }
                _ => {}
            }
        }
        out
    }

    async fn send(&mut self, out: String) -> bool {
        if self.tx.send(Ok(Bytes::from(out))).await.is_err() {
            self.client_gone = true;
        }
        !self.client_gone
    }

    // 结束：写训练数据日志，打印 status + usage 日志
    fn finish(mut self, read_error: Option<String>) {
        let mut notes = Vec::new();
        let mut error = read_error.or(self.error.take());
        if self.client_gone {
            notes.push("client disconnected before the response finished".to_string());
        } else if self.final_resp.is_none() && error.is_none() {
            error = Some(format!(
                "upstream stream ended without response.completed / incomplete / failed (after {} events)",
                self.events
            ));
        }
        if let Some(e) = &error {
            notes.push(format!("ERROR: {e}"));
        }

        let resp = self.final_resp.as_ref();
        if let Some(p) = self.pending.take() {
            let err = error.as_deref().or(self.client_gone.then_some("client disconnected"));
            p.finish(&self.model, resp, err);
        }
        report::log(&report::Report::new(self.req_id, true, self.started.elapsed(), &self.model, resp, notes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prefix_is_added_once() {
        let mut v = json!({ "model": "gpt-5", "response": { "model": "viv-gpt-5" } });
        add_model_prefix(&mut v);
        assert_eq!(v["model"], "viv-gpt-5");
        assert_eq!(v["response"]["model"], "viv-gpt-5");
    }

    #[test]
    fn rewrite_only_touches_data_lines() {
        assert_eq!(rewrite_line("event: response.created\n").0, "event: response.created\n");
        assert_eq!(rewrite_line("data: [DONE]\n").0, "data: [DONE]\n");
        // 没有 model 的事件原样返回（不重新序列化）
        let delta = "data:{\"type\": \"response.output_text.delta\", \"delta\": \"1.0e-5\"}\n";
        assert_eq!(rewrite_line(delta).0, delta);
        let (out, ev) = rewrite_line("data: {\"type\":\"response.completed\",\"response\":{\"model\":\"m\"}}\r\n");
        assert_eq!(out, "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"viv-m\"}}\r\n");
        assert_eq!(ev.unwrap()["response"]["model"], "m");
    }
}

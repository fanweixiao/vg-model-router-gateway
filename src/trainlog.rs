//! 训练数据日志（mode = "model-router" 时）：每个请求一行 JSONL，记录路由决策、
//! 发给上游的完整 Chat 请求和模型的完整输出；再用 `vg-mirror export` 导出成 LoRA 微调数据：
//!   - router：分类文本 → 分类结果，用来训练自己的路由模型
//!   - sft：   完整对话 → 模型输出，用来把 frontier 模型蒸馏到小模型
//! （README: Model Router → Training data）

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};
use tracing::warn;

use crate::router::{Decision, Source};

pub struct TrainLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl TrainLog {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append(&self, record: &Value) {
        let mut line = record.to_string();
        line.push('\n');
        let mut f = self.file.lock().unwrap();
        if let Err(e) = f.write_all(line.as_bytes()) {
            warn!("failed to write training log {}: {e}", self.path.display());
        }
    }
}

// 一个还没拿到响应的记录：请求时创建，响应结束时补上输出后写入
pub struct Pending {
    log: Arc<TrainLog>,
    record: Value,
    started: Instant,
}

impl Pending {
    pub fn new(log: Arc<TrainLog>, req_id: u64, requested_model: &str, decision: &Decision, chat_body: &Value) -> Self {
        let (source, classifier) = match &decision.source {
            Source::Classified(v) => (
                json!({ "type": "classified" }),
                json!({
                    "model": v.classifier_model,
                    "state": v.state,
                    "answers": v.answers,
                    "latency_ms": v.latency.as_millis() as u64,
                }),
            ),
            Source::Cached { from_req } => (json!({ "type": "cached", "from_req": from_req }), Value::Null),
            Source::Fallback(e) => (json!({ "type": "fallback", "error": e }), Value::Null),
        };
        let record = json!({
            "ts": now_millis(),
            "req_id": req_id,
            "session": decision.session,
            "new_turn": decision.new_turn,
            "requested_model": requested_model,
            "routed_model": decision.model,
            "tier": decision.tier.as_str(),
            "route": source,
            "classifier": classifier,
            "request": {
                "messages": chat_body.get("messages").cloned().unwrap_or(json!([])),
                "tools": chat_body.get("tools").cloned().unwrap_or(json!([])),
            },
        });
        Self {
            log,
            record,
            started: Instant::now(),
        }
    }

    // message：Chat 格式的 assistant 消息；error：上游报错 / 流中断时的错误信息
    pub fn finish(mut self, model: &str, message: Value, finish_reason: Option<&str>, usage: Option<&Value>, error: Option<&str>) {
        self.record["response"] = json!({
            "model": model,
            "message": message,
            "finish_reason": finish_reason,
            "usage": usage,
            "error": error,
        });
        self.record["latency_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.log.append(&self.record);
    }
}

// Chat 格式的 assistant 消息（非流式直接用 choices[0].message，流式由 stream.rs 拼出来）
pub fn assistant_message(content: Option<String>, reasoning: Option<String>, tool_calls: Vec<Value>) -> Value {
    let mut m = json!({ "role": "assistant", "content": content });
    if let Some(r) = reasoning {
        m["reasoning_content"] = json!(r);
    }
    if !tool_calls.is_empty() {
        m["tool_calls"] = Value::Array(tool_calls);
    }
    m
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 导出：vg-mirror export <router|sft> --in <log.jsonl> --out <data.jsonl> [...]
// ---------------------------------------------------------------------------

pub const EXPORT_USAGE: &str = "\
usage: vg-mirror export router --in <log.jsonl> --out <data.jsonl>
       vg-mirror export sft    --in <log.jsonl> --out <data.jsonl> [--tier frontier|medium|small|all] [--with-reasoning]

  router  one sample per classified request: classifier input → classifier answer
  sft     one sample per successful request: chat messages + tools → model output
          (--tier defaults to frontier; reasoning_content is dropped unless --with-reasoning)";

const ROUTER_SYSTEM: &str = "Classify the difficulty of the AI request for model routing. \
Reply with JSON: {\"difficulty_level\": \"cheap\"|\"medium\"|\"expensive\", \
\"difficulty_score\": 0-4, \"prefer_cheap_model\": 0-1}.";

pub fn export(args: &[String]) -> Result<String, String> {
    let kind = args.first().map(String::as_str).ok_or(EXPORT_USAGE)?;
    let (mut input, mut output, mut tier, mut with_reasoning) = (None, None, "frontier".to_string(), false);
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--in" => input = it.next().cloned(),
            "--out" => output = it.next().cloned(),
            "--tier" => tier = it.next().cloned().ok_or(EXPORT_USAGE)?,
            "--with-reasoning" => with_reasoning = true,
            other => return Err(format!("unknown argument: {other}\n\n{EXPORT_USAGE}")),
        }
    }
    let (Some(input), Some(output)) = (input, output) else {
        return Err(EXPORT_USAGE.into());
    };

    let reader = BufReader::new(File::open(&input).map_err(|e| format!("open {input}: {e}"))?);
    let mut writer = BufWriter::new(File::create(&output).map_err(|e| format!("create {output}: {e}"))?);
    let (mut read, mut written, mut bad) = (0u64, 0u64, 0u64);
    for line in reader.lines() {
        let line = line.map_err(|e| format!("read {input}: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        read += 1;
        let Ok(rec) = serde_json::from_str::<Value>(&line) else {
            bad += 1;
            continue;
        };
        let sample = match kind {
            "router" => router_sample(&rec),
            "sft" => sft_sample(&rec, &tier, with_reasoning),
            _ => return Err(EXPORT_USAGE.into()),
        };
        if let Some(s) = sample {
            writeln!(writer, "{s}").map_err(|e| format!("write {output}: {e}"))?;
            written += 1;
        }
    }
    writer.flush().map_err(|e| format!("write {output}: {e}"))?;
    Ok(format!("{kind}: read {read} records ({bad} unparseable), wrote {written} samples → {output}"))
}

// 只导出调过分类服务的记录：[system, user: 分类文本, assistant: 分类结果 JSON]
fn router_sample(rec: &Value) -> Option<Value> {
    let c = rec.get("classifier").filter(|c| c.is_object())?;
    let state = c.get("state")?.as_str()?;
    let a = c.get("answers")?;
    let label = json!({
        "difficulty_level": a.pointer("/difficulty_level/choice")?,
        "difficulty_score": a.pointer("/difficulty_score/score").cloned().unwrap_or(Value::Null),
        "prefer_cheap_model": a.pointer("/prefer_cheap_model/noul").cloned().unwrap_or(Value::Null),
    });
    Some(json!({
        "messages": [
            { "role": "system", "content": ROUTER_SYSTEM },
            { "role": "user", "content": state },
            { "role": "assistant", "content": label.to_string() },
        ]
    }))
}

// 只导出正常结束（stop / tool_calls）、没有报错、档位匹配的记录：{messages: 请求 + 输出, tools}
fn sft_sample(rec: &Value, tier: &str, with_reasoning: bool) -> Option<Value> {
    if tier != "all" && rec.get("tier")?.as_str()? != tier {
        return None;
    }
    let resp = rec.get("response")?;
    if !resp.get("error").is_none_or(Value::is_null) {
        return None;
    }
    if !matches!(resp.get("finish_reason")?.as_str()?, "stop" | "tool_calls") {
        return None;
    }
    let mut answer = resp.get("message")?.clone();
    if !with_reasoning && let Some(o) = answer.as_object_mut() {
        o.remove("reasoning_content");
    }
    let mut messages = rec.pointer("/request/messages")?.as_array()?.clone();
    messages.push(answer);
    let mut sample = json!({ "messages": messages });
    if let Some(tools) = rec.pointer("/request/tools").filter(|t| t.as_array().is_some_and(|a| !a.is_empty())) {
        sample["tools"] = tools.clone();
    }
    Some(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tier: &str, finish: &str) -> Value {
        json!({
            "tier": tier,
            "classifier": { "state": "hi", "answers": {
                "difficulty_level": { "choice": "cheap" },
                "difficulty_score": { "score": 0.3 },
                "prefer_cheap_model": { "noul": 0.9 }
            }},
            "request": { "messages": [{ "role": "user", "content": "hi" }], "tools": [] },
            "response": {
                "message": { "role": "assistant", "content": "hello", "reasoning_content": "think" },
                "finish_reason": finish, "error": null
            }
        })
    }

    #[test]
    fn router_sample_has_label() {
        let s = router_sample(&record("small", "stop")).unwrap();
        let label: Value = serde_json::from_str(s.pointer("/messages/2/content").unwrap().as_str().unwrap()).unwrap();
        assert_eq!(label["difficulty_level"], "cheap");
        assert_eq!(label["prefer_cheap_model"], 0.9);
    }

    #[test]
    fn sft_sample_filters_and_strips_reasoning() {
        assert!(sft_sample(&record("small", "stop"), "frontier", false).is_none());
        assert!(sft_sample(&record("frontier", "length"), "frontier", false).is_none());
        let s = sft_sample(&record("frontier", "stop"), "frontier", false).unwrap();
        assert_eq!(s["messages"][1]["content"], "hello");
        assert!(s["messages"][1].get("reasoning_content").is_none());
        assert!(s.get("tools").is_none());
        let s = sft_sample(&record("frontier", "stop"), "frontier", true).unwrap();
        assert_eq!(s["messages"][1]["reasoning_content"], "think");
    }
}

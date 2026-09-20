//! 训练数据日志（mode = "model-router" 时）：每个请求一行 JSONL，记录路由决策、
//! 发给上游的 Responses 请求（instructions / input / tools）和模型的完整输出（response 对象）；再用 `vg-model-router export` 导出成 LoRA 微调数据：
//!   - router：分类文本 → 分类结果，用来训练自己的路由模型
//!   - sft：   完整对话 → 模型输出（转成 Chat 格式），用来把 frontier 模型蒸馏到小模型
//!
//! （README: Model Router → Training data）

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};
use tracing::warn;

use crate::convert;
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
    pub fn new(log: Arc<TrainLog>, req_id: u64, requested_model: &str, decision: &Decision, req: &Value) -> Self {
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
                "instructions": req.get("instructions").cloned().unwrap_or(Value::Null),
                "input": req.get("input").cloned().unwrap_or(json!([])),
                "tools": req.get("tools").cloned().unwrap_or(json!([])),
            },
        });
        Self {
            log,
            record,
            started: Instant::now(),
        }
    }

    // resp：上游最终的 response 对象（未加 viv- 前缀）；error：上游报错 / 流中断时的错误信息
    pub fn finish(mut self, model: &str, resp: Option<&Value>, error: Option<&str>) {
        let get = |k: &str| resp.and_then(|r| r.get(k)).cloned().unwrap_or(Value::Null);
        let model = resp.and_then(|r| r.get("model")).and_then(Value::as_str).unwrap_or(model);
        self.record["response"] = json!({
            "model": model,
            "status": get("status"),
            "output": resp.and_then(|r| r.get("output")).cloned().unwrap_or(json!([])),
            "incomplete_details": get("incomplete_details"),
            "usage": get("usage"),
            "error": error.map(Value::from).unwrap_or_else(|| get("error")),
        });
        self.record["latency_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.log.append(&self.record);
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 导出：vg-model-router export <router|sft> --in <log.jsonl> --out <data.jsonl> [...]
// ---------------------------------------------------------------------------

pub const EXPORT_USAGE: &str = "\
usage: vg-model-router export router --in <log.jsonl> --out <data.jsonl>
       vg-model-router export sft    --in <log.jsonl> --out <data.jsonl> [--tier frontier|medium|small|all] [--with-reasoning]

  router  one sample per classified request: classifier input → classifier answer
  sft     one sample per completed request, in Chat format: messages + tools → model output
          (--tier defaults to frontier; reasoning is dropped unless --with-reasoning)";

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

// 只导出 status = completed、没有报错、有输出、档位匹配的记录，转成 Chat 格式：{messages: 请求 + 输出, tools}
fn sft_sample(rec: &Value, tier: &str, with_reasoning: bool) -> Option<Value> {
    if tier != "all" && rec.get("tier")?.as_str()? != tier {
        return None;
    }
    let resp = rec.get("response")?;
    if !resp.get("error").is_none_or(Value::is_null) || resp.get("status")?.as_str()? != "completed" {
        return None;
    }
    let output = resp.get("output")?.as_array().filter(|o| !o.is_empty())?;
    let req = rec.get("request")?;
    let mut items = match req.get("input")? {
        Value::String(s) => vec![json!({ "type": "message", "role": "user", "content": s })],
        Value::Array(a) => a.clone(),
        _ => return None,
    };
    items.extend(output.iter().cloned());
    let instructions = req.get("instructions").and_then(Value::as_str);
    let mut sample = json!({ "messages": convert::to_chat_messages(instructions, &items, with_reasoning) });
    let tools = convert::to_chat_tools(req.get("tools").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]));
    if !tools.is_empty() {
        sample["tools"] = Value::Array(tools);
    }
    Some(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tier: &str, status: &str) -> Value {
        json!({
            "tier": tier,
            "classifier": { "state": "hi", "answers": {
                "difficulty_level": { "choice": "cheap" },
                "difficulty_score": { "score": 0.3 },
                "prefer_cheap_model": { "noul": 0.9 }
            }},
            "request": { "instructions": "sys", "input": "hi", "tools": [] },
            "response": {
                "status": status, "error": null,
                "output": [
                    { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "think" }] },
                    { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "hello" }] }
                ]
            }
        })
    }

    #[test]
    fn router_sample_has_label() {
        let s = router_sample(&record("small", "completed")).unwrap();
        let label: Value = serde_json::from_str(s.pointer("/messages/2/content").unwrap().as_str().unwrap()).unwrap();
        assert_eq!(label["difficulty_level"], "cheap");
        assert_eq!(label["prefer_cheap_model"], 0.9);
    }

    #[test]
    fn sft_sample_filters_and_strips_reasoning() {
        assert!(sft_sample(&record("small", "completed"), "frontier", false).is_none());
        assert!(sft_sample(&record("frontier", "incomplete"), "frontier", false).is_none());
        let s = sft_sample(&record("frontier", "completed"), "frontier", false).unwrap();
        assert_eq!(s["messages"][0]["role"], "system");
        assert_eq!(s["messages"][1]["content"], "hi");
        assert_eq!(s["messages"][2]["content"], "hello");
        assert!(s["messages"][2].get("reasoning_content").is_none());
        assert!(s.get("tools").is_none());
        let s = sft_sample(&record("frontier", "completed"), "frontier", true).unwrap();
        assert_eq!(s["messages"][2]["reasoning_content"], "think");
    }
}

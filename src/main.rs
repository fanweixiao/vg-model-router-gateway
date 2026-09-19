//! vg-mirror（Vivgrid Mirror）：codex 用的本地 LLM API 代理：
//! 对外暴露 OpenAI Responses 接口 `/v1/responses`，转换成 Chat Completions 请求发给上游，
//! 并在日志里打印每次响应的 stop 值（finish_reason）和 usage。

mod config;
mod convert;
mod report;
mod router;
mod stream;
mod trainlog;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

// 默认上游和监听端口（README: Configuration）
const DEFAULT_UPSTREAM: &str = "https://api.vivgrid.com/v1/chat/completions";
const DEFAULT_LISTEN: &str = "127.0.0.1:33333";

// 需要改名后转发给上游的 header：(收到的名字, 发给上游的名字)
// （README: Headers forwarded upstream）
const HEADER_RENAMES: &[(&str, &str)] = &[("x-codex-turn-metadata", "x-viv-meta")];

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    upstream: String,
    models_url: String,
    // 请求没带 Authorization 时才用的备用 key（UPSTREAM_API_KEY）
    fallback_key: Option<String>,
    // 请求编号，日志里的 #1、#2…
    counter: Arc<AtomicU64>,
    // mode = "model-router" 时才有（README: Model Router）
    router: Option<Arc<router::Router>>,
    trainlog: Option<Arc<trainlog::TrainLog>>,
}

const USAGE: &str = "\
usage: vg-mirror [--config <path>]          run the proxy (config defaults to ./vg-mirror.toml, or $CONFIG)
       vg-mirror export <router|sft> ...    export the router log as LoRA fine-tuning JSONL";

// 命令行：`export` 子命令，或 --config 指定配置文件
fn parse_args() -> Option<std::path::PathBuf> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => std::env::var_os("CONFIG").map(Into::into),
        Some("export") => match trainlog::export(&args[1..]) {
            Ok(msg) => {
                println!("{msg}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        },
        Some("--config") if args.len() == 2 => Some(args[1].clone().into()),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

#[tokio::main]
async fn main() {
    let config_path = parse_args();
    // 日志级别，默认 vg_mirror=info（RUST_LOG）
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("vg_mirror=info")),
        )
        .with_target(false)
        .init();

    // 读取环境变量配置（README: Configuration）
    let listen = std::env::var("LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.into());
    let upstream = std::env::var("UPSTREAM_URL").unwrap_or_else(|_| DEFAULT_UPSTREAM.into());
    let models_url = match upstream.strip_suffix("/chat/completions") {
        Some(base) => format!("{base}/models"),
        None => upstream.clone(),
    };
    let fallback_key = std::env::var("UPSTREAM_API_KEY").ok().filter(|k| !k.is_empty());

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .expect("build http client");

    // 配置文件（README: Model Router → Configuration）
    let (cfg, loaded_from) = config::Config::load(config_path.as_deref()).unwrap_or_else(|e| {
        error!("{e}");
        std::process::exit(1);
    });
    if let Some(p) = &loaded_from {
        info!("config loaded from {} (mode = {:?})", p.display(), cfg.mode);
    }
    let (router, trainlog) = match (cfg.mode, cfg.model_router) {
        (config::Mode::ModelRouter, Some(rc)) => {
            let Some(key) = std::env::var("TYPESAFE_API_KEY").ok().filter(|k| !k.is_empty()) else {
                error!("mode = \"model-router\" requires the TYPESAFE_API_KEY environment variable");
                std::process::exit(1);
            };
            let trainlog = match rc.log_path.as_ref().filter(|p| !p.as_os_str().is_empty()) {
                Some(p) => match trainlog::TrainLog::open(p) {
                    Ok(l) => Some(Arc::new(l)),
                    Err(e) => {
                        error!("open training log {}: {e}", p.display());
                        std::process::exit(1);
                    }
                },
                None => None,
            };
            info!(
                "model router: small={}, medium={}, frontier={}, context={}, log={}",
                rc.small,
                rc.medium,
                rc.frontier,
                if rc.include_context { "on" } else { "off" },
                trainlog.as_ref().map_or("off".into(), |l| l.path().display().to_string()),
            );
            (Some(Arc::new(router::Router::new(rc, client.clone(), key))), trainlog)
        }
        _ => (None, None),
    };

    let state = AppState {
        client,
        upstream: upstream.clone(),
        models_url,
        fallback_key,
        counter: Arc::new(AtomicU64::new(0)),
        router,
        trainlog,
    };

    // 路由（README: Endpoints）
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route("/v1/models", get(models))
        .route("/health", get(|| async { "ok" }))
        .fallback(|method: axum::http::Method, uri: axum::http::Uri| async move {
            warn!("unhandled route: {method} {uri}");
            error_json(StatusCode::NOT_FOUND, &format!("no route for {method} {uri}"))
        })
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind listen address");
    info!("vg-mirror listening on http://{listen}  →  upstream {upstream}");
    axum::serve(listener, app).await.expect("server error");
}

fn error_json(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": { "message": msg, "type": "proxy_error" } }))).into_response()
}

// 透传 Bearer Token：优先用 codex 发来的 Authorization，没有才用 UPSTREAM_API_KEY
fn auth_header(st: &AppState, headers: &HeaderMap) -> Option<HeaderValue> {
    if let Some(v) = headers.get(header::AUTHORIZATION) {
        return Some(v.clone());
    }
    let key = st.fallback_key.as_ref()?;
    HeaderValue::from_str(&format!("Bearer {key}")).ok()
}

// 发请求给上游：只转发 Authorization、User-Agent 和 HEADER_RENAMES 里的 header
// （README: Headers forwarded upstream）
async fn send_upstream(
    st: &AppState,
    headers: &HeaderMap,
    auth: &HeaderValue,
    is_stream: bool,
    body: &Value,
    req_id: u64,
) -> reqwest::Result<reqwest::Response> {
    let accept = if is_stream { "text/event-stream" } else { "application/json" };
    let mut rb = st
        .client
        .post(&st.upstream)
        .header(header::AUTHORIZATION, auth.clone())
        .header(header::ACCEPT, accept);
    if let Some(ua) = headers.get(header::USER_AGENT) {
        rb = rb.header(header::USER_AGENT, ua.clone());
    }
    for (from, to) in HEADER_RENAMES {
        if let Some(v) = headers.get(*from) {
            debug!("#{req_id} forwarding header {from} → {to}");
            rb = rb.header(*to, v.clone());
        }
    }
    rb.json(body).send().await
}

fn upstream_send_failed(req_id: u64, e: reqwest::Error) -> Response {
    error!("#{req_id} ✖ upstream request failed: {e}");
    error_json(StatusCode::BAD_GATEWAY, &format!("upstream request failed: {e}"))
}

// 上游返回 HTTP 错误：打日志，并把原状态码和 body 原样返回给 codex
// （README: finish_reason 映射表里的 "upstream HTTP error"）
fn upstream_error(req_id: u64, code: StatusCode, text: String, started: Instant) -> Response {
    error!(
        "#{req_id} ✖ upstream HTTP {code} ({:.2}s)\n    stop   ▸ n/a (no completion)\n    body   ▸ {text}",
        started.elapsed().as_secs_f64()
    );
    (code, [(header::CONTENT_TYPE, "application/json")], text).into_response()
}

// 排查用：上游报错时，打印实际发出去的请求（除 messages 外的字段 + 转发的 header），
// 并把完整请求体存到临时文件，方便用 curl 原样重放、逐项排除
fn dump_sent_request(st: &AppState, headers: &HeaderMap, body: &Value, req_id: u64) {
    let mut fields = body.clone();
    let n_messages = fields.as_object_mut().and_then(|o| o.remove("messages")).and_then(|m| m.as_array().map(Vec::len));
    let mut sent_headers = Vec::new();
    if let Some(ua) = headers.get(header::USER_AGENT) {
        sent_headers.push(format!("user-agent: {}", String::from_utf8_lossy(ua.as_bytes())));
    }
    for (from, to) in HEADER_RENAMES {
        if let Some(v) = headers.get(*from) {
            sent_headers.push(format!("{to}: {}", String::from_utf8_lossy(v.as_bytes())));
        }
    }
    let path = std::env::temp_dir().join(format!("vg-mirror-req-{req_id}.json"));
    let saved = match std::fs::write(&path, serde_json::to_vec_pretty(body).unwrap_or_default()) {
        Ok(()) => format!(
            "{}\n             replay: curl -sS {} -H \"Authorization: Bearer $VIVGRID_API_KEY\" -H 'content-type: application/json' -d @{}",
            path.display(),
            st.upstream,
            path.display()
        ),
        Err(e) => format!("<failed to save: {e}>"),
    };
    error!(
        "#{req_id} ✖ request that was sent upstream:\n    body   ▸ {fields}  (+ {} messages)\n    header ▸ {}\n    saved  ▸ {saved}",
        n_messages.unwrap_or(0),
        if sent_headers.is_empty() { "-".to_string() } else { sent_headers.join("\n             ") },
    );
}

// POST /v1/responses：Responses → Chat Completions → 上游 → 转回 Responses
async fn responses(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let req_id = st.counter.fetch_add(1, Ordering::Relaxed) + 1;
    let started = Instant::now();

    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            error!(req = req_id, "invalid JSON body: {e}");
            return error_json(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {e}"));
        }
    };
    let is_stream = req.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut model = req.get("model").and_then(Value::as_str).unwrap_or("").to_string();

    // 请求格式转换（README: Conversion details → Request）
    let chat = convert::to_chat_request(&req, is_stream);
    let mut body = chat.body;

    // 请求日志：概要行 + tools 信息 + input item 类型统计 + 上游 body 里的工具痕迹
    // （README: Reading the logs → Request line）
    let n_messages = body["messages"].as_array().map_or(0, Vec::len);
    let n_tools = body.get("tools").and_then(Value::as_array).map_or(0, Vec::len);
    let n_input = req.get("input").and_then(Value::as_array).map_or(1, Vec::len);
    let mode = if is_stream { "stream" } else { "non-stream" };
    let tools = report::tools_summary(&req, &chat.dropped_tools);
    let items = report::input_items_summary(&req);
    let traces = report::upstream_tool_traces(&body);
    let line = format!(
        "#{req_id} ▶ request  [{mode}, model={model}, input_items={n_input} → messages={n_messages}, tools={n_tools}]\n{tools}\n{items}\n{traces}"
    );
    if tools.contains('⚠') || items.contains('⚠') {
        warn!("{line}");
    } else {
        info!("{line}");
    }

    let Some(auth) = auth_header(&st, &headers) else {
        warn!("#{req_id} no Authorization header (and UPSTREAM_API_KEY unset)");
        return error_json(StatusCode::UNAUTHORIZED, "missing Authorization: Bearer <token>");
    };

    // model-router：分类后改写 model，返回给 codex 的也是实际的 model_id（README: Model Router）
    let mut pending = None;
    if let Some(router) = &st.router {
        let messages = body["messages"].as_array().map(Vec::as_slice).unwrap_or_default();
        let decision = router.route(req_id, &req, &headers, messages).await;
        let line = format!("#{req_id} ⇢ route  [requested model={model}]\n{}", decision.summary());
        if matches!(decision.source, router::Source::Fallback(_)) {
            warn!("{line}");
        } else {
            info!("{line}");
        }
        body["model"] = json!(decision.model);
        if let Some(log) = &st.trainlog {
            pending = Some(trainlog::Pending::new(log.clone(), req_id, &model, &decision, &body));
        }
        model = decision.model;
    }
    debug!("#{req_id} upstream request body: {body}");

    let upstream = match send_upstream(&st, &headers, &auth, is_stream, &body, req_id).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(p) = pending {
                p.finish(&model, Value::Null, None, None, Some(&format!("upstream request failed: {e}")));
            }
            return upstream_send_failed(req_id, e);
        }
    };

    // 上游报错（非 2xx，或 stream 请求却返回了 JSON）：原样返回
    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !status.is_success() || (is_stream && content_type.starts_with("application/json")) {
        let text = upstream.text().await.unwrap_or_default();
        let code = if status.is_success() { StatusCode::BAD_GATEWAY } else { status };
        if let Some(p) = pending {
            p.finish(&model, Value::Null, None, None, Some(&format!("upstream HTTP {code}: {text}")));
        }
        let resp = upstream_error(req_id, code, text, started);
        dump_sent_request(&st, &headers, &body, req_id);
        return resp;
    }

    // stream=true：后台任务把 Chat SSE 翻译成 Responses SSE（见 stream.rs）
    if is_stream {
        let (tx, rx) = mpsc::channel(64);
        let translator = stream::Translator::new(tx, req_id, started, model, chat.custom_tools, pending);
        tokio::spawn(translator.run(upstream));
        return Sse::new(ReceiverStream::new(rx)).into_response();
    }

    // stream=false：整体转换后返回（README: Conversion details → Response）
    let chat_resp: Value = match upstream.json().await {
        Ok(v) => v,
        Err(e) => {
            if let Some(p) = pending {
                p.finish(&model, Value::Null, None, None, Some(&format!("invalid upstream JSON: {e}")));
            }
            error!("#{req_id} ✖ failed to read upstream JSON: {e}");
            return error_json(StatusCode::BAD_GATEWAY, &format!("invalid upstream JSON: {e}"));
        }
    };
    debug!("#{req_id} upstream response body: {chat_resp}");

    let resp = convert::chat_to_response(&chat_resp, &convert::new_id("resp"), &model, &chat.custom_tools);
    let choice = chat_resp.pointer("/choices/0").cloned().unwrap_or(Value::Null);
    if let Some(p) = pending {
        p.finish(
            resp["model"].as_str().unwrap_or(&model),
            choice.get("message").cloned().unwrap_or(Value::Null),
            choice.get("finish_reason").and_then(Value::as_str),
            chat_resp.get("usage"),
            None,
        );
    }
    let mut notes = Vec::new();
    if let Some(n) = chat_resp.get("choices").and_then(Value::as_array).map(Vec::len)
        && n != 1
    {
        notes.push(format!("upstream returned {n} choices (only choice 0 used)"));
    }

    // 响应日志：stop 值 + usage（README: Reading the logs → Response line）
    report::log(&report::Report {
        req_id,
        stream: false,
        elapsed: started.elapsed(),
        model: resp["model"].as_str().unwrap_or(&model),
        finish_reason: choice.get("finish_reason").and_then(Value::as_str),
        extra_stop: &report::extra_stop_fields(&choice),
        status: resp["status"].as_str().unwrap_or("?"),
        usage: report::Usage::from_chat(chat_resp.get("usage")),
        notes: &notes,
    });

    Json(resp).into_response()
}

// GET /v1/models：直接透传给上游 /v1/models（README: Endpoints）
async fn models(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let mut rb = st.client.get(&st.models_url);
    if let Some(auth) = auth_header(&st, &headers) {
        rb = rb.header(header::AUTHORIZATION, auth);
    }
    if let Some(ua) = headers.get(header::USER_AGENT) {
        rb = rb.header(header::USER_AGENT, ua.clone());
    }
    match rb.send().await {
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            (status, [(header::CONTENT_TYPE, "application/json")], text).into_response()
        }
        Err(e) => error_json(StatusCode::BAD_GATEWAY, &format!("upstream request failed: {e}")),
    }
}

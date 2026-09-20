//! vg-model-router（Vivgrid Model Router）：codex 用的本地 LLM API 代理：
//! 对外暴露 OpenAI Responses 接口 `/v1/responses`，原样透传给上游的 `/v1/responses`
//! （model-router 模式下只改写 model），返回给 codex 的 model-id 加上 `viv-` 前缀，
//! 并在日志里打印每次响应的 status 和 usage。

mod config;
mod convert;
mod report;
mod router;
mod stream;
mod trainlog;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

// 默认上游和监听端口（README: Configuration）
const DEFAULT_UPSTREAM: &str = "https://api.vivgrid.com/v1/responses";
const DEFAULT_LISTEN: &str = "127.0.0.1:33333";

// 需要改名后转发给上游的 header：(收到的名字, 发给上游的名字)
// （README: Headers forwarded upstream）
const HEADER_RENAMES: &[(&str, &str)] = &[
    ("session_id", "x-viv-session_id"),
    ("x-codex-turn-metadata", "x-viv-meta"),
];

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    upstream: String,
    models_url: String,
    // 请求没带 Authorization 时才用的备用 key（VIVGRID_API_KEY），上游和分类服务都用它
    fallback_key: Option<String>,
    // 请求编号，日志里的 #1、#2…
    counter: Arc<AtomicU64>,
    // mode = "model-router" 时才有（README: Model Router）
    router: Option<Arc<router::Router>>,
    trainlog: Option<Arc<trainlog::TrainLog>>,
}

const USAGE: &str = "\
usage: vg-model-router [--config <path>]          run the proxy (config defaults to ./vg-model-router.toml, or $CONFIG)
       vg-model-router export <router|sft> ...    export the router log as LoRA fine-tuning JSONL";

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

// 从当前目录的 .env.local、.env 读取环境变量（README: Configuration → .env files）。
// 优先级：真实环境变量 > .env.local > .env —— 两个文件都不覆盖已有的变量，所以先读 .env.local。
// 返回读到的文件名，文件格式错误时直接退出
fn load_env_files() -> Vec<&'static str> {
    let mut loaded = Vec::new();
    for name in [".env.local", ".env"] {
        match dotenvy::from_filename(name) {
            Ok(_) => loaded.push(name),
            Err(e) if e.not_found() => {}
            Err(e) => {
                eprintln!("failed to load {name}: {e}");
                std::process::exit(1);
            }
        }
    }
    loaded
}

// 不用 #[tokio::main]：要在启动 tokio 的工作线程之前改环境变量（set_var 在多线程下不安全）
fn main() {
    let env_files = load_env_files();
    tokio::runtime::Runtime::new().expect("build tokio runtime").block_on(run(env_files));
}

async fn run(env_files: Vec<&'static str>) {
    let config_path = parse_args();
    // 日志级别，默认 vg_model_router=info（RUST_LOG）
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("vg_model_router=info")),
        )
        .event_format(report::ReqColor::new())
        .init();
    if !env_files.is_empty() {
        info!("environment loaded from {}", env_files.join(", "));
    }

    // 读取环境变量配置（README: Configuration）
    let listen = std::env::var("LISTEN").unwrap_or_else(|_| DEFAULT_LISTEN.into());
    let upstream = std::env::var("https://api.vivgrid.com/v1/responses").unwrap_or_else(|_| DEFAULT_UPSTREAM.into());
    let models_url = match upstream.strip_suffix("/responses") {
        Some(base) => format!("{base}/models"),
        None => upstream.clone(),
    };
    let fallback_key = std::env::var("VIVGRID_API_KEY").ok().filter(|k| !k.is_empty());

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
            (Some(Arc::new(router::Router::new(rc, client.clone()))), trainlog)
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
        .route(
            "/health",
            get(|uri: Uri| async move {
                debug!("▶ GET {}", uri.path());
                "ok"
            }),
        )
        .fallback(|method: axum::http::Method, uri: axum::http::Uri| async move {
            warn!("unhandled route: {method} {uri}");
            error_json(StatusCode::NOT_FOUND, &format!("no route for {method} {uri}"))
        })
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind listen address");
    info!("vg-model-router listening on http://{listen}  →  upstream {upstream}");
    axum::serve(listener, app).await.expect("server error");
}

fn error_json(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": { "message": msg, "type": "proxy_error" } }))).into_response()
}

// 透传 Bearer Token：优先用 codex 发来的 Authorization，没有才用 VIVGRID_API_KEY。
// 上游 /v1/responses 和分类服务（vivgrid 的 systemone）用的是同一个 Authorization
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
    body: Bytes,
    req_id: u64,
) -> reqwest::Result<reqwest::Response> {
    let accept = if is_stream { "text/event-stream" } else { "application/json" };
    let mut rb = st
        .client
        .post(&st.upstream)
        .header(header::AUTHORIZATION, auth.clone())
        .header(header::ACCEPT, accept)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(ua) = headers.get(header::USER_AGENT) {
        rb = rb.header(header::USER_AGENT, ua.clone());
    }
    for (from, to) in HEADER_RENAMES {
        if let Some(v) = headers.get(*from) {
            debug!("#{req_id} forwarding header {from} → {to}");
            rb = rb.header(*to, v.clone());
        }
    }
    rb.body(body).send().await
}

fn upstream_send_failed(req_id: u64, e: reqwest::Error) -> Response {
    error!("#{req_id} ✖ upstream request failed: {e}");
    error_json(StatusCode::BAD_GATEWAY, &format!("upstream request failed: {e}"))
}

// 上游返回 HTTP 错误：打日志，并把原状态码、content-type 和 body 原样返回给 codex
// （README: Reading the logs）
fn upstream_error(req_id: u64, code: StatusCode, content_type: HeaderValue, text: String, started: Instant) -> Response {
    error!(
        "#{req_id} ✖ upstream HTTP {code} ({:.2}s)\n    stop   ▸ n/a (no response)\n    body   ▸ {text}",
        started.elapsed().as_secs_f64()
    );
    (code, [(header::CONTENT_TYPE, content_type)], text).into_response()
}

// 排查用：上游报错时，打印实际发出去的请求（除 instructions / input / tools 外的字段 + 转发的 header），
// 并把完整请求体存到临时文件，方便用 curl 原样重放、逐项排除
fn dump_sent_request(st: &AppState, headers: &HeaderMap, body: &[u8], req_id: u64) {
    let mut fields: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let mut omitted = Vec::new();
    if let Some(o) = fields.as_object_mut() {
        for k in ["instructions", "input", "tools"] {
            if o.remove(k).is_some() {
                omitted.push(k);
            }
        }
    }
    let mut sent_headers = Vec::new();
    if let Some(ua) = headers.get(header::USER_AGENT) {
        sent_headers.push(format!("user-agent: {}", String::from_utf8_lossy(ua.as_bytes())));
    }
    for (from, to) in HEADER_RENAMES {
        if let Some(v) = headers.get(*from) {
            sent_headers.push(format!("{to}: {}", String::from_utf8_lossy(v.as_bytes())));
        }
    }
    let path = std::env::temp_dir().join(format!("vg-model-router-req-{req_id}.json"));
    let saved = match std::fs::write(&path, body) {
        Ok(()) => format!(
            "{}\n             replay: curl -sS {} -H \"Authorization: Bearer $VIVGRID_API_KEY\" -H 'content-type: application/json' -d @{}",
            path.display(),
            st.upstream,
            path.display()
        ),
        Err(e) => format!("<failed to save: {e}>"),
    };
    error!(
        "#{req_id} ✖ request that was sent upstream:\n    body   ▸ {fields}  (+ {})\n    header ▸ {}\n    saved  ▸ {saved}",
        if omitted.is_empty() { "-".to_string() } else { omitted.join(", ") },
        if sent_headers.is_empty() { "-".to_string() } else { sent_headers.join("\n             ") },
    );
}

// POST /v1/responses：原样透传给上游 /v1/responses，返回的 model 加 viv- 前缀
async fn responses(State(st): State<AppState>, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    let req_id = st.counter.fetch_add(1, Ordering::Relaxed) + 1;
    let started = Instant::now();

    // 必须是 JSON 对象：model-router 要改写 body["model"]
    let req: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(v) if v.is_object() => v,
        Ok(_) => {
            error!("#{req_id} ✖ request body is not a JSON object");
            return error_json(StatusCode::BAD_REQUEST, "request body must be a JSON object");
        }
        Err(e) => {
            error!("#{req_id} ✖ invalid JSON body: {e}");
            return error_json(StatusCode::BAD_REQUEST, &format!("invalid JSON body: {e}"));
        }
    };
    let is_stream = req.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let mut model = req.get("model").and_then(Value::as_str).unwrap_or("").to_string();

    // 请求日志：概要行 + tools 信息 + input item 类型统计（README: Reading the logs → Request line）
    let n_tools = req.get("tools").and_then(Value::as_array).map_or(0, Vec::len);
    let n_input = match req.get("input") {
        Some(Value::Array(a)) => a.len(),
        Some(Value::String(_)) => 1,
        _ => 0,
    };
    let mode = if is_stream { "stream" } else { "non-stream" };
    let line = format!(
        "#{req_id} ▶ POST {}  [{mode}, model={model}, input_items={n_input}, tools={n_tools}]",
        uri.path()
    );
    // 用户提问每个请求都打（用来区分 codex 的不同用途请求）；
    // tools 和 input items 的明细只在有未配对的 tool call（⚠）时打印
    let line = format!("{line}\n{}", report::user_message_summary(&req));
    let tools = report::tools_summary(&req);
    if tools.contains('⚠') {
        warn!("{line}\n{tools}\n{}", report::input_items_summary(&req));
    } else {
        info!("{line}");
    }

    let Some(auth) = auth_header(&st, &headers) else {
        warn!("#{req_id} no Authorization header (and VIVGRID_API_KEY unset)");
        return error_json(StatusCode::UNAUTHORIZED, "missing Authorization: Bearer <token>");
    };

    // model-router：分类后只改写 body 里的 model，其余字段原样透传（README: Model Router）
    let mut body = body;
    let mut pending = None;
    if let Some(router) = &st.router {
        let decision = router.route(req_id, &req, &headers, &auth).await;
        let line = format!("#{req_id} ⇢ route  [requested model={model}]\n{}", decision.summary());
        if matches!(decision.source, router::Source::Fallback(_)) {
            warn!("{line}");
        } else {
            info!("{line}");
        }
        let mut routed = req.clone();
        routed["model"] = json!(decision.model);
        body = Bytes::from(serde_json::to_vec(&routed).unwrap_or_default());
        if let Some(log) = &st.trainlog {
            pending = Some(trainlog::Pending::new(log.clone(), req_id, &model, &decision, &req));
        }
        model = decision.model;
    }
    debug!("#{req_id} upstream request body: {}", String::from_utf8_lossy(&body));

    let upstream = match send_upstream(&st, &headers, &auth, is_stream, body.clone(), req_id).await {
        Ok(r) => r,
        Err(e) => {
            if let Some(p) = pending {
                p.finish(&model, None, Some(&format!("upstream request failed: {e}")));
            }
            return upstream_send_failed(req_id, e);
        }
    };

    // 上游报错（非 2xx）：原样返回
    let status = upstream.status();
    if !status.is_success() {
        let content_type = upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or(HeaderValue::from_static("application/json"));
        let text = upstream.text().await.unwrap_or_default();
        if let Some(p) = pending {
            p.finish(&model, None, Some(&format!("upstream HTTP {status}: {text}")));
        }
        let resp = upstream_error(req_id, status, content_type, text, started);
        dump_sent_request(&st, &headers, &body, req_id);
        return resp;
    }

    // stream=true 且上游返回 SSE：后台任务逐行转发，只改写 model（见 stream.rs）
    let is_sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    if is_stream && is_sse {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(stream::Relay::new(tx, req_id, started, model, pending).run(upstream));
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(ReceiverStream::new(rx)))
            .unwrap_or_else(|e| error_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()));
    }

    // stream=false：整个 response 对象加前缀后返回
    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            if let Some(p) = pending {
                p.finish(&model, None, Some(&format!("upstream read failed: {e}")));
            }
            error!("#{req_id} ✖ failed to read upstream body: {e}");
            return error_json(StatusCode::BAD_GATEWAY, &format!("upstream read failed: {e}"));
        }
    };
    debug!("#{req_id} upstream response body: {}", String::from_utf8_lossy(&bytes));
    let mut resp: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            if let Some(p) = pending {
                p.finish(&model, None, Some(&format!("invalid upstream JSON: {e}")));
            }
            error!("#{req_id} ✖ invalid upstream JSON: {e}");
            return error_json(StatusCode::BAD_GATEWAY, &format!("invalid upstream JSON: {e}"));
        }
    };
    if let Some(p) = pending {
        p.finish(&model, Some(&resp), None);
    }
    let mut notes = Vec::new();
    if is_stream {
        notes.push("stream=true but upstream returned a non-SSE response".to_string());
    }
    // 响应日志：status + usage（README: Reading the logs → Response line）
    report::log(&report::Report::new(req_id, false, started.elapsed(), &model, Some(&resp), notes));

    stream::add_model_prefix(&mut resp);
    (status, Json(resp)).into_response()
}

// GET /v1/models：直接透传给上游 /v1/models（README: Endpoints）
async fn models(State(st): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let req_id = st.counter.fetch_add(1, Ordering::Relaxed) + 1;
    let started = Instant::now();
    let path = uri.path().to_string();
    info!("#{req_id} ▶ GET {path}  [→ {}]", st.models_url);

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
            let line = format!("#{req_id} ◀ GET {path}  [{}, {:.2}s]", status.as_u16(), started.elapsed().as_secs_f64());
            if status.is_success() {
                info!("{line}");
            } else {
                warn!("{line}");
            }
            (status, [(header::CONTENT_TYPE, "application/json")], text).into_response()
        }
        Err(e) => {
            warn!("#{req_id} ◀ GET {path}  [upstream request failed: {e}]");
            error_json(StatusCode::BAD_GATEWAY, &format!("upstream request failed: {e}"))
        }
    }
}

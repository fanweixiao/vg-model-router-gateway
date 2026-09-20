# AGENTS.md

给参与这个仓库的人和 AI coding agent 看的协作说明。约定和边界写在这里，**用法**写在 [README.md](README.md)，**设计与取舍**写在 [router_design.md](router_design.md)。

## 项目是什么

`vg-model-router`（包名 = 二进制名）是给 [Codex](https://github.com/openai/codex) 用的**本地 Responses API 代理**：

```
Codex ──POST /v1/responses──▶ vg-model-router ──▶ https://api.vivgrid.com/v1/responses
```

两件事：

1. **透传**：请求原样转发（流式 / 非流式都支持），只把返回给 Codex 的 model-id 加上 `viv-` 前缀。
2. **Model Router**（`mode = "model-router"`）：先调分类服务判断本轮难度，再把 `model` 改写成 small / medium / frontier 三档之一，同时把每个请求写成一行 JSONL 训练数据。

**不做**的事：负载均衡、多上游切换、在代理里训练模型。改动前先确认新需求没有落在这条线外面。

## 上手

```bash
cargo build              # Rust 1.88+ / edition 2024（当前开发机 1.97）
cargo test               # 9 个单元测试，秒级
cargo run                # 默认监听 127.0.0.1:33333，mode = none
make release             # aarch64-apple-darwin 的 strip 过的 release 二进制
```

本地跑起来先复制两个模板（都已 gitignore）：

```bash
cp .env.example .env                                  # LISTEN / https://api.vivgrid.com/v1/responses / VIVGRID_API_KEY ...
cp vg-model-router.example.toml vg-model-router.toml  # mode 和 [model-router]
```

调试时开 debug 日志，会打印发给上游的完整 body 和每个 SSE 事件类型：

```bash
RUST_LOG=vg_model_router=debug cargo run
```

## 代码地图

| 文件 | 职责 |
|---|---|
| [src/main.rs](src/main.rs) | HTTP 服务、路由表、header 转发、非流式路径、`.env` 加载、命令行参数 |
| [src/stream.rs](src/stream.rs) | SSE 中继：逐行转发，给带 model 的事件加 `viv-` 前缀 |
| [src/report.rs](src/report.rs) | 终端日志格式化（请求/工具摘要、用户提问、status、usage）+ 日志样式 `ReqColor`（时间/级别/按请求编号上色） |
| [src/router.rs](src/router.rs) | Model Router：分类文本、调分类服务、选档、按轮缓存 |
| [src/config.rs](src/config.rs) | TOML 配置、默认值、校验 |
| [src/trainlog.rs](src/trainlog.rs) | 训练数据 JSONL 与 `export` 子命令 |
| [src/convert.rs](src/convert.rs) | Responses → Chat 格式转换，**只**给 `export sft` 用 |

Model Router 在 `main.rs` 的 `responses()` 里只有**一个接入点**：`auth_header` 检查之后、`send_upstream` 之前。不要把路由逻辑散到别处。

## 必须守住的不变量

改代码时优先保证这几条，它们是这个代理能被 Codex 信任的原因：

1. **除了 `model`，什么都不改。** 没开路由时请求体按原始字节转发；开了路由也只替换 `body["model"]` 后重新序列化一次。不要顺手规整字段、补默认值或删未知字段。
2. **`viv-` 前缀只加在返回给 Codex 的 model 上。** 上游收到的和日志里记录的都是真实 model-id。前缀常量在 `stream.rs` 的 `MODEL_PREFIX`。
3. **SSE 只重写带 model 的 `data:` 行。** `event:` 行、delta 事件、`[DONE]` 必须逐字节原样转发。
4. **一轮只分类一次。** `input` 最后一项是 user message 才算新的一轮；同一轮内（按 `prompt_cache_key`，退化到 `session_id` / `conversation_id`）的后续请求沿用已有决策，否则会打断 prompt cache。
5. **安静的成功路径，吵闹的失败路径。** 正常请求每个方向只打一条；`tools` / `items` / `stop` / `output` / `usage` 明细只在 WARN（未配对 tool call、`status != completed`、有 note）时打印。加日志前先想清楚它属于哪一边。
   常打的只有两样：请求行下面的 `ask`（用户这一轮问的那句话，取自 `router::last_user_text`，和分类服务看到的是同一条；用来区分 codex 那些概要行长得一模一样、用途却不同的请求），以及下面这条分类服务的例外。
   唯一的例外是分类服务：**每一次调用 `jev-latest` 都必须把响应打出来**，成功 INFO、失败 WARN，超长重试的那一次也要打。这是路由决策唯一的外部依据，日志是线上唯一能复盘它的地方。这行日志写在 `router.rs::classify_once` 里——挨着发请求的地方，而不是调用方，这样以后新增调用点不会漏打。
6. **分类失败必须降级，不能报错。** 超时、报错、key 不对，一律走 `frontier` 并打 WARN，且**不写缓存**。代理挂掉分类服务不能拖垮 Codex。
7. **`mode = "none"`（也就是没有配置文件时）行为必须和引入 Model Router 之前完全一致。**
8. **分类服务用同一个 `Authorization`。** `route()` 拿的是 `auth_header()` 已经算好的那个 header，和上游 `/v1/responses` 完全一致，不要在 Router 里单独持有 key 或读环境变量。
9. **header 白名单。** 只有 `Authorization`、`User-Agent` 和 `HEADER_RENAMES` 里的 pair 会转发给上游，其余一律丢弃。加 header 就改 `main.rs` 的 `HEADER_RENAMES`，别在别处硬塞。

## 常见改动落点

| 想做的事 | 改哪里 |
|---|---|
| 调路由规则 | `Router::decide` + `decide_follows_design_rules` 测试 |
| 改分类问题 | `router::questions()`（会让新旧日志不可比，改前先想清楚） |
| 加一档模型 | `Tier`、`RouterConfig`、`model_for`、`decide`、`export --tier` |
| 多识别一种 Codex 注入的消息 | `router::is_injected` |
| 换分类服务 | `classify_once` 的请求构造与解析；`Verdict` 结构保持不变 |
| 转发更多 header | `main.rs` 的 `HEADER_RENAMES` |
| 改日志排版 | `report.rs`（响应行 + 异常时的明细、`ReqColor` 的时间/级别/配色）、`main.rs` 的 `responses()` / `models()`（`▶ METHOD path` 请求行）；同步更新 README 的 “Reading the logs” 示例 |

## 代码风格

- 模块头用 `//!` 写一句话职责，行内注释用**中文**，涉及对外行为的地方标出对应文档，例如 `// （README: Headers forwarded upstream）`。保持这个密度：解释「为什么」，不复述代码。
- 单元测试内联写在文件底部的 `#[cfg(test)] mod tests`，测试名是一句陈述句（`state_is_truncated_from_the_front`、`prefix_is_added_once`）。
- **不要跑 `cargo fmt`。** 仓库没有 `rustfmt.toml`，现有代码的行宽超过 rustfmt 默认的 100，全量格式化会把整棵树重排、淹掉真实 diff。要引入统一格式就单独开一个 PR，先加 `rustfmt.toml` 再一次性格式化。
- 新代码跟着邻近代码的风格写：短函数、`match` 早返回、错误用 `Result<_, String>` 往上抛并带上文件名等上下文。

## 文档同步义务

行为一变，文档就得跟着走，否则下一个人（和下一个 agent）会照着旧文档改坏东西：

- **README.md** — 面向用户，**英文**。配置项、endpoint、日志格式、CLI 参数变了必须更新对应表格/示例。
- **router_design.md** — 面向维护者，**中文**。路由规则、分类文本、JSONL schema、导出格式变了必须更新（第 5、7、8、9 节），并同步第 11 节代码地图。
- **AGENTS.md**（本文）— 不变量、落点表、流程变了再改。

## 密钥与数据安全

- **API key 绝不进配置文件、绝不进 git。** 正常路径下代理根本不持有 key：上游和分类服务都用 Codex 发来的 `Authorization`。`VIVGRID_API_KEY` 只是请求没带 `Authorization` 时的兜底，只从环境变量或 `.env` / `.env.local` 读；TOML 里永远不读 key。
- `.env`、`.env.local`、`vg-model-router.toml`、`*.jsonl` 都在 `.gitignore` 里，别用 `git add -f` 绕过。
- **训练日志 `vg-model-router.jsonl` 含完整对话内容**（instructions、input、工具输出、模型回复），等同于源代码和内部信息。不要贴进 issue、不要上传到第三方、不要在共享环境里留着。Codex 每次请求重发全部历史，这个文件涨得很快。
- 贴日志求助前先确认没带 `Authorization`、路径和业务内容。

## 测试

```bash
cargo test    # build_state / decide / 导出格式
```

改了路由、分类文本、导出格式，要**同时加/改单元测试**。

端到端手工测试用一个 mock Responses 服务当上游（流式返回 `response.created` … `response.completed`），分类服务用真的：

```bash
NO_PROXY=127.0.0.1 https://api.vivgrid.com/v1/responses=http://127.0.0.1:<mock>/v1/responses VIVGRID_API_KEY=... \
  ./target/debug/vg-model-router --config vg-model-router.toml
```

要覆盖的场景：简单问题 → small / 复杂问题 → frontier；同一轮后续请求日志显示 `same turn as #N`；故意用错 key → WARN 且走 frontier；不带配置文件 → 没有 route 行、`model` 原样透传；流式和非流式都能写出完整的 `response.output` 且返回给 Codex 的 model 带 `viv-` 前缀。

> 两个坑：Claude Code / agent 沙箱会拦截连向本地 mock 的回环连接，需要在沙箱外跑；`http_proxy` 环境变量会让 reqwest 走代理，所以要带 `NO_PROXY=127.0.0.1`。

## 提交与协作

- 主分支是 `main`。改动开分支，PR 合并，别直接推 `main`。
- 提交信息写清楚**改了什么行为**，不是改了哪些文件；涉及不变量的改动在 PR 描述里说明为什么安全。
- 提 PR 前的自检：`cargo build` 通过、`cargo test` 通过、`git status` 里没有 `.env` / `.toml` / `.jsonl`、README 和 router_design.md 已同步。
- AI agent 额外注意：不要为了「顺手清理」而全量格式化、重排 import、或把中文注释改成英文；这类改动会淹没真实 diff，要做就单独开 PR。

## 给 agent 的会话提示

- 先读 [router_design.md](router_design.md) 的 TL;DR 和第 2 节（请求处理流程），再动 `router.rs` / `main.rs`。
- 动 `trainlog.rs` 的 schema 前，先看第 8 节；已有的 JSONL 是资产，schema 改动要么向后兼容，要么在导出时兼容旧行。
- 需要真实请求验证时，问一下人类要不要跑真的上游和分类服务（会花钱、会产生日志），不要自己拿真实 key 发请求。

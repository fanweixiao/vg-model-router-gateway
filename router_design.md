# Model Router 设计文档

> 状态：已实现 · 相关代码：`src/config.rs`、`src/router.rs`、`src/trainlog.rs`、`src/convert.rs`（仅导出用），以及 `src/main.rs`、`src/stream.rs` 中的接入点

## TL;DR

- vg-model-router 是给 Codex 用的本地代理，把 Responses API 请求（`/v1/responses`，流式和非流式）**原样透传**给 vivgrid 的 `/v1/responses`，返回给 Codex 的 model-id 统一加上 `viv-` 前缀。
- 这次新增 **Model Router**：打开 `mode = "model-router"` 后，代理先让分类服务（typesafe `systemone`）判断请求难度，再把请求路由到 small、medium、frontier 三档模型中的一个。
- **一轮对话只分类一次**，同一轮里工具调用之后的请求沿用第一次的决策。
- 每个请求都写一行 JSONL 日志，事后可以导出两种 LoRA 微调数据：**路由器训练数据**和**蒸馏数据**。
- 默认 `mode = "none"`，这时只做透传和加前缀。

---

## 1. 目标与非目标

**目标**
1. 按请求难度选模型：简单任务用便宜模型，复杂任务用 frontier 模型，在成本和质量之间取得平衡。
2. 积累训练数据，将来可以：
   - (a) 训练自己的路由模型，替换外部分类服务；
   - (b) 把 frontier 模型的输出蒸馏到小模型。
3. 不启用时，现有行为完全不变。

**非目标**
- 不做负载均衡和多上游切换。路由只改写 `model` 字段，上游始终是 `https://api.vivgrid.com/v1/responses`。
- 不在代理里训练模型，只负责产出数据。

---

## 2. 请求处理流程

```
Codex ──POST /v1/responses──▶ vg-model-router
   │
   ├─ 打印请求日志、检查 Authorization
   │
   ├─ [mode = model-router]  Router::route
   │     ├─ 判断是不是新的一轮：input 最后一个 item 是 role == "user" 的 message
   │     ├─ 不是新的一轮，且会话缓存里有这一轮的决策 → 直接沿用（Source::Cached）
   │     ├─ 否则：build_state 生成分类文本 → 调用分类服务 → decide 选档
   │     │        分类服务失败或超时 → frontier（Source::Fallback，这次不写缓存）
   │     ├─ 只改写 body["model"]，其余字段原样保留（重新序列化一次）
   │     └─ 创建 trainlog::Pending（请求部分）
   │
   ├─ send_upstream：POST 到 https://api.vivgrid.com/v1/responses（/v1/responses）
   └─ 响应：
         ├─ 流式：stream::Relay 逐行转发 SSE，data 里的 response.model 加 viv- 前缀，
         │        记下 response.completed / incomplete / failed 里的最终 response 对象
         ├─ 非流式：整个 response 对象的 model 加 viv- 前缀后返回
         └─ Pending::finish 补上最终 response（未加前缀），追加一行到 JSONL
```

接入点只有一处：`main.rs` 的 `responses()` 里，位于 `auth_header` 检查之后、`send_upstream` 之前。不启用路由时，请求体按原始字节转发。

---

## 3. 配置

配置文件格式是 TOML。加载顺序：
1. `--config <path>`；
2. 环境变量 `CONFIG`；
3. 当前目录下的 `./vg-model-router.toml`。

只有第 3 种默认路径允许文件不存在，不存在时用全部默认值，也就是 `mode = none`。完整示例见 `vg-model-router.example.toml`。

```toml
mode = "model-router"          # "none"（默认）| "model-router"

[model-router]                 # 也可以写成 [model_router]
small    = "gpt-5.6-luna"      # 必填
medium   = "gpt-5.6-terra"     # 必填
frontier = "gpt-6-astra"       # 必填
include_context  = true        # 分类时是否带上前面的对话
max_state_chars  = 24000       # 发给分类服务的文本上限（字符数）
cheap_threshold  = 0.75        # cheap 请求走 small 所需的 noul 下限（必须大于它）
classifier_url   = "https://api.vivgrid.com/v1/systemone"
classifier_model = "jev-latest"
timeout_ms       = 5000        # 分类服务超时，超时走 frontier
log_path         = "vg-model-router.jsonl"   # 设为 "" 关闭训练数据日志
```

| 项 | 来源 | 说明 |
|---|---|---|
| `VIVGRID_API_KEY` | **只从环境变量读**（可以写在 `.env` / `.env.local`，见下） | 只是兜底：请求没带 `Authorization` 时才用。分类服务和上游用的是同一个 `Authorization`，所以开 `model-router` 也不要求这个变量 |
| 未知字段 | — | `deny_unknown_fields`，拼错的字段启动时就会报错 |
| `mode = "model-router"` 但没有 `[model-router]` 节点 | — | 启动报错 |

**环境变量文件**：启动时（在创建 tokio 运行时之前，因为 `set_var` 在多线程下不安全）从当前目录读取 `.env.local` 和 `.env`，两个文件都可以不存在。优先级从高到低是：shell 里已有的变量、`.env.local`、`.env`。dotenvy 不会覆盖已经存在的变量，所以代码先读 `.env.local`。文件格式错误时启动报错。两个文件都在 `.gitignore` 里，模板是 `.env.example`。

---

## 4. 分类服务（typesafe systemone）

### 4.1 请求

```http
POST https://api.vivgrid.com/v1/systemone
Authorization: <与这次 /v1/responses 上游请求完全相同的 Authorization>
Content-Type: application/json
```

分类服务和上游在同一个网关（vivgrid）上，用同一个 token：`Router::route` 收到 `main.rs` 里 `auth_header()` 算出来的 `Authorization`，原样转给 `classify_once`。也就是说优先用 Codex 发来的 token，没有才退回 `VIVGRID_API_KEY`。

```json
{
  "model": "jev-latest",
  "state": "<build_state 生成的文本>",
  "questions": {
    "difficulty_level":   { "type": "choice", "instructions": "...", "criteria": { "cheap": "...", "medium": "...", "expensive": "..." } },
    "difficulty_score":   { "type": "score",  "instructions": "Rate the difficulty from 0 (easiest) to 4 (hardest)", "criteria": ["...", "...", "...", "...", "..."] },
    "prefer_cheap_model": { "type": "noul",   "instructions": "Is it safe and high-quality enough to route this request to a cheap/small model?" }
  }
}
```

`questions` 的完整内容写在 `router::questions()` 里，修改时只改这一处。

### 4.2 响应（实测）

```json
{
  "model": "jev-1.13.0",
  "answers": {
    "difficulty_level":   { "type": "choice", "choice": "expensive", "confidence": 0.89,
                            "probabilities": { "expensive": 0.93, "medium": 0.07, "cheap": 0.0 } },
    "difficulty_score":   { "type": "score", "score": 3.54, "confidence": 0.62,
                            "legend": { "0": "...", "4": "..." }, "probabilities": { "0": 0.0, "4": 0.6 } },
    "prefer_cheap_model": { "type": "noul", "noul": 0.25 }
  },
  "usage": { "input_tokens": 538, "output_tokens": 77 }
}
```

代码读取的字段：

| 字段 | 用途 |
|---|---|
| `answers.difficulty_level.choice` | 选档依据，**必需**，缺失时按失败处理 |
| `answers.difficulty_level.confidence` | 写日志 |
| `answers.difficulty_score.score` | 写日志和导出数据（0–4，可以是小数） |
| `answers.prefer_cheap_model.noul` | 选档依据（0–1） |

整个 `answers` 原样存进训练日志。

### 4.3 已知特性

- **延迟**：大约 0.25–1 秒，每一轮对话多出这么一次。
- **输入上限**：在约 32k 到 48k tokens 之间，超出返回 `400 {"detail":{"error_type":"max_tokens_exceeded"}}`。代码的处理：
  - 默认只发送最后 24000 个字符；
  - 仍然超限时，只保留后一半再**重试一次**。
- **错误**：key 错误返回 `401 authentication_error`。

---

## 5. 路由规则

| `difficulty_level.choice` | `prefer_cheap_model.noul` | 档位 |
|---|---|---|
| `cheap` | `> cheap_threshold`（0.75） | **small** |
| `cheap` | `≤ cheap_threshold`，或字段缺失 | **medium** |
| `medium` | 任意 | **medium** |
| `expensive` 或其他值 | 任意 | **frontier** |
| 分类服务失败、超时、响应格式不对 | — | **frontier** |

> 和最初的设计相比有一处改动：原稿中 "cheap 但 noul 不够高" 的请求会落到 frontier，跳过了 medium。实测中出现过 "解释一段列表推导式" 被判为 `cheap / noul 0.74`、结果用了 frontier 的情况，所以改成走 medium。

实现：`Router::decide`（`src/router.rs`）。

---

## 6. 按"轮"路由

**为什么**：Codex 的一轮对话（用户说一句话）会发出很多个请求，每次工具调用返回结果后都要再发一次。如果每个请求都分类：
- 每个请求都要多等一次分类调用；
- 同一轮里可能中途换模型，行为前后不一致，上游的 prompt cache（日志里的 `cd-inp`）也会失效。

**规则**：
- **新的一轮**：`input` 的最后一个 item 是 `role == "user"` 的 message（`input` 是字符串时也算）。这时一定分类，并写入缓存。
- **同一轮的后续请求**：最后一个 item 是 `function_call_output`、`custom_tool_call_output` 等工具输出，或 assistant 消息。如果缓存里有这个会话的决策就沿用，日志显示 `same turn as #N`。
- **缓存未命中**（比如代理重启过，或者没有会话标识）：重新分类。
- **分类失败**：不写缓存，下一个请求再试。

**会话标识**，按顺序取第一个有值的：
1. 请求体里的 `prompt_cache_key`（Codex 会填 conversation id）；
2. header `session_id`；
3. header `conversation_id`。

三者都没有时，每个请求都会分类。

**缓存**：保存在内存里，按会话最多保留 1024 个，超出时淘汰最早写入的会话。缓存内容是 `{req_id, tier}`，存的是档位而不是 model_id。

---

## 7. 分类文本（`build_state`）

输入是 Responses 请求的 `input` items。先找出"当前请求"：从后往前数第一条**真实的** user 消息。以下开头的 user 消息是 Codex 自动插入的，不算：

```
<environment_context>   <user_instructions>   # AGENTS.md instructions   <INSTRUCTIONS>
```

**`include_context = false`**：只发当前请求的文本。

**`include_context = true`（默认）**：

```
[Conversation context]
user: ...
assistant: ...
assistant tool call: shell({"command":["ls"]}…)   ← 参数最多保留 300 个字符
tool output: ...                                  ← 最多保留 1000 个字符
...

[Current request]
<当前请求的文本>
```

- **tool call** 包括 `function_call`（arguments）、`custom_tool_call`（input）、`local_shell_call`（action.command）；**tool output** 是对应的 `*_output` item。
- **不包含**：`instructions`、system 和 developer 消息（Codex 的系统提示很长，而且每次都一样）、自动插入的 user 消息、`reasoning` item。图片和文件显示为 `[image]` / `[file]`。
- **超长处理**：总长度超过 `max_state_chars` 时，从最早的上下文开始丢，**当前请求始终保留**。如果当前请求本身就超长，只保留它的尾部。

---

## 8. 训练数据日志（JSONL）

`mode = model-router` 且 `log_path` 不为空时，**每个请求**（包括同一轮的后续请求、失败的请求）在响应结束时写一行：

```jsonc
{
  "ts": 1789790000000,                 // 毫秒时间戳
  "req_id": 2,                         // 和终端日志里的 #2 对应
  "session": "conv-1",                 // 会话标识，可能为 null
  "new_turn": true,
  "requested_model": "gpt-6-astra",    // Codex 请求里写的模型
  "routed_model": "gpt-6-astra",       // 实际发给上游的模型
  "tier": "frontier",                  // small | medium | frontier
  "route": { "type": "classified" },   // 或 {"type":"cached","from_req":N}，或 {"type":"fallback","error":"..."}
  "classifier": {                      // 只有 classified 时有值，否则为 null
    "model": "jev-1.13.0",
    "state": "[Conversation context]\n...",
    "answers": { ... },                // 分类服务返回的 answers，原样保存
    "latency_ms": 250
  },
  "request": {                         // Codex 发来的 Responses 请求（只保存这 3 个字段）
    "instructions": "...",
    "input": [ ... ],                  // 字符串或 item 数组，原样保存
    "tools": [ ... ]
  },
  "response": {                        // 上游最终的 response 对象（流式取自 response.completed 等事件）
    "model": "gpt-6-astra",            // 上游返回的 model，不带 viv- 前缀
    "status": "completed",             // completed | incomplete | failed；没有最终 response 时为 null
    "output": [ ... ],                 // 输出 items：reasoning / message / function_call / custom_tool_call …
    "incomplete_details": null,
    "usage": { ... },                  // 上游原始的 usage
    "error": null                      // 上游报错、流中断或客户端断开时有值
  },
  "latency_ms": 8420                   // 从路由完成到响应结束
}
```

**注意**

- **体积**：Codex 每个请求都会重发完整历史，所以日志增长很快，大约是 "一轮的请求数 × 历史长度"。
- **敏感信息**：日志包含完整的代码、工具输出和用户输入，可能带有密钥或私有代码。不要提交到仓库（`.gitignore` 已经忽略 `*.jsonl`），分享前要先清洗。
- **格式**：日志里保存的是 Responses 原生格式，不做任何转换；转成 Chat 格式只发生在导出时（见第 9 节）。
- **写入方式**：同步追加，一行一次 `write_all`，用 Mutex 保证行不会交错。

---

## 9. 导出为 LoRA 数据

```bash
vg-model-router export router --in vg-model-router.jsonl --out router.jsonl
vg-model-router export sft    --in vg-model-router.jsonl --out sft.jsonl [--tier frontier|medium|small|all] [--with-reasoning]
```

两种导出都是 OpenAI chat fine-tuning 格式，每行一个 `{"messages": [...]}`。日志里是 Responses 格式，导出 sft 时由 `src/convert.rs` 转成 Chat 格式。

### 9.1 `router`：训练自己的路由器

- **筛选**：只导出 `classifier` 不为 null 的记录，也就是调用过分类服务的请求。
- **格式**：

```json
{"messages": [
  {"role": "system", "content": "Classify the difficulty of the AI request for model routing. Reply with JSON: {...}"},
  {"role": "user", "content": "<classifier.state>"},
  {"role": "assistant", "content": "{\"difficulty_level\":\"cheap\",\"difficulty_score\":0.3,\"prefer_cheap_model\":0.91}"}
]}
```

### 9.2 `sft`：蒸馏

- **筛选**：
  - 档位匹配 `--tier`，默认是 frontier；
  - `response.error` 为空；
  - `response.status` 是 `completed`，且 `output` 不为空。
- **格式**：`{"messages": chat(instructions + input + output), "tools": chat(tools)}`。没有工具时不输出 `tools` 字段。转换规则：
  - `instructions`、developer / system 消息 → `system`；
  - `function_call` / `custom_tool_call` → assistant 的 `tool_calls`（连续的调用合并到一条消息）；custom 工具的参数包装成 `{"input": "..."}`，工具定义也包装成只有一个 `input` 参数的 function；
  - `function_call_output` / `custom_tool_call_output` → `tool` 消息；
  - `local_shell_call` 及其输出、`web_search` 等内置工具在 Chat 格式里没有对应物，导出时丢掉。
- **reasoning**：默认丢掉。加 `--with-reasoning` 时，`reasoning` item 里的明文（`content`，没有就用 `summary`）放进后一条 assistant 消息的 `reasoning_content`。只有 `encrypted_content` 的 reasoning 导不出来。
- 同一轮里的每个请求各自是一条样本（前缀 → 下一步动作），这是有意的设计。

---

## 10. 终端日志

每个请求都有编号，首行是 `#<id> ▶ <METHOD> <path>`：

```
INFO #1 ▶ GET /v1/models  [→ https://api.vivgrid.com/v1/models]
INFO #1 ◀ GET /v1/models  [200, 0.33s]
INFO #2 ▶ POST /v1/responses  [stream, model=gpt-6-astra, input_items=24, tools=6]
    ask    ▸ (34 chars) instr 那行改成只打用户的问题
```

`ask ▸` 是**用户这一轮真正问的那句话**：字符数 + 折叠空白后的前 200 个字符。只有这一条——不打 `instructions`（system prompt 每个请求都一样）、不打历史消息、不打工具输出。取的是 `router::last_user_text`，和 `build_state` 挑的是同一条消息（跳过 codex 注入的 `<environment_context>` / `<user_instructions>` / AGENTS.md），所以它就是分类服务被问的那段文本，下面的路由决策正是针对它做的。codex 会用同一个 model、长得差不多的概要行发出用途完全不同的请求，靠这一行区分，所以**每个请求都打**。一轮内的后续请求（工具循环）打的都是这一轮的提问；完全没有用户消息的请求（compact、起标题）打 `<no user message>`。

终端里每条 `#<id>` 日志整条（含续行）会按编号上色，8 种颜色轮换（`report::ReqColor`）：请求是并发的，一个请求的 `▶` / `⇢` / `◀` 之间会插进别的请求的行，同色才串得起来。stdout 不是终端（重定向、管道）或设了 `NO_COLOR` 时不输出任何转义序列。

`/v1/models` 上游返回非 2xx 或连不上时是 **WARN**；`/health` 只在 `RUST_LOG=vg_model_router=debug` 下打印，避免健康检查刷屏；没有匹配到的路径由 `fallback` 打 `WARN unhandled route: <method> <uri>`。

分类服务的**每一次调用**都单独打一行 `⇢ <分类模型>`，紧跟着是路由结果行 `⇢ route`。分类服务是路由决策唯一的外部依据，所以这一行不受"明细只在异常时打印"的约束：无论成败都打，成功是 INFO，失败是 WARN。

```
INFO #1 ⇢ jev-1.13.0  [0.55s, in 538 / out 77]   answer ▸ (conf 1.00 | chp 1.00, med 0.00, exp 0.00), scr 0.00 (conf 0.62), noul 0.94
INFO #1 ⇢ route  [requested model=gpt-6-astra]
    route  ▸ small → gpt-5.6-luna  (classified by jev-1.13.0 in 0.55s; cheap, noul 0.94 > 0.75)
INFO #2 ⇢ route  [requested model=gpt-6-astra]
    route  ▸ frontier → gpt-6-astra  (same turn as #1)
WARN #3 ⇢ jev-latest  [0.02s]   answer ▸ ✖ HTTP 400 Bad Request: {"detail": {"error_type": "max_tokens_exceeded"}}
INFO #3 ⇢ jev-1.13.0  [0.31s, in 271 / out 77, attempt 2]   answer ▸ (conf 0.55 | chp 0.71, med 0.29, exp 0.00), scr 1.10 (conf 0.67), noul 0.43
INFO #3 ⇢ route  [requested model=gpt-6-astra]
    route  ▸ medium → gpt-5.6-terra  (classified by jev-1.13.0 in 0.31s; cheap but noul 0.43 ≤ 0.75)
WARN #4 ⇢ jev-latest  [5.00s]   answer ▸ ✖ request failed: operation timed out
WARN #4 ⇢ route  [requested model=gpt-6-astra]
    route  ▸ frontier → gpt-6-astra  (⚠ classifier failed, fallback: request failed: operation timed out)
```

`⇢ <分类模型>` 行（一行）：方括号里是响应里的模型版本（`answers.model`，失败时退回配置的 `jev-latest`）、耗时、`usage` 的 token 数，重试时多一个 `attempt N`；`answer ▸` 是响应里除 `legend` 外的全部内容——`conf` 是 `difficulty_level` 的把握，竖线后面是各档概率（`chp` / `med` / `exp`），`scr` 是 0–4 的难度分和它自己的 `conf`，`noul` 是 `prefer_cheap_model`。选中的那一档不在这里重复，它在下面 route 行的依据里。**超长重试（`max_tokens_exceeded`）会调两次，两次各打一行**，第一次的失败不会被重试吞掉。`⇢ route` 行是决策本身：分号后面是 `decide()` 选这一档的依据（第 5 节的规则），`choice` 和最终档位对不上时——比如 `cheap` 却走了 medium——不用翻文档就能看懂。命中同一轮的缓存时（`same turn as #N`）没有发生分类调用，所以既没有 `⇢ jev` 行，也没有这个依据。

分类服务的原始请求文本和原始响应体在 `RUST_LOG=vg_model_router=debug` 下打印。

**明细只在异常时打印**：正常的一次请求只有两条（`▶ POST …` 和 `◀ response …`，前者带一行 `ask`），发生了分类调用的那一轮再多两条（`⇢ jev …` 和 `⇢ route …`）。请求行的 `ask` 每个请求都打，`tools` / `items` 明细只在历史里有未配对的 tool call（`⚠`）时打印，响应行的 `stop` / `output` / `usage` / `note` 明细只在 `status != completed` 或有 note 时打印，这两种情况下整行都是 **WARN**。每个请求的 usage 仍然完整写进训练日志 JSONL（第 8 节），终端上看不到不等于没记录。

---

## 11. 代码地图

| 文件 | 职责 | 关键符号 |
|---|---|---|
| `src/config.rs` | 加载 TOML、默认值、校验 | `Config::load`、`Mode`、`RouterConfig` |
| `src/router.rs` | 分类文本、调用分类服务、选档、会话缓存 | `Router::route`、`Router::decide`、`build_state`、`questions`、`Decision::summary` |
| `src/trainlog.rs` | JSONL 日志与导出 | `TrainLog`、`Pending::{new, finish}`、`export`、`sft_sample` |
| `src/convert.rs` | 导出 sft 时把 Responses items / tools 转成 Chat 格式 | `to_chat_messages`、`to_chat_tools` |
| `src/main.rs` | 读取 `.env` / `.env.local`、命令行参数（`--config`、`export`）、启动时构建 Router、在 `responses()` 里接入 | `load_env_files`、`parse_args`、`AppState.router` / `trainlog` |
| `src/stream.rs` | 流式转发 SSE，带 model 的事件加 `viv-` 前缀（其他行原样转发），结束时用最终 response 写日志 | `Relay.pending`、`Relay::finish`、`add_model_prefix` |

**常见修改**

| 想做的事 | 改哪里 |
|---|---|
| 改路由规则 | `Router::decide` 和对应的单元测试 |
| 改分类问题 | `router::questions()`（会影响已有日志和新日志的可比性） |
| 加档位 | `Tier`、`RouterConfig`、`model_for`、`decide`、导出参数 `--tier` |
| 识别更多 Codex 自动插入的消息 | `router::is_injected` |
| 换分类服务 | `classify_once` 的请求和解析部分；`Verdict` 结构保持不变 |

---

## 12. 测试

```bash
cargo test          # build_state、decide、导出格式的单元测试
```

**手动端到端测试**：用一个 mock 的 Responses 服务（`/v1/responses`，流式返回 `response.created` … `response.completed`）当上游，分类服务用真实的。

```bash
https://api.vivgrid.com/v1/responses=http://127.0.0.1:<mock>/v1/responses VIVGRID_API_KEY=... \
  ./target/debug/vg-model-router --config vg-model-router.toml
```

要覆盖的场景：
- 简单问题 → small，复杂问题 → frontier；
- 同一轮的后续请求显示 `same turn as #N`；
- 故意用错误的 key → WARN 并走 frontier；
- 不带配置文件 → 没有 route 行，`model` 原样透传；
- 流式和非流式请求都能写出完整的 `response.output`，返回给 Codex 的 model 带 `viv-` 前缀。

> 本机的 Claude Code 沙箱会拦截连向本地 mock 的回环连接，测试时需要在沙箱外运行。另外 `http_proxy` 环境变量会让 reqwest 走代理，要设置 `NO_PROXY=127.0.0.1`。

---

## 13. 已知限制与后续可做

- **会话缓存只在内存里**：代理重启后缓存丢失，下一个请求会重新分类，可能选到和之前不同的档位。
- **缓存不过期**：只按数量淘汰。用户在同一个会话里长时间没有输入新消息的情况不影响结果，因为每个新的一轮都会重新分类。
- **没有会话标识的客户端**：每个请求都会分类。
- **日志同步写入**：写日志时会短暂阻塞 tokio worker 线程。如果日志量很大，可以改成 channel 加单独的写线程。
- **训练日志没有自动轮转和清理**。
- **路由不看 `reasoning.effort`**，也不会根据档位调整它。
- 可以考虑用 `difficulty_score` 或 `confidence` 做更细的规则，目前它们只记录不参与决策。
- 路由模型训练好之后，可以加一个 `classifier_url` 指向自建服务的模式。只要响应格式和第 4.2 节一致，就不用改代码。

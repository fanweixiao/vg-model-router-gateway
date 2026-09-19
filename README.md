# vg-mirror (Vivgrid Mirror)

A local LLM API proxy for [Codex](https://github.com/openai/codex).

It exposes the OpenAI **Responses API** (`POST /v1/responses`) on your machine, converts each request to the **Chat Completions API**, and forwards it to `https://api.vivgrid.com/v1/chat/completions`. Replies (streaming and non-streaming) are converted back into Responses format.

For each request, the log shows the upstream **stop value** (`finish_reason`), the **token usage**, and a summary of the **tools** in the request. This makes it easier to debug odd behaviour between Codex and the upstream.

## Quick start

### 1. Download and run

Prebuilt binary for macOS (Apple Silicon). Download it and run it locally:

```bash
curl -L -o vg-mirror https://github.com/fanweixiao/vg-mirror/releases/download/v0.1/vg-mirror
chmod +x vg-mirror
xattr -d com.apple.quarantine vg-mirror 2>/dev/null   # only needed if downloaded via a browser
./vg-mirror
# INFO vg-mirror listening on http://127.0.0.1:33333  →  upstream https://api.vivgrid.com/v1/chat/completions
```

Keep this terminal open while you use Codex. Logs show up here.

<details>
<summary>Or build from source</summary>

Requires Rust 1.88+ (edition 2024).

```bash
cargo build --release
./target/release/vg-mirror
```

For a stripped Apple Silicon binary, see [Building a release binary for macOS](#building-a-release-binary-for-macos-apple-silicon).

</details>

### 2. Point Codex at the proxy

Edit `~/.codex/config.toml`:

```toml
model_provider = "local"
model = "gpt-6-astra"

[model_providers.local]
name = "Local Proxy"
base_url = "http://127.0.0.1:33333/v1"
experimental_bearer_token = "<VIVGRID_API_KEY>"
```

- `base_url` must match the proxy's listen address, including `/v1`.
- `experimental_bearer_token` is your vivgrid API key. Codex sends it as `Authorization: Bearer ...`, and the proxy forwards it upstream unchanged.
- `model` is passed through unchanged, so use any model name vivgrid accepts.

Then run `codex` as usual and watch the proxy's terminal for logs.

## Building a release binary for macOS (Apple Silicon)

The target triple for Apple Silicon (M1/M2/M3/M4…) is `aarch64-apple-darwin`.

**With make (recommended)**

```bash
make release   # add target, build, strip, print arch & size
               # → target/aarch64-apple-darwin/release/vg-mirror
make install   # also copy it to ~/.local/bin (override with PREFIX=/usr/local/bin)
```

**By hand**

```bash
# 1. Add the target (already installed if you're on an Apple Silicon Mac)
rustup target add aarch64-apple-darwin

# 2. Build an optimized binary
cargo build --release --target aarch64-apple-darwin

# 3. Check the architecture
file target/aarch64-apple-darwin/release/vg-mirror
# → Mach-O 64-bit executable arm64
```

On an Apple Silicon Mac, a plain `cargo build --release` also produces an arm64 binary, in `target/release/`. Passing `--target` is still useful: it works from an Intel Mac too, and it keeps the output path explicit.

**Optional: make it smaller and install it**

```bash
# Strip debug symbols (~8.6 MB → ~6.8 MB)
strip target/aarch64-apple-darwin/release/vg-mirror

# Put it on your PATH
mkdir -p ~/.local/bin
cp target/aarch64-apple-darwin/release/vg-mirror ~/.local/bin/
vg-mirror
```

**Copying the binary to another Mac**

A binary you build yourself is not quarantined. If you send it to another Mac (AirDrop, browser download, chat), Gatekeeper may block it with *"cannot be opened because the developer cannot be verified"*. On that machine, run:

```bash
xattr -d com.apple.quarantine ./vg-mirror
```

## Configuration

All settings are optional environment variables:

| Variable | Default | Description |
|---|---|---|
| `LISTEN` | `127.0.0.1:33333` | Address the proxy listens on |
| `UPSTREAM_URL` | `https://api.vivgrid.com/v1/chat/completions` | Upstream Chat Completions endpoint |
| `UPSTREAM_API_KEY` | – | Fallback key, used **only** when the incoming request has no `Authorization` header |
| `RUST_LOG` | `vg_mirror=info` | Log level. `vg_mirror=debug` also logs the full request/response bodies sent to and received from upstream, and each `finish_reason` chunk |

Example:

```bash
RUST_LOG=vg_mirror=debug LISTEN=127.0.0.1:40000 ./target/release/vg-mirror
```

## Model Router

With `mode = "model-router"` in the config file, the proxy picks the upstream model itself. For each new turn it asks a classifier ([typesafe](https://api.typesafe.ai) `systemone`) how hard the request is, then routes it:

| Classifier result | Routed to |
|---|---|
| `difficulty_level = cheap` and `prefer_cheap_model.noul > cheap_threshold` (0.75) | `small` |
| `difficulty_level = cheap` with lower `noul`, or `difficulty_level = medium` | `medium` |
| anything else, or the classifier failed / timed out | `frontier` |

- **One classification per turn.** A request whose last message is from the user starts a new turn and is classified. The follow-up requests Codex sends after each tool call reuse that turn's model (keyed by `prompt_cache_key`), so a turn never switches models halfway and the prompt cache keeps working. If the proxy restarts mid-turn, the next request is classified again.
- **What the classifier sees.** The latest real user message. With `include_context = true` (the default), earlier user/assistant messages, tool calls and trimmed tool outputs go with it. System prompts and Codex's injected `<environment_context>` / AGENTS.md messages are left out. The text is capped at `max_state_chars`, and the oldest context is dropped first. If the classifier still reports `max_tokens_exceeded`, the proxy retries once with the newer half.
- **Response `model`.** Codex receives the real model ID that served the request.

### Configuration

Copy [`vg-mirror.example.toml`](vg-mirror.example.toml) to `./vg-mirror.toml`, or pass `--config <path>` (or set `CONFIG`). Put the classifier key in an environment variable. It is never read from the file:

```bash
cp vg-mirror.example.toml vg-mirror.toml
TYPESAFE_API_KEY=... ./vg-mirror
```

Without a config file, `mode` is `none` and the proxy behaves as before.

### Logs

Each request gets a route line before the upstream call. It is logged as **WARN** when the classifier failed:

```
INFO #2 ⇢ route  [requested model=gpt-6-astra]
    route  ▸ frontier → gpt-6-astra  (classified 0.25s: expensive 1.00, score 3.99, noul 0.09)
INFO #3 ⇢ route  [requested model=gpt-6-astra]
    route  ▸ frontier → gpt-6-astra  (same turn as #2)
```

### Training data

Every routed request is appended to `log_path` (default `vg-mirror-router.jsonl`) as one JSON line. Each line holds the routing decision, the classifier input and answers, the full Chat request sent upstream (`messages`, `tools`), and the model's full output (`content`, `reasoning_content`, `tool_calls`, `finish_reason`, `usage`).

Export it as LoRA fine-tuning JSONL (chat `messages` format):

```bash
# Train your own router: classifier input → {"difficulty_level", "difficulty_score", "prefer_cheap_model"}
./vg-mirror export router --in vg-mirror-router.jsonl --out router.jsonl

# Distill the frontier model: full conversation (+ tools) → its reply.
# Only requests that finished with stop / tool_calls are exported.
./vg-mirror export sft --in vg-mirror-router.jsonl --out sft.jsonl [--tier frontier|medium|small|all] [--with-reasoning]
```

Heads-up: Codex resends the whole history on every request, so the log grows quickly.

## Endpoints

| Method & path | Description |
|---|---|
| `POST /v1/responses` (also `/responses`) | Responses API → Chat Completions; supports `stream: true` and `stream: false` |
| `GET /v1/models` | Passed through to upstream `/v1/models` |
| `GET /health` | Returns `ok` |

## Headers forwarded upstream

Only these headers go to the upstream. Everything else from Codex is dropped.

| Incoming header | Sent upstream as |
|---|---|
| `Authorization` | `Authorization` (unchanged) |
| `User-Agent` | `User-Agent` (unchanged) |
| `x-codex-turn-metadata` | `x-viv-meta` |

To rename more headers, add pairs to `HEADER_RENAMES` in `src/main.rs`.

## Reading the logs

```
INFO #3 ▶ request  [stream, model=gpt-5.6-luna, input_items=24 → messages=21, tools=6]
    tools  ▸ declared (6): shell(command, workdir, timeout_ms) [function], apply_patch(input) [custom→function], ...
             dropped (1): web_search
             parallel_tool_calls=false
             in input: 5 calls (shell ×4, apply_patch ×1), 5 outputs
    items  ▸ message:developer ×1, message:user ×3, reasoning ×5 [dropped], function_call ×4, custom_tool_call ×1, function_call_output ×4, custom_tool_call_output ×1
    sent   ▸ tool traces in upstream body: field `tools`, field `parallel_tool_calls`, 3 assistant messages with tool_calls, 5 tool-role messages
INFO #3 ◀ response  [stream, 8.42s, model=gpt-5.6-luna]
    stop   ▸ finish_reason = "tool_calls"  →  status = completed (sent response.completed)
    usage  ▸ inp: 8,029, cd-inp: 6,144 (76.5%), opt: 212 (reasoning: 64)
```

**Request line (`▶`)**
- `input_items → messages`: how many Responses input items became how many Chat messages.
- `tools ▸ declared`: tools sent upstream, with parameter names and how each was converted.
- `dropped`: tool types the proxy can't send upstream (e.g. `web_search`).
- `in input`: tool calls and outputs replayed in the conversation history.
- `⚠ calls without output` / `⚠ outputs without call`: the history has unpaired tool calls. Chat Completions backends often reject this. When it happens, the request line is logged as **WARN**.
- `items ▸`: every input item counted by type (messages split by role). `[dropped]` = reasoning items. `[⚠ skipped: unknown type]` = item types the proxy can't convert, which are left out of the upstream request (also logged as **WARN**).
- `sent ▸`: whether the Chat body actually sent upstream still carries any tool traces (`tools` / `tool_choice` / `functions` fields, assistant `tool_calls`, `tool`-role messages).
- On an upstream error, the full body sent upstream is saved to `$TMPDIR/vg-mirror-req-<id>.json`, and the log prints a `curl` command to replay it.

**Response line (`◀`)**
- `stop`: the raw upstream `finish_reason` and the Responses status Codex was sent. If upstream also sends `stop_reason`, `native_finish_reason` or `matched_stop`, they are shown on the same line.
- `usage`: `inp` = prompt tokens, `cd-inp` = cached prompt tokens (with hit rate), `opt` = completion tokens (with reasoning tokens). `n/a` means upstream didn't report that field.
- `note`: any abnormal event (see below). The response line is logged as **WARN** when any note appears or `finish_reason` is not `stop` / `tool_calls`.

### How `finish_reason` maps to what Codex receives

| Upstream `finish_reason` | Codex receives |
|---|---|
| `stop`, `tool_calls` | `response.completed` |
| `length` | `response.incomplete` (reason `max_output_tokens`) |
| `content_filter` | `response.incomplete` (reason `content_filter`) |
| *missing, and no `[DONE]`* | `response.failed`: `upstream stream ended without finish_reason and without [DONE]` |
| upstream HTTP error | same HTTP status and body |

Codex treats `response.incomplete` and `response.failed` as errors.

Other notes that can appear: `upstream stream closed without [DONE]`, `finish_reason changed mid-stream`, `upstream sent error event`, `client disconnected before the response finished`.

## Conversion details

**Request (Responses → Chat Completions)**

- `instructions` and `developer`/`system` messages become `system` messages.
- `function_call` and `function_call_output` become assistant `tool_calls` and `tool` messages. Consecutive calls are merged into one assistant message.
- `custom` tools (Codex's freeform `apply_patch`) are sent as a function with a single `input: string` parameter. The tool's grammar is appended to the description, and the reply is converted back into a `custom_tool_call`.
- `reasoning.effort` → `reasoning_effort`, `max_output_tokens` → `max_tokens`, `text.format` (json_schema) → `response_format`.
- `temperature`, `top_p`, `tool_choice`, `parallel_tool_calls` are passed through.
- Streaming requests add `stream_options.include_usage = true` so usage is reported.

**Response (Chat Completions → Responses)**

- `content` becomes a `message` item.
- `tool_calls` become `function_call` / `custom_tool_call` items.
- `reasoning_content` / `reasoning` become a `reasoning` item, shown in Codex as a reasoning summary.
- In streaming mode, the full Responses SSE event sequence is emitted (`response.created` … `output_item.added` / deltas / `output_item.done` … `response.completed`).

## Limitations

- Built-in tools other than `function` / `custom` (e.g. `web_search`, `local_shell`) are not sent upstream.
- Reasoning items from earlier turns (often encrypted) can't be replayed to a Chat backend and are dropped.
- `input_image` works with URLs and data URLs only, not `file_id`.
- Only the first choice (`n = 1`) is used.

## Project layout

```
src/
├── main.rs      # HTTP server, routes, header forwarding, non-stream path
├── convert.rs   # Request/response conversion between the two APIs
├── stream.rs    # Chat Completions SSE → Responses SSE translator
├── report.rs    # Log formatting: request/tools, stop, usage
├── config.rs    # Config file (mode, [model-router])
├── router.rs    # Model Router: classifier call, routing rules, per-turn cache
└── trainlog.rs  # Training-data JSONL log and `export`
```

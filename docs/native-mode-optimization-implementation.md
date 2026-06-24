# Native Mode 长上下文优化实施说明

> 交给 Claude Opus 4.8 的落实文档。目标是在保持 Claude / Claude Code 原生兼容的前提下，
> 优化长上下文请求的体感速度、队列延迟和账号池吞吐。请先重新阅读当前工作树，因为本仓库
> 可能已有未提交改动；不要回滚不属于你的改动。

## 一句话目标

在 **Native Mode** 下，不减少模型实际看到的上下文，不改写 Claude Code 语义，只优化：

- `/cc/v1/messages` 的流式体感首包延迟。
- 多账号并发调度的吞吐和尾延迟。
- 连接、端点、观测和本地转换开销。
- 如有真实上游缓存证据，再接入上游缓存；没有证据时只保留本地计量缓存。

## Native Mode 不可破坏边界

以下规则是硬约束。实现任何优化前后都必须保持：

1. 不改写 `system`。
2. 不删除 `messages`。
3. 不替换 Claude Code system prompt。
4. 不做代理侧 compact / summarize。
5. 不扁平化历史工具结构，除非当前 Kiro 协议转换本来必须这样做。
6. 保持 `/v1/messages`、`/cc/v1/messages`、`/messages/count_tokens` 的 Anthropic 兼容行为：
   - SSE 事件顺序合法：`message_start -> content_block_* -> message_delta -> message_stop`。
   - `usage` 结构兼容 Anthropic / Claude Code。
   - `count_tokens` 只反映请求本身 token，不受本地 cache 计量影响。
   - 不因本地 cache 展示而少转发上游 prompt。

不要引入这些 Kiro-Go 风格的 Fast Mode 优化到 Native Mode：

- 900KB 总 payload 裁剪。
- Claude Code prompt filter / prompt replacement。
- 历史 tool_use/tool_result 扁平化。
- 代理侧提前 compact / summarize。

这些可以以后作为显式 `Fast Mode` 做，但必须默认关闭，并明确标注会改变语义。

## 当前代码事实

请以函数名为准重新核对行号；以下是当前工作树观察到的关键入口。

### 1. `/v1/messages` 流式路径已经是边收边发

入口在 `src/anthropic/handlers.rs`：

- `post_messages` 转换请求、计算 `total_input_tokens`。
- 流式请求走 `handle_stream_request`。
- `handle_stream_request` 先生成 `message_start` 和初始内容块，再通过 `create_sse_stream`
  持续转发上游事件。

这一路径体感首包不应该被完整响应结束阻塞。

### 2. `/cc/v1/messages` 当前仍是缓冲流

入口在 `src/anthropic/handlers.rs`：

- `post_messages_cc` 流式请求调用 `handle_stream_request_buffered`。
- `handle_stream_request_buffered` 使用 `BufferedStreamContext`。
- `create_buffered_sse_stream` 的注释明确写着：
  1. 等待上游流完成，期间只发送 ping。
  2. 缓冲所有事件。
  3. 流结束后修正 `message_start.usage`。
  4. 一次性发送所有事件。

这不改变模型输入，但会严重拖慢 Claude Code 体感首包。长上下文下用户会感觉“模型一直不吐字”。

### 3. usage 低估问题已有修复痕迹

`src/anthropic/stream.rs::StreamContext::resolved_usage` 当前使用：

```rust
max(contextUsage 折算值, 本地 count_all_tokens)
```

这是正确方向：上游 `contextUsagePercentage` 是百分比折算，可能低估；Native Mode 需要上报不低于本地真实估算，才能让 Claude Code 的 auto-compact 逻辑有机会按真实上下文增长触发。

请确认非流式路径、websearch loop 路径也采用同一原则。尤其要检查：

- `src/anthropic/handlers.rs` 非流式 usage。
- `src/anthropic/websearch_loop.rs` 最终 usage。

### 4. HTTP client 已有连接池配置

`src/http_client.rs::build_client` 当前已配置：

- `connect_timeout`
- `read_timeout`
- `tcp_keepalive`
- `pool_idle_timeout`
- `pool_max_idle_per_host`

`src/kiro/provider.rs` 通过 `client_for` 按 proxy 缓存 `reqwest::Client`。

当前实际请求构建路径没有发现显式发送 `Connection: close`，但 `src/kiro/endpoint/mod.rs`
里仍有旧注释说 provider 设置了 `Connection`。请以代码为准，清理误导注释可以做，但不是性能核心。

### 5. 调度侧已有较多修复痕迹

`src/kiro/token_manager.rs` 当前已有：

- `CredentialRuntime`，内部维护 semaphore、capacity、in_flight、shrink_debt。
- 动态缩容不再简单替换 semaphore。
- `CredentialCandidate::load_per_mille` / `effective_load`，按负载率和错误率软降权。
- `CredentialLease` drop 时维护在途状态。

因此不要按旧文档重复实现“按负载率排序 / 不替换 semaphore”，先复核现状，再补剩余缺口。

### 6. 本地 cache_metering 不是上游真实缓存

`src/anthropic/cache_metering.rs` 的作用是本地 usage 展示和计费口径模拟。它不能减少上游处理 token，也不能让单个 200K prompt 自动变快。

若要做“真实上游缓存”，必须先有 Kiro 协议字段、header、event 或实测证据。没有证据时，不要把本地 cache 伪装成真实加速。

## P0：修复 `/cc/v1/messages` 缓冲流导致的体感慢

### 目标

让 `/cc/v1/messages` 在保持 SSE 顺序和 usage 兼容的前提下，不再等完整上游响应结束才吐真实事件。

### 推荐方案：usage-gated streaming

不要直接照搬 `/v1/messages` 的立即 `message_start`，因为 `/cc/v1/messages` 当初缓冲的目的，是让 `message_start.usage.input_tokens` 尽量准确。

更好的方案：

1. 调用上游后开始读取 event-stream。
2. 暂存上游事件，直到满足以下任一条件：
   - 收到 `contextUsageEvent`，可计算 `context_input_tokens`。
   - 收到第一条会产生可见内容的事件，但还没等到 `contextUsageEvent`，则使用本地
     `fallback_input_tokens` 作为 `message_start.usage.input_tokens`。
3. 一旦决定 usage，立即生成并发送 `message_start`。
4. 把等待期间暂存的可见事件按原顺序 flush。
5. 后续事件边读边发，不再等流结束。
6. 流结束时发送 `message_delta` / `message_stop`，并在 `message_delta.usage` 上报最终
   `resolved_usage`。

### 关键兼容点

`message_start.usage.input_tokens` 一旦发出就不能回头改。因此要接受：

- `message_start` 使用“当时已知的最佳值”。
- `message_delta.usage` 使用最终 `resolved_usage`。

这和 Anthropic SSE 常见语义是兼容的：最终 usage 以后续 delta / stop 前的累计口径为准。

如果非常担心 Claude Code 依赖 `message_start.usage`，可采用更保守策略：

- 最多等待 `contextUsageEvent` 或首个可见内容事件前的一小段时间。
- 如果 `contextUsageEvent` 通常在内容前到达，首包延迟会显著改善。
- 如果上游迟迟不发 `contextUsageEvent`，不能无限缓冲，必须用本地估算开流。

### 不要做的实现

- 不要等完整上游结束再一次性发所有事件。
- 不要为了修正 `message_start.usage` 重排内容事件。
- 不要牺牲 thinking / tool_use 的块顺序。
- 不要吞掉上游中途错误前已经可合法发送的事件。

### 代码入口

主要文件：

- `src/anthropic/handlers.rs`
  - `post_messages_cc`
  - `handle_stream_request_buffered`
  - `create_buffered_sse_stream`
  - 可新增 `create_usage_gated_sse_stream`
- `src/anthropic/stream.rs`
  - `StreamContext`
  - `BufferedStreamContext`
  - `resolved_usage`
  - `generate_initial_events`
  - `generate_final_events`

建议保留 `BufferedStreamContext` 测试或作为 fallback，但默认 `/cc/v1/messages` stream 应走 usage-gated streaming。

### P0 验收

构造一个上游 event-stream 测试或单元测试，验证：

1. `/cc/v1/messages` 不再等 `None`/流结束才发 `message_start`。
2. `contextUsageEvent` 在内容前到达时，`message_start.usage.input_tokens` 使用
   `max(context_input_tokens, fallback_input_tokens)` 分摊后的值。
3. 没有 `contextUsageEvent` 时，`message_start` 使用本地 fallback，不无限缓冲。
4. thinking 开启时，thinking block 仍在 text block 前。
5. tool_use 的 `content_block_start/delta/stop` 顺序不变。
6. 最终 `message_delta.usage` 使用 `resolved_usage`，包含：
   - `input_tokens`
   - `output_tokens`
   - `cache_creation_input_tokens`
   - `cache_read_input_tokens`

## P1：补齐 token-aware 调度

### 目标

不要让一个 200K token 请求和一个 4K token 请求在调度上完全等价。Native Mode 不能减少 200K 的上游计算成本，但可以避免它拖垮短请求和整个账号池。

### 推荐设计

1. handler 已经计算 `total_input_tokens`，把它传入 provider / token_manager 的 acquire 路径。
2. 将请求分级：
   - small：`< 32K`
   - medium：`32K-128K`
   - large：`>= 128K`
   阈值可先写常量，后续再配置化。
3. 调度策略：
   - large 请求优先选择当前 `effective_load` 更低、近期耗时更低的账号。
   - large 请求可占用更高权重，避免同一账号同时塞太多 large。
   - small 请求不应被 large 队列长期饥饿。
4. 不要做请求级 hedging。并发发给多个上游会增加真实计费和副作用风险。

### 可选实现路径

保守版本：

- 不改 semaphore permit 数，只在候选排序中加入 request size penalty。
- 对 large 请求选择 `in_flight_large` 更少的账号。
- 在 `CredentialRuntime` 或 metrics 里记录 active request 的 token class。

增强版本：

- 引入 weighted lease，large 请求占多个虚拟单位。
- 注意 Tokio `Semaphore` 是整数 permit，可让 large acquire 多个 permit，但必须处理动态缩容和 drop 释放。
- 这版风险更高，建议 P0 完成后再做。

### P1 验收

压测场景：

- 多账号，每账号 `max_concurrency > 1`。
- 同时混合 4K、40K、200K 请求。

期望：

- small 请求 P95 排队时间下降。
- large 请求不会集中打到同一个账号。
- 无账号在 `max_concurrency` 之外超发。
- 429 / rate limit 不上升。

## P1：统一长上下文可观测性

### 目标

优化前后必须能解释“慢在哪里”。至少区分：

- 代理本地转换耗时。
- token 估算耗时。
- 等账号 permit 的时间。
- token refresh / profile resolution 时间。
- 上游连接和首字节时间。
- 上游完整响应时间。
- `/cc/v1/messages` 缓冲等待时间。
- 下游第一个真实 SSE 事件发送时间。

### 建议新增 trace 字段

如果当前 `TraceStore` schema 方便扩展，建议记录：

- `request_bytes`
- `local_input_tokens`
- `credential_wait_ms`
- `conversion_ms`
- `token_count_ms`
- `upstream_first_byte_ms`
- `downstream_first_event_ms`
- `buffering_delay_ms`
- `endpoint`
- `region`
- `credential_id`
- `attempt_count`
- `context_input_tokens`

若不想先改 DB schema，可先用 structured logs，字段名稳定即可。

### P1 验收

对同一 200K 请求，trace/log 能回答：

- 是排队慢、上游 prefill 慢，还是 `/cc` 缓冲慢。
- 是否命中了重试 / fallback。
- 是否同一账号被 large 请求压满。

## P2：真实上游缓存验证与接入

### 目标

只在有证据时接入真实上游缓存。不要把本地 cache_metering 当作加速。

### 验证实验

用同账号、同模型、同 endpoint、同 region、同 session、同大前缀，连续发两轮请求：

1. 第一轮：稳定 system/tools/history + 当前小问题。
2. 第二轮：完全相同稳定前缀 + 新的小问题。

记录：

- total duration
- upstream first byte latency
- downstream first event latency
- credits / metering usage
- contextUsagePercentage
- request bytes
- 本地 cache read / creation 展示值

再对比带/不带 Anthropic `cache_control` 的请求。当前 Kiro wire format 未发现 cache 字段，因此如果要传递 `cache_control`，必须先找到 Kiro 后端接受的字段或 header。

### 判定

只有当第二轮在同等条件下稳定更快，且 credits 或上游事件显示缓存收益，才能认为存在真实上游缓存。

否则：

- 保持本地 cache_metering 仅用于下游 usage / 计费展示。
- 不声称其提升上游速度。

## P2：连接与端点微优化

当前 `build_client` 已有连接池。后续只做低风险微调：

- 复核是否启用 HTTP/2；reqwest 通常可自动协商，但可显式确认。
- `pool_max_idle_per_host` 是否需要从 8 提高到 16/32，取决于账号数、代理数、endpoint 数。
- 对 endpoint / region 做 EWMA 观测，失败或慢的 endpoint 降权。
- 不要为了降低延迟做同请求多 endpoint 并发竞速，避免重复计费和副作用。

## P2：本地 CPU/内存开销优化

这些优化不会改变语义，但收益通常小于 P0：

- 避免大请求 body 多次 clone / stringify。
- `count_all_tokens` 如果昂贵，增加耗时观测后再优化。
- debug 日志只打印截断后的 body；当前 `truncate_for_log` 已有保护，继续保持。
- 大字段 `TextLimitConfig::from_env()` 可避免在每个字段重复读 env，但注意不要扩大改动面。

## 回归测试清单

实现后至少跑：

```bash
cargo test
```

若时间不够，至少跑相关模块：

```bash
cargo test anthropic::stream
cargo test anthropic::handlers
cargo test anthropic::converter
cargo test kiro::token_manager
```

重点新增/维护这些测试：

- `/cc/v1/messages` usage-gated streaming 不等流结束。
- 无 `contextUsageEvent` 时不会无限缓冲。
- `resolved_usage` 始终不低于本地 input token。
- thinking/tool_use SSE 顺序不变。
- token-aware 调度不超发 permit。
- 动态调并发时不突破新 cap。
- 本地 cache 展示不影响真实上游 request body。

## 最终验收标准

优化完成后，Native Mode 应满足：

1. 发送给上游的 system/messages/tools 与优化前语义等价。
2. Claude Code 可正常使用工具、thinking、streaming。
3. `/cc/v1/messages` 首个真实 SSE 事件不再被完整响应结束阻塞。
4. 200K 长上下文单请求仍可能慢，但 trace 能证明慢在上游 prefill，而不是代理缓冲。
5. 混合短/长请求时，短请求 P95 排队时间下降。
6. `usage.input_tokens` 不低估真实转发量，auto-compact 仍按客户端原生逻辑工作。
7. 没有新增默认启用的 prompt 改写、历史裁剪、summary、tool history flatten。

## 给实现者的优先级建议

按这个顺序做：

1. **P0：把 `/cc/v1/messages` 从 full-buffered 改成 usage-gated streaming。**
2. **P1：加 trace/log 字段，证明延迟构成。**
3. **P1：做 token-aware 调度，先保守排序，不急着 weighted semaphore。**
4. **P2：真实上游缓存实验；有证据再实现。**
5. **P2：连接池、HTTP/2、endpoint EWMA 和本地 CPU 微优化。**

如果实现过程中发现某项优化必须改变模型输入，请停止把它归入 Native Mode，改为单独的
显式 Fast Mode 设计。

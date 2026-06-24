# kiro-rs 代码审阅请求（两类问题）

> 本文档面向没有本仓库访问权限的外部审阅者。引用代码处均附 `文件:行号`（基于
> 审阅时的工作副本，行号可能随后续改动漂移，以函数名为准）。

## 项目背景

kiro-rs 是一个 Rust（Axum + Tokio）写的协议代理网关：对外暴露 Anthropic Messages
API（`/v1/messages`、`/cc/v1/messages` 等），对内把请求翻译成 AWS Q / Kiro 后端的
私有协议，再把上游的 AWS event-stream 转换回 Anthropic SSE。核心能力之一是把多个
Kiro 账号凭据组成一个带优先级 / 负载均衡 / 故障转移的令牌池。

下面两类问题，请帮忙判断诊断是否成立、修复方向是否合理。

---

## 问题 A：多账号调度系统（由新增「凭据级并发上限」特性引入 / 激活）

### 背景

近期改动给每个凭据加了可选并发上限 `max_concurrency: Option<usize>`
（`src/kiro/model/credentials.rs:82` 附近；`None` = 用全局默认
`account_max_concurrency`，`Some(n)` = 该账号专属上限），并新增了进程内调度指标
（EWMA 耗时 / 错误率、在途数、最老在途秒数，见 `CredMetrics`
`src/kiro/token_manager.rs:1083` 附近）。账号并发用 `tokio::Semaphore` 控制，
每凭据一个信号量，存在 `credential_locks: HashMap<u64, Arc<Semaphore>>`。

### A1（最关键，正确性 / 路由质量）：排序用「绝对在途数」而非「负载率」

候选凭据排序的首要键是**绝对在途请求数**
（`ranked_available_credentials`，`src/kiro/token_manager.rs:1521-1539`）：

```rust
available.sort_by_key(|e| {
    let cap = self.cap_of(&e.credentials);
    (self.in_flight_with_cap(e.id, cap),  // ← 绝对在途数
     e.success_count, e.credentials.priority, e.id)
});
```

在所有账号 `cap` 相同的旧版本里，绝对在途数 ≈ 相对负载，没问题。但现在 `cap` 可不同：

- 账号 A：`cap=2`，在途 2  → 已 **100% 打满**
- 账号 B：`cap=20`，在途 5 → 仅 **25% 负载**

排序比较 `2 < 5`，于是**优先选已打满的 A**；每个新请求先 `try_acquire` A 失败
（`NoPermits`，`src/kiro/token_manager.rs:1595`）再轮到 B，既偏向小账号又浪费一次
acquire 尝试，违背并发均衡目的。

**建议修复**：排序按负载率而非绝对值（整数千分比避免浮点）：

```rust
let cap = self.cap_of(&e.credentials).max(1);
let load = self.in_flight_with_cap(e.id, cap) * 1000 / cap;
(load, e.success_count, e.credentials.priority, e.id)
```

### A2（正确性）：`set_credential_concurrency` 直接替换整个信号量

Admin 调整某账号并发时（`src/kiro/token_manager.rs:2633` 附近），做的是
`locks.insert(id, Arc::new(Semaphore::new(new_cap)))`（`:2648` 附近），即**整体替换
信号量**。但正在跑的请求持有的是「旧信号量」的 `OwnedSemaphorePermit`，它们释放时
还回旧信号量（已无人观测）；新请求面对一个全空的新信号量。后果：

- **瞬时突破上限**：实际并发 = 旧在途（drain 中）+ `new_cap`，调小 `cap` 想限流反而
  做不到。
- **在途瞬间被低估**：换完后 `in_flight_with_cap` 读新信号量显示在途 ≈ 0，叠加 A1 会把
  更多流量灌向该账号，放大超额。

**建议修复**：不替换信号量，用 `Semaphore::add_permits` 增量扩容 / 获取后 `forget`
缩容；或把目标 `cap` 存 `AtomicUsize`，在途用独立计数器而非「`cap − available_permits`」
反推。

### A3（可观测性）：两套在途计数会脱节

系统里有两个独立的在途来源：

- **(a) 信号量推导** `cap − available_permits`（用于排序，及 snapshot 的 `in_flight`
  字段，`src/kiro/token_manager.rs:2387` 附近）。
- **(b) `metrics.active.len()`**（用于 snapshot 的 `oldest_in_flight_secs`）。

token 刷新窗口里 (a) 已占用而 (b) 尚未登记（`make_call_context` 在刷新成功后才登记，
`src/kiro/token_manager.rs:1789` 附近）；触发 A2 后两者彻底脱节，snapshot 可能出现
`in_flight=0` 但 `oldest_in_flight_secs=120s` 的自相矛盾。建议统一为单一权威来源
（推荐 `metrics.active`，它独立于信号量、跨换锁仍准确）。

### A4（次要）

- `lock_for_credential` 懒创建时忽略凭据级 `cap`、用全局值兜底
  （`src/kiro/token_manager.rs:1462`）；当前不触发（add 路径把 `max_concurrency` 硬编码
  `None`，`src/admin/service.rs:1055` 附近），但属埋雷。
- `delete_credential`（`src/kiro/token_manager.rs:3399`）不清理 `credential_locks` /
  `refresh_locks` / `metrics` 三张表；因 ID 单调不复用所以无正确性问题，但长期增删会
  无界增长。
- `effective_concurrency`（`src/kiro/token_manager.rs:1456`）是死代码（无调用点），已被
  `cap_of` + `in_flight_with_cap` 取代。
- EWMA 耗时 / 错误率只采集不参与调度（排序键里没用到）。若想做质量感知路由可把高错误率
  账号降权；若仅 Admin 展示则现状 OK。

### 已确认无问题的点

- 锁顺序无环、无死锁风险（Drop→metrics；report_*→metrics 释放后再 entries；
  ranked→entries 套 credential_locks；snapshot 分段采集后再锁 entries）。
- 429 `USER_REQUEST_RATE_EXCEEDED` 走短冷却、不计失败、不喂 error EWMA
  （`report_rate_limited`，`src/kiro/token_manager.rs:2586`）—— 这是有意设计（速率超限是
  路由问题不是账号故障），正确。

---

## 问题 B：经 Claude Code 等客户端使用时上下文不自动压缩，越用越慢

### 现象

通过本代理跑 Claude Code，对话不触发客户端的 auto-compact，上下文持续增长，响应越来
越慢。

### 前提

本代理是 pass-through，**自身不做上下文压缩**，每轮把完整对话原样转发给 Kiro，所以
延迟随对话增长本属预期。真正该触发压缩的是**客户端**，而客户端是否压缩取决于代理
回报的 `usage.input_tokens`（客户端按 `input_tokens / 模型上下文窗口` 逼近约 92% 阈值
时压缩）。问题出在这个回报值。

### 机制（已在代码中确认）

回报给客户端的 `input_tokens` 有两个来源，代码**优先用上游的、不用本地真实计数**
（`resolved_usage`，`src/anthropic/stream.rs:1107`）：

```rust
let total_real = self.context_input_tokens.unwrap_or(self.input_tokens);
```

- `context_input_tokens`（**优先**）来自上游 `contextUsageEvent` 的百分比换算
  （`src/anthropic/stream.rs:1216-1219`）：

  ```rust
  let window_size = get_context_window_size(&self.model);
  let actual = context_usage.context_usage_percentage * window_size as f64 / 100.0;
  ```

- `self.input_tokens`（**仅兜底**，无 `contextUsageEvent` 时）= 本地 `count_all_tokens`
  对完整 payload 的真实估算（`src/anthropic/handlers.rs:694`），随对话单调增长。

而 `get_context_window_size`（`src/anthropic/converter.rs:210`）对 sonnet/opus
4.6/4.7/4.8 **写死 `1_000_000`**，其余 `200_000`。

两个叠加的失效点：

1. **代理丢掉本地真实计数**：只要上游回了 `contextUsageEvent`（Kiro 基本每轮都回），
   本地 `count_all_tokens` 的真实增长值就被 `unwrap_or` 覆盖，客户端看到的是 Kiro
   自报百分比 × 1M，而非实际转发量。
2. **1M 窗口把压缩阈值推到约 92 万 token**：若 Kiro 的 `contextUsagePercentage` 相对其
   大窗口增长缓慢，回报值长期停在低位、永远到不了阈值 → 客户端不压 → 代理继续转发
   完整历史 → 越用越慢。
   （注：`context_usage_percentage >= 100.0` 时代码会设
   `stop_reason = model_context_window_exceeded`，`src/anthropic/stream.rs:1220-1223`。）

### 建议修复（按推荐度）

1. **让回报值反映真实转发量**，取两者较大值，避免上游低估盖过本地真实计数
   （`src/anthropic/stream.rs:1107`）：

   ```rust
   let total_real = self.context_input_tokens
       .map(|c| c.max(self.input_tokens))   // 不让上游低估盖过本地真实计数
       .unwrap_or(self.input_tokens);
   ```

   风险最低、最可能直接恢复 auto-compact，且不影响缓存计量口径。
2. **校准 `get_context_window_size`**，使其与客户端实际假设的有效窗口一致（注意上面
   `pct >= 100%` 会设 `model_context_window_exceeded`，调太小会过早压缩 / 报错）。
3. **核对客户端版本**对这些模型名假设的上下文窗口与此处 1M 是否一致（涉及 1M beta
   header）。

### 无法仅凭本仓库验证的部分（需运行时数据）

- Kiro `contextUsagePercentage` 实际随对话如何增长。
- 客户端版本对各模型假设的上下文窗口大小。

这两个对上即可确认主导失效点是 1 还是 2。

---

## 想请教的问题

1. 问题 A 的 **A1**（排序改负载率）与 **A2**（信号量不替换、改增量调整）诊断是否成立？
   修复方向有无更稳妥的做法？
2. 问题 B 中「优先信任上游百分比、丢弃本地真实计数」是否就是 auto-compact 不触发的
   根因？建议修复 #1（取 `max`）会不会带来 usage 口径 / 计费上的副作用？

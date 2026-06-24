# 负载均衡器重设计方案（adaptive 模式）

> 状态：设计稿，待评审。基于 2026-06 对生产 `traces.db` 的 29.6 小时、24548 请求实证分析。
> 暂不改代码。

## 1. 背景与问题

当前支持两种模式（`src/kiro/token_manager.rs::ranked_available_credentials`）：

- **priority**：排序键 `(effective_load, priority, id)`
- **balanced**：排序键 `(effective_load, success_count, priority, id)`

其中 `effective_load = load_per_mille + ewma_error 惩罚`（已有，行 937），是个好基础。
但 `balanced` 仍把 **`success_count`（累计成功次数，只增不减、跨重启持久化）** 作为第二排序键，
这是核心缺陷来源。

### 1.1 实证结论（详见 memory: kiro-rs-scheduler-findings）

| 发现 | 证据 |
|---|---|
| ① 大部分"失败"是容量耗尽，非账号坏 | 6444 个 `unknown` 全是"等待并发槽超时(30s)"，6409 个集中在 hr20（QPS 2640、错误率 53.5%）。并发峰值 ~375 > 理论容量 275 |
| ② balanced 在"喂养"生病账号 | 失败不增 `success_count` → 高失败账号计数最低 → 被判"最该用" → 持续被选。cred 13 被试 3991 次仅成 1131 次 |
| ③ 慢性差 vs 事故要分开 | 剔除事故小时后 cred 13 成功率回到 76.1%、cred 14 到 80.2%（曾被冤枉）；cred 5 即便非事故期仍仅 65.9%（真·慢性差） |
| ④ 按次均衡对额度不公 | 模型 credits 差 26×（opus-4-8 1.27 vs haiku 0.05），cred 1 烧 4857 credits vs cred 19 的 512 |

### 1.2 用户原始痛点的根因

"必须重置成功次数账号才重新参与调度" = `success_count` 单调累计 + 跨重启保留，
导致老账号/曾被禁账号长期排序垫底。**新方案应让这个痛点从根上消失。**

## 2. 设计目标

1. 调度依据改为**短时间窗口的近期健康度**（EWMA），事故恢复后账号**自动回池**，无需手动重置。
2. 避免**全局排序的羊群效应**：高并发下不让请求挤向同一个"当前最优"账号。
3. `success_count` **降级为纯展示统计**（与已废弃的 currentId 同理），不再参与调度决策。
4. 与现有 `balanced`/`priority` **并存**，灰度可切换、可回退。
5. **不引入新埋点**：复用 runtime 已有信号。

## 3. 核心算法：P2C + 健康度评分

### 3.1 为什么用 Power of Two Choices（P2C）

全局排序（当前做法）在高并发下有"羊群效应"：多个并发请求同时看到同一个"评分最优"账号、
一起涌入，直到它被打满才换下一个 → 倾斜 + 局部过载。这正是 cred 1/2 各被命中 6000+ 次、
而 cred 15-19 仅 ~700 次的成因之一。

P2C（Nginx、gRPC、Finagle 等均采用）：**从可用池随机抽 2 个候选，选评分更优的那个**。
- 无全局有序状态，O(1) 决策
- 数学上已证明能把最大负载从 O(log n) 降到 O(log log n)
- 并发请求随机分散，天然抗羊群

### 3.2 健康度评分函数

复用 runtime 已有信号（`CredentialRuntime` + `CredentialCandidate`），分数越低越优先：

```text
score(cred) = w_load  * load_ratio              // in_flight / capacity ∈ [0,1+]
            + w_err   * ewma_error               // 近期错误率 EWMA ∈ [0,1]（已有字段）
            + w_lat   * norm(ewma_duration_ms)   // 延迟 EWMA 归一化 ∈ [0,1]
            + cooldown_penalty                   // throttled_until 未到期 → +∞（硬避让）

建议初始权重（待离线回放调参）：w_load=1.0, w_err=0.6, w_lat=0.2
```

要点：
- **全部是滑动窗口/EWMA 量**，自带衰减 → 事故结束后 `ewma_error` 自然回落，账号自动回到候选 →
  **彻底消除"必须重置 success_count"的痛点**。
- `cooldown_penalty` 对 `rate_limited`/`account_throttled` 冷却中的账号给极大惩罚（等价软排除），
  到期自动恢复。
- `success_count` **不在评分内**。

### 3.3 选择流程（伪代码）

```rust
fn pick_adaptive(candidates: &[Candidate]) -> Option<&Candidate> {
    // 1. 过滤：disabled / 冷却中 / 不匹配 model·group（沿用现有 ranked 过滤逻辑）
    let pool: Vec<_> = candidates.iter().filter(is_eligible).collect();
    match pool.len() {
        0 => None,
        1 => Some(pool[0]),
        _ => {
            // 2. P2C：随机抽两个不同候选
            let a = fastrand::usize(..pool.len());
            let mut b = fastrand::usize(..pool.len() - 1);
            if b >= a { b += 1; }
            // 3. 选 score 更低者
            Some(if score(pool[a]) <= score(pool[b]) { pool[a] } else { pool[b] })
        }
    }
}
```

实际接入点：`acquire_idle_permit`（行 ~1714）的 `ranked_available_credentials` 调用。
新模式下不再返回全排序 Vec，而是返回候选池让 P2C 现场抽选；抢槽失败（NoPermits）时
重新抽选而非顺序遍历。

### 3.4 容量耗尽的兜底（与调度正交，但必须配套）

hr20 证明：**算法再好也救不了容量不足**。配套项（可分阶段做）：
- **自适应并发上限**：健康账号（低 ewma_error、低延迟）的 cap 在高负载时自动上调；
  问题账号下调。比静态 `accountMaxConcurrency=25` 更弹性。
- **事故期熔断**：当全池 `ewma_error` 同时飙高（如本次 hr20），判定为上游全局事故，
  对 `transient` 改用更长退避，避免重试风暴把 53% 错误进一步放大。
- **加号告警**：当 `slot_timeout` 在窗口内超阈值，Admin UI 提示"高峰容量不足，建议增加账号"。

## 4. "均衡"的语义选择（需你拍板）

发现 ④：按成功次数均衡 → 额度消耗 10× 倾斜。两个选项：

- **A. 均衡调度次数**（现状目标）：简单，但贵账号烧得快、便宜账号闲置。
- **B. 均衡额度消耗**：评分加一项 `w_credit * norm(累计 credits in window)`，让烧得多的账号降权。
  更贴合"让所有账号额度同步消耗、整体寿命最长"的运营目标。

建议：adaptive 模式默认走 **A**（调度均衡由 P2C 自然达成），把 B 作为可选权重项
（`w_credit` 默认 0，需要时打开），避免一次引入过多变量。

## 5. 灰度与回退

- 新增枚举值 `LoadBalancingMode::Adaptive`，与 `Priority`/`Balanced` 并存。
- Admin UI 负载均衡模式选择器加一项；运行时可热切换（已有 `set_load_balancing_mode`）。
- 出问题一键切回 `balanced`，零风险回退。
- 权重做成 config 可调（`adaptiveWeights`），便于不改代码调参。

## 6. 离线回放验证（落地前的关键一步）

用这份历史 `traces.db` 做**反事实回放**，在不碰生产的前提下对比新旧策略：

1. 从 `traces` + `trace_attempts` 重建每个请求的到达时刻、模型、真实耗时、各账号当时的成败。
2. 用一个离线模拟器按时间轴重放：分别用 balanced 和 adaptive 做选择决策。
3. 对比指标：
   - **倾斜度**：各账号命中次数的基尼系数 / 最大最小比
   - **预估成功率**：避开当时正在失败的账号能挽回多少（用同账号同分钟的真实成败近似）
   - **额度均衡度**：credits 消耗的标准差
   - **被饿死账号数**：命中次数 < 阈值的账号

模拟器可作为一次性脚本（Rust test 或 Python），不进主线。这能在改生产前用真实数据
量化收益，避免拍脑袋调权重。

## 7. 实施阶段建议

| 阶段 | 内容 | 风险 |
|---|---|---|
| P0 | 离线回放模拟器 + 权重初调（用历史数据） | 无（不碰生产） |
| P1 | 新增 `Adaptive` 模式 + P2C + 评分，灰度开关，默认仍 balanced | 低（可热切回退） |
| P2 | `success_count` 降级为纯展示（调度不再读它） | 低 |
| P3 | 自适应并发上限 + 事故熔断 + 容量告警 | 中（涉及 cap 动态调整） |

## 8. 不做什么

- 不删 `success_count` 字段本身（前端统计、`reset` 接口仍用，只是调度不读）。
- 不动 `priority` 模式语义（用户显式按优先级时仍可用）。
- 不改重试分类逻辑（`kiro/endpoint` 的错误识别已经很完善）。

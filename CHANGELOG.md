# Changelog

All notable changes to this project are documented in this file. The format
loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [0.8.0] - 2026-08-26

主题：**Responses / Codex 流式链路稳定性、客户端 Key 配额治理、运维控制台与凭据元数据管理，以及 Enterprise / IdC 兼容性修复**。本版合并 PR #66、#67、#70、#71、#74、#75，并补充后续的控制台主题、刷新和移动端布局改进。新增字段均提供默认值，升级无需迁移现有配置、凭据或客户端 Key 文件。

### ✨ 新增 — 按入口 Key 用量分布与积分上限

> 来源：[PR #70](https://github.com/ZyphrZero/kiro.rs/pull/70)。提交人：[@bestK](https://github.com/bestK)，感谢贡献。

- 新增 `GET /api/admin/stats/by-key` 与「按入口 Key 分布」面板，按时间窗横向汇总调用次数、输入/输出/缓存 Token、异常数和 credit，并支持按分组过滤。
- `ClientKey` 支持 `maxCredits` 累计积分上限；新增 `POST /api/admin/client-keys/{id}/max-credits`，创建 Key 时也可设置，传 `null` 可清除限制。
- 达到上限的请求返回 HTTP 429 `rate_limit_error`，不累加调用次数，也不写入用量日志与链路追踪；重置统计后可重新计费。
- 上限是请求结束时入账的软配额，并发中的请求可能在最终计量前同时通过检查，实际用量可能略超配置值。

### ✨ 新增 — 运维控制台、凭据元数据与模型管理

> 来源：[PR #71](https://github.com/ZyphrZero/kiro.rs/pull/71)。提交人：[@bestK](https://github.com/bestK)，感谢贡献。

- 凭据、设置、请求日志三页重构为运维控制台：设置项集中管理并即时保存，日志支持时间范围 / 关键字筛选、URL 状态同步、列控制和右侧详情抽屉，凭据页提供状态筛选条、分页、批量操作和优先级预览。
- 新增可扩展凭据 Metadata schema：字段定义、默认值、`oneOf` 描述和值标签由后端统一提供；支持实时预览、全局缓存刷新、批量编辑 / 导入，以及凭据卡片和列表中的元数据展示。
- 新增自定义模型管理与凭据编辑 Tab，按厂商组织模型设置；校验并报告模型持久化失败，代理 URL / 认证字段校验更严格，凭据级代理故障时可自动切换，全局代理支持独立用户名和密码。
- 优先级调度严格跳过无效凭据；空字符串 `proxyUrl` 按未配置处理，避免启动失败。

### 🔧 修复 — Responses / Codex 流式传输与 WebSearch 长连接

> 来源：[PR #67](https://github.com/ZyphrZero/kiro.rs/pull/67)。提交人：[@stormrise](https://github.com/stormrise)，感谢贡献。

- Responses API 改为增量翻译 Anthropic 事件，稳定输出 item / SSE 顺序，减少 Codex 长连接中的断流、空响应和重复结束事件。
- WebSearch 多轮请求持续发送保活与搜索进度；客户端取消会向上游传播，错误、中断和正常结束统一结算 usage 与 trace，避免悬挂请求或漏记计量。
- 改进近期 Codex reasoning、function / custom 工具及工具结果续接的兼容性，保持与 Anthropic 流式链路一致。

### 🔧 修复 — 混合 WebSearch 请求的链路追踪

> 来源：[PR #66](https://github.com/ZyphrZero/kiro.rs/pull/66)。提交人：[@KtzeAbyss](https://github.com/KtzeAbyss)，感谢贡献。

- 混合 `web_search` + 客户端工具的请求现在完整记录 trace、尝试链路和最终状态；内部搜索循环不再让请求从追踪统计中消失。
- 统一混合场景的 WebSearch 路由、工具去重和失败传播，便于在 Admin 日志中还原实际执行过程。

### 🔧 修复 — Enterprise / IdC 的真实 `profileArn`

> 来源：[PR #74](https://github.com/ZyphrZero/kiro.rs/pull/74) 与 [PR #75](https://github.com/ZyphrZero/kiro.rs/pull/75)。提交人：[@lijmyeah](https://github.com/lijmyeah)、[@ZyphrZero](https://github.com/ZyphrZero)，感谢贡献。

- `getUsageLimits`、`ListAvailableModels` 等用量 / 模型接口调用前解析并携带真实 `profileArn`，修复 Enterprise / IAM Identity Center 凭据刷新余额和模型列表时的 403。
- `setUserPreference` 同样按账号解析真实 ARN；Builder ID 占位 ARN 不再误用于需要真实用户配置的请求。
- 复用已有的凭据级解析与缓存逻辑，解析失败时返回明确错误，不改变普通 Builder ID / Social 请求行为。

### 🎨 Admin UI 体验改进

- 新增可持久化的多主题选择器，主题偏好写入本地存储并在启动时恢复。
- 控制台页面统一页头、自动刷新与刷新状态；模型、Metadata、日志和分组页面的筛选与操作布局更紧凑。
- 修复客户端 Key 表格圆角裁切、凭据卡片与批量操作栏在窄屏下的溢出和遮挡，移动端菜单与操作按钮布局更加稳定。

### 🔒 兼容性与测试

- `Cargo.toml`、`Cargo.lock`、`admin-ui/package.json` 版本统一为 `0.8.0`。
- 新增配置、客户端 Key、凭据 Metadata 字段均兼容旧文件；未设置 `maxCredits` 时行为与此前一致。

## [0.7.6] - 2026-08-13

主题：**修复 GPT-5.6 推理参数、OpenAI 会话缓存与 Token 用量统计，同时校正 Claude Code 会话隔离和 Opus 5 上下文窗口识别**。本版聚焦协议转换与计量准确性，不新增配置项或迁移步骤。

### 🔧 修复 — GPT-5.6 推理参数与 OpenAI 上游缓存

> 来源：[PR #64](https://github.com/ZyphrZero/kiro.rs/pull/64)。提交人：[@kiim-wong](https://github.com/kiim-wong)，感谢贡献。

- **按模型族生成正确的 effort 字段**：GPT-5.6 sol/terra/luna 现在通过 `additionalModelRequestFields.reasoning.effort` 传递 `none`、`low`、`medium`、`high`、`xhigh` 或 `max`；Claude 模型继续使用 `output_config.effort`。
- **为 OpenAI 请求建立稳定的会话亲和**：Chat Completions 与 Responses 会依次从 `prompt_cache_key`、`x-session-affinity`、`x-client-request-id` 和 `session_id` 提取 UUID，并复用为 Kiro `conversationId`，使同一会话能够命中上游缓存。
- **保持无状态请求的原有边界**：会话标识缺失或非法时仍生成随机 `conversationId`，不会把不同请求错误归入同一会话。

### 📊 修复 — OpenAI Token 与缓存用量统计

- **优先采用服务端精确计量**：解析 `metadataEvent.tokenUsage` 中的未缓存输入、缓存写入、缓存读取和输出 Token；服务端未提供时再依次回退到上下文用量与本地估算。
- **修正 OpenAI Usage 映射**：`input_tokens` 现在包含未缓存输入、缓存写入和缓存读取；Responses API 的 `cached_tokens` 对应实际缓存读取量。
- **统一流式请求的最终用量**：多轮 Web Search 会累加各轮计量，正常完成、错误和中断路径使用同一份最终用量快照。

### 🔧 修复 — Claude Code 缓存计量的会话隔离

> 来源：[PR #63](https://github.com/ZyphrZero/kiro.rs/pull/63)。提交人：[@childe](https://github.com/childe)，感谢贡献。

- **支持 JSON 形态的 `user_id`**：缓存计量会从 Claude Code 当前发送的 JSON 元数据中提取 `session_id`，恢复共享系统密钥下的会话级缓存隔离。
- **兼容旧格式**：JSON 解析失败时继续识别原有的 `..._session_<uuid>` 字符串，既有客户端无需迁移。
- **避免并行会话相互干扰**：使用客户端密钥时，缓存作用域不再因 JSON 元数据无法识别而退化到密钥级别。

### 🔧 修复 — Opus 5 的 1M 上下文窗口

> 来源：[PR #61](https://github.com/ZyphrZero/kiro.rs/pull/61)。提交人：[@lijmyeah](https://github.com/lijmyeah)，感谢贡献。

- **将 `claude-opus-5` 纳入 1M 模型族**：Opus 5 及其别名、带后缀模型名不再错误回退到 200K 上下文窗口。
- **校正上下文进度与自动压缩时机**：Context Usage 换算基于正确的窗口大小，避免用量显示偏低以及上下文接近上限时未及时触发压缩。
- **补充模型识别回归测试**：覆盖 Opus 5 的标准名、别名和后缀形式，并确保 Opus 4.5 不被误判为 1M 模型。

### 🔒 兼容性

- 无新增配置项、依赖或数据迁移。
- 旧式 Claude Code 会话标识继续受支持；缺少合法会话标识的 OpenAI 请求继续使用随机会话 ID。

## [0.7.5] - 2026-08-05

主题：**为多账号调度加入单账号 RPM 主动限流，并集中增强管理端的凭据筛选、批量操作、创建时间、请求计费与移动端可用性**。本版同时加固 WebSearch MCP 的查询参数兼容和 Enterprise / IdC 路由；新增配置默认关闭或带有 `serde(default)`，旧 `config.json` 与 `credentials.json` 无需迁移。

### ✨ 新功能 — 单账号 RPM 主动限流

> 来源：[PR #55](https://github.com/ZyphrZero/kiro.rs/pull/55)。提交人：[@bestK](https://github.com/bestK)，感谢贡献。

- **每凭据独立滑动窗口**：新增 `accountRpmLimitEnabled`（默认 `false`）与 `accountRpmLimit`（默认 `60`），每个账号独立维护 60 秒请求窗口；达到上限后临时退出候选，请求自动故障转移到下一可用账号。
- **只统计真实业务请求**：Admin 模型发现等只读操作不占用额度；配置可通过 `GET|PUT /api/admin/config/account-rpm-limit` 在运行时读取、修改并持久化。
- **并发额度原子预留**：过期清理、上限校验与请求记账在同一把凭据锁内完成；并发请求竞争失败时重新选择账号，不会同时穿透只读检查而超过配置上限。
- **标准 429 响应**：所有匹配账号都耗尽 RPM 时返回类型化 HTTP 429，并按最早释放的滑动窗口计算 `Retry-After`，不再退化为“所有凭据均已禁用”的通用错误。
- **完整管理端设置**：顶栏提供启停、常用预设与自定义每分钟上限；移动端与桌面端复用同一配置面板。

### ✨ 增强 — 凭据列表与批量操作

> 来源：[PR #56](https://github.com/ZyphrZero/kiro.rs/pull/56) 与 [PR #58](https://github.com/ZyphrZero/kiro.rs/pull/58)。提交人：[@bestK](https://github.com/bestK)，感谢贡献。

- **多字段排序**：支持按优先级、成功次数、累计失败、最后使用时间与 ID 排序；重复选择同一字段可切换升降序，“从未使用”始终排在末尾，同值使用 ID 稳定排序。
- **按状态隐藏**：可组合隐藏当前优先、已启用、已禁用、冷却中和已超额凭据；排序或筛选变化后自动回到第一页。
- **排序与拖拽语义隔离**：仅“手动顺序”允许拖拽调整优先级，字段排序期间隐藏拖拽手柄，避免视觉顺序与服务端优先级混淆。
- **批量删除进度与失败重试**：逐项展示删除进度和最终成功/失败统计；部分失败时仅保留失败凭据的选择状态，方便直接重试，并确保异常路径也会退出删除中状态。
- **记录凭据添加时间**：凭据新增可选 RFC3339 `createdAt`，所有新增入口统一补写，导入数据已有时间则保留；旧凭据无值时显示“未知”，无需迁移存量文件。

### 📊 增强 — 请求计费与缓存效率

> 来源：[PR #60](https://github.com/ZyphrZero/kiro.rs/pull/60)。提交人：[@bestK](https://github.com/bestK)，感谢贡献。

- **Trace 详情新增计费面板**：展示上游 `meteringEvent` 的真实 credit、每千输入 Token 的 credit，以及缓存创建、缓存读取和未缓存输入等拆分指标。
- **修正计费效率分母**：每千输入 credit 使用“未缓存输入 + cache creation + cache read”的总输入作为分母；存在缓存拆分时单独展示未缓存输入，避免高缓存命中请求被计算成异常高成本。

### 🔧 修复 — WebSearch MCP 路由加固

> 来源：[PR #57](https://github.com/ZyphrZero/kiro.rs/pull/57)。提交人：[@soeric](https://github.com/soeric)，感谢贡献。

- **兼容多种查询入参**：统一提取 `query`、`search_query`、`q`、`queries` 及嵌套 `text` / `value`，自动去除首尾空白并选择首个非空查询。
- **区分空结果与真实失败**：缺少有效查询时不调用 MCP；上游明确返回“无结果”时作为空搜索继续，其它 MCP 错误仍显式传播，不伪装为成功。
- **Enterprise / IdC 搜索可用**：纯 MCP / WebSearch 请求在调用前补齐 `profileArn`，与常规模型请求保持一致，同时继续遵守客户端 Key 的凭据分组隔离。

### 📱 修复 — 移动端管理体验

- **客户端 Key 操作列固定**：宽表横向滚动时最后的编辑、启停、重置与删除操作列始终固定在右侧，并使用实体背景和边界避免内容透叠。
- **自愈与限流选项不再缺失**：移动端紧凑菜单展示完整的自愈开关、403 封禁识别、冷却间隔、连续轮数、RPM 开关、预设和自定义上限。
- **长菜单适配动态视口**：顶部工具菜单限制在移动端动态视口内并支持纵向滚动，避免浏览器地址栏或短屏裁掉底部配置。

### 🔒 兼容性与测试

- RPM 限流默认关闭；新增配置与 `createdAt` 均兼容旧文件，升级不改变现有账号的默认调度行为。
- 新增 WebSearch 查询规范化、MCP 空结果、RPM 滑动窗口与 429、凭据添加时间等回归测试；Rust 全量测试与 Admin UI 类型检查、生产构建均通过。

## [0.7.4] - 2026-07-28

主题：**修复 IdC / Enterprise 重新登录后 Token 无法刷新，以及持续 403 场景下“全账号自愈”陷入 `全禁 → 自愈 → 403 → 再禁` 死循环的问题**。本版合并 [PR #52](https://github.com/ZyphrZero/kiro.rs/pull/52) 与 [issue #51](https://github.com/ZyphrZero/kiro.rs/issues/51) 的修复：重新登录会整体替换与 OIDC 客户端绑定的凭据；账号池则精准识别 403 封禁，并通过配置驱动的**节流 + 连续上限 + 可观测**治理自愈行为。新增配置字段均 `serde(default)`，旧 `config.json` 无需改动。

### 🔧 修复 — IdC / Enterprise 重新登录凭据失配

> 来源：[PR #52](https://github.com/ZyphrZero/kiro.rs/pull/52)。提交人：[@Xm798](https://github.com/Xm798)，感谢贡献。

- **整体替换 OIDC 客户端绑定凭据**：IdC refresh token 与注册时生成的 `clientId` / `clientSecret` 绑定；重新登录不再只写入新 refresh token，而是同步替换 access token、refresh token、客户端注册、区域、Start URL 与 provider，避免下一次刷新因“新 token + 旧客户端”组合返回 `invalid_grant`。
- **清理已失效或跨认证方式的字段**：重新登录后清除属于旧身份的 `profileArn`，使其在后续请求中重新解析；同时移除 `tokenEndpoint` / `issuerUrl` / `scopes` / `kiroApiKey` 等非 IdC 残留字段，并保留邮箱、API 区域、分组等与本次客户端注册无关的账号配置。
- **Enterprise 不再静默降级**：重新登录请求未显式提供 Start URL 或区域时继承原凭据配置，不会把 Enterprise 账号按 Builder ID 默认端点重新注册。
- **失败不再伪装成成功**：上游未返回 refresh token 时显式报错；失效 refresh token 归类为 HTTP 400，管理端可提示重新登录，而不是返回 500 或在未更新凭据时报告成功。
- **收紧并发窗口**：强制刷新先取得凭据刷新锁再读取快照，避免与重新登录并发时继续使用旧凭据；完成内存替换后先释放全局刷新锁再持久化，防止文件写入阻塞其它账号刷新。
- **合并后的健康状态一致**：重新登录会重置失败、禁用、自愈连续轮数、冷却时间与模型状态，但保留累计自愈次数；与本版新增的每凭据自愈状态可以共同工作。

### 🐛 问题背景

当所有凭据均因连续失败被自动禁用时，系统会执行"自愈"——重置失败计数并重新启用（等价于重启）。旧实现**无冷却、无上限**：持续 403 会形成 `全禁 → 自愈(重置) → 403 → 累计 3 次 → 全禁 → 自愈` 的紧密死循环，表现为自愈日志刷屏、持续无效打上游、面板状态抖动。其中一类高频根因是**账号被上游封禁**（响应体形如 `Your User ID (...) temporarily is suspended. We've locked your account ...`）——这类凭据不可能自愈恢复，重置只是徒劳地推迟下一次失败。

### 🔒 修复方案一：403 账号封禁识别

- 新增端点级 `is_account_suspended`：仅当 403 响应体**同时**命中 `suspended` 与 `locked your account` 两个高特异短语（大小写不敏感）时判定为封禁。只针对这类明确文案，**不影响**普通 403（权限/WAF/区域抖动），避免误伤瞬态 403。
- 命中后立即标记凭据为 `Suspended` 并禁用、切换到下一个可用凭据，**不累计、不参与自愈**；需人工联系客服核实后经 Admin API / 面板手动重置（误判逃生途径）。
- 新增配置项 **`suspendedDetectionEnabled`（默认 `true`）** 作为总开关；trace 新增 `account_suspended` 分类；管理面板凭据卡片新增「账号封禁」徽标。

### 🔧 修复方案二：自愈治理（配置入手）

对齐既有账号级风控配置的运行时可改 + 持久化模式，新增 3 个 `selfHeal*` 配置项，并将恢复状态从全局状态重构为每凭据状态：

- **`selfHealEnabled`（默认 `true`）**：凭据自愈总开关。关闭后当前请求池全灭即直接失败。
- **`selfHealMinIntervalSecs`（默认 `300`）**：同一凭据两次自愈的最小冷却间隔，将持续故障下的探测频率限制为每 5 分钟一次。
- **`selfHealMaxConsecutiveRounds`（默认 `5`，`0`=不限）**：同一凭据、同一模型连续自愈达到上限后停止；其它凭据、分组或模型的成功不会重置该计数。
- 自愈只恢复当前 `model/group` 作用域内可路由的凭据；不存在的分组、不支持的模型和纯 429 冷却不会修改无关凭据。
- `disabledReason`、连续轮数、累计恢复次数和最近恢复时间随凭据原子落盘，重启不会重新启用 `Suspended` 账号或绕过连续上限。
- 所有运行时 `config.json` 部分更新经共享锁串行化，避免并发 PUT 丢字段。

### 📊 可观测性

- 自愈日志记录请求 model/group、恢复凭据 ID 和数量，达上限时按凭据输出人工介入提示。
- 新增 Admin API `GET|PUT /api/admin/config/self-heal`，读写全部 4 个开关（含 `suspendedDetectionEnabled`）并返回只读观测值 `consecutiveRounds` / `totalCount`。
- 管理面板顶栏新增「凭据自愈」设置项，并展示最大连续轮数与累计恢复凭据次数。

### 🔒 兼容性

- 封禁识别只匹配 `suspended` + `locked your account` 两个高特异短语同时出现的情形，普通 403 仍走既有累计路径，不误伤瞬态 403。
- 全部新增字段 `serde(default)`，缺省即默认值；如需完全回退旧行为，可将 `suspendedDetectionEnabled` 设为 `false`、`selfHealMinIntervalSecs` 与 `selfHealMaxConsecutiveRounds` 均设为 `0`。

## [0.7.3] - 2026-07-28

主题：**以 Kiro 上游实际返回的模型目录替代本地静态列表，新增按凭据缓存、分组聚合和模型感知路由，并开放未知合法模型 ID 的直接透传**。本次兼容性补丁同时扩展了 Admin 模型面板：可按账号池策略查询模型、查看输入/输出 Token 上限，并发送真实的最小化请求验证模型。已有 `customModels`、`-thinking` 请求方式和静态上下文估算继续兼容。

### ✨ 新功能 — 动态模型发现与模型感知路由

- **`GET /v1/models` 改为上游动态目录**：按当前客户端 Key 的凭据分组查询可访问账号，合并并去重各账号实际返回的模型；保留上游显示名和输入/输出 Token 上限，自定义模型元数据优先，最终按模型 ID 稳定排序。
- **逐凭据模型缓存**：新增 `modelCacheTtlSecs` 配置（默认 `3600` 秒），每个凭据独立缓存并使用 singleflight 锁避免并发重复刷新；启动后后台预热。刷新失败时可继续使用最后一次成功结果，部分凭据失败不会丢弃其它凭据已经取得的模型。
- **缓存随凭据状态失效**：编辑代理、刷新或替换凭据、删除凭据以及整体重载时会清理对应缓存，避免模型目录与实际账号配置脱节。
- **路由优先选择已确认支持模型的凭据**：账号缓存明确包含目标模型时优先使用，明确不包含时跳过；尚无缓存的账号仍可尝试，确保上游模型列表临时不可用时不会退化成本地硬白名单。该规则同时适用于 `priority` 和 `balanced` 模式，并保持客户端 Key 分组隔离。

### ✨ 增强 — 开放模型 ID 透传

- **未知模型不再被静态映射表拦截**：`customModels` 显式别名仍具有最高优先级；常见 Claude 日期后缀、`latest`、`-thinking`、点号/连字符版本和旧式命名会先规范化，其余非空且格式合法的 ID（如 `glm-5`、`minimax-m2.5`、`deepseek-3.2`）原样下发给 Kiro，由上游决定可用性。
- **未来 Claude 型号兼容**：Claude 模型规范化不再局限于当前硬编码版本，新增型号和常见别名可复用现有请求路径；未知动态模型不会盲目附加尚未确认支持的 `output_config`。
- **请求默认值与校验补齐**：Anthropic 请求缺省 `max_tokens` 时使用 `32000`，显式传入 `0` 或负数会返回参数错误；动态目录未提供输出上限时同样展示 `32000`，GPT-5.6 和自定义模型的既有上限仍保留。

### ✨ 新功能 — Admin 模型查询与真实请求测试

- **账号池模型查询**：新增 `GET /api/admin/models`，按正常账号池策略选择凭据并返回 `specified` / `priority` / `balanced` 选择方式；原单凭据模型接口也返回选择方式和 `maxOutputTokens`。
- **真实模型验证**：新增 `POST /api/admin/models/test`，使用所选凭据向指定模型发送最小化请求，返回响应文本、凭据 ID、耗时及可用的 credit 计费信息，便于区分“目录可见”和“实际可调用”。
- **管理端模型面板升级**：支持当前账号池与指定凭据两种查看方式，展示模型 ID、名称和输入/输出 Token 上限，并可直接触发真实请求测试；刷新模型不会修改账号池的调度指针。

### 🔧 修复 — 负载模式状态语义

- **均衡模式不再伪造当前账号**：`balanced` 模式下状态接口的 `currentId` 固定为 `0`，所有凭据的 `isCurrent` 固定为 `false`；`priority` 模式继续准确展示当前优先凭据。
- **只读查询不扰动调度**：模型目录查询所需的账号选择不会增加成功计数或改写均衡调度状态；从均衡模式切回优先级模式时会重新选择最高优先级的可用凭据。真实模型测试仍走正常请求链路并反映实际账号状态。
- **不再发布合成的 thinking 别名**：动态模型列表只展示上游真实模型与显式自定义模型，不额外生成 `-thinking` 条目；请求中使用 `-thinking` 后缀自动启用 Thinking 的兼容行为保持不变。

### 🧪 测试

- 新增动态目录聚合、Token 上限、自定义模型覆盖、开放透传与非法 ID、`max_tokens` 校验、逐凭据缓存 TTL / singleflight / 失效、陈旧缓存降级、分组隔离、模型感知路由，以及 priority / balanced 状态切换和只读选择等回归测试。

## [0.7.2] - 2026-07-26

主题：**新增 `config.json` 配置驱动的自定义模型映射，并把上游 meteringEvent 的 credit 计费字段透传到 Anthropic / OpenAI 响应的 usage 对象**。本次为兼容性补丁版本：自定义模型默认空数组、完全向后兼容，credit 字段仅在收到 meteringEvent 时才追加，不影响任何既有响应结构。

### ✨ 新功能 — 自定义模型支持

> 来源：[PR #46](https://github.com/ZyphrZero/kiro.rs/pull/46)。提交人：[@bestK](https://github.com/bestK)，感谢贡献。

- **`config.json` 新增 `customModels` 数组**：把任意客户端模型别名映射到 Kiro 后端模型 ID，并可声明 `displayName` / `contextWindow` / `maxTokens` / `supportsReasoning` / `ownedBy`。自定义条目按 `id`（大小写不敏感）精确匹配，**优先于**内置关键词模糊映射——既能新增模型，也能覆盖内置模型的后端指向。
- **thinking 后缀回退**：客户端传 `<alias>-thinking` 而无同名精确条目时，自动剥离后缀回退到 `<alias>`，与内置映射对 thinking 变体的处理一致。
- **`GET /v1/models` 展示**：所有自定义模型追加到列表尾部（保持配置顺序）。
- **上下文窗口 / reasoning**：设了 `contextWindow` 时以其为准；`supportsReasoning: true` 让对应 backend_id 放行 `additionalModelRequestFields`。
- **零透传实现**：复用项目既有的 `OnceLock` 全局配置惯例（同 `token.rs`），启动时装载一次只读注册表，`map_model` / `get_context_window_size` / `available_models` 内部查表，未改动任何函数签名。`/v1/chat/completions` 与 `/v1/responses` 因复用同一映射链路自动生效。默认空数组，向后兼容。

### ✨ 新功能 — meteringEvent credit 字段透传

> 来源：[PR #47](https://github.com/ZyphrZero/kiro.rs/pull/47)。提交人：[@childe](https://github.com/childe)，感谢贡献。

- **usage 携带 credit 计费元数据**：把上游 meteringEvent 的 `usage` / `unit` / `unitPlural` 透传到 Anthropic 与 OpenAI 响应的 usage 对象（`credit_usage` / `credit_unit` / `credit_unit_plural`），让客户端拿到与 Kiro 后端一致的计费口径——与 kiro-rs 行为对齐。
- **四条出口一并接线**：非流式 handler、流式 stream、OpenAI（chat completions）与 websearch_loop 均注入；字段仅在确实收到 meteringEvent 时才追加，未下发时 usage 结构保持原样，不影响既有客户端解析。
- **metering 解析字段补齐**：`MeteringEvent` 新增 `unit` / `unit_plural` 持久化字段（默认空串），解析失败仍由 ParseError 上抛，新增空载荷默认值测试覆盖。

## [0.7.1] - 2026-07-15

主题：**打通 Codex CLI 完整工具链——桥接 function / custom / namespace 工具到 Anthropic 模型，并修复工具结果后空响应导致任务误标记完成的问题**。0.7.0 引入了 Responses 端点使 Codex CLI 能连接 kiro-rs，但此前仅支持纯聊天与 Web 搜索——Codex 的真实工具（shell / apply_patch / view_image / MCP 等）被全部剥离，导致 Codex 无法读写文件、执行命令或编辑代码。本版补全工具桥接的全链路：从 Codex 的工具声明收集、到 Anthropic 模型侧的 schema 翻译、再到响应侧按声明类型正确生成 `function_call` 或 `custom_tool_call`——实现 Codex CLI 与 kiro-rs 的完整能力对齐。

> 来源：[PR #39](https://github.com/ZyphrZero/kiro.rs/pull/39)。提交人：[@yeeyon](https://github.com/yeeyon)，感谢贡献。

### ✨ 新功能 — Codex CLI 完整工具桥接

- **请求方向收集工具声明**：同时从 `req.tools`（顶层）和 `additional_tools` input item（Codex 0.144 把工具声明放在此处）收集工具定义，合并转换后转发给上游模型，不再忽略 Codex 的真实工具。
- **区分 `function` 与 `custom` 工具类型**：Codex 要求应答 item 类型与声明严格一致（否则抛出 "tool invoked with incompatible payload" 并终止本轮）。`function` 类型（shell / MCP / view_image 等）→ 应答 `function_call`（JSON arguments）；`custom` 类型（apply_patch / code-mode exec 等自由文本工具）→ 应答 `custom_tool_call`（原始字符串 input）。每请求维护一张 `ToolKindMap`，请求翻译时生成、响应构造时消费，保证出方向 item 类型永远正确。
- **自由文本工具包装与解包**：Anthropic 侧没有自由文本工具概念，进方向将 custom 工具包装为 `{"input": <string>}` 单字段 schema（grammar / format 附到 description 提示模型输入格式），出方向通过多级回退链解出原始 input 字符串（模型偶尔不守 schema 时也能兜住）。
- **Namespace 分组支持**：Codex 0.144 的 collaboration 子代理等工具挂在 `namespace` 分组下——对 Anthropic 模型展平为 `ns__name`（`__` 连接，避免与工具名中的 `.` 冲突），应答时还原为原 `name` + `namespace` 字段。
- **混合工具集的 Agentic Loop**：有 Codex 工具时仍注入原生 `web_search_20250305`（除非客户端已声明同名工具），请求进入 web_search agentic loop——loop 内部消化 web_search、把其它 client 工具的 `tool_use` 原样透传，实现搜索与代码工具无缝共存。
- **软化 System Prompt**：有 Codex 工具时使用软化 nudge（"Use your other tools normally for all other work"），替代无工具时的严格 nudge（"Do not call any other tool"），让模型自由选择搜索或执行代码工具。
- **Developer 角色 → System 映射**：Codex 的 `role:developer` message item（AGENTS.md / user_instructions / environment_context）转为 Anthropic system 消息，确保技能文件和环境上下文到达模型。

### ✨ 新功能 — 推理摘要与搜索展示（Phase 2）

- **推理摘要 `reasoning` item**：从上游 reasoning 事件收集思考文本，通过顶层 `kiro_thinking` 字段（非 content block）出带传递——Anthropic 客户端忽略未知顶层字段，不会回放未签名的 thinking block；Responses 译者则将其渲染为 `reasoning` summary item，供 Codex UI 展示"模型正在思考"。
- **`web_search_call` 展示项**：内部代答的 web_search 以 `server_tool_use` 块收集，在 Responses 响应中渲染为 `web_search_call` item（含 query 与 status），Codex 界面可展示 "Searched the web"。
- **SSE 事件序列补齐**：新增 `reasoning`、`custom_tool_call`、`web_search_call` 三类 output item 的完整 SSE 事件序列（added → delta/part → done），每个 item 保证 `output_item.done` 携带完整内容（Codex 仅从 done 构建回合）。

### 🔧 修复 — 工具结果后空助手响应

- **问题**：上游 Kiro 偶尔在收到 `tool_result` 后返回一个只有思考文本（无可见 assistant text、无 client 工具调用）的回合——旧代码将其序列化为 `end_turn`，导致 Codex 将该回合视为任务完成、在工具尚未执行完时错误标记任务结束。
- **修复**：新增 `empty_tool_result_disposition` 判别——仅当最后一轮 user 消息含 `tool_result`、且助手回合无可见文本、无工具调用、无终止原因时，判定为"空洞继续"。此时重试一次；重试仍空洞则返回 `502 Bad Gateway`（而非静默标记完成）。纯思考文本本身不足以构成有效继续——Codex 需要真实 assistant 文本或 client tool call 才能保持任务生命周期正确。
- **kiro_thinking 不重复**：只有被接受的（非空洞）回合的思考文本才累积到 `all_thinking`；被丢弃的空洞回合的思考不被回放，避免与成功回合的总结重复或矛盾。

### 📝 文档

- **README 更新**：反映项目已支持 OpenAI Chat Completions / Responses 端点与 Codex CLI；补充 GPT-5.6 模型族说明、流式格式说明、部署示例更新到 0.7.0。

### 🧪 测试

- **20 个纯单元测试**（无网络依赖）覆盖：工具声明收集（additional_tools / 顶层 / 混合）、namespace 展平与还原、custom 工具包装 schema、function 工具 schema 原样映射、noop 回退、nudge 软化、web_search 名字冲突处理、custom_tool_call 回放往返、function_call_output 数组 stringify、developer→system 映射、reasoning/web_search_call/compaction 跳过、custom_input 多级回退链、build_view 输出类型与顺序、SSE 事件完整性。
- **4 个空响应判别测试**：工具结果后空洞重试一次→失败、有文本/工具调用则不重试、仅思考文本无可见输出仍触发重试、仅最后一条消息决定是否为工具继续场景。
- E2E 验证（gpt-5.6-sol + claude-sonnet-4-6）：shell 读文件、apply_patch 写文件、多轮循环、实时 web_search、只读 /plan 项目研究、技能发现——均无 incompatible payload 错误。

## [0.7.0] - 2026-07-15

主题：**新增 GPT-5.6 模型与 OpenAI Chat Completions / Responses 兼容端点，并统一入口 API Key 的生成与自定义配置语义**。OpenAI 协议客户端（包括仅支持 Responses API 的新版 Codex CLI）现在可以直接复用 Kiro 的模型映射、凭据故障转移、用量计量、工具调用与 WebSearch 链路；同时，程序生成的入口 Key 统一使用 `sk-` 前缀，鉴权不再限制前缀，`config.json` 中的 `apiKey` 可使用任意自定义值并作为系统密钥的权威配置。

### ✨ 新功能 — GPT-5.6 与 OpenAI 协议兼容

> 来源：[PR #38](https://github.com/ZyphrZero/kiro.rs/pull/38)。提交人：[@yeeyon](https://github.com/yeeyon)，感谢贡献。

- **新增 GPT-5.6 模型族**：支持 `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna`，模型 ID 原样传递给 Kiro；上下文窗口按 272K 处理，并通过 `GET /v1/models` 对外公布。
- **新增 Chat Completions 端点**：`POST /v1/chat/completions` 支持 OpenAI 消息、工具调用、`reasoning_effort`、非流式响应与 SSE 响应，内部复用既有 Anthropic 请求管道。
- **新增 Responses 端点**：`POST /v1/responses` 支持新版 Codex CLI 使用的 Responses API，转换 instructions / input / reasoning / function call，并生成对应的非流式响应或 SSE 事件序列。
- **复用现有运行时能力**：两个 OpenAI 端点沿用同一套 API Key 鉴权、模型映射、多凭据故障转移、用量统计及 Kiro MCP WebSearch；`/cc/v1` Claude Code 路径保持不变。

### 🔧 修复 — API Key 生成、自定义与轮换语义

- **生成格式统一为 `sk-`**：服务端创建和轮换的客户端 Key 均为 `sk-` 加 32 位 base62 随机字符串；默认配置的生成值与示例配置值同样以 `sk-` 开头。
- **鉴权只做完整值匹配**：删除 `csk_` 前缀常量与前缀校验，不增加旧前缀兼容分支；请求携带的 Key 只与未禁用的已存储明文做常量时间精确比较。
- **允许任意自定义配置值**：`config.json.apiKey` 不要求 `sk-` 前缀。每次启动都将其同步为唯一的系统密钥 `id=0`；修改配置后旧系统密钥立即失效，现有名称、描述、分组与统计保持不变。
- **Unicode 自定义 Key 安全脱敏**：管理端按 Unicode 字符而非 UTF-8 字节切片，非 ASCII 自定义 Key 不再因切到字符中间而触发运行时 panic。
- **清理过时说明**：README、示例配置、Rust 注释与 Admin UI 统一为“系统密钥不可删除、可轮换”，移除当前文档中的 `csk_*` 生成规则描述。

## [0.6.11] - 2026-07-12

主题：**修复 AWS Enterprise / IAM Identity Center 凭据首次模型调用后，Admin 余额与可用模型查询持续返回 400 的问题**。企业凭据会在首次流式模型请求前通过 `ListAvailableProfiles` 解析真实 `profileArn` 并持久化；旧代码随后将该 ARN 复用到固定使用 Kiro 0.9.2 兼容协议的 `getUsageLimits` 与 `ListAvailableModels` REST GET，导致上游返回 `400 Bad Request {"message":"Improperly formed request."}`。本版隔离流式端点与旧版 REST 端点的 ARN 语义，让企业模型调用和 Admin 查询可以同时正常工作。

### 🔧 修复 — AWS Enterprise / IdC 余额与模型列表查询

- **旧版 REST GET 不再携带 `profileArn`**：`getUsageLimits` 与 `ListAvailableModels` 继续使用 Kiro 0.9.2 兼容 User-Agent，但 URL 不再拼接首次模型调用解析出的真实 `profileArn`，避免上游将请求判定为格式错误。
- **保留企业流式调用所需 ARN**：不删除、不回滚凭据中已经解析并持久化的 `profileArn`；`generateAssistantResponse` / `SendMessageStreaming` 等流式模型请求仍正常注入真实 ARN，Enterprise 的 `tokentype: EXTERNAL_IDP`、IdC Token 刷新与区域回退逻辑保持不变。
- **覆盖状态迁移回归场景**：新增 URL 构造测试，模拟企业凭据从“刚导入、无真实 ARN”进入“首次模型调用后、已有真实 ARN”的状态，确保余额与模型列表请求始终省略 `profileArn`。

### 🔧 修复 — 上游 429 全链路传播与 WebSearch 一致性

> 合并并扩展 [PR #35](https://github.com/ZyphrZero/kiro.rs/pull/35)：保留类型化上游限流及 `Retry-After`，并补齐 Admin、MCP / WebSearch、Token 刷新和分组隔离链路。

- **429 与 `Retry-After` 不再丢失**：模型、MCP、余额、模型列表、超额开关、凭据添加与强制刷新等链路统一返回 HTTP 429；只转发合法的秒数或 HTTP-date，存在明确等待时间时不再在服务端提前重试。
- **WebSearch 正确传播失败**：纯 WebSearch 不再把 MCP 429 吞成空搜索结果和 HTTP 200；`stream: false` 返回普通 JSON，`stream: true` 保持 SSE。
- **客户端 Key 分组隔离**：纯 WebSearch 与混合 WebSearch 的 MCP 调用沿用客户端 Key 对应的凭据组，不会越组选择或统计凭据。
- **并发冷却保留原始 429**：请求已收到类型化 429 后，即使并发冷却使下一次凭据选择失败，也优先返回原始限流，而不是退化为通用 502。
- **刷新限流不再误伤凭据**：Social、AWS IdC 与 External IdP 刷新端点的 429 不计入 Token 刷新失败次数；自动刷新与 401/403 触发的强制刷新均会立即保留类型化 429，不会重复撞刷新端点或因临时限流永久禁用有效凭据。
- **Enterprise profile 发现保留限流**：`ListAvailableProfiles` 返回 429 时停止后续模型请求并原样传播合法 `Retry-After`，不再吞掉限流后继续使用缺失或占位 `profileArn`。
- **隐藏上游敏感错误正文**：通用 502 对客户端使用稳定错误消息，AWS 账号、请求标识等原始响应仅保留在服务端日志中。

## [0.6.10] - 2026-07-10

主题：**放宽 Admin API 请求体上限，修复批量导入凭据时大 JSON 被拒的 413 问题**。批量导入会一次性提交待导入凭据，包含 `refreshToken`、`clientSecret` 等较长字段；当条目较多时，请求体容易超过 axum 默认 2MB 限制并返回 HTTP 413。本版将 Admin 路由请求体上限统一放宽到 50MB，与 Anthropic 路由保持一致，确保大批量导入请求能进入服务端有界并发处理流程。

### 🔧 修复 — Admin 批量导入 413

> 来源：[PR #34](https://github.com/ZyphrZero/kiro.rs/pull/34)。提交人：[@l-spaces](https://github.com/l-spaces)（哈哈先生），感谢贡献。

- **Admin 路由请求体上限提升至 50MB**：为 Admin router 增加 `DefaultBodyLimit::max(50 * 1024 * 1024)`，覆盖 `POST /credentials/batch-import` 等 Admin API，避免批量导入凭据时因默认 2MB body limit 被 axum 提前拒绝。
- **保持批量导入处理链路不变**：前端仍一次性 POST 待导入条目，服务端继续按既有并发度处理并通过 SSE 回传逐条结果；本次只调整网关层请求体限制，不改变导入、验活、去重或回滚语义。

## [0.6.9] - 2026-07-06

主题：**Tool Call 全链路加固、tool inputSchema 规范化、CCH 缓存计量与 Thinking effort 修复、凭据持久化原子落盘，并回退 v0.6.7 的远程部署 Social 登录**。本版汇总 0.6.8 以来累积的多项 Rust 侧健壮性加固与一处回退：工具调用改为按 `tool_use_id` 缓冲后整体解析并显式暴露非法 JSON、Claude Code 内置工具名双向映射与 `<tool_use>` XML 泄漏过滤；规范化 MCP 工具 schema 以规避 Bedrock `TOOL_SCHEMA_INVALID` 400；修正主 Key 缓存计量口径并放宽原生 Thinking effort 下发范围；凭据回写改为 tmp+rename 原子落盘并锁定整个「快照 + 写盘」临界区（issue #23）；同时因 Kiro 收紧 OAuth 回调白名单，回退 v0.6.7 的远程部署 Social 登录（`redirect_uri` 恢复为本机 `127.0.0.1`，远程访问保留手动粘贴兜底）。多数改动参考 `Kiro-RS-Tool` 定位并移植 / 优化。

### ✨ 增强 — Tool Call 全链路加固

> 参考 [GreyGunG/Kiro-RS-Tool](https://github.com/GreyGunG/Kiro-RS-Tool) 定位并移植 / 优化其 `ToolJsonAccumulator`、统一工具调用管道等基础设施。

- **分片缓冲后整体解析**：新增 `ToolJsonAccumulator`，按 `tool_use_id` 缓冲工具入参分片，`stop` 时整体解析；半截 / 非法 JSON 显式暴露（非流式 / CCH 回 502，实时流补发 `error` 事件），不再把半截 JSON 当完整工具调用转发；非流式移除静默回退 `{}` 与截断静默丢弃。
- **Claude Code 内置工具双向兼容**：`toolCompatibilityMode`（默认 `claude-code`，`raw` 供排障）下对内置工具名与入参双向映射（`Write`↔`fs_write` 等）、替换内置 schema、隐藏 `fs_append`；入站还原以 Kiro 工具名匹配实现自动门控，`raw` 模式不误伤客户端同名工具。
- **统一工具调用管道**：收敛到 `CompletedToolUse`（`from_kiro` 唯一还原、`emit_completed_tool_use` 唯一流式发出、`to_anthropic_block` 唯一非流式块），删除重复的 `synthesize_tool_use`。
- **过滤 `<tool_use>` XML 泄漏**：`strip_tool_use_xml_leaks` + 跨 chunk `ToolUseXmlLeakFilter`（修复闭合标签跨 chunk 的场景）。

### 🔧 修复 — tool inputSchema 规范化（规避 Bedrock `TOOL_SCHEMA_INVALID` 400）

> 参考 [GreyGunG/Kiro-RS-Tool PR #6](https://github.com/GreyGunG/Kiro-RS-Tool/pull/6)。部分 MCP 工具（尤其 Claude Code workflow 并行子代理携带的）会因 schema 触发 Bedrock 400。

- **顶层 `type` 强制 `object`**：`normalize_json_schema` 原先仅在 `type` 缺失 / 为空时补 `object`，`type:"array"` 等会漏过；现一律强制为 `object`（非 object 时告警并修正）。
- **剥离顶层组合关键字**：新增 `strip_top_level_combinators`，剥离顶层 `oneOf` / `anyOf` / `allOf`（Bedrock / Anthropic 不支持顶层组合关键字）；原 schema 无 `properties` 时，从首个 `type:object` 的 variant 恢复 `properties` / `required` / `additionalProperties` / `description`，避免退化成空对象。
- **命中即终止**：`CLIENT_VALIDATION_REASONS` 新增 `TOOL_SCHEMA_INVALID`——根因在请求体，重试 / 换号无用，命中即立即终止。

### 🔧 修复 — CCH 缓存计量

- **主 Key 不再模拟跨用户缓存**：`isolation_seed` 改为 `Option`，主 Key（`id=0`）无 session 时不再模拟跨用户缓存。
- **分母口径修正**：被跳过的动态 system 前缀计入 `prompt_total` 分母。

### 🔧 修复 — Thinking effort 下发范围

- **放宽原生 effort**：原生 `effort` 下发扩展至 Opus 4.6 / 4.7 / 4.8 + Sonnet 4.6 + 5 系。
- **从预算推导**：支持从 `thinking.budget_tokens` 推导 `effort`。

### 🔐 修复 — 凭据持久化原子落盘（issue #23）

- **锁定整个临界区**：`persist_lock` 覆盖「快照 + 序列化 + 写盘」整个临界区，最后写盘者必在临界区内重新快照到最新内存，杜绝陈旧快照覆盖已轮换的 token；`entries.lock` 仅在快照期短暂持有、不跨磁盘 I/O，故不阻塞请求路由。
- **tmp+rename 原子落盘**：先写临时文件再同目录 `rename`（原子操作），失败时清理临时文件，避免崩溃 / 并发导致半截凭据文件。

### ⏪ 回退 — 远程部署 Social 登录（撤销 Issue #20）

- **移除公网回调路由**：删除免鉴权 `GET /api/admin/auth/callback/{*tail}` 及其回调投递逻辑（`social_oauth_callback` / `deliver_remote_social_callback` / `RemoteCallbackOutcome`）。
- **移除回调地址派生**：删除 `config.callbackBaseUrl` 配置项与前端 `deriveCallbackBaseUrl`（按 `location.origin` 自动派生），`start_social_login` / `start_social_relogin` 恢复为始终启动本机临时 TCP 回调端口，`redirect_uri` 固定为 `http://127.0.0.1:{port}`。
- **移除 `remote` 响应字段**：`StartSocialLoginResponse` 不再返回 `remote`；前端登录 / 重新登录对话框回到「本地访问自动轮询、远程访问手动粘贴」两态。
- **保留手动完成兜底**：`POST /auth/social/complete/{sessionId}`（及重新登录版本）保留不变——远程访问用户仍可从浏览器地址栏复制 localhost 失败页的完整 URL 粘贴完成登录。

## [0.6.8] - 2026-07-06

主题：**新增 Claude Sonnet 5 / Claude Fable 5 模型映射 + 企业 SSO（Microsoft Entra ID / Azure AD）`external_idp` 认证**。这一版把请求模型关键词映射扩展到 5 代 Sonnet / Fable，并让 `/v1/models` 能列出它们；同时新增第四种认证方式 `external_idp`，支持以 JSON 导入 Microsoft Entra ID / Azure AD 企业租户账号（既不是 AWS Builder ID 也不是 IAM Identity Center，原先无法接入），Token 走 IdP 的 OAuth2 `refresh_token` grant 刷新，并在导入与刷新两处对 IdP 端点做 allow-list 校验，防止 refresh token 外泄。

### ✨ 新功能 — 新增模型映射：Claude Sonnet 5 / Claude Fable 5

- **`claude-sonnet-5`**：`map_model` 的 sonnet 分支新增主版本 5，精确匹配 `sonnet-5` / `sonnet5` / `sonnet.5`（含 `-5-20xxx`、`-5-thinking` 等后缀），排在 4.x 判断之前/相邻并精确到 `sonnet-5`，避免把 `4-5` / `4.5` 误判为 5，也不会命中 legacy 的 `claude-3-5-sonnet`。
- **`claude-fable-5`**：新增独立 `fable` 分支，映射到上游 `claude-fable-5`（Fable 5 与 Mythos 5 同底座，目前仅 5 代），放在最前以免干扰其它关键词。
- **上下文窗口**：`claude-sonnet-5` 与 `claude-fable-5` 均按 `1_000_000` 上下文处理。
- **`/v1/models` 静态列表**：新增 `claude-sonnet-5` / `claude-sonnet-5-thinking`、`claude-fable-5` / `claude-fable-5-thinking` 四个条目，客户端可直接发现。
- **effort 分级**：两者默认支持 `xhigh`（`fable-5` 显式在允许列表，`sonnet-5` 不在旧模型黑名单），无需额外配置。

### ✨ 新功能 — 企业 SSO（Microsoft Entra ID / Azure AD）`external_idp` 认证

> 核心逻辑参考 [Quorinex/Kiro-Go#131](https://github.com/Quorinex/Kiro-Go/pull/131) 移植。本版仅实现凭据导入与刷新，**不含**浏览器门户登录 / 回调监听 / 两段式状态机——按需手动获取 Azure 凭据后以 JSON 导入即可。

- **新增第四种认证方式 `external_idp`**：适用于 Microsoft 365 / Entra ID / Azure AD 企业租户账号。凭据新增三个字段：`tokenEndpoint`（IdP 的 OAuth2 token 端点）、`issuerUrl`（OIDC issuer，纯备注）、`scopes`（空格分隔的已授权 scope，需含 `offline_access` 才能拿到 refresh token）。`authMethod` 可写 `external_idp`，也接受 `azuread` / `azure` / `entra` / `entra-id` / `microsoft` / `m365` / `office365` / `external` 等别名统一归一化；未声明 `authMethod` 但带 `tokenEndpoint` 时自动推断为 `external_idp`。
- **IdP token 端点刷新**：`external_idp` 账号的刷新走 IdP OAuth2 `refresh_token` grant（公共客户端，`application/x-www-form-urlencoded` 表单，无 `clientSecret`），区别于 Social / IdC 的刷新路径。IdP 未下发新 refresh token 时保留原值（Azure AD 有时不轮换）；`invalid_grant` 复用既有的永久失效检测，自动禁用凭据。IdP 不返回 `profileArn`，真实 ARN 仍由 `ListAvailableProfiles` 懒解析回填（与 IdC 一致）。
- **`TokenType: EXTERNAL_IDP` 头**：数据面（`generateAssistantResponse` / MCP）与 `getUsageLimits` / `ListAvailableProfiles` / `setUserPreference` 等 REST 请求，对 `external_idp` 账号自动携带该头——否则 CodeWhisperer 静默返回空 profile 列表并拒绝数据面调用。
- **Admin 导入 / 导出**：`AddCredentialRequest` 与账号导出结构均新增 `tokenEndpoint` / `issuerUrl` / `scopes`；「添加凭据」对话框新增「企业 SSO (Microsoft Entra / Azure AD)」选项与对应输入（Client ID / Token Endpoint / Issuer URL / Scopes），批量导入与嵌套账号（KAM 格式）导入均支持 `external_idp`；凭据卡片展示 `Entra ID` / `企业 SSO` 标签。导出会无损保留新字段与 `external_idp` 认证方式。

### 🔐 安全 — 外部 IdP 端点 allow-list 校验

- **防 SSRF / refresh token 外泄**：`tokenEndpoint` 是外发 refresh token 的目标，属新的信任边界。导入 `external_idp` 凭据时，以及**每次刷新外发前**，都会校验 `tokenEndpoint`（及 `issuerUrl`，若提供）：必须为 `https`、host 非 IP 字面量、且命中允许列表后缀（`*.microsoftonline.com` / `.us` / `.cn`，前导点锚定到真实子域边界）。校验不通过的凭据直接拒绝导入，不会把 refresh token 发往非法主机。新增 IdP 时可扩展该允许列表。
- **必填校验**：导入 `external_idp` 时若缺少 `clientId` 或 `tokenEndpoint`（刷新的前提），提前返回明确错误而非等到刷新失败。

## [0.6.7] - 2026-06-17

主题：**远程部署 Social 登录零配置化（OAuth 回调地址自动派生）+ 凭据列表卡片 / 列表双视图与分页增强 + 来源渠道模糊搜索与移动端体验优化 + output_config.effort 分级归一化**。这一版解决了远程部署（Render / Docker / VPS）下 Social 登录回调指向 `127.0.0.1` 无法使用的痛点——前端按当前访问地址自动派生公网回调地址，远程部署零配置即可完成 Google / GitHub 登录；凭据列表新增 iOS 风格的卡片 / 列表双视图切换、可配置每页数量与跨页全选；同时归一化 `output_config.effort` 分级，避免较老模型收到不支持的 `xhigh` 报错，并在删除凭据时清理其历史失败记录。凭据管理页面还新增按来源渠道（备注）/ 邮箱的模糊搜索、批量导入 / 验活 / 刷新余额的 8 路并发化，以及一轮移动端工具栏布局与下拉菜单渲染异常的修复。

### ✨ 新功能 — 远程部署 Social 登录（Issue #20）

- **OAuth 回调地址自动派生**：前端发起 Social 登录时按当前浏览器访问地址自动算出回调地址（`${origin}/api/admin/auth/callback`）随请求发送。远程部署（Render / Docker / VPS）下浏览器知道自己的公网地址，授权后会落到同源的本服务回调路由，**零配置即可用**；本地访问（`http://localhost:8990`）同样适用。
- **公网回调路由自动接收**：新增免鉴权 `GET /api/admin/auth/callback/{*tail}`，浏览器授权后导航至此路由，服务端按 OAuth `state` 定位会话并把回调数据投递进既有轮询通路，由 `poll_social_login` 统一完成 token 兑换——无需重复实现、无并发消费竞态。Admin UI 自动轮询并在回调到达后显示登录成功。CSRF 仍由每会话随机 `state` 保障，与本地回调服务器同等信任级别。
- **`callbackBaseUrl` 配置逃生口**：当浏览器看到的地址 ≠ 真正可达公网地址（如经内网 IP 访问面板）时，可在 `config.json` 配置 `callbackBaseUrl` 强制覆盖，优先级高于前端派生值；未配置则完全自动。

### ✨ 新功能 — 凭据列表卡片 / 列表双视图 + 分页增强

- **卡片 / 列表视图切换**：凭据列表工具栏新增 iOS 分段控件，可在卡片视图与紧凑列表视图间切换；列表行完整继承卡片的全部操作（拖拽排序、勾选、优先级编辑、刷新 Token / 余额、启用 / 禁用、编辑、更多菜单），并在窄屏渐进隐藏次要信息保证可读；切换偏好持久化到本地。
- **每页数量可配置**：翻页区新增每页数量下拉（12 / 24 / 48 / 96 / 全部），`pageSize=0` 即单页展示全部已筛选凭据；选择即复位到第 1 页并持久化。
- **跨页全选**：筛选结果跨多页时显示「全选所有页」按钮，支持跨页批量删除 / 验活 / 刷新等操作；取消时仅清除筛选范围内的选择，保留筛选外已选项。

### ✨ 新功能 — 来源渠道（备注）模糊搜索

- **凭据列表新增模糊搜索**：筛选栏新增搜索框，按来源渠道（`sourceChannel`，即账号备注）与邮箱做大小写不敏感的子串匹配，与分组 / 分级筛选叠加生效；输入或切换筛选时自动复位到第 1 页。移动端整行展示、桌面端 200px 内联，非空时右侧一键清除。

### ⚡ 优化 — 批量操作并发化（导入 / 验活 / 刷新余额）

- **批量导入并发化**：批量导入改为一次性提交（请求携带 `concurrency: 8`），由服务端有界并发处理、逐条通过 SSE 实时回传导入 / 验活结果，告别前端逐条串行等待。
- **批量验活并发化**：批量验活改为客户端 8 路并发 worker pool（去掉原先逐条之间的固定 2s 间隔），逐条更新验活结果与进度。
- **「刷新当前页余额」并发化**：由原先逐条串行查询改为 8 路并发 worker pool（与批量验活一致），逐条更新卡片余额与进度，大批量刷新耗时大幅下降。

### 🛠 修复 / 改进 — 凭据管理页面体验

- **修复「更多操作」菜单在移动端导致页面渲染异常**：页内所有 DropdownMenu 改为非模态（`modal={false}`），避免 Radix 在 `<html>` 上施加 `overflow:hidden` 滚动锁——该锁在 iOS Safari 下与背景层 `backdrop-blur` / 固定定位叠加，会触发整页渲染错乱或横向位移。
- **工具栏响应式重构**：筛选 + 操作行在移动端改为上下两段堆叠（筛选下拉两列并排、操作按钮两列网格、视图切换整行），桌面端保持「左筛选右操作」单行，消除窄屏下筛选器与按钮交错拥挤。
- **修复列表视图优先级编辑被相邻列遮挡**：编辑栏改用绝对定位浮层（带背景与 `z-index`），输入框加宽以完整显示数字，并支持 Enter 确认 / Esc 取消；卡片视图优先级输入框字号在移动端提升至 16px，避免 iOS 聚焦自动放大整页。

### 🛠 修复 / 改进 — 来自社区贡献

> 以下改进来自 PR #21（@emojiiii），感谢 🙏

- **`output_config.effort` 分级归一化**：`effort` 值会先归一化大小写与空格；已知较老的 4.5 / 4.6 系列（Opus / Sonnet / Haiku）不接受 `xhigh`，会自动降级为最接近的 `high`，避免上游返回 `Invalid additionalModelRequestFields`；Opus 4.7 / 4.8、Fable 5、Mythos 5、Claude 5 等较新模型保留 `xhigh`；其它未知模型对已知 effort 值保持原样（用紧凑黑名单而非易过期的模型白名单），未知 effort 值回退到 `high`。README 同步更新 effort 兼容说明。
- **删除凭据清理历史失败记录**：删除凭据时同步清除其在 `traces.db` 的失败统计（`delete_for_credential`），配合凭据 ID 单调递增不复用已删除 ID，确保新增账号以干净的失败 / trace 历史起步，不会继承同 ID 旧账号的失败 baggage。

## [0.6.6] - 2026-06-13

主题：**账号分组管理（独立实体 + 调度隔离）+ 密钥模型重构（系统默认密钥 id=0）+ Native web_search 工具检测收窄**。分组从依附于凭据 / Key 的字符串标签提升为一等实体，独立持久化、改名 / 删除自动级联，并打通凭据列表筛选、概览页按分组统计与客户端 Key 的调度隔离；同时重构密钥模型——移除 `/v1` 流量主密钥概念，`apiKey` 每次启动幂等导入为不可删除的系统「默认密钥」（固定 `id=0` 对齐历史用量），`adminApiKey` 保留为管理面板登录密钥；此外收窄原生 web_search 工具识别，避免客户端自定义的同名普通工具被误判进内部搜索循环。

### ✨ 新功能 — 账号分组管理（独立实体 + 调度隔离）

- **分组提升为一等实体**：分组在 `groups.json` 独立持久化，凭据 / 客户端 Key 通过名字引用；增删改时校验引用，防 typo 漂移。改名 / 删除自动级联同步所有引用，强删支持 `?force=true` 级联清理。新增 4 个 Admin API（`GET/POST /groups`、`PATCH/DELETE /groups/:name`）。
- **启动平滑迁移**：首次升级时扫描已有凭据 `groups` + 客户端 Key.group 反向写入注册表，老用户零改动切换。
- **分组管理页 `/admin#/groups`**：卡片网格展示凭据 / Key 引用计数，创建 / 编辑 / 删除（有引用二次确认 + force 级联）。
- **凭据列表分组筛选**：dashboard 顶部新增分组下拉，切换时分页自动复位。
- **概览页按分组统计**：时序与按凭据分布支持 `?group=` 过滤；模型分布卡片在分组筛选时给出明确限制提示。
- **客户端 Key 调度隔离**：Key 绑定分组后只调度该分组内账号（严格隔离，分组内无可用账号时请求失败不回退）。
- **429 限流退避优化**：上游 429 改用更长退避（base 1s / cap 8s），总重试上限下调，避免多账号同时触顶连环撞墙。

> 以上分组管理能力来自社区贡献 PR #19（@daniellee2015），感谢 🙏

### ✨ 新功能 — 密钥模型重构

- **移除 `/v1` 流量主密钥概念**：`apiKey` 不再作为独立的 master 鉴权分支，所有 `/v1` 流量统一走客户端 Key 系统。
- **系统「默认密钥」固定 id=0**：每次启动幂等确保 `config.apiKey` 作为系统密钥存在，占用 `id=0` 以对齐历史 master 用量桶（`keyId=0`），保证「默认密钥」可查到升级前全部用量；旧版误建在其它 id 时启动自动迁移到 id=0。
- **系统密钥不可删除、可轮换**：轮换时同步写回 `config.json` 的 apiKey，保留名称 / 描述 / 绑定分组 / 累计统计，避免重复导入。

### 🛠 修复 / 改进

- **保留 `adminApiKey` 为登录密钥**：管理面板登录仍用 `adminApiKey`，可在后台「修改登录API密钥」修改；Admin 鉴权走登录密钥校验。
- **凭证删除行为统一**：单个卡片删除不再因凭证处于启用态而阻止确认，与批量删除行为一致。
- **请求日志支持按分组筛选**：Trace 查询新增 `group` 参数（转为凭据 id 白名单过滤），前端日志页新增分组下拉。
- **客户端 Key 列表显示 ID 列**：按 id 升序，系统密钥居首并显示「系统」徽章。
- **移除「管理员API密钥」筛选项**：概览页与请求日志不再单列该选项（`keyId=0` 已由系统默认密钥覆盖），历史 `keyId=0` 记录回退显示 `#0`。

### 🛠 修复 — 来自社区贡献

- **Native web_search 工具检测收窄为类型匹配**（PR #17 @XuDONGCui）：`web_search` 工具识别从仅按名称匹配改为「名称 + `tool_type` 前缀」双重判定——只有 `name == "web_search"` 且 `tool_type` 以 `web_search_` 开头的 Anthropic 原生工具才会触发内部搜索循环。客户端自定义的、恰巧也命名为 `web_search` 的普通工具不再被误判，确保混合工具集请求走正常的对话路径。新增对应单测验证两类工具的区分行为，同步覆盖纯 web_search、混合工具及自定义同名工具的识别场景。

## [0.6.5] - 2026-06-11

主题：**Claude Code 字面工具调用容错 + 退化复读熔断**。这一版聚焦 Anthropic 兼容层在上游退化输出下的稳定性：当 Claude Code 场景中本应结构化返回的工具调用泄漏成字面 `<invoke>` 文本时，中转层会在严格边界内恢复为真实 `tool_use`；同时新增异常引导词复读熔断，避免 `call` / `count` / `card` 等垃圾文本刷屏、耗尽输出预算或污染会话历史。

### ✨ 新功能 — 来自社区贡献

感谢以下 PR 贡献者 🙏

- **字面 `<invoke>` 工具调用泄漏容错**（PR #15 @xiaojiou176）：当上游把 `<invoke name="...">...</invoke>` 作为普通文本输出时，流式路径会在行首、非代码围栏、工具名已声明的前提下恢复为结构化 `tool_use`，避免客户端看到原始 XML 或漏执行真实工具调用。web_search agentic loop 复用同一嗅探逻辑，但 `web_search` 本身仍作为内部搜索处理，不会作为 raw client `tool_use` 暴露给宿主。
- **退化 stray token 复读熔断**（PR #16 @xiaojiou176）：流式文本出口会检测 `call` / `count` / `card` 等引导词的连续独占行复读，超过阈值后丢弃本轮后续文本，避免上游退化输出刷屏和耗尽 `max_tokens`。非流式与 web_search 路径也会在 `<invoke>` 嗅探前折叠同类复读洪水，避免垃圾文本进入最终响应或后续会话历史。

### 🛠 修复

- **避免重复执行同一工具调用**（PR #15 @xiaojiou176）：若退化模型同时返回文本泄漏和结构化 `tool_use`，会按工具名与规范化 input 去重，防止客户端收到两个相同调用并重复执行。超长工具名被缩短发送给上游后，泄漏恢复路径会识别短名并还原为客户端原始工具名。
- **保留 stray token 剥离前的换行**（PR #16 @xiaojiou176）：剥离 `call` / `count` / `card` 独占行时保留前一行换行，避免把叙述文本和后续 `<invoke>` 压到同一行而漏判真实工具调用。

### ⚡ 优化

- **减少 invoke 嗅探缓冲复制**（PR #16 @xiaojiou176）：`drain_invoke_sniff_buffer` 改为一次性取出本地 buffer 处理，避免退化大缓冲下每轮 clone 带来的额外开销。

## [0.6.4] - 2026-06-09

主题：**入口 Key 级用量分析 + 请求链路入口来源追踪 + Admin UI 移动端体验优化**。这一版把概览页从固定时间窗扩展为可按日期、粒度与入口 Key 过滤的分析面板；请求日志和凭据失败详情会区分“管理员API密钥”与已分发的客户端 Key；同时重排后台顶栏工具、统计图表、凭据卡片和表格在移动端的显示，减少窄屏溢出与操作拥挤。

### ✨ 新功能 — 入口 Key 级用量分析

- **概览页支持入口 Key 筛选**：统计页新增“全部入口 Key / 管理员API密钥 / 指定客户端 Key”筛选，调用量、Token、Credit、模型分布和上游凭据分布可按入口来源查看，方便定位某个客户端 Key 的成本与错误情况。
- **支持自定义日期范围与统计粒度**：统计接口新增 `startDate` / `endDate` / `granularity` 参数，前端可在预设 24h / 7d / 30d 之外选择自定义日期，并在按小时 / 按天聚合之间切换。
- **后端聚合按 Key 维度保留明细**：`UsageAggregator` 新增按 `key_id`、`key_id + model`、`key_id + credential` 的桶内聚合，`/stats/timeseries`、`/stats/by-model`、`/stats/by-credential` 均可用 `keyId` 过滤；非法 range、granularity、日期和 keyId 会返回明确的 400 错误。

### ✨ 改进 — 请求日志与失败详情可追踪入口 Key

- **Trace 记录入口 Key 类型**：请求链路新增 `keySource`，区分管理员API密钥与客户端 Key；鉴权中间件会在请求上下文中标记来源，trace 入库时持久化该字段。
- **请求日志显示入口 Key**：请求日志表格新增“入口 Key”列，客户端 Key 会显示名称（缺失时回退 id），管理员业务 Key 显示为“管理员API密钥”；展开链路仍保留最终凭据与每跳尝试详情。
- **凭据失败详情补充入口来源**：单个凭据的失败日志行现在同步显示触发该失败的入口 Key，便于区分是哪个客户端或管理员密钥导致某个凭据累计失败。

### 🎨 改进 — Admin UI 全局工具与移动端布局

- **顶栏工具全局化**：负载均衡切换、账号级风控故障转移、刷新、镜像在线更新和密钥管理从凭据页抽到全局顶栏，概览、凭据、客户端 Key、请求日志页面都可直接访问；移动端顶栏收敛为“更多操作”菜单。
- **凭据管理移动端重排**：凭据页统计卡压缩为窄屏可读布局，工具栏改为两列按钮网格；凭据卡片增加长文本截断、单列信息行、余额面板稳定三列和底部操作区两行布局，避免小屏横向滚动和按钮挤压。
- **概览图表移动端适配**：趋势图、模型饼图、凭据柱状图改用响应式高度与更紧凑边距，图例和坐标轴在窄屏下减少占用；趋势图系列名改为中文，图表空态高度同步收窄。
- **表格窄屏可横向浏览**：客户端 Key 表格和请求日志表格设置稳定最小宽度、单行表头和单元格截断，避免列内容在移动端被压到不可读。

### 🛠 修复 — 登录与文案细节

- **Social 无痕登录链接复制更可靠**：复制登录链接前检查 Clipboard 权限与安全上下文；浏览器拒绝写入剪贴板时会选中链接并提示用户手动 `Ctrl+C`，避免无痕登录流程卡在“复制失败”。
- **统一密钥命名**：后台文案将管理面板登录用 Key 统一为“登录API密钥”，将 `/v1/*` 客户端调用用 Key 统一为“管理员API密钥”，减少 Admin API Key / 业务 API Key 命名混用。

## [0.6.3] - 2026-06-08

主题：**Claude Code Thinking 兼容 + Kiro 原生 reasoning 事件 + 后台弹窗表单体验修复**。这一版聚焦暂存区中的协议兼容与 Admin UI 表单体验：转换层按上游模型能力处理 Opus / Sonnet Thinking 请求，流式 / 非流式路径支持 Kiro 原生 `reasoningContentEvent`，后台管理页修复导入 / 登录类弹窗的焦点裁切、标签间距和 textarea 拖拽卡顿问题。

### 🛠 修复 — Claude Code / Opus 与 Sonnet Thinking 兼容

- **Opus / Sonnet Thinking 兼容**：Claude Code 可能在普通模型名或 `-thinking` 模型下发送 `thinking` / `output_config`；转换层现在按上游模型能力决定是否发送 `additionalModelRequestFields`，不再因为开启 thinking 或客户端携带 `output_config` 就直接透传不受支持的字段，避免 `additionalModelRequestFields is not supported for this model`。
- **收窄 `output_config.effort` 透传范围**：`additionalModelRequestFields.output_config` 只在已知可接受的 Opus 4.6 adaptive thinking 路径上传递；Opus 4.6 非 adaptive thinking、Opus 4.7 / 4.8、Sonnet 系列与其它模型会显式跳过该字段。

### ✨ 新功能 — Kiro 原生 reasoning 事件

- **支持 `reasoningContentEvent`**：新增 Kiro 原生 reasoning 事件解析，流式响应会把 `text` 转为 Anthropic `thinking_delta`、把 `signature` 转为 `signature_delta`、把 `redactedContent` 转为 `redacted_thinking`。
- **非流式响应保留原生 thinking**：非流式路径会优先使用上游原生 thinking / signature / redacted content 组装 Anthropic content block；没有原生 reasoning 时仍保留旧的 `<thinking>...</thinking>` 文本提取兼容路径。
- **thinking disabled 明确降级**：请求未启用 thinking 时，原生 reasoning 明文会作为普通 text 输出，不输出签名或 redacted thinking，避免客户端收到未请求的 thinking block。
- **token 估算覆盖 thinking 内容**：输出 token 估算现在计入 `thinking` block，并为 `redacted_thinking` 计入固定开销，减少用量统计漏算。
- **补充边界测试与真实 Claude Code 验证**：新增请求转换、流式顺序、非流式内容组装、redacted thinking、signature-only、thinking disabled 降级和 token 估算测试；真实 Claude Code 请求验证普通 Sonnet 4.5 与 `-thinking` 模型均可返回 thinking/signature/text 合法事件序列。

### 🎨 改进 — 后台弹窗表单体验

- **修复表单控件焦点态裁切 / 贴边**：`Input` / `Select` / `Textarea` 与按钮焦点环改为内嵌显示，避免在 Dialog 滚动区域、KAM 导入、批量导入、重新登录、重新导入、远程登录回调和代理池批量导入等窗口中被容器边缘裁掉。
- **恢复标签与控件垂直间距**：普通 `label` 改为块级显示，修复 `space-y-*` 不能作用于 inline label 导致标签和输入框 / 下拉框过近的问题，同时保留 checkbox / switch 这类 flex label 布局。
- **改善 textarea 拖拽调整高度体验**：textarea 不再使用 `transition-all` 过渡高度，只保留边框、背景和阴影过渡；拖动改变高度会立即跟手，KAM 导入、批量导入、Token 重新导入、远程登录回调和代理池批量导入中的原生 textarea 样式同步统一。

## [0.6.2] - 2026-06-07

主题：**Builder ID/free 流式对话 profileArn 400 修复 + 后台前端依赖清理**。上一版为规避占位符 ARN 的 403 风险，在流式请求中剥离了 BuilderID 占位 `profileArn`；但 `q.* /generateAssistantResponse` 对 Builder ID/free 账号仍强制要求该字段，调用 `claude-sonnet-4.5` 等模型会报 `400 "profileArn is required for this request."`。这一版恢复纯 Builder ID/free 流式请求体的占位 ARN，同时保留 Enterprise / IdC 账号解析真实 ARN 的路径。

### 🛠 修复 — Builder ID/free 流式对话 profileArn 400

- **恢复 Builder ID 占位 profileArn 注入**：`KiroCredentials::streaming_profile_arn()` 对 OAuth Builder ID/free 凭据会原样返回显式占位 ARN；未填充时按官方 IDE 行为回退到 Builder ID 默认占位 ARN，避免流式端点因缺少 `profileArn` 直接返回 400。
- **保留 Enterprise / IdC 真实 ARN 优先级**：发起流式请求前仍会通过 `resolve_profile_arn_for` 尝试解析并回填 Enterprise / IdC 真实 `profileArn`；解析成功后使用真实 ARN，纯 Builder ID 无 Enterprise profile 时才回退占位 ARN。
- **补充回归测试**：新增断言覆盖显式 Builder ID 占位 ARN、未填充 Builder ID/free 凭据、Social 固定 ARN、真实 ARN 与 API Key 凭据的流式 `profileArn` 行为。

### 🧹 清理 — 后台前端依赖

- **移除未使用的 `@radix-ui/react-select` 依赖**：后台下拉框已在 0.6.1 改为基于 `DropdownMenu` 的实现，本版清理残留依赖，避免前端依赖树继续携带未使用包。

## [0.6.1] - 2026-06-07

主题：**缓存命中/创建 token 精确计量 + 流式对话 profileArn 占位符 403 修复 + 后台前端组件统一**。上一版把流式端点改成始终发送 profileArn（含 BuilderID 占位符），但占位符指向调用者无权访问的 profile，仍会被上游以 `403 "User is not authorized to make this call"` 拒绝；这一版改为只发送真实 / Social 共享 ARN。同时把中转层缓存计量从粗略估算重写为按前缀链匹配 + 互斥口径分摊的精确计量，请求日志新增 token 列；后台前端把原生确认框 / 下拉框统一为风格一致的组件。

### 🛠 修复 — 流式对话 profileArn 占位符 403

- **占位符 ARN 不再发送**：`KiroCredentials::streaming_profile_arn()` 对 BuilderID 占位符（及未填充 profileArn 的 BuilderID 账号）返回 `None`。占位符指向调用者无权访问的 profile，发送会触发 `403 "User is not authorized to make this call"`；该端点本就不强制此字段。Enterprise / IdC 的真实 ARN 已由 `resolve_profile_arn_for` 回填，与 Social 共享 ARN 一并原样发送。

### ✨ 改进 — 缓存命中 / 创建 token 精确计量

- **前缀链匹配替代锚点**：缓存命中模拟改用「最长公共前缀」链式匹配，消除 `tool_result`（role=user）导致的「倒数第二个 user」锚点漂移，跨轮对话命中稳定。
- **会话隔离**：按 `metadata` 的 user / session（缺失时回退 client key id）派生隔离种子，不同会话不会互相串缓存。
- **互斥口径分摊**：`input` / `cache_creation` / `cache_read` 按比例分摊，保证三者互斥且总和等于 total，不再重复计入被缓存覆盖的前缀。
- **token 估算与签名解耦**：哈希用签名、计量用原文，去除签名噪声对 token 数的污染。
- **图片 token 估算**：按 `(宽 × 高) / 750` 估算（长边封顶 1568px），图片块的媒体类型 + 数据纳入缓存哈希。
- **请求日志记录 token**：`traces.db` 新增 input / output / cache_creation / cache_read 列（幂等迁移），日志接口返回并合计 totalTokens。
- 模块 `prompt_cache` 更名为 `cache_metering`，持久化文件相应更名。

### ✨ 改进 — 后台前端组件统一

- **统一二次确认弹窗**：新增 `useConfirm` / `ConfirmProvider`，全站确认操作改用风格一致的弹窗替代原生 `confirm()`。
- **重写下拉框**：以 `DropdownMenu`（`modal={false}`）重写 `Select`，替换原生 `select` 与 radix `Select`。后者 Content 硬编码 `disableOutsidePointerEvents`，经 `DismissableLayer` 给 `body` 上 `pointer-events` 锁，嵌套在 Dialog 内同时关闭时卸载顺序竞态会把 body 误留为 `none` 导致整页不可点；non-modal 分支不触碰 body 锁，从源头规避。下拉默认值改为从 children 静态推导，修复菜单未打开时默认值显示为空。

## [0.6.0] - 2026-06-07

主题：**Enterprise / IAM Identity Center 凭据全链路打通 + 流式对话 profileArn 修复 + 登录体验对齐官方 IDE**。此前导入或登录企业（Enterprise）IdC 账号后，获取订阅/用量会报 `403 {"message":"Invalid token"}`，且发起对话会报 `400 profileArn is required` / `403 bearer token invalid`——根因是这类账号在请求里带了 BuilderID 占位 profileArn 或缺失真实 profileArn。这一版定位并修复了用量查询与流式对话两条链路，同时把添加凭据 / 登录 / 导出的整体行为与官方 IDE / 账号管理器对齐，并新增 Enterprise 登录入口与一批凭据管理体验改进。

### 🛠 修复 — 流式对话 400「profileArn is required」/ 403「bearer token invalid」

新版上游对流式端点（`generateAssistantResponse`）强制要求请求体携带 `profileArn`，且校验其与 token 身份匹配。表现为对话直接失败（新模型如 `claude-opus-4-8-thinking` 同样命中）：

- 不带 profileArn → `400 {"message":"profileArn is required for this request."}`；
- 带 BuilderID 占位符 ARN → `403 {"message":"The bearer token included in the request is invalid."}`。

按官方 Kiro IDE 的行为分两类账号修复：

- **流式端点始终发送 profileArn（含 BuilderID 占位符）**：新增 `KiroCredentials::streaming_profile_arn()`，流式端点不再像用量类接口那样剥离占位符。纯 BuilderID 账号的占位符与其 token 身份匹配，可正常使用。
- **Enterprise / IdC 账号解析并回填真实 profileArn**：这类账号的占位符与 token 不匹配（403），必须使用真实 profileArn——而真实 ARN 既不是占位符也不在 OIDC 刷新响应里返回。新增 `ListAvailableProfiles` 上游调用（AWS JSON 1.0，target `AmazonCodeWhispererService.ListAvailableProfiles`，端点 `q.us-east-1` / `q.eu-central-1`）与 `MultiTokenManager::resolve_profile_arn_for()`：首次请求时按需解析真实 profileArn、写回凭据并持久化，之后直接命中。无 Enterprise profile 的账号（纯 BuilderID）进程内只查询一次，回退到占位符逻辑。
- 用量类接口（getUsageLimits / ListAvailableModels / setUserPreference）继续使用 `effective_profile_arn()`（跳过占位符）；回填真实 ARN 后它们也会带上真实 profileArn，行为更贴近官方 IDE。

### 🛠 修复 — Enterprise/IdC 用量查询 403

- **跳过占位 profileArn**：新增 `KiroCredentials::effective_profile_arn()` 与 `is_placeholder_profile_arn()`——只向上游发送真实 ARN（含 Social 共享 ARN），跳过 `BUILDER_ID_PROFILE_ARN` 占位符。BuilderID / Enterprise / IdC 账号本就没有可用 profileArn，发送占位符会被上游以 403 "Invalid token" 拒绝。`getUsageLimits` / `ListAvailableModels` / `setUserPreference` 以及流式端点（ide/cli）的请求体与 `x-amzn-kiro-profile-arn` 头全部改用它。
- **用量类接口固定使用兼容版本**：`getUsageLimits` / `ListAvailableModels` / `setUserPreference` 固定以 `0.9.2` 作为 `KiroIDE-<version>` 标识——新版上游对这些接口强制要求 profileArn，对无 profileArn 的 Enterprise/IdC 账号会失败；该版本下无需 profileArn 即可返回订阅与用量。
- **区域映射 + 403 回退**：上述接口仅在 `us-east-1` / `eu-central-1` 两个端点提供服务，依据凭据 SSO 区域选择主端点（`eu-*` → eu-central-1，其余 → us-east-1），主端点 403 时自动回退到另一个端点。
- **解析并回填邮箱**：`getUsageLimits` 响应的 `userInfo.email` 现在会被解析，凭据无邮箱时自动回填。

### ✨ 新功能 — Kiro IDE 版本自动获取

- 新增 `src/kiro/kiro_version.rs`：启动时从官方稳定版元数据端点（`prod.download.desktop.kiro.dev/stable/metadata-linux-x64-stable.json` 的 `currentRelease`）拉取当前 Kiro IDE 版本，进程内缓存 + 每 12h 后台刷新，失败回退到 `config.kiroVersion`。流式端点 User-Agent 与 Social 刷新随真实版本走，替代写死的版本号。

### ✨ 新功能 — Enterprise 登录入口与登录体验

- **新增 Enterprise (IAM Identity Center) 登录入口**：仅显示 SSO Start URL（必填）+ SSO 区域，与官方交互一致；登录成功的凭据带 `provider=Enterprise`、`startUrl`、`region`。
- **SSO 区域可选 / 自定义**：登录对话框区域字段改为「分组下拉（US / Europe / Asia Pacific / Other 常用区域）+ 始终可输入的自定义文本框」。
- **AWS SSO 与 Enterprise 均支持无痕登录**：勾选后复制验证链接，由用户在无痕 / 隐身窗口打开，避免与已登录的 AWS 账号串号。
- **IdC 登录对齐官方**：注册客户端使用 5 个 codewhisperer 作用域并带上 `issuerUrl`（Builder ID 为默认 Start URL，Enterprise 为组织 Start URL）。
- **新增 `startUrl` 字段**：凭据模型新增 SSO Start URL 字段，登录 / 导入 / 导出全链路保留。

### ✨ 改进 — 凭据管理体验

- **添加 / 登录成功后自动刷新余额**：添加凭据、Social 登录、IdC/Enterprise 登录成功后主动拉取一次余额（含订阅等级、邮箱）并写入缓存，新凭据卡片立即显示余额。
- **凭据标签按登录方式显示**：卡片身份标签根据 `provider` 细分为 GitHub / Google / Builder ID / Enterprise / IAM SSO / API Key，不再统一显示 Social / IdC。
- **删除登录与添加凭据中的优先级输入项**：保留卡片上对已有凭据的优先级编辑与拖拽排序。
- **无需先停用即可直接删除凭据**：单个与批量删除都不再要求凭据处于禁用状态（仍有二次确认）。
- **一键超额遇 403 友好提示**：开启超额（一键 / 单条）命中 403 / 权限不足时统一提示「请联系您的组织管理员以获取支持」。
- **导出格式调整**：凭据导出改为嵌套 `Account` 结构（凭据收进 `credentials` 子对象、`expiresAt` 毫秒时间戳、含顶层 `groups`/`tags` 数组），便于第三方账号管理工具直接重新导入。

## [0.5.9] - 2026-06-03

主题：**客户端行为纠偏 + 按凭据拉取上游真实可用模型**。此前客户端传入的畸形请求体（tool_use/tool_result 不配对等）会导致上游 Bedrock 返回 503，触发重试风暴；批量工具混入 native web_search 也缺少端到端 handler。模型列表方面，免费凭据无法用 Opus 但此前无可见性——现在可按凭据实时查看上游订阅的真实可用模型。

### ✨ 新功能

- **凭据级上游模型查询**（PR #12 @ZyphrZero）：新增 `GET /api/admin/credentials/{id}/models` 接口，实时查询上游 `ListAvailableModels` API 获取该凭据当前可用模型（随订阅等级变化，FREE 不含 Opus）。在凭据卡片「更多操作」弹出独立弹窗，展示模型名、ID、最大输入 token 数。后端整条链路对标现有"余额"功能，新增 `src/kiro/model/available_models.rs` 响应 DTO、`token_manager` 抽出 `prepare_request_token` helper 消除重复的 token 刷新逻辑。
  > 仅限 admin 面板按需拉取，不影响客户端 `GET /v1/models` 静态聚合列表。

### 🛠 修复

- **从源头阻断 503 风暴**（@ZyphrZero）：`provider.rs` 与 endpoint 层新增 `is_bad_request` 判别，把上游 Bedrock 因客户端格式错误（tool_use/tool_result 不配对等）返回的 503 在结束即刻识别为不可重试错误——不走重试、不切换凭据，直接返回 400 给客户端。此前这类错误被当成瞬态故障反复重试，会放大成一次坏请求 → 全部凭据接连被打 → 瞬时数百次 503。
- **Bedrock 客户端校验错误映射为 400**（PR #10 @xiaojiou176）：`src/anthropic/handlers.rs` 对 Bedrock 返回的 `ValidationException`（消息序列非法、缺少 content 等）统一返回 `400 Bad Request` 而非 `502 Bad Gateway`，避免下游客户端误判为上游故障并无效重试。

### ✨ 新功能 — 来自社区贡献

感谢以下 PR 贡献者 🙏

- **Native WebSearch 端到端循环**（PR #9 @xiaojiou176）：批量工具中混入 `web_search` 时，进入 agentic 内部循环——先调上游获取搜索结果，把结果注入回消息作为 tool_result，再继续对话。完全在 MCP 端点 `q.{region}.amazonaws.com/mcp` 上完成，不依赖外部搜索 API。
- **`output_config.effort` 直通上游**（PR #8 @xiaojiou176）：Anthropic 协议 `reasoning.effort` 字段（low/medium/high）映射到 Kiro/Q 协议 `outputConfiguration.agentMode` 字段，让不同推理强度的请求在 Kiro 上游获得对应的资源分配。
- **图片 MIME 修正与智能降采样**（PR #7 @xiaojiou176）：用 `magic-bytes` 从二进制头识别真实图片格式，修正错误声明的 MIME；超尺寸图片自动降采样到 1M 像素并重编码为 JPEG；`tool_result` 中的 base64 图片上浮到 user message 级 `images: [...]`，避免被上游忽略。

### 📦 升级指南

1. **`docker compose pull && docker compose up -d`** 即可。无破坏性变更，无需清理状态文件。
2. **查看凭据可用模型**：登录管理面板 → 凭据管理 → 任一凭据卡片「更多操作」→「查看可用模型」，实时查询上游。

## [0.5.8] - 2026-06-01

主题：IP 代理池从「仅能增删改查 + 手动分配」升级为**具备主动健康检查、失败累计自动剔除、轮询批量分配**的完整代理管理能力。此前加完代理只能等真实请求才知道是否可用，代理失效也不会被记录或自动禁用，且只能逐个手动分配给凭据。

### ✨ 新功能 — 主动健康检查与连通性测试

- **探测连通性与延迟**：`ProxyEntry` 新增 `health / latencyMs / lastCheckedAt / consecutiveFailures / autoDisabled` 字段（`serde(default)` 向后兼容旧 `proxy_pool.json`）。通过该代理请求轻量公网端点 `https://www.gstatic.com/generate_204`（8s 超时）验证「能否走通 + 往返延迟」，不依赖上游 Kiro。
- **后台健康检查调度器**：照搬 `start_balance_refresher` 模式新增 `start_proxy_health_checker`，每 5 分钟对所有已启用代理用 `join_all` 并发探测一次。
- **新接口 `POST /proxy-pool/{id}/check` 与 `/proxy-pool/check-all`**：分别供 UI「测试」按钮即时探测单个代理、以及手动触发全量健康检查。

### ✨ 新功能 — 失败累计与自动剔除

- **连续探测失败自动禁用**：探测失败累计 `consecutive_failures`，达阈值（3 次，与凭据 `MAX_FAILURES_PER_CREDENTIAL` 对齐）即自动 `enabled=false, auto_disabled=true`；探测成功立即清零。用户手动重新启用时清除自动禁用标记与失败计数。仅由健康检查触发，不侵入 `provider.rs` 请求热路径。

### ✨ 新功能 — 轮询批量分配

- **新接口 `POST /proxy-pool/assign-round-robin`**：取「已启用且非 Unhealthy」的可用代理，对目标凭据（默认全部）按取模轮询写入 `proxy_url`，复用 `token_manager.update_credential`，免去逐个手动分配。

### ⚡ 优化 — HTTP Client 缓存

- **缓存容量上限淘汰**：`provider.rs` 的 `client_cache` 原为无界 `HashMap`，代理数增长会令每个代理常驻一个 `reqwest::Client` 导致内存无界增长。改为带容量上限（64）的 `ClientCache`，按插入顺序淘汰最旧的非全局代理 client，全局代理 client 常驻不被淘汰。

### 🎨 前端

- 代理池弹窗每行新增健康状态徽章（绿：可用 + 延迟 ms / 红：异常 + 连续失败次数 / 灰：未检测）与最近检测时间，并区分「自动禁用」与用户「手动禁用」。
- 每行新增「测试」按钮，顶部新增「全部检测」「轮询分配」按钮。

## [0.5.7] - 2026-05-30

主题：凭据失败次数从单一"连续失败计数器"升级为**累计统计 + 按类型三色分类展示**。此前卡片"失败次数"绑定 `failure_count`（连续失败计数器，成功即清零、账号风控与瞬态不计入），导致鉴权失败被其他凭据救回后立即清零、账号风控压根不显示，与用户对"这个凭据到底失败了多少次、什么原因"的直觉不符。

### ✨ 新功能 — 累计失败统计

- **拆分计数,避免误禁用**：`token_manager` 新增 `total_failure_count`——所有失败类型（鉴权 / 额度 / 风控 / 瞬态 / 网络）都 +1、只增不减、仅手动「重置失败计数 / 恢复异常」(`reset_and_enable`) 时归零。原 `failure_count` 保持"连续失败、成功清零"语义,继续驱动"连续失败 N 次自动禁用",因此健康凭据不会被终身累计的失败数误禁用。持久化到 `kiro_stats.json`（`serde(default)` 向后兼容旧文件）,贯通快照 → admin API → 前端。

### ✨ 新功能 — 失败次数按类型三色分类

- **三色展示（鉴权 / 风控 / 其他）**：卡片"失败次数"改为 `auth / throttle / other` 三个分色数字（如 `3/1/2`,鉴权红、账号风控橙、其他灰）。数据来自 trace 库聚合——新增 `trace_db::failure_stats()` 对 `trace_attempts` 按 `credential_id + outcome` 分组 COUNT 并归并三类（鉴权=`auth_failed`、风控=`account_throttled`、其他=额度/瞬态/网络/请求错误/未知）。
- **新接口 `GET /api/admin/traces/failure-stats`**：返回 `{credentialId: {auth, throttle, other}}`。前端 dashboard 每 30s 拉一次并按凭据分发给各卡片;无 trace 数据（trace 关闭 / 已过期清理）时回退显示 `totalFailureCount`。鼠标悬停 title 说明各类含义,点击仍打开失败日志详情弹框。

## [0.5.6] - 2026-05-30

维护版本：仅版本号递增，无功能或代码变更（内容同 0.5.5）。用于刷新发布产物 / 镜像。

## [0.5.5] - 2026-05-30

主题：新增**请求链路追踪（Trace）+ 「请求日志」排查页面**。此前 `/v1/messages` 的失败链路几乎不可观测——provider 重试循环里每跳失败（402 禁用 / 429 风控冷却 / 401/403 鉴权 / 5xx / 网络错误）只有 `tracing::warn!` 日志，handler 最终只写一条 `UsageRecord` 且失败时 `credential_id=0`、status 仅 success/error，无错误类型、无重试次数、无上游错误体；流式中途断开也只记 `error`。这一版把每个外部请求的完整重试链路（含每跳命中凭据、HTTP 状态码、失败分类、上游错误体片段、耗时）落到 SQLite，并提供可筛选、可展开链路的前端页面，专门用于排查"中断"类问题。配套加日志治理（trace 开关 / 保留天数可配且运行时可改），以及一批凭据卡片交互改进（拖拽排序优先级、失败日志详情弹框、卡片等高对齐等）、Kiro 账号无痕登录选项。

> 0.5.3 / 0.5.4 因发布间隔过短被合并进 0.5.5，请直接使用 0.5.5。下方为合并后的完整内容。

### ✨ 新功能 — Kiro 账号无痕登录

- **「使用无痕窗口登录」选项**：Social 登录对话框新增勾选框。勾选后发起登录不自动 `window.open`（浏览器不允许网页 JS 直控无痕模式，远程部署后端也无法拉起访客本地浏览器），改为把登录链接复制到剪贴板并提示用户自行用无痕 / 隐身窗口（Ctrl+Shift+N）打开，避免与当前已登录的 Google / GitHub 账号串号；waiting 界面提供「复制登录链接」按钮可重复复制。不勾选维持原自动打开行为。

### 🛠 修复 — 凭据失败详情查询与展示

- **失败记录覆盖"中间跳失败但整体成功"**：此前凭据失败详情弹框用 `credentialId`（最终凭据）+ `onlyFailed`（最终状态）过滤，导致"某凭据某一跳失败、但请求最终被其他凭据救回成功"的记录查不到——而这正是凭据因失败过多被禁用的典型成因。`TraceQuery` 新增 `failed_attempt_credential_id`，用 `EXISTS` 子查询匹配 `trace_attempts` 里该凭据 `outcome != 'success'` 的跳（不论 trace 最终状态）；`GET /api/admin/traces` 新增 `failedAttemptCredentialId` 参数。前端弹框改用该维度查询。
- **失败次数与日志条数一致**：弹框原按 trace 渲染、每条只取该凭据第一个失败跳，导致同一请求里该凭据连续失败多跳被折叠成一行（如 3 次 403 只显示 1 条）。改为摊平该凭据的所有失败跳逐条展示，每行标注「第 N/M 跳」，单跳只显示本跳的 outcome / HTTP / 错误体；整条 trace 最终成功时标注"本次请求最终由其他凭据成功"。

### ✨ 新功能 — 请求链路追踪（尝试级）

- **SQLite 持久化**：新增 `src/admin/trace_db.rs`（rusqlite + bundled，自带 SQLite 源码静态编译，无系统库依赖）。`traces.db` 与凭据文件同目录，WAL 模式。两张表：`traces`（请求级汇总）+ `trace_attempts`（每跳明细，外键 trace_id）。一个外部请求 = 1 条 trace + N 条 attempt。
- **每跳结构化记录**：provider 重试循环（`src/kiro/provider.rs`）每一跳结束时通过 `TraceSink` 上报：第几次尝试、命中凭据 id、endpoint、HTTP 状态码（网络层失败为 null）、失败分类、上游错误体片段（截断 2KB）、单跳耗时。失败分类复用现有判别：`quota_exhausted` / `account_throttled` / `auth_failed` / `transient` / `network_error` / `bad_request` / `unknown` / `success`。
- **请求级汇总**：handler 层 `RequestTracer`（`src/anthropic/handlers.rs`）累积 attempts，请求结束时 finalize：`final_status`（success / error / interrupted）、`final_credential_id`、顶层 `error_type`（提升自最后一跳分类，便于筛选）、`error_message`、总尝试次数、端到端耗时。
- **流式中断检测**：流式 / 缓冲流式两路的 SSE unfold 都累计已发送字节数，上游中途断流时标记 `final_status=interrupted` + `interrupted_after_bytes`，区分"完整失败"与"半截中断"。
- **保留期可配**：后台任务（复用现有 cleanup tokio 循环）每天 `DELETE` 掉超过保留天数的 traces + 关联 attempts，保留天数默认 7 天、运行时可改（见下方"日志治理"）。`traces.db` 打开失败不致命——降级为内存库，trace 不可用但服务正常。
- **零侵入**：`KiroCallResult` 签名不变，attempt 走 `TraceSink` 旁路上报；未启用 trace（开关关闭或 store 为 None）时所有路径零开销。MCP（WebSearch）路径本期不接 trace。

### ✨ 新功能 — Admin API + 「请求日志」页面

- **`GET /api/admin/traces`**：query 参数 `status` / `errorType` / `credentialId` / `model` / `onlyFailed` / `limit`（默认 200，上限 1000），动态拼参数化 WHERE + `ORDER BY ts_epoch DESC LIMIT`，返回含每跳明细的链路；附带 credential email 反查（与 `stats_by_credential` 一致）。
- **前端独立「请求日志」Tab**（`admin-ui/src/components/trace-log-page.tsx`）：与概览 / 凭据管理 / 客户端 Key 并列。表格列：时间、模型、状态徽章（成功绿 / 失败红 / 中断橙）、最终凭据（email）、错误类型、重试次数、耗时。顶部筛选：状态下拉 + 错误类型下拉 + "只看失败"开关 + 刷新。点击行展开完整重试链路时间线，每跳显示凭据 / endpoint / HTTP 状态 / outcome 徽章 / 耗时，失败跳展示上游错误体片段（等宽可折叠）。
- **新增前端文件**：`api/traces.ts`、`hooks/use-traces.ts`（复刻 stats 的 30s 刷新 + keepPreviousData）、类型 `TraceAttempt` / `TraceRecord` / `TraceQuery`。

### ✨ 新功能 — 日志治理（可配置 + 运行时可改）

- **三个 config 字段**（`src/model/config.rs`，camelCase）：`traceEnabled`（默认 true）/ `traceRetentionDays`（默认 7）/ `usageLogRetentionDays`（默认 31）。启动时读入，分别初始化 `TraceStore` 与 `UsageRecorder`。`config.example.json` 已补充示例。
- **运行时可改 + 持久化**：保留期与 trace 开关改为 `AtomicBool` / `AtomicU64`（参照 `account_throttle` 的运行时可变模式）。`GET/PUT /api/admin/config/log-governance` 改完立即生效并回写 `config.json`，无需重启；保留天数校验 `1..=365`，写盘失败时运行时值仍生效并 warn。关闭 `traceEnabled` 后 `TraceStore::insert` 直接短路，不再写新链路（历史记录仍可查）。
- **前端治理面板**：「请求日志」页筛选栏新增"治理设置"下拉（参照顶栏风控配置）——trace 启用开关 + trace 保留天数输入 + usage 日志保留天数输入，保存即调 `PUT /config/log-governance`。

### ✨ 新功能 — 凭据卡片交互改进

- **拖拽排序优先级**（`@dnd-kit`）：每张凭据卡片操作区新增 `⋮⋮` 拖拽手柄，按住手柄即可在当前页内拖动重排。松手后按新视觉顺序赋连续递增的 `priority`（全局位置 = 页起始索引 + 页内序号），只对实际变化的卡片发 `set_priority`，乐观更新 + 失败回滚。手柄带 `data-no-rect-select`，与既有矩形框选 / 点击选中完全隔离；拖拽中关掉 Card 的 `transition-all` 与 hover 位移，保证"跟手"。**移除原优先级 ↑/↓ 按钮**，操作区恢复单行。仅当前页内排序，翻页清除本地顺序覆盖。
- **失败日志详情弹框**：卡片"失败次数"改为可点击，弹框（`credential-failures-dialog.tsx`）展示该凭据最近 50 条失败链路（复用 `GET /traces?credentialId=X&onlyFailed=true`，懒加载——弹框未打开不查询）。每条含时间、错误类型徽章、HTTP 状态、错误消息、上游错误体片段。补足了卡面"失败次数"计数器看不到的瞬态 / 网络失败历史（该计数器是连续失败计数、成功即清零、瞬态错误故意不计入，语义不变）。
- **可交互数值统一标识**：优先级（`Pencil` 编辑）/ 失败次数（`ScrollText` 看日志）/ 成功次数（`RotateCcw` 重置）三个可点击数值统一加图标 + `hover:bg-accent` 悬停反馈 + `cursor-pointer`，此前无可点击标识。
- **启用凭据后自动刷新余额**：在卡片开关把凭据从禁用切到启用且成功后，自动触发一次该卡片的余额查询。
- **卡片等高对齐**：Card 改 `flex h-full flex-col` 填满 grid 行高、CardContent `flex-1`、操作区 `mt-auto` 固定贴底；余额面板加 `min-h-[150px]`，未查询 / 查询中 / 已查询三态高度一致。同行卡片整体对齐。
- **徽章合并减少换行**：标题下的配置元信息徽章（endpoint / Profile ARN）合并为单个 `endpoint · ARN` 徽章；状态类徽章（订阅 / 活跃 / 已禁用 / 已超额 / 冷却）保留独立以维持颜色语义。

### 📦 依赖 / 构建

- **新增 Rust 依赖**：`rusqlite = { version = "0.32", features = ["bundled"] }`。bundled 自带 SQLite C 源码静态编译，跨平台一致、无需系统库。
- **新增前端依赖**：`@dnd-kit/core` / `@dnd-kit/sortable` / `@dnd-kit/utilities`（凭据卡片拖拽排序，vendor chunk 约 +42KB / gzip +14KB）。
- **`.gitignore` / `.dockerignore`** 新增 `traces.db` 及 WAL 边车文件（`traces.db-shm` / `traces.db-wal`，运行时产物不入库）。
- **测试覆盖**：247 通过（trace_db 新增 5：insert/query roundtrip、disabled 短路、only_failed/status/model 筛选、cleanup 按保留期、错误体截断）。

### 📦 升级指南

1. **`docker compose pull && docker compose up -d`** 即可。`traces.db` 首次请求时自动创建于凭据文件同目录，无需手动初始化。
2. **排查中断**：登录管理面板 → 顶栏「请求日志」Tab → 用状态 / 错误类型筛选或开"只看失败" → 点击任一行展开看完整重试链路（哪个凭据、第几跳、因为什么失败、上游原始错误体）。
3. **日志治理**：「请求日志」页"治理设置"下拉可随时开关 trace、调整 trace / usage 日志保留天数，改完立即生效并写回 `config.json`；也可直接在 `config.json` 配 `traceEnabled` / `traceRetentionDays` / `usageLogRetentionDays`（缺省即用默认 true / 7 / 31）。
4. **凭据排序与失败排查**：「凭据管理」Tab 拖动卡片 `⋮⋮` 手柄即可在当前页内调整优先级（实时持久化）；点击卡片"失败次数"可看该凭据的失败日志详情（依赖 trace 开启）。
5. **无破坏性变更**：trace 与现有 usage_log / 概览统计完全独立，不影响既有功能；升级无需清理任何状态文件。

## [0.5.2] - 2026-05-29

主题：在 0.5.1（prompt cache 重构 + Credit 全链路 + 仪表盘改造）基础上加入**账号级风控识别与冷却失败转移**——上游 Kiro/Q-Developer 在风控触发时返回带 `suspicious-activity` body 的 429，与"高负载 429"完全不同；旧版本一刀切当成 transient 重试，导致单账号被反复打到。同时修复 thinking 模式跨轮 replay 的客户端校验失败。前端配套加风控冷却倒计时徽章、单卡刷新余额按钮、整页刷余额按钮提级、趋势图 range 切换动效等若干细节。

> 0.5.0 因 Credit 数值显示问题被作废、0.5.1 在小流量场景下仍有单账号被打死风险，**0.5.2 整合三个版本所有内容，请直接升级到 0.5.2，跳过 0.5.0 / 0.5.1**。下方按特性分块罗列从 0.4.x 升上来需要知道的所有变更（标注「0.5.2 新增」的小节是相对 0.5.1 的增量，其余为 0.5.1 内容继承）。

### ✨ 新功能 — 账号级风控识别与冷却失败转移（0.5.2 新增）

- **`is_account_throttled` 端点判别器**：新增 `src/kiro/endpoint/mod.rs::is_account_throttled`，匹配 `429` + body 含 `suspicious-activity`（Kiro/Q-Developer 在账号触发风控时下发的标志）。同步扩展 `is_monthly_request_limit` 也匹配 `OVERAGE_REQUEST_LIMIT_EXCEEDED`，把"超额请求次数耗尽"识别为月度配额耗尽并下线该凭据。
- **provider 拆分 429 路径**：`src/kiro/provider.rs` 把原本一刀切的 429 处理改成两路——账号风控走"放入冷却 + 失败转移到下一凭据"，high-traffic 429 仍走 transient 重试。冷却中的凭据在 `select_credential` / `available_count` / `snapshot` 全部跳过，调度器不会反复打到同一个被风控的账号。
- **`TokenEntry::throttled_until` 字段**：`token_manager.rs` 给每条凭据加 `throttled_until: Option<Instant>`，并在 `MultiTokenManager` 暴露 `mark_account_throttled(id, secs)` / `clear_throttle(id)` 两个 API。
- **`account_throttle_failover` / `accountThrottleCooldownSecs` 配置**：两个原子可在运行时切换，无需重启；持久化到 `config.json`。冷却时长默认 600s（10 分钟），可在面板自定义分钟数。
- **Admin API 三件套**：
  - `GET /api/admin/config/account-throttle` 读取当前开关 + 冷却秒数
  - `PUT /api/admin/config/account-throttle` 修改并落盘
  - `POST /api/admin/credentials/:id/clear-throttle` 手动解除单条凭据冷却
- **凭据快照 `throttled_remaining_secs` 字段**：`CredentialStatus` 新增剩余秒数字段，前端按秒递减渲染倒计时。
- **前端 UI**：
  - 顶栏「设置」下拉新增"账号风控失败转移"开关 + 冷却预设按钮（5 / 10 / 30 / 60 分钟）+ 自定义分钟输入。
  - 凭据卡片在风控冷却中：橙红描边 + `mm:ss` 倒计时徽章（`Clock` 图标），到期或手动解除后自动恢复调度。倒计时本地用 `setInterval` 自然递减，避免 30s 拉取间隔之间数字停顿。
  - 卡片"更多操作"菜单冷却中显示"解除风控冷却（mm:ss）"项。

### 🛠 修复 — Thinking 模式跨轮 replay 兼容（0.5.2 新增）

- **thinking block 必带 `signature`**：Claude Code、Anthropic SDK 等思考模式客户端会拒绝下一轮请求中 `assistant.content[].thinking` 缺 `signature` 的消息，抛 `The content[].thinking in the thinking mode must be passed back to the API`。Kiro 上游不是 Anthropic API、永不下发真签名。修复方案：流式与非流式两路都在思考块结束前注入稳定的占位符 signature，使客户端校验通过；converter 在请求转发时只读 `block.thinking` 文本字段，占位符对上游完全不可见。
  - 流式：每个 thinking block 的 `content_block_stop` 之前发出一个 `signature_delta` 事件（4 条收尾路径全部覆盖：正常 stop、tool_use、客户端中断、错误）。
  - 非流式：`assemble_response` 在组装 thinking content block 时直接带上 `signature` 字段。
  - 测试：新增"signature_delta 必须先于 content_block_stop 且非空"断言（242 通过，+1）。

### ✨ 新功能 — 凭据管理体验改进（0.5.2 新增）

- **每张凭据卡片单独「刷新余额」按钮**：放在「刷新 Token」旁，单 GET `/api/admin/credentials/:id/balance`，loading 时按钮 spin 不阻塞其他卡片。原来只能整页批量"查询当前页信息"才能看到单条凭据的余额。
- **整页余额刷新按钮提升到工具栏**：之前藏在「更多操作」下拉里，新版作为独立 outline 按钮放到工具栏右侧（"添加凭据"前），并带 `刷新中… N/M` 进度。
- **「一键开启超额」拆分两态**：之前一个按钮根据可开启数 / 待确定数文案切换，且会对待确定凭据直接调写接口（FREE 订阅 403）。现在拆成两个独立路径：
  - 有可开启凭据 → 调写接口 `setUserPreference`，文案 `一键开启超额（N）`。
  - 全部凭据状态待确定 → 改走只读批量查余额，文案 `重试拉取超额状态（N）`，附 `刷新中… N/M` 进度，绝不触发写接口。
- **趋势图 range 切换动效**：`OverviewPage` 给 `<TimeSeriesChart>` 包一层 `key={range}` 强制重挂，外加 `chart-range-fade` CSS 动画（`opacity + translateY`，`prefers-reduced-motion` 自动禁用）。Recharts 折线动画 `isAnimationActive=true / 550ms ease-out` 同步打开，按下 24h / 7d / 30d 切换器有"刷新"反馈。
- **字体栈切换到 Plus Jakarta Sans + JetBrains Mono**：`index.html` 通过 Google Fonts `preconnect` 预连 + `display=swap` 异步加载（300/400/500/600/700/800 + Mono 400/500），`tailwind.config.js` 把 `font-sans` 首位换成 `Plus Jakarta Sans`、新增 `font-mono` 栈以 `JetBrains Mono` 为先。中文回落 `PingFang SC / Hiragino Sans GB / 微软雅黑` 不变；移除原本永远不命中的 `SF Pro Display/Text` 与 `Helvetica Neue`。`display=swap` 确保字体未到达时先用回落字体渲染、不阻塞首屏。

## [0.5.1] - 2026-05-29 *(superseded by 0.5.2)*

> **此版本已被 0.5.2 整合并取代**——0.5.1 在小流量场景下仍存在单账号被打死的风险（账号风控 429 当 transient 重试），0.5.2 修复并整合所有功能。请直接升级到 0.5.2，跳过 0.5.1。

下方为 0.5.1 的原始内容，保留以便追溯。

### 💥 Breaking — 基础设施

- **彻底移除 Redis 依赖**：`anthropic/cache.rs` 整模块删除（约 740 行），`Cargo.toml` 删 `redis` crate，`docker-compose.yml` 删 `redis` 服务、`depends_on`、`redis-data` 命名卷，`config.example.json` 删 `redisUrl` / `cacheDebugLogging` / `cacheMaxReadRatio`，对应的 `Config::redis_url` / `cache_debug_logging` / `cache_max_read_ratio` 字段也删。已有部署里这三个配置字段会被忽略；不会破坏功能（只是无法识别），但**升级前请把它们从 `data/config.json` 删掉以免日后误以为还在生效**。
- **API 响应字段含义变化**：`/v1/messages` 响应里的 `usage.cache_creation_input_tokens` / `cache_read_input_tokens` 不再是「Redis 缓存」（已下线）也不是「Anthropic 上游缓存」（实测上游不下发），而是**中转层自己根据请求体 `cache_control` 断点产出的提示词缓存计数**。详见下方"中转层 Prompt Cache"章节。
- **`UsageRecordHook::record` 签名加 `credits: f64` 参数**；`ClientKeyManager::record_usage` 同步加。下游若 fork 了 handler 调用链需要补一个参数。

### ✨ 新功能 — 中转层 Prompt Cache（无外部依赖）

- **进程内提示词缓存**：新模块 `src/anthropic/prompt_cache.rs`。按 Anthropic 协议把请求体里 `cache_control` 断点（最多 4 个，分布于 `tools` / `system` / `messages[].content`）切成一组前缀段，对每段累加 SHA-256 哈希作为 key，TTL 默认 5 分钟、`cache_control.ttl="1h"` 解析为 1 小时。
  - **命中规则**：取最深命中段索引 `i*` → `cache_read = segments[i*].cumulative_tokens`，`cache_creation = total - segments[i*].cumulative_tokens`；全部 miss 时 `cache_creation = total`、`cache_read = 0`。每次请求结束时把所有段（命中 / 未命中）写回，刷新 LRU `last_hit_at` 与 TTL。
  - **持久化**：cache_dir 下 `prompt_cache.json`（按字节哈希 → `{tokens, expires_at, last_hit_at}`），后台 60s 一次 flush（仅 dirty 时落盘），启动时过滤过期条目重建。LRU 上限 4096 条。
- **流式 / 非流式两路接线**：`StreamContext` / `BufferedStreamContext` 新增 `set_initial_cache_tokens(cc, cr)`。`message_start` / `message_delta.usage` 与非流响应的 `usage.cache_creation_input_tokens` / `cache_read_input_tokens` 全部由 PromptCache 真实产出，不再硬编码 0。
- **真实验证**：两次完全相同的 `/v1/messages` 请求（带 `cache_control: ephemeral` 系统提示），第一次 `cache_creation=94 / cache_read=0`，第二次 `cache_creation=0 / cache_read=94`，精确按协议工作。
- **9 个新单测**覆盖 lookup / record / TTL / LRU / flush + reload / 多断点命中。

### ✨ 新功能 — Credit 计费维度

- **解析上游 meteringEvent**：之前 `Event::Metering` 被丢成 `()`。新模块 `src/kiro/model/events/metering.rs` 严格解析真实 payload `{unit, unitPlural, usage(f64)}`（实测确认上游不下发 token / cache 字段；不做字段名候选 fallback，直接读 `usage`）。
- **Credit 全链路**：`UsageRecord` / `BucketStats` / `TimeSeriesPoint` / `OverviewStats` / `ClientKey` 全部新增 `credits` 字段；流式 / 非流式 hook 都把 `credits` 累加并写入。
- **API 暴露**：`GET /api/admin/stats/overview` 多 `todayCredits` / `weekCredits`；`GET /api/admin/stats/timeseries` 每个时序点多 `credits`。
- **前端展示**：概览页顶部新增 "近 X Credit" 卡片（grid 由 4 列改为 5 列）；时序图 Tooltip 单独一行展示「本桶 Credit」（量级与 token 差异过大，不画线）。

### ✨ 新功能 — 仪表盘改造

- **Token 使用趋势图重做**（`time-series-chart.tsx`）：5 系列折线（Input / Output / Cache Creation / Cache Read / Cache Hit Rate），双 Y 轴：左轴 token 量级（紧凑 K/M/B），右轴 0–100% 命中率（紫色虚线，刻度固定 [0, 20, 40, 60, 80, 100]）；自定义深色 Tooltip，命中率 = `cacheRead / (input + cacheRead)`。全零数据时左轴强制显示 `0` 刻度，避免空白图表；Legend 改空心圆 + 英文标签。
- **顶部卡片随时间窗切换**：之前调用 / Token 卡片永远显示「今日」，新增 `useMemo` 把当前 `seriesData` 按 24h / 7d / 30d 聚合，标题动态变成"近 24 小时调用 / 输入 Token"等。`activeClientKeys` 仍是当前活跃数。
- **数值紧凑格式 K/M/B**：新增 `formatNumber()` 工具（基于 `Intl.NumberFormat` compact notation），覆盖概览卡片 / 模型表 / 凭据柱图 / 时序图 / 凭据列表 Badge。`formatCredits()` 对 credit 浮点专用：`≤ 0` → `"0"`、`< 1000` → 3 位小数、`≥ 1000` → K/M/B。Y 轴 / Tooltip / 表格全走同一格式器。
- **凭据柱图按 email 显示**：之前 X 轴 label 是 `#id`（email 字段始终空），后端 `stats_by_credential` 在 handler 拼装时已经反查注入了 `email`，前端改为以 email 为主、`#id` 兜底；过长 email 截断到 22 字符（保留 @domain），完整 email 在 Tooltip 显示。

### ✨ 新功能 — KAM 凭据导出

- **新端点 `GET /api/admin/credentials/export?ids=...`**：导出选中凭据为 KAM 1.8.3+ 平铺 JSON 格式，含 `refreshToken` / `accessToken` / `clientSecret` 等敏感字段。
- **`MultiTokenManager::clone_all_credentials`** 用于 admin 服务层取完整凭据快照（脱敏由调用方控制）。
- **新 admin-ui 类型 `KamExportAccount` / `KamExportResponse`**，前端凭据列表批量选择后可一键下载。

### ✨ 新功能 — 体验改进

- **在线更新对话框 Release Notes 支持 Markdown 渲染**：之前折叠面板里的 Changelog 只走 `whitespace-pre-wrap` 渲染原文，标题 / 列表 / 链接全都显示成纯文本。改用项目内自带的轻量 markdown 渲染器（`admin-ui/src/components/markdown.tsx`，~280 行单文件、无外部依赖）：覆盖 `# – ####` 标题、`-/*/+` 与 `1. 2. 3.` 列表、`> 引用`、`---` 分隔线、围栏代码块、行内 `code`、`**加粗**` / `*斜体*` / `[文本](url)`。不引入 markdown-it / remark 等大型依赖，体积可忽略。
- **KAM 导入支持多文件批量合并**：`KamImportDialog` 文件选择器加 `multiple` 属性，一次可选多个 KAM 导出 JSON；前端把每个文件的 `accounts` 数组合并成一份再走原有解析与预览流程，单文件失败不影响其他文件继续导入；toast 总结展示成功合并的记录数与失败文件名。

### ✨ 新功能 — KAM 导入兼容
- **兼容 KAM 1.6.9+ 的毫秒时间戳 `expiresAt`**：旧版导出 RFC3339 字符串、新版改为毫秒数字。前端在解析时统一规范化为 ISO 字符串，下游导入逻辑无需关心两种格式。
- **打开对话框自动触发文件选择器**：减少一次点击，用户打开 KAM 导入对话框后直接进入选文件流程。

### 🛠 修复

- **Credit 数值小数位失控（0.5.0 → 0.5.1）**：`formatCredits()` 中 `value ≥ 1` 的分支会回退到 `formatNumber`，而 `formatNumber` 对 `< 1000` 的数直接 `String(value)`，导致 `1.5755479141293534` 这类长浮点被原样打印。修复后统一规则：
  - `≤ 0 / null / NaN` → `"0"`
  - `0 < value < 1000` → 保留 3 位小数（`1.576` / `0.017`）
  - `value ≥ 1000` → `Intl.NumberFormat` compact notation（`1.2K` / `3.4M`）
- **重启后用量统计丢失**：根因是当 `--credentials credentials.json`（无目录前缀）启动时，`PathBuf::from("credentials.json").parent()` 返回 `Some("")`，导致 `cache_dir = ""`：`UsageRecorder` 把 `usage_log.*.jsonl` 写到 CWD（路径无前缀），`UsageAggregator::rebuild_from_logs("")` 调用 `read_dir("")` 失败，重启后历史记录看似全丢。修复：`MultiTokenManager::cache_dir()` 与 `UsageRecorder::new` / `rebuild_from_logs` 都把空路径归一为 `.`，并把"创建目录失败 / 读取目录失败"由静默 `_` 改成 `tracing::warn!` 显式打印路径。重建完成日志带上目录与条目数。
- **`StatsResponse` 不再有 `let mut overview = ...` + `let _ = (&mut overview).today_calls;` 这种 dead-code 黑魔法**——直接用不可变 `overview`。

### 🎨 体验

- **API Key 随机生成器收紧**：之前默认 40 字节 base64url，会产生 `sk-admin--Wt2ZN...` 这种双连字符的视觉断裂。改为：字符表只含 `a-zA-Z0-9`（拒绝采样保证均匀），32 字符（~190 bit 熵），按对话框模式选择前缀（admin Key 用 `sk-admin-`，业务 Key 仍用 `sk-kiro-`）。**移除 `Math.random` 弱熵 fallback**，缺 `crypto.getRandomValues` 时直接抛错。

### 📦 依赖 / 构建

- **删除依赖**：Rust 端 `redis = "0.27"`。
- **前端构建分块**：`recharts` 及其 d3 依赖链单独成块（约 410 KB / gzip 106 KB），仅"概览"路由懒加载触发；`vendor` chunk 从 510 KB 缩到 69 KB；`sonner` 也单独成块；`chunkSizeWarningLimit` 提到 600 KB。
- **`.gitignore` / `.dockerignore`** 新增 `prompt_cache.json`（运行时落盘，不入库）。
- **测试覆盖**：单测从 233 增到 237（PromptCache 9 + Metering 2 - 现有路径调整）。

### 📦 升级指南

1. **`docker compose pull && docker compose up -d`** 即可。如果之前部署了 `redis` 服务，可以一并停掉删掉（数据无价值）。
2. **删除过时配置**：编辑 `data/config.json`，删除 `redisUrl` / `cacheDebugLogging` / `cacheMaxReadRatio` 三个字段（保留也只是被忽略，不会报错）。
3. **下游客户端**：响应里的 `cache_creation_input_tokens` / `cache_read_input_tokens` 字段含义变了——现在反映的是中转层提示词缓存而非上游缓存。如果下游用这两个字段做计费对账，需要重新理解口径（中转层缓存命中并不会减少上游 credit 消耗，是 SDK 体验优化）。
4. **历史用量**：`usage_log.*.jsonl` 的旧记录会被自动加载（`credits` 字段缺失时默认 0），重启不丢趋势。新的请求开始会带 credit。
5. **若你已经升级到 0.5.0**：直接升 0.5.2；不需要清理任何状态文件。
6. **0.5.2 增量项**：升 0.5.2 后，「账号风控失败转移」默认开启、冷却 600s。如不希望自动冷却（例如只用一两个账号、宁愿等冷却也不想被识别为风控），登录管理面板 → 顶栏「设置」→ 关闭"账号风控失败转移"。Thinking 模式 replay 修复无需手动操作。

## [0.4.0] - 2026-05-22

主题：把 kiro.rs 从「单 Key 的 Anthropic 协议适配器」推进到 Key 分发场景——加入面向下游用户的客户端 Key 分发、按 Key/凭据/模型维度的 Token 用量统计与仪表盘趋势可视化。

### ✨ 新功能 — 客户端 API Key 分发

- **新的两层 Key 模型**：`config.apiKey`（master）保留向后兼容，新增 `csk_*` 客户端 Key 层。每把 Key 独立启用/禁用、独立计数，泄露后只需替换一把而非全员换 master。
  - 持久化到 `client_api_keys.json`（与 `credentials.json` 同目录），无 SQLite 依赖
  - `subtle::ConstantTimeEq` 全表常量时间比对，防 HashMap 短路引发的时序攻击
  - 鉴权顺序：master apiKey → 客户端 Key；命中后通过 `Extension(KeyContext { key_id })` 注入下游 handler
- **Admin API**：6 个新端点
  - `GET /api/admin/client-keys` 列表（脱敏展示 `csk_abcd...mnop`）
  - `POST /api/admin/client-keys` 创建（响应里返回明文 key，**仅此一次**）
  - `PUT /api/admin/client-keys/:id` 改名 / 改描述
  - `DELETE /api/admin/client-keys/:id` 删除
  - `POST /api/admin/client-keys/:id/disabled` 启用/禁用
  - `POST /api/admin/client-keys/:id/reset-stats` 重置累计计数
- **新前端 Tab「客户端 Key」**：表格展示名称、脱敏 Key、状态、总调用、总输入/输出 Token、最后使用时间、操作按钮；新建后弹出明文一次性展示对话框（带显示/隐藏切换、复制按钮）。

### ✨ 新功能 — Token 用量统计与仪表盘

- **请求级用量记录**：`/v1/messages` 流式 / 缓冲流式 / 非流式三条路径在结束（含错误）时统一写入用量。`KiroProvider` 改造返回 `KiroCallResult { response, credential_id }`，把命中凭据 ID 透传到 handler 用于按上游凭据维度聚合。
- **JSONL 持久化 + 内存聚合**：
  - `usage_log.YYYY-MM-DD.jsonl` 按日滚动，单行一条记录（ts/keyId/credentialId/model/inputTokens/outputTokens/cacheCreation/cacheRead/durationMs/status）
  - `UsageAggregator` 维护 168 小时桶 + 31 天桶的 ring buffer，启动时从历史 JSONL 重建，重启不丢趋势
  - 后台任务每 24 小时清理超过 31 天的旧日志
- **统计 API**：4 个新端点
  - `GET /api/admin/stats/overview` — 今日 / 最近 7 天的调用次数、Token、错误数 + 活跃 Key/凭据数
  - `GET /api/admin/stats/timeseries?range=24h|7d|30d` — 按桶聚合的时序点
  - `GET /api/admin/stats/by-model?range=...` — 各模型的 calls / input / output 排行
  - `GET /api/admin/stats/by-credential?range=...` — 各上游凭据贡献，附 email
- **新前端 Tab「概览」**：4 张统计卡片 + 三类图表
  - 时间 × Token 折线图（input/output/cacheRead/cacheCreation 四条线）
  - 按模型分布饼图 + 详情表
  - 按上游凭据堆叠柱图（Top 12）
  - 右上 24h / 7d / 30d 切换器
- **客户端 Key 维度的累计**：成功请求会同时把 input/output/cacheCreation/cacheRead 累加到对应客户端 Key 的总数，列表页直接看到每把 Key 的总消耗。

### 🎨 界面 — 多 Tab 导航 + 顶栏统一

- **从单 Dashboard 改为三 Tab SPA**：概览（默认）/ 凭据管理 / 客户端 Key。`App.tsx` 顶栏内置 Tab，URL hash（`#/overview` / `#/credentials` / `#/keys`）同步，未引入 react-router。
- **`TopbarTools` 工具组件**：把"负载均衡切换 / 刷新 / 在线更新 / 设置（含 Key 修改对话框）"从凭据管理 Tab 抽到 App 顶栏，三个 Tab 都可访问；刷新按钮一次性失效凭据 / 客户端 Key / stats 三类查询。
- **响应式 Tab 行**：桌面端 Tab 在 logo 旁，移动端折到顶栏第二行。
- **Dashboard 嵌入模式**：新增 `embedded` prop，在 Tab 内渲染时隐藏自带顶栏、跳过外层 padding，避免与 App 顶栏重复。

### 🛠 性能 / 体验

- **图表渲染优化**：三个 chart 全部 `React.memo` + `useMemo` 稳定 props 引用，关闭 recharts 默认 1.5s 入场动画；时序图根据点数自动稀疏 X 轴 ticks（≤12 全显，≤48 取 12 个，更长取 16 个）避免标签重叠引发的反复布局测量。
- **数据查询节流**：所有 stats hook 加 `staleTime: 25s`（30s refetchInterval 之内切 Tab 不重复请求）+ `placeholderData: keepPreviousData`（切 range 期间复用旧数据避免 chart 卸载重挂）+ `refetchOnWindowFocus: false`（避免窗口聚焦同时打 4 个请求）。
- **图表 Tooltip 暗色主题**：抽出 `tooltip-style.ts` 共享样式，`labelStyle` / `itemStyle` 单独设白色——recharts 不让 label/item 继承 `contentStyle.color`，这是之前看不清的根因。
- **柱图布局修复**：图例从底部移到右上，X 轴 `height: 56` + bottom margin `48`，避免「输入/输出」图例覆盖倾斜的 X 轴标签。

### 📦 依赖 / 构建

- **新增前端依赖**：`recharts ^2.15`（仪表盘图表，~95KB gzip）。
- **`.gitignore` 新增 4 类条目**：`client_api_keys.json`（含明文 csk）、`usage_log.*.jsonl`、`usage_stats.json`、`*.staged-*` / `*.backup`（在线更新产物）。

### 📦 升级指南

1. **现有部署直接 `docker compose pull && docker compose up -d`**，旧 master `apiKey` 完全兼容，所有现有客户端无需改动。
2. **想用客户端 Key 分发**：登录 Admin 面板 → 切到「客户端 Key」Tab → 新建 → 把弹窗里的明文 `csk_xxx` 给下游用户，让客户端把它放进 `x-api-key` 或 `Authorization: Bearer` 头。
3. **想看仪表盘**：`/admin` → 概览 Tab，新部署默认无历史数据，发起几次请求即可看到趋势开始填充。
4. **历史日志**：服务启动时自动从 `usage_log.*.jsonl` 重建近 31 天聚合，无需迁移脚本。

## [0.3.2] - 2026-05-22

主题：把在线更新对话框打磨成可日常使用的工具——加入 GitHub Token 配置消除限流问题，加入版本验证防止重复更新，加入 staged 复用让两步操作变成无缝衔接，并清理视觉噪音。

### ✨ 新功能

- **GitHub Token 配置**：在线更新对话框新增 GitHub Personal Access Token 输入区，保存后所有 GitHub API 调用都会带上 `Authorization: Bearer <token>`，把限流从匿名 60/小时 提升到认证 5000/小时。匿名访问触发 `403 API rate limit exceeded` 时不再无解。
  - 配置文件新增 `githubToken` 字段（顶层）
  - Admin API：`GET /api/admin/config/update` 返回 `githubTokenSet: bool`（不回明文，避免泄露），`PUT /api/admin/config/update` 接受 `githubToken: string`（空字符串表示清除）
- **Token 验证 + 限流可视化**：新增 `POST /api/admin/system/update/rate-limit` 端点，调用 GitHub `/rate_limit` 实时返回当前限额状态。该 GitHub 端点本身不消耗任何配额，可放心反复调用。
  - 前端在 token 输入框旁加「验证」按钮：保存前用输入的 token 试一次，避免保存了无效 token
  - 对话框打开时自动用已保存 token 查一次限额，展示「已认证 / 匿名」徽章、`@username`、`已用 N/上限`、进度条、重置时间
  - 剩余次数低于上限 5% 时进度条变 amber 提醒
- **「上次更新于」时间戳**：apply 成功后记录 RFC3339 时间到 `updateLastAppliedAt` 字段，对话框展示「上次更新于：YYYY-MM-DD HH:MM:SS」（本地时区）。回退时清空。

### 🛠 体验优化

- **拉取镜像 → 更新并重启 复用 staged**：「拉取镜像」按钮不再是死功能。下载产物保存到 `<exe>.staged-<version>`，「更新并重启」检测到同版本 staged 时直接 install + exit，跳过重复下载。两步操作之间几乎无感知延迟。
- **当前已是最新版本时禁用「更新并重启」**：避免对相同版本做无意义的下载-替换-重启。后端在 `apply_image_update` 入口加版本检查，前端按钮根据 `hasUpdate` 同步禁用，鼠标悬停显示原因。
- **GitHub Token Scopes 不再展示**：原本会把 token 的 OAuth scopes 列出来（如 `admin:org, repo, ...`），是不必要的权限信息泄露。后端不再读取 `X-OAuth-Scopes` header，前端不再显示 Scopes 行。

### 🎨 界面调整

- **更新对话框扁平化**：移除外层卡片包装与 4 层嵌套边框，三个分区改为 `<section>` + `border-t pt-4` 顶分隔线。
- **取消「有更新」时整块变黄**：原本有更新时整个面板背景变 amber，已经有绿色「可更新」徽章传达同样信息。现在面板始终是中性背景，只保留徽章。
- **限流摘要卡内嵌**：限流状态展示不再是独立带边框的卡片，而是直接平铺在 GitHub Token 区下方，仅用图标颜色（绿/红）和进度条颜色（绿/黄）区分状态。

## [0.3.1] - 2026-05-22

### ⚠️ 不兼容变更（Breaking changes）

- **配置字段清理**：`config.json` 删除 `updateImage` 与 `updatePreviousImage` 字段，新增 `updatePreviousVersion`。`updateImage` 在新方案里没有意义（在线更新已不再操作 docker 镜像），保留只会误导。已存在的 `updateImage` 字段会被静默忽略。
- **Admin API 响应字段调整**：`GET /api/admin/config/update` 返回值移除 `image`，把 `previousImage` 改为 `previousVersion`；`PUT /api/admin/config/update` 不再接受 `image` 参数；`POST /api/admin/system/update/{pull,apply,rollback}` 响应移除 `image` 字段。前端已同步更新。
- **`docker-compose.yml` 移除 docker socket 与 compose 文件挂载**：在线更新不再需要这两个挂载点。继续使用旧 compose 文件部署也能跑通，但会带着不必要的安全风险。

### 🛠 在线更新机制改造

- **从「容器自管自重建」改为「文件级二进制替换」**：`apply_image_update` 不再调用 `docker compose pull/up`，改成下载 GitHub Releases 上对应平台的二进制压缩包，校验 `SHA256SUMS.txt`，原子替换 `<exe>`，旧版本备份为 `<exe>.backup`，最后调用 `std::process::exit(0)` 退出，由 `docker-compose.yml` 里的 `restart: unless-stopped` 接管重启。这样从根本上消除了"网络错误时旧容器被停止、新镜像没拉到、服务挂起"的事故路径。
- **回退也改为文件级**：`rollback_image_update` 从 `<exe>.backup` 还原可执行文件并退出进程，不再依赖 `kiro-rs:rollback` 镜像 tag，断网也能恢复。
- **`check_update` 统一走 GitHub Releases API**：取消对 Docker Hub `/v2/repositories/.../tags` 的依赖，单一 endpoint 既拿版本号又拿 changelog，请求次数减半。
- **移除 docker socket 与 docker CLI 依赖**：`Dockerfile` / `Dockerfile.release` 不再安装 `docker-cli` 与 `docker-cli-compose`；`docker-compose.yml` 删除 `/var/run/docker.sock` 与 `docker-compose.yml` 的挂载。镜像体积更小，容器逃逸面显著缩小。
- **删除 600+ 行旧逻辑**：`ComposeContext` / `detect_compose_metadata` / `tag_rollback_image` / `validate_image_ref` / `dockerhub_owner_repo` / `DockerHubTagsResponse` 等 docker 相关代码全部移除；`UpdateConfigResponse` / `ImageUpdateResponse` / `SetUpdateConfigRequest` 同步精简。
- **前端 UI 同步**：「在线更新」对话框移除「镜像」输入框与「保存配置」按钮（这两个控件操作的字段已不存在），保留「拉取镜像」「更新并重启」「回退到上一版本」三大功能按钮的位置、名称、操作流程不变。
- 配套加 `flate2` / `tar` / `zip` 依赖用于解压 release archive。

### 🚀 CI/CD 加速

- **前端只构建一次**：新增 `build-frontend` job，跑一次 `bun run build` 并把 `admin-ui/dist` 上传为 artifact；后续 7 个二进制矩阵 + 2 个镜像矩阵直接 `download-artifact` 复用，多平台 runner 不再重复装 Bun / 跑 vite。
- **release profile 调优**：`Cargo.toml` 把 `lto = true`（fat）改为 `lto = "thin"` + `codegen-units = 16`，单作业 `cargo build` 的链接耗时显著下降，对运行时性能影响可忽略。
- **Docker 镜像复用预编译二进制**：新增 `Dockerfile.release`，CI 里 `build-images` 改为 `needs: build-artifacts`，下载已经构建好的 `Linux-musl-x64` / `Linux-musl-arm64` 二进制后直接 `COPY` 进 alpine，跳过 Dockerfile 内重复的 cargo 编译阶段。开发用 `Dockerfile`、`docker-build.yaml` 仍走完整源码构建。
- **mold linker（Linux gnu 目标）**：在 `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu` 矩阵上通过 `rui314/setup-mold@v1` 启用 mold，`RUSTFLAGS=-C link-arg=-fuse-ld=mold`，链接阶段从 5–15s 降至 1–3s。macOS / Windows / musl 目标保持默认链接器以避开兼容性风险。
- **`cargo build` 全部加 `--locked`**：确保 CI 构建严格按提交的 `Cargo.lock` 解析，避免锁文件漂移导致重复编译。

### 📦 升级指南

1. **保留 docker compose 部署的用户**：直接 `docker compose pull && docker compose up -d` 升到 0.3.1；老 compose 文件里的 `docker.sock` / `docker-compose.yml` 挂载可以从下次 PR 起删掉，不影响功能。
2. **手动跑二进制的用户**：从 GitHub Releases 下载新版本替换原有二进制即可。
3. **配置文件清理**：可以从 `data/config.json` 中删除 `updateImage` / `updatePreviousImage` 字段，服务不会再使用它们。

## [0.3.0] - 2026-05-22

### ⚠️ 不兼容变更（Breaking changes）

- 容器发布渠道从 GitHub Container Registry **迁移到 Docker Hub**。
  - 默认镜像由 `ghcr.io/zyphrzero/kiro-rs:latest` 改为 `zyphrzero/kiro-rs:latest`。
  - 旧的 GHCR 镜像 **不再发布新版本**；继续使用 GHCR 的部署需要把镜像引用改回 `ghcr.io/...` 自行同步。
- 配置文件移除以下字段（直接删除即可，迁移逻辑参见下方"在线更新"小节）：
  - `githubToken`
  - `updateComposeFile`
  - `updateService`
- `docker-compose.yml` 默认镜像同步切换到 Docker Hub。

### 🛠️ 构建工具链升级

- **包管理器迁移到 Bun**
  - 删除 `pnpm-lock.yaml` / `pnpm-workspace.yaml` / `.npmrc`，新增 `admin-ui/bun.lock` 锁文件。
  - `package.json` 用 `trustedDependencies` 字段替代 pnpm 的 `onlyBuiltDependencies`，继续放行 `@swc/core`、`esbuild` 的安装脚本。
  - `Dockerfile` 前端构建阶段改用 `oven/bun:1-alpine`，命令统一为 `bun install --frozen-lockfile --ignore-scripts` + `bun run build`。
  - GitHub Actions（`build.yaml` / `release.yaml`）用 `oven-sh/setup-bun@v2` 替换 `setup-node` + `pnpm/action-setup`，CI 不再依赖 corepack；bun 版本锁定到 `1.3`，并通过 `actions/cache` 缓存 `~/.bun/install/cache`，多平台矩阵复用同一份依赖缓存。
  - `README.md` 与 `src/admin_ui/router.rs` 中的 `pnpm` 命令提示同步更新为 `bun`。
- **前端依赖整体升级到 2026 主版本**
  - Vite 5 → **8**（Rolldown 引擎，构建时间从约 3.7 s 降到约 0.4 s）。
  - React 18.3 → **19.2**，类型包 `@types/react` / `@types/react-dom` 同步升到 19.x。
  - TypeScript 5.6 → **6.0**；移除 TS 6 已弃用的 `tsconfig.json#baseUrl`，仅保留 `paths`（依赖 `moduleResolution: bundler` 解析）。
  - 前端 React 插件 `@vitejs/plugin-react-swc` 4 → **`@vitejs/plugin-react` 6**：Vite 8 + Rolldown 自带 oxc 转换，官方推荐切回原版 `plugin-react`，移除 swc 二进制依赖。
  - Tailwind 3.4 → **4.3**：新增 `@tailwindcss/postcss` PostCSS 插件，`postcss.config.js` 切换插件键名；`src/index.css` 用 `@import "tailwindcss"` 替代 `@tailwind base/components/utilities`，并通过 `@config "../tailwind.config.js"` 复用既有 hsl 主题变量与 `@apply` 配置。
  - Radix UI 套件、`@tanstack/react-query`、`axios`、`lucide-react`、`sonner`、`tailwind-merge` 一并升到当前 latest。
  - 新增 `src/vite-env.d.ts`（`/// <reference types="vite/client" />`），让 TS 6 严格模式下 `import './index.css'` 类型检查通过。
- **构建产物分包优化**
  - `vite.config.ts` 启用 `build.rolldownOptions.output.codeSplitting.groups`，按 `react` / `radix` / `query` / `icons` / `vendor` 拆分三方依赖 chunk，业务 chunk 体积全部回落到 500 kB 以下，便于浏览器缓存复用。
  - `App.tsx` 改用 `lazy` + `Suspense` 懒加载 `Dashboard`，未登录用户首屏不再下载管理面板代码。

### ✨ 新功能

- **首次启动自动初始化配置文件**
  - 启动时若 `config.json` 不存在，会自动写入一份最小默认配置：监听 `0.0.0.0:8990`、随机生成 `apiKey`（`sk-kiro-rs-...`）和 `adminApiKey`（`sk-admin-...`），并打印到日志。
  - `credentials.json` 不存在时自动写入 `[]`，后续可直接在 Admin UI 添加凭据。
  - Docker 首次部署不再需要手工准备 `data/config.json` / `data/credentials.json`，挂上 `data/` 目录直接 `docker compose up -d` 即可。
- **镜像在线更新**
  - 全新 Admin UI「镜像在线更新」面板：支持一键更新、回退、查看版本信息。
  - compose 文件路径与 service 名运行时从当前容器的 docker compose 标签自动发现，前端无需配置。
  - 更新前自动给当前镜像打 `kiro-rs:rollback` 本地 tag，断网也能一键回退到上一版本。
  - 失败提示更友好：检测到 compose yml 不存在 / 是目录时给出可操作的中文提示。
- **检查更新**
  - 后台轮询 Docker Hub 仓库 tags，发现新语义化版本时在工具栏图标显示红点。
  - 弹窗内展示「当前版本 / 最新版本 / 构建类型 / 发布时间」，并提供"立即检查"按钮。
- **无人值守自动更新**
  - 新增 `updateAutoApply` / `updateAutoApplyTime` 两个配置：开启后每天到指定时间自动检查并应用新版本，单分钟去重 + 单版本去重。
  - Admin UI 提供开关 + 时间选择器，修改即时生效。
- **凭据列表**
  - 支持鼠标左键拖拽框选凭据，跨网格区域均可触发；按住 Ctrl/Meta 拖拽可附加到既有选区。
  - 新增「全选当前页 / 取消全选」按钮，与既有"已选 N"徽章并存。
  - 卡片左侧勾选框命中区放大到 28×28，更易点击。

### 🎨 界面调整

- 顶栏与登录页 logo 改为项目自定义 PNG（`kirors.png`），不再使用占位的渐变方块图标。
- 镜像在线更新弹窗精简：标题旁的 ℹ️ 图标 hover/点击展示前置条件 Tooltip，不再占用主体空间。
- Tooltip 触发逻辑修复：弹窗打开时不会再因为焦点自动落到 ℹ️ 上而立即弹出。

### 🛠️ 维护

- `Cargo.toml` 升级到 `0.3.0`；`admin-ui/package.json` 同步对齐到 `0.3.0`。
- GitHub Actions 工作流（`release.yaml` / `docker-build.yaml`）切换到 Docker Hub 推送，使用 `DOCKERHUB_USERNAME` + `DOCKERHUB_TOKEN` secrets 登录。
- Release Notes 自动从 `CHANGELOG.md` 抽取对应版本章节。

### 📦 升级指南

1. **Docker Hub 部署**（推荐）
   - 直接使用 `zyphrzero/kiro-rs:latest` 替换现有镜像引用。
   - 不再需要 `githubToken` 字段；默认 `docker-compose.yml` 已切换到 Docker Hub。
2. **保留 GHCR 部署**
   - 把 `updateImage` 改回 `ghcr.io/<owner>/kiro-rs:latest`；但此后该镜像不再随项目更新，请自行 fork 或镜像同步。
3. **配置文件清理**
   - 删除 `githubToken`、`updateComposeFile`、`updateService`（如果仍存在）。
   - 如需开启每日自动更新，添加 `"updateAutoApply": true` 与 `"updateAutoApplyTime": "03:00"`。
4. **首次发布**
   - 维护者需在仓库 Settings → Secrets 添加 `DOCKERHUB_USERNAME` + `DOCKERHUB_TOKEN`，否则 CI 推送会失败。

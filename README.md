# kiro-rs

`kiro-rs` 是一个用 Rust 编写的 Anthropic Messages API 兼容代理。它把
`/v1/messages`、`/v1/models`、`/v1/messages/count_tokens` 等 Anthropic 风格请求转换为 Kiro / Amazon Q 后端请求，并提供一个可选的 Web Admin 面板来管理凭据、客户端 Key、用量、代理池、请求日志和在线更新。

项目当前的核心目标是：让 Claude Code、Anthropic SDK 或其它兼容 Anthropic API 的客户端，通过统一的本地 / 自托管服务访问 Kiro 账号能力，同时在服务端集中处理多凭据、token 刷新、故障转移、用量统计和可观测性。

## 🔎 快速引导

- [声明](#notice)
- [功能](#features)
- [快速开始](#quick-start)
- [调用 API](#api-usage)
- [API 路由](#api-routes)
- [配置](#configuration)
- [凭据](#credentials)
- [模型](#models)
- [Thinking、工具与 WebSearch](#thinking-tools-websearch)
- [图片处理](#images)
- [用量、缓存与日志](#usage-cache-logs)
- [Admin UI](#admin-ui)
- [代理和 Region](#proxy-region)
- [负载均衡与故障转移](#load-balancing-failover)
- [在线更新和发布](#updates-release)
- [开发](#development)
- [目录结构](#project-structure)
- [License](#license)
- [社区支持](#community)
- [致谢](#acknowledgements)

<a id="notice"></a>
## 📚 声明

本项目仅供研究和自用。使用本项目产生的任何后果由使用者自行承担。本项目与 AWS、Kiro、Amazon Q、Anthropic、Claude 等官方实体无关，不代表任何官方立场。

<a id="features"></a>
## ✨ 功能

- **Anthropic Messages API 兼容**：`/v1/messages`、`/v1/models`、`/v1/messages/count_tokens`。
- **Claude Code 兼容端点**：`/cc/v1/messages`、`/cc/v1/messages/count_tokens`。
- 流式和非流式响应：支持 Anthropic SSE 事件格式。
- **多凭据管理**：OAuth、Builder ID、Social、Enterprise / IdC、Kiro API Key。
- 自动 token 刷新：支持刷新后回写 `credentials.json`。
- **多凭据调度**：`priority` 固定优先级和 `balanced` 均衡分配。
- **故障转移**：凭据失败、额度用尽、账号级 429 风控冷却、token 失效强制刷新。
- **profileArn 策略**：流式端点按账号类型注入真实 ARN 或 Builder ID 占位 ARN；用量类 / 头部类调用跳过占位 ARN。
- **端点抽象**：按凭据选择 `ide` 或 `cli` endpoint。
- **工具调用**：支持 `tool_use` / `tool_result` 配对、工具名缩短与反向映射。
- **Thinking / Reasoning 兼容**：支持 `thinking.type=enabled` / `adaptive`、Claude Code 默认 thinking 请求、Kiro 原生 `reasoningContentEvent` 到 Anthropic thinking / signature / redacted thinking 事件的转换。
- **WebSearch**：支持纯 `web_search` 请求和混合工具场景下的本地 agentic web_search loop。
- **图像处理**：入站图片按环境变量自动缩放 / 重编码，降低 AWS Q 单字段大小限制导致的 400 风险。
- **Prompt cache 计量**：模拟 Anthropic cache_control 的 `cache_creation` / `cache_read` token 统计。
- **用量统计**：按客户端 Key、模型、凭据、日期聚合 input/output/cache token 和 credits。
- **请求链路追踪**：SQLite `traces.db`，记录成功 / 失败请求、尝试链路和错误类型。
- 客户端 Key 分发：Admin 面板生成 `csk_*` Key，支持独立启停和统计。
- **Admin UI**：概览、凭据管理、客户端 Key、请求日志四个主视图。
- 代理能力：全局代理、凭据级代理、代理池、健康检查、轮询分配。
- **在线更新**：从 GitHub Release / Docker Hub 拉取新版本，支持镜像定时自动更新与手动回退。
- **多平台发布**：GitHub Release 构建 Windows、Linux、macOS 和 Docker Hub 多架构镜像。

<a id="quick-start"></a>
## 🚀 快速开始

### Docker

推荐生产部署使用 Docker。仓库提供的 `docker-compose.yml` 默认使用 Docker Hub 镜像：

```yaml
image: ${KIRO_RS_IMAGE:-zyphrzero/kiro-rs:latest}
ports:
  - "8990:8990"
volumes:
  - ./data/:/app/config/
```

部署：

```bash
mkdir -p /opt/kiro-rs/data
cd /opt/kiro-rs
curl -O https://raw.githubusercontent.com/ZyphrZero/kiro.rs/master/docker-compose.yml
docker compose up -d
```

首次启动时，程序会在挂载目录中自动生成：

```text
data/
├── config.json
└── credentials.json
```

`config.json` 会包含随机生成的 `apiKey` 和 `adminApiKey`。查看日志：

```bash
docker compose logs --tail=200 kiro-rs
```

也可以直接打开 `data/config.json` 查看：

```json
{
  "host": "0.0.0.0",
  "port": 8990,
  "apiKey": "sk-kiro-rs-...",
  "adminApiKey": "sk-admin-...",
  "region": "us-east-1",
  "tlsBackend": "rustls",
  "defaultEndpoint": "ide"
}
```

访问：

- API: `http://<host>:8990/v1/messages`
- Admin UI: `http://<host>:8990/admin`

指定镜像版本：

```bash
KIRO_RS_IMAGE=zyphrzero/kiro-rs:0.6.3 docker compose up -d
```

### 下载二进制

正式版本会在 GitHub Release 中发布以下平台产物：

- Windows x64
- Linux x64 / arm64
- Linux musl x64 / arm64
- macOS x64 / arm64

下载后把二进制放到工作目录，首次启动会自动生成 `config.json` 和 `credentials.json`。

```bash
./kiro-rs
```

Windows:

```powershell
.\kiro-rs.exe
```

指定配置文件：

```bash
./kiro-rs --config /path/to/config.json --credentials /path/to/credentials.json
```

### 从源码构建

前端 Admin UI 会通过 `rust-embed` 嵌入到最终二进制。构建后端前先构建前端：

```bash
cd admin-ui
bun install
bun run build
cd ..
cargo build --release
```

测试：

```bash
cargo test
```

<a id="api-usage"></a>
## 调用 API

`/v1` 路由支持 `x-api-key` 和 `Authorization: Bearer` 两种鉴权方式。Key 可以是主 `apiKey`，也可以是 Admin 面板生成的 `csk_*` 客户端 Key。

```bash
curl http://127.0.0.1:8990/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: sk-kiro-rs-..." \
  -d '{
    "model": "claude-sonnet-4-5-20250929",
    "max_tokens": 1024,
    "stream": true,
    "messages": [
      { "role": "user", "content": "Hello" }
    ]
  }'
```

Claude Code 兼容端点：

```bash
curl http://127.0.0.1:8990/cc/v1/messages \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sk-kiro-rs-..." \
  -d '{
    "model": "claude-sonnet-4-8",
    "max_tokens": 1024,
    "stream": true,
    "messages": [
      { "role": "user", "content": "Hello from Claude Code style endpoint" }
    ]
  }'
```

列出模型：

```bash
curl http://127.0.0.1:8990/v1/models \
  -H "Authorization: Bearer sk-kiro-rs-..."
```

估算 token：

```bash
curl http://127.0.0.1:8990/v1/messages/count_tokens \
  -H "Content-Type: application/json" \
  -H "x-api-key: sk-kiro-rs-..." \
  -d '{
    "model": "claude-sonnet-4-5-20250929",
    "messages": [
      { "role": "user", "content": "Count this." }
    ]
  }'
```

<a id="api-routes"></a>
## API 路由

### Anthropic 兼容

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/models` | 返回本服务声明支持的 Anthropic 模型列表 |
| `POST` | `/v1/messages` | Anthropic Messages API 兼容入口 |
| `POST` | `/v1/messages/count_tokens` | Anthropic count_tokens 兼容入口 |
| `POST` | `/cc/v1/messages` | Claude Code 兼容入口，流式事件顺序针对 Claude Code 调整 |
| `POST` | `/cc/v1/messages/count_tokens` | Claude Code 兼容 count_tokens |

### Admin

启用 `adminApiKey` 后会挂载：

| 路径 | 说明 |
|---|---|
| `/admin` | 嵌入式 Web 管理界面 |
| `/api/admin/credentials` | 凭据列表、新增、编辑、删除 |
| `/api/admin/credentials/{id}/balance` | 查询单个凭据订阅 / 用量 |
| `/api/admin/credentials/{id}/models` | 查询该凭据上游实际可用模型 |
| `/api/admin/client-keys` | 客户端 Key 管理 |
| `/api/admin/stats/*` | 用量统计 |
| `/api/admin/traces` | 请求链路追踪查询 |
| `/api/admin/proxy-pool` | 代理池 |
| `/api/admin/config/*` | 运行时配置 |
| `/api/admin/auth/*` | Social / IdC 登录流程 |
| `/api/admin/system/update/*` | 在线更新、回退、版本检查 |

Admin API 鉴权同样支持：

- `x-api-key: <adminApiKey>`
- `Authorization: Bearer <adminApiKey>`

<a id="configuration"></a>
## ⚙️ 配置

默认配置文件名是 `config.json`。首次启动如果文件不存在，会自动生成最小配置。

### 最小配置

```json
{
  "host": "0.0.0.0",
  "port": 8990,
  "apiKey": "sk-kiro-rs-change-me",
  "adminApiKey": "sk-admin-change-me",
  "region": "us-east-1",
  "tlsBackend": "rustls",
  "defaultEndpoint": "ide"
}
```

### 常用字段

| 字段 | 默认值 | 说明 |
|---|---:|---|
| `host` | `127.0.0.1` | 监听地址。自动生成配置时为 `0.0.0.0` |
| `port` | `8080` | 监听端口。自动生成配置时为 `8990` |
| `apiKey` | 无 | 主 API Key，调用 `/v1` 和 `/cc/v1` 必填 |
| `adminApiKey` | 无 | 设置后启用 `/admin` 和 `/api/admin` |
| `region` | `us-east-1` | 全局默认 Region |
| `authRegion` | 无 | token 刷新用 Region，未配置时回退 `region` |
| `apiRegion` | 无 | Kiro API 请求用 Region，未配置时回退 `region` |
| `defaultEndpoint` | `ide` | 凭据未指定 endpoint 时使用的端点 |
| `tlsBackend` | `rustls` | `rustls` 或 `native-tls` |
| `proxyUrl` | 无 | 全局代理，支持 `http://`、`https://`、`socks5://` |
| `proxyUsername` / `proxyPassword` | 无 | 全局代理认证 |
| `loadBalancingMode` | `priority` | `priority` 或 `balanced` |
| `accountThrottleFailover` | `true` | 账号级 429 suspicious activity 时是否冷却并切换凭据 |
| `accountThrottleCooldownSecs` | `1800` | 账号级风控冷却秒数 |
| `extractThinking` | `true` | 非流式响应是否把旧 `<thinking>` 文本提取成 thinking block |
| `traceEnabled` | `true` | 是否写入 `traces.db` |
| `traceRetentionDays` | `7` | trace 保留天数 |
| `usageLogRetentionDays` | `31` | `usage_log.*.jsonl` 保留天数 |
| `countTokensApiUrl` | 无 | 外部 count_tokens API 地址 |
| `countTokensApiKey` | 无 | 外部 count_tokens API Key |
| `countTokensAuthType` | `x-api-key` | `x-api-key` 或 `bearer` |
| `githubToken` | 无 | 在线更新访问 GitHub API 时使用，降低 rate limit 风险 |
| `updateAutoApply` | `false` | 是否每天自动检查并应用新版本 |
| `updateAutoApplyTime` | `03:00` | 自动更新时间，本地时区 `HH:MM` |

<a id="credentials"></a>
## 🔐 凭据

默认凭据文件名是 `credentials.json`。推荐通过 Admin UI 添加、登录和重登凭据；直接编辑文件时建议使用数组格式。

```json
[
  {
    "id": 1,
    "refreshToken": "xxx",
    "expiresAt": "2026-12-31T00:00:00Z",
    "authMethod": "idc",
    "provider": "BuilderId",
    "clientId": "xxx",
    "clientSecret": "xxx",
    "priority": 0
  }
]
```

### 支持的凭据类型

#### Builder ID / IdC

```json
{
  "refreshToken": "xxx",
  "expiresAt": "2026-12-31T00:00:00Z",
  "authMethod": "idc",
  "provider": "BuilderId",
  "clientId": "xxx",
  "clientSecret": "xxx"
}
```

#### Enterprise IAM Identity Center

```json
{
  "refreshToken": "xxx",
  "expiresAt": "2026-12-31T00:00:00Z",
  "authMethod": "idc",
  "provider": "Enterprise",
  "startUrl": "https://example.awsapps.com/start",
  "region": "us-east-1",
  "clientId": "xxx",
  "clientSecret": "xxx"
}
```

Enterprise / IdC 账号在流式调用前会按需调用 `ListAvailableProfiles` 解析真实 `profileArn`，成功后写回凭据。纯 Builder ID/free 账号没有 Enterprise profile 时，会回退到官方 IDE 使用的 Builder ID 占位 ARN，以避免流式端点缺少 `profileArn` 返回 400。

#### Social 登录

```json
{
  "refreshToken": "xxx",
  "expiresAt": "2026-12-31T00:00:00Z",
  "authMethod": "social",
  "provider": "Github"
}
```

`provider` 可为 `Github` 或 `Google`。Social 登录会使用固定 Social profile ARN。

#### Kiro API Key

```json
{
  "kiroApiKey": "ksk_xxx",
  "authMethod": "api_key",
  "endpoint": "cli"
}
```

也可以通过环境变量临时注入最高优先级 API Key 凭据：

```bash
KIRO_API_KEY=ksk_xxx ./kiro-rs
```

### 凭据字段

| 字段 | 说明 |
|---|---|
| `id` | 凭据 ID，Admin 管理时自动分配 |
| `refreshToken` / `accessToken` | OAuth token |
| `expiresAt` | RFC3339 过期时间 |
| `authMethod` | `idc`、`social`、`api_key`。旧值 `builder-id`、`iam` 会规范化为 `idc` |
| `provider` | `BuilderId`、`Enterprise`、`Github`、`Google`、`IAM_SSO` 等 |
| `clientId` / `clientSecret` | IdC 刷新 token 所需 OIDC client |
| `startUrl` | Enterprise IAM Identity Center Start URL |
| `profileArn` | 真实 profile ARN 或已知固定 ARN；通常由程序维护 |
| `priority` | 数字越小优先级越高 |
| `region` | 凭据级 Region，兼容旧配置 |
| `authRegion` | 凭据级 token 刷新 Region |
| `apiRegion` | 凭据级 API 请求 Region |
| `machineId` | 凭据级 machine id，未填时自动派生 |
| `email` / `subscriptionTitle` | Admin 查询后回填的展示信息 |
| `proxyUrl` | 凭据级代理；填 `direct` 表示绕过全局代理 |
| `proxyUsername` / `proxyPassword` | 凭据级代理认证 |
| `disabled` | 是否禁用 |
| `kiroApiKey` | `ksk_*` Kiro API Key |
| `endpoint` | `ide` 或 `cli`，未填使用 `config.defaultEndpoint` |

<a id="models"></a>
## 模型

`GET /v1/models` 返回本服务声明支持的模型 ID。真实可用性仍取决于上游账号订阅；Admin 的“凭据模型”会查询该凭据的上游真实可用模型列表。

当前静态列表包含：

- `claude-opus-4-8` / `claude-opus-4-8-thinking`
- `claude-sonnet-4-8` / `claude-sonnet-4-8-thinking`
- `claude-opus-4-7` / `claude-opus-4-7-thinking`
- `claude-opus-4-6` / `claude-opus-4-6-thinking`
- `claude-sonnet-4-6` / `claude-sonnet-4-6-thinking`
- `claude-opus-4-5-20251101` / `claude-opus-4-5-20251101-thinking`
- `claude-sonnet-4-5-20250929` / `claude-sonnet-4-5-20250929-thinking`
- `claude-haiku-4-5-20251001` / `claude-haiku-4-5-20251001-thinking`

模型映射按关键词归一化到 Kiro 内部模型 ID：

| 请求模型关键词 | 上游模型 |
|---|---|
| `sonnet` + `4-8` / `4.8` | `claude-sonnet-4.8` |
| `sonnet` + `4-6` / `4.6` | `claude-sonnet-4.6` |
| `sonnet` + `4-5` / `4.5` | `claude-sonnet-4.5` |
| `opus` + `4-8` / `4.8` | `claude-opus-4.8` |
| `opus` + `4-7` / `4.7` | `claude-opus-4.7` |
| `opus` + `4-6` / `4.6` | `claude-opus-4.6` |
| `opus` + `4-5` / `4.5` | `claude-opus-4.5` |
| 任意 `haiku` | `claude-haiku-4.5` |

没有命中上述规则的模型会作为不支持模型处理。

上下文窗口估算：

- `claude-sonnet-4.6`、`claude-sonnet-4.8`、`claude-opus-4.6`、`claude-opus-4.7`、`claude-opus-4.8`：`1_000_000`
- 其它模型：`200_000`

<a id="thinking-tools-websearch"></a>
## Thinking、工具与 WebSearch

### Thinking

客户端可以显式发送 Anthropic `thinking` 字段，也可以直接使用带 `-thinking` 后缀的模型名。Claude Code 当前也可能在普通模型名下默认发送 `thinking.type=enabled`；服务端会按请求体实际 thinking 配置处理，不依赖模型名是否带后缀。

普通 thinking：

```json
{
  "model": "claude-sonnet-4-8-thinking",
  "max_tokens": 4096,
  "thinking": {
    "type": "enabled",
    "budget_tokens": 20000
  },
  "messages": [
    { "role": "user", "content": "推理一下这个问题" }
  ]
}
```

`budget_tokens` 会限制在 `24576` 以内。

模型名带 `-thinking` 后缀时会自动覆写 thinking 配置：

- Opus 4.6：`thinking.type=adaptive`，并默认设置 `output_config.effort=high`。
- 其它 thinking 模型：`thinking.type=enabled`，`budget_tokens=20000`。

Adaptive thinking：

```json
{
  "model": "claude-opus-4-6-thinking",
  "max_tokens": 4096,
  "thinking": {
    "type": "adaptive"
  },
  "output_config": {
    "effort": "high"
  },
  "messages": [
    { "role": "user", "content": "给出完整分析" }
  ]
}
```

`additionalModelRequestFields.output_config` 是 Kiro 上游的窄兼容字段。当前只会在已知可接受该字段的 Opus 4.6 adaptive thinking 路径上传递；Sonnet 4.5 / 4.8、Opus 4.6 非 adaptive thinking 等路径会跳过该字段，避免上游返回 `additionalModelRequestFields is not supported for this model`。

Kiro 上游可能返回原生 `reasoningContentEvent`。`kiro-rs` 会把它转换为 Anthropic 兼容内容：

- `text` → 流式 `thinking_delta`，非流式 `thinking` block。
- `signature` → 流式 `signature_delta`，非流式 `thinking.signature`。
- `redactedContent` → `redacted_thinking` block。
- 如果当前请求没有启用 thinking，明文 reasoning 会降级为普通 text；签名和 redacted 内容不会输出。

非流式响应优先使用原生 reasoning 事件；只有没有原生 reasoning 时，才回退到旧的 `<thinking>...</thinking>` 文本提取路径。

### Tool Use

服务端会把 Anthropic tools 转成 Kiro 工具定义，并处理以下兼容逻辑：

- 长工具名会被缩短，并在响应流中恢复原始名称。
- 孤立的 `tool_use` / `tool_result` 会被过滤或修复，避免上游因消息配对错误返回不可恢复错误。
- tool_result 中的图片会提升到 Kiro 顶层图片字段，并走同一套图片缩放逻辑。

### WebSearch

支持 Anthropic web_search tool：

```json
{
  "model": "claude-sonnet-4-8",
  "max_tokens": 2048,
  "stream": true,
  "tools": [
    {
      "type": "web_search_20250305",
      "name": "web_search",
      "max_uses": 5
    }
  ],
  "messages": [
    { "role": "user", "content": "搜索今天的相关信息" }
  ]
}
```

纯 web_search 请求会直接走上游 MCP 搜索接口。混合工具场景下，如果上游返回只包含 `web_search` 的工具调用，`kiro-rs` 会内部调用同一套 MCP 搜索接口，把结果作为 tool_result 喂回上游，直到上游停止搜索或达到轮数限制；其它工具调用会原样返回给客户端。

<a id="images"></a>
## 图片处理

入站图片会在本地 CPU 上按需压缩，默认策略：

- 长边上限：`1568px`
- base64 字段大小上限：`400000`
- JPEG 质量：`85`
- PNG / JPEG / WebP 大图会重编码为 JPEG
- GIF 保留原格式，避免破坏动画
- 解码失败时保留原图并记录 warning，不会让整个请求失败

环境变量：

| 变量 | 默认值 | 说明 |
|---|---:|---|
| `KIRO_RS_IMAGE_RESIZE` | `1` | `0`、`false`、`no`、`off` 可关闭 |
| `KIRO_RS_IMAGE_MAX_LONG_SIDE` | `1568` | 长边像素上限 |
| `KIRO_RS_IMAGE_MAX_BYTES` | `400000` | base64 字段大小阈值 |
| `KIRO_RS_IMAGE_JPEG_QUALITY` | `85` | JPEG 输出质量 |

<a id="usage-cache-logs"></a>
## 用量、缓存与日志

运行数据默认落在 `credentials.json` 所在目录。Docker 部署时就是 `./data/`。

```text
data/
├── config.json
├── credentials.json
├── client_api_keys.json
├── kiro_stats.json
├── kiro_balance_cache.json
├── proxy_pool.json
├── cache_metering.json
├── traces.db
└── usage_log.YYYY-MM-DD.jsonl
```

说明：

- `client_api_keys.json`：Admin 生成的 `csk_*` 客户端 Key，明文存储，用于鉴权。
- `kiro_stats.json`：凭据成功 / 失败 / 额度 / 冷却等统计。
- `kiro_balance_cache.json`：凭据订阅、额度、邮箱等缓存。
- `proxy_pool.json`：代理池与健康状态。
- `cache_metering.json`：prompt cache 计量缓存，定期落盘。
- `traces.db`：SQLite 请求链路追踪数据库，WAL 模式。
- `usage_log.*.jsonl`：按日滚动请求用量日志。

`CacheMeter` 会基于 `cache_control` 和会话信息模拟 Anthropic prompt cache 口径，输出互斥的：

- `input_tokens`
- `cache_creation_input_tokens`
- `cache_read_input_tokens`
- `output_tokens`

<a id="admin-ui"></a>
## 🖥️ Admin UI

启用 `adminApiKey` 后访问 `/admin`。当前页面：

- 概览：整体请求量、token、模型分布、凭据贡献。
- 凭据管理：添加、登录、重登、删除、禁用、优先级、余额、模型列表、超额开关、代理绑定。
- 客户端 Key：创建、编辑、禁用、删除、重置统计。
- 请求日志：查询 `traces.db`，查看失败原因、状态码、凭据尝试链路和 token 用量。

Admin 还提供：

- Social 登录和 IdC / Enterprise 登录流程。
- 全局代理设置和代理池健康检查。
- 负载均衡模式配置。
- 账号级风控故障转移配置。
- trace / usage log 保留策略。
- 在线更新、自动更新和回退。

<a id="proxy-region"></a>
## 代理和 Region

### Region 优先级

Token 刷新：

```text
credential.authRegion -> credential.region -> config.authRegion -> config.region
```

API 请求：

```text
credential.apiRegion -> config.apiRegion -> config.region
```

部分 REST / 管理类上游接口只在 `us-east-1` 和 `eu-central-1` 提供服务，代码会按账号区域选择候选端点并在必要时回退。

### 代理优先级

```text
credential.proxyUrl -> config.proxyUrl -> direct
```

凭据级 `proxyUrl` 填 `direct` 表示即使配置了全局代理也直连。

支持：

- `http://host:port`
- `https://host:port`
- `socks5://host:port`

如果 `rustls` 环境下代理或证书行为异常，可以在 `config.json` 中切到：

```json
{
  "tlsBackend": "native-tls"
}
```

<a id="load-balancing-failover"></a>
## 负载均衡与故障转移

`loadBalancingMode` 支持：

- `priority`：优先使用 priority 数字最小的可用凭据。
- `balanced`：在可用凭据之间均衡分配。

故障处理：

- 单凭据连续 API 失败会增加失败计数，达到阈值后跳过。
- 402 / quota exhausted 会禁用该凭据并切换。
- 401 / 403 中识别到 bearer token 失效时，会对该凭据强制刷新一次 token 后重试。
- 429 + suspicious activity 可触发账号级冷却并切换凭据。
- 400 客户端请求错误不会切换凭据。
- 网关超时和部分不可恢复错误会快速失败，避免一次请求内无限放大重试。

<a id="updates-release"></a>
## 在线更新和发布

发布 tag `vX.Y.Z` 会触发 Release workflow：

- 校验 `Cargo.toml` 版本和 tag 一致。
- 构建 Admin UI。
- 构建多平台二进制。
- 构建并推送 Docker Hub 多架构镜像。
- 创建 GitHub Release。

Docker 镜像：

- `zyphrzero/kiro-rs:<version>`
- `zyphrzero/kiro-rs:latest`
- `zyphrzero/kiro-rs:beta`（master beta 构建）

容器内在线更新会下载对应平台二进制并替换当前可执行文件；替换后进程退出，由 Docker `restart: unless-stopped` 拉起新进程。回退依赖本地 `<exe>.backup`。

<a id="development"></a>
## 开发

常用命令：

```bash
# 后端测试
cargo test

# 前端构建
cd admin-ui && bun run build

# 后端 release 构建
cargo build --release

# 开启 debug 日志
RUST_LOG=debug ./target/release/kiro-rs
```

发布前建议：

```bash
cargo test
cd admin-ui && bun run build
git diff --check
```

<a id="project-structure"></a>
## 目录结构

```text
.
├── src/
│   ├── anthropic/      # Anthropic API 兼容层
│   ├── kiro/           # Kiro / Amazon Q 上游、token、endpoint、event-stream
│   ├── admin/          # Admin API、用量、trace、代理池、在线更新
│   ├── admin_ui/       # 嵌入式 Admin UI 静态资源路由
│   ├── model/          # CLI 参数和 config.json 模型
│   ├── common/         # 通用鉴权工具
│   ├── image_resize.rs # 图片缩放与 token 估算
│   ├── token.rs        # count_tokens 估算和远程 count_tokens 调用
│   └── main.rs         # 入口
├── admin-ui/           # React Admin UI
├── .github/workflows/  # build、docker、release workflows
├── docker-compose.yml
├── Cargo.toml
└── CHANGELOG.md
```

<a id="license"></a>
## License

见 [LICENSE](LICENSE)。

<a id="community"></a>
## 💬 社区支持

欢迎到 [linux.do](https://linux.do/) 交流、分享和反馈。

<a id="acknowledgements"></a>
## 🙏 致谢

本项目的实现离不开社区项目和反馈的帮助：

- [hank9999/kiro.rs](https://github.com/hank9999/kiro.rs)
- [kiro2api](https://github.com/caidaoli/kiro2api)
- [proxycast](https://github.com/aiclientproxy/proxycast)
- [Kiro-account-manager](https://github.com/chaogei/Kiro-account-manager)

感谢所有 issue、PR、测试和部署反馈的贡献者。

# Changelog

All notable changes to this project are documented in this file. The format
loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### 🚀 CI/CD 加速

- **前端只构建一次**：新增 `build-frontend` job，跑一次 `bun run build` 并把 `admin-ui/dist` 上传为 artifact；后续 7 个二进制矩阵 + 2 个镜像矩阵直接 `download-artifact` 复用，多平台 runner 不再重复装 Bun / 跑 vite。
- **release profile 调优**：`Cargo.toml` 把 `lto = true`（fat）改为 `lto = "thin"` + `codegen-units = 16`，单作业 `cargo build` 的链接耗时显著下降，对运行时性能影响可忽略。
- **Docker 镜像复用预编译二进制**：新增 `Dockerfile.release`，CI 里 `build-images` 改为 `needs: build-artifacts`，下载已经构建好的 `Linux-musl-x64` / `Linux-musl-arm64` 二进制后直接 `COPY` 进 alpine，跳过 Dockerfile 内重复的 cargo 编译阶段。开发用 `Dockerfile`、`docker-build.yaml` 仍走完整源码构建。
- **mold linker（Linux gnu 目标）**：在 `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu` 矩阵上通过 `rui314/setup-mold@v1` 启用 mold，`RUSTFLAGS=-C link-arg=-fuse-ld=mold`，链接阶段从 5–15s 降至 1–3s。macOS / Windows / musl 目标保持默认链接器以避开兼容性风险。
- **`cargo build` 全部加 `--locked`**：确保 CI 构建严格按提交的 `Cargo.lock` 解析，避免锁文件漂移导致重复编译。

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


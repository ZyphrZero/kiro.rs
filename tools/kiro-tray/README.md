# KiroTray

Windows 系统托盘服务管理器，用于在后台无窗口运行 kiro-rs.exe，并提供可视化界面。

## 功能

- **系统托盘图标**：三种状态（绿色=运行中、黄色=启动中、红色=错误）
- **Chrome 应用模式管理后台**：双击图标或右键菜单打开独立窗口
- **自动重启**：崩溃后 3 秒自动重启，2 分钟内反复崩溃超过 5 次则停止
- **日志管理**：自动记录输出，超过配置大小后滚动
- **进程绑定**：使用 Job Object 确保托盘退出时 kiro-rs 也一并退出
- **开机自启**：可选开机自动启动

## 编译

需要 .NET Framework 4.0 或更高版本（Windows 7/10/11 自带）：

```cmd
build.cmd
```

编译成功后生成 `KiroTray.exe`（约 30KB）。

## 使用

1. 确保 `KiroTray.ini` 里的 `KiroDir` 指向正确的 kiro-rs 目录
2. 运行 `KiroTray.exe`
3. 托盘图标出现后，右键菜单有以下选项：
   - **打开管理后台**：用 Chrome 应用模式打开 http://localhost:8990/admin
   - **查看日志**：打开 `logs\kiro-rs.log`
   - **打开 kiro-rs 目录**：资源管理器打开配置的目录
   - **重启服务**：手动重启 kiro-rs
   - **崩溃自动重启**：开关自动重启功能
   - **开机自启**：添加/删除注册表启动项
   - **退出**：停止 kiro-rs 并退出托盘

## 配置文件

`KiroTray.ini`：

```ini
# KiroTray 配置。升级 kiro-rs 时只需改 KiroDir 指向新目录。
KiroDir=%USERPROFILE%\kiro-rs
ExeName=kiro-rs.exe
AutoRestart=1
LogMaxMB=10
```

- `KiroDir`：kiro-rs 所在目录，支持环境变量（如 `%USERPROFILE%`）
- `ExeName`：可执行文件名，默认 `kiro-rs.exe`
- `AutoRestart`：崩溃自动重启，`1` 或 `true` 启用，`0` 或 `false` 禁用
- `LogMaxMB`：日志文件超过此大小（MB）后滚动到 `.log.1`

## 图标

托盘图标使用 `icon_ok.ico`、`icon_bad.ico`、`icon_wait.ico`。

生成自定义图标：

```bash
pip install Pillow
python generate_icon.py
```

## 技术细节

- **单实例**：使用 Mutex 防止多开
- **Job Object**：托盘进程退出时（包括被任务管理器结束），子进程 kiro-rs 自动退出
- **Chrome 窗口复用**：检测已打开的管理后台窗口，避免重复打开
- **ANSI 转义码清理**：日志里移除颜色代码
- **健康检查**：每 5 秒检测端口连通性，更新托盘状态

## 兼容性

- Windows 7 或更高版本
- .NET Framework 4.0 或更高版本
- C# 5 语法（兼容 Windows 自带的 csc.exe）

## 许可

与 kiro-rs 主仓库保持一致。

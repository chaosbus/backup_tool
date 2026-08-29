# 应用备份工具（backup-tool）

基于 Rust + Tauri v2 的跨平台应用配置备份工具：按应用打包常用配置目录，支持 zip / tar.gz / 目录复制三种格式、glob 排除规则、保留份数清理、SHA-256 校验与备份历史，提供桌面 GUI 与无头 CLI 两种前端，共享同一核心引擎。

> 当前版本 `v0.1.0`，支持 Windows / Linux。设计文档见 [DESIGN.md](DESIGN.md)，任务规划见 [PLAN.md](PLAN.md)。

## 功能特性

- **按应用备份**：配置文件维护应用清单（名称、启用状态、分平台路径、排除规则），每次备份生成时间戳命中的归档
- **跨平台路径解析**：支持 `%APPDATA%`、`%USERPROFILE%`（Windows）与 `$HOME`、`~`、`${VAR}`（Linux/macOS）变量展开，其他平台的路径自动忽略、不报错
- **三种备份格式**：`zip`（默认）/ `tar.gz` 流式压缩，`dir` 纯目录复制；归档内保留源目录 basename 层级，符号链接保留链接本身
- **排除规则**：全局排除 + 每应用排除，glob 语法（如 `Cache/**`、`*.log`）
- **保留份数（retention）**：备份后自动清理超出 N 份的旧归档（0 = 永久保留），归档与 summary 同步删除
- **备份历史与自愈**：`history.json` 索引 + 每份归档旁的 `.summary.json` 元数据；索引丢失或损坏时自动扫描目录重建
- **实时进度**：字节 + 文件双维度进度，扫描 / 打包两阶段事件流，支持随时取消（文件边界中断，清理临时文件，保留已完成应用）
- **SHA-256 校验**：zip / tar.gz 备份自动计算校验和并写入 summary
- **原子写入**：归档、summary、history、配置文件均为"临时文件 + rename"原子落盘，异常断电不留半文件

## 架构

```
┌─────────────────────────────────────────────┐
│  GUI (Tauri v2 + 原生 JS)     CLI (headless) │
├─────────────────────────────────────────────┤
│  backup-core（纯 Rust 库，无 UI 依赖）         │
│  配置加载 │ 路径解析 │ 打包 │ 历史 │ 事件流     │
└─────────────────────────────────────────────┘
```

| Crate | 说明 |
|-------|------|
| `crates/backup-core` | 核心引擎：配置加载与校验、环境变量路径展开、zip/tar.gz/dir 打包、history 索引、summary sidecar、retention 清理、进度事件流 |
| `crates/backup-cli` | 无头命令行，适合 cron / Windows 任务计划程序调度 |
| `crates/gui` | Tauri v2 桌面应用：应用列表、实时进度条与日志、备份历史、设置管理 |

## 快速开始

### 环境要求

- Rust stable（edition 2021，建议 1.80+；本机开发验证于 1.96）
- GUI 额外需要 [Tauri v2 前置依赖](https://tauri.app/start/prerequisites/)：Windows 上需 WebView2 运行时与 MSVC Build Tools；Linux 上需 `libwebkit2gtk-4.1`、`libgtk-3` 等

### 构建

```bash
cd src

# 构建全部（core / cli / gui）
cargo build --release

# 产物位置
#   CLI: src/target/release/backup-tool(.exe)
#   GUI: src/target/release/backup-gui(.exe)
```

## CLI 用法

```bash
backup-tool backup [--app <APP_ID>]... [--all] [--format zip|tar.gz|dir]
backup-tool history [APP_ID]
backup-tool apps list | add <id> <name> <path> | remove <id>
backup-tool check
```

- 全局参数 `--config <path>` 指定配置文件；不指定时使用默认路径（见下文）
- `backup` 不带 `--app` 时备份所有已启用应用；`--format` 临时覆盖配置中的格式
- 退出码：有失败应用时为 `1`，可用于调度脚本判断
- `apps add` 未传 id 或 id 重复时自动生成 slug（如 `my-app` → `my-app-2`）并持久化

示例：

```bash
# 备份全部已启用应用
backup-tool backup

# 只备份 vscode，覆盖为 tar.gz 格式
backup-tool backup --app vscode --format tar.gz

# 校验配置与路径解析
backup-tool check

# 查看某应用备份历史
backup-tool history vscode
```

## GUI 使用

```bash
cargo run -p backup-gui
```

- **主界面**：三栏布局 —— 应用列表（勾选备份）/ 备份历史 / 操作与日志
- **备份**：勾选应用后点 [备份所选]，或 [全部备份]；进度条实时显示每个应用的扫描与打包进度，[停止] 在当前文件写完后取消
- **应用编辑**：双击应用行或 [+ 添加应用]；路径支持手动输入（实时解析环境变量并提示存在性）与系统目录选择器；ID 由名称自动生成
- **设置**：备份位置、格式、并行数、保留份数、清理时机、全局排除；支持恢复默认与从磁盘重新加载
- **恢复向导**：当前为占位（仅展示备份列表），完整恢复（预览、冲突策略、回滚）规划为 P2

## 配置文件

默认路径：

| 平台 | 路径 |
|------|------|
| Windows | `%APPDATA%\backup_tool\config.json` |
| Linux | `~/.backup_tool/config.json` |

自定义：CLI 用 `--config <path>`；GUI 启动前设置环境变量 `BACKUP_TOOL_CONFIG=<path>`。完整示例见 [src/examples/config.sample.json](src/examples/config.sample.json)。

```json
{
  "version": 1,
  "backup": {
    "dest": "~/backups",
    "format": "zip",
    "parallel": 2,
    "retention": 10,
    "cleanup": "after_each",
    "checksum": true,
    "excludes": ["**/.DS_Store", "**/desktop.ini"]
  },
  "apps": [
    {
      "id": "vscode",
      "name": "VSCode",
      "enabled": true,
      "paths": {
        "windows": ["%APPDATA%\\Code\\User"],
        "linux": ["$HOME/.config/Code/User"]
      },
      "excludes": ["Cache/**", "*.log"],
      "compress": true
    }
  ]
}
```

### backup 字段

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `dest` | string | `~/backups` | 备份基目录，支持变量展开；相对路径相对配置文件所在目录 |
| `format` | string | `zip` | `zip` / `tar.gz` / `dir`（应用级 `compress: false` 时强制 `dir`） |
| `parallel` | int | `2` | 并行备份的应用数；SSD 建议 2~4，HDD 建议 1 |
| `retention` | int | `10` | 每应用保留份数，超出删除最旧；`0` = 永久保留 |
| `cleanup` | string | `after_each` | `after_each`（尽早释放磁盘）/ `at_end` |
| `checksum` | bool | `true` | 为 zip / tar.gz 计算 SHA-256 |
| `excludes` | string[] | `[]` | 全局排除 glob |

### apps 字段

| 字段 | 说明 |
|------|------|
| `id` | 小写字母 / 数字 / `-` / `_`；用于归档命名与去重，新增时自动生成 |
| `paths` | 按平台键（`windows` / `linux` / `macos`）分组；只解析当前平台，其他平台静默跳过 |
| `excludes` | 应用级排除 glob，与全局规则合并生效 |
| `enabled` | `false` 时不参与 `backup`（GUI 中勾选仅是运行时行为，不改配置） |
| `compress` | `false` 时该应用直接复制目录（忽略 `backup.format`） |

### 备份存储布局

```
{dest}/
├── history.json                     # 索引（丢失时自动重建）
└── vscode/
    ├── vscode_20260829_093000.zip
    └── vscode_20260829_093000.zip.summary.json   # 自描述元数据
```

## 定时备份

按设计不内置定时器，用系统调度器调用无头 CLI：

```bash
# Linux crontab：每天 09:30 备份
30 9 * * * /usr/local/bin/backup-tool backup

# Windows 任务计划程序
schtasks /create /tn "BackupTool" /sc daily /st 09:30 /tr "C:\path\to\backup-tool.exe backup"
```

## 开发

```bash
cd src

cargo test --workspace                     # 运行全部测试
cargo clippy --workspace -- -D warnings    # Lint（CI 级别）
cargo fmt                                  # 格式化

node crates/gui/ui/check-gui.js            # GUI 前端守卫（构建时自动执行）
```

GUI 前端守卫会在构建时校验：脚本加载结构、Tauri 配置、以及 JS 引用的每个 DOM `#id` 必须存在于 `index.html`（防止绑定中断导致按钮无响应）。

## 路线图

| 阶段 | 内容 | 状态 |
|------|------|------|
| P0 | 核心备份流程 + 配置 + 路径解析 + 基础 GUI | ✅ 已完成 |
| P1 | 备份历史 + 设置页 + 排除规则 + CLI headless | ✅ 基本完成 |
| P2 | 恢复功能（预览 / 冲突策略 / 安全网回滚） | 🔜 规划中（恢复向导为 UI 占位） |
| P3 | 内置定时器 + 系统通知 + TUI + 并行优化 | ⏳ 待开始 |

## License

[MIT](LICENSE)

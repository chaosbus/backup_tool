# 设计修复计划

> 依据 DESIGN.md 评审结论整理。所有任务状态维护在本文件中。
> 提交信息格式：`[T-xxx] 简短描述`。

## 任务清单

| 任务 | 范围 | 目标 | 状态 |
|------|------|------|------|
| T-001 | 设计 + backup-core | 恢复/回滚闭环：新增 restore manifest，回滚读 manifest 而非日志；恢复支持 zip/tar.gz/dir；恢复前校验 checksum；路径穿越与符号链接越界防护；权限/mtime 保留策略 | 待开始 |
| T-002 | 设计 + backup-core | 备份源/目标隔离：禁止或自动排除 dest、.restore-tmp、.pre-restore-* 被当作源；app_id 路径安全校验；同应用重叠根路径去重/规范化 | 待开始 |
| T-003 | 设计 + backup-core + GUI | 跨平台配置保存：编辑器仅显示当前平台但保存时保留其他平台路径；config upsert 改为按平台合并 | 已完成 |
| T-004 | backup-core + GUI/TUI | 进度/事件模型：统一字节口径；核心侧节流或环形缓冲；有界 channel；保留结构事件 | 待开始 |
| T-005 | 设计 + backup-core | retention/history/storage：文件与目录备份统一删除；唯一命名与可靠排序；status 枚举及 cancelled 语义；dir checksum；history/manifest 原子写 | 待开始 |
| T-006 | backup-core | 路径解析：只对严格匹配的变量语法做平台判定；保留 `%`/`$` 字面量；补边界测试 | 待开始 |
| T-007 | 设计 + backup-cli | CLI 契约：定义 backup 默认行为；apps add/remove 持久化；restore 暴露全部冲突策略；文件日志与退出码 | 待开始 |
| T-008 | DESIGN.md | 范围/分发一致性：单文件与 NSIS/deb 表述、WebView2 运行时、macOS 范围、里程碑对齐 | 待开始 |
| T-009 | 全仓 | 验证收尾：cargo fmt、clippy、workspace 测试，Windows/Linux 关键路径回归 | 待开始 |
| T-010 | GUI | 修复添加应用无效：save_app 保留当前平台 paths；配置文件缺失时创建默认配置 | 已完成 |
| T-011 | GUI | 前端守卫：check-gui.js 扫描 ui/*.js 与 index.html，并接入 build.rs 自动执行 | 已完成 |
| T-012 | GUI + backup-core | 配置编辑一致性：upsert 仅读取当前平台 paths，save/remove 写盘失败不污染内存，日志安全 DOM 构建 | 已完成 |
| T-013 | GUI | 添加应用运行时修复收尾：启用 withGlobalTauri、加固 check-gui.js、write_config fsync 与命令级回归测试 | 已完成 |
| T-101 | GUI | 主界面布局对齐：顶部[全部备份][恢复][设置][退出]，操作区[备份所选][停止]，应用列表底部[+添加应用][编辑][删除] | 已完成 |
| T-102 | GUI | 应用编辑抽屉化：路径支持输入+系统目录选择，ID 隐藏 | 已完成 |
| T-103 | backup-core | ID 自动生成规则与冲突处理 | 已完成 |
| T-104 | backup-core | 备份 summary sidecar：JSON、checksum、路径与恢复信息 | 已完成 |
| T-105 | backup-core | summary 与 retention/rebuild 兼容 | 已完成 |
| T-106 | GUI | 设置页与 save_settings/default_settings/reload_settings | 已完成 |
| T-107 | GUI | 恢复向导 UI 占位（完整恢复为 P2） | 已完成 |
| T-108 | DESIGN.md | 同步 ID 自动生成、summary、恢复范围等设计基线 | 已完成 |
| T-109 | 全仓 | 前端守卫与回归：node check、fmt、workspace tests、GUI clippy | 已完成 |
| T-110 | 全仓 | reviewer 对照更新后的 DESIGN.md 检查 UI 与功能一致性 | 已完成 |

## 执行顺序建议

1. 先完成 T-001、T-002、T-003，这三项直接影响数据安全和配置不丢失。
2. 再推进 T-004、T-005、T-006，完善核心引擎的正确性与可观测性。
3. 随后处理 T-007、T-008，最后统一做 T-009 验证。

## 验收基线

- 备份源包含备份目标时不会递归打包历史备份。
- 恢复可从任意已支持格式执行，回滚不依赖可选日志。
- 分平台路径编辑不会丢失非当前平台配置。
- `cargo test --workspace`、`cargo clippy --workspace -- -D warnings` 通过。

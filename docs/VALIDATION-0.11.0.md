# 0.11.0 验证与发布记录

日期：2026-09-21，Windows x64。功能源码以已合并 `6383c9a01df9612dce66dcd957f45b7634649a23` 为基线；本轮补交接/发布文档，功能代码不变。发布与后续交接提交可由 `v0.11.0` 标签和 main 历史定位。

## 源码回归证据

- 已完成的全目标回归：404 项库测试、3 项 Rust CLI、18 项桌面、7 项协议集成，共 432 项通过，7 项外部条件测试默认忽略。
- [main CI 35604819755](https://github.com/Alice-Marx/Rust_for_AI-agent/actions/runs/35604819755) 的 head 为上述提交，结果 success。Windows 跑格式、全目标 Rust 与 npm；Linux/macOS 跑编译及选定核心模块回归，不能表述为三平台全量一致测试。
- 本轮重新执行 npm CLI 14 项测试，全部通过；Rust 功能代码没有变化，不重复把同一套结果记为新增测试。
- `cargo build --locked --release --bins` 成功，Rust CLI 报告 0.11.0；本地工具链 rustc/cargo 1.98.1，Node 24.18.0，Python 3.13.14。正式二进制不启用 `ui-snapshots`。

## release 程序与打包 npm 的迁移检查

从历史 v1 测试数据库通过 SQLite 只读备份生成独立副本，以 release 后端和从 0.11.0 npm tarball 解出的客户端执行，不访问模型端点：

1. v1 → v2 迁移成功，原有 5 个成功任务的 ID、模型、状态和输出保持一致。
2. Rust CLI 创建 Claude `high` 只读草稿，npm CLI 创建 DeepSeek `off` 草稿，两者档位按请求持久化。
3. 旧客户端省略档位时保持 `null`。
4. 后端重启后上述草稿/档位保持；启动时尝试覆盖配置被拒绝，草稿没有被启动。
5. 不支持的 Kimi `high` 在创建时被拒绝。
6. 原始 v1 数据库散列没有变化，模型调用数为 0。

脱敏结果见 [validation/0.11.0-smoke.json](validation/0.11.0-smoke.json)。完整本地日志不含在发行包中，也不需要接手者访问原测试数据。源码中 `workflow::tests`、`workbench_service::tests`、`team_service::tests` 保留可重新运行的回归。

## 发布包检查

npm tarball 只有 5 个预期文件：许可证、README、package.json 和两个客户端脚本；SHA-1 为 `2fd9e6ba84634179ead402f330c82dfef55dac73`。没有后端、账号配置或数据库。Windows 构建使用明确文件清单，CLIProxyAPI 沿用已校验的官方 7.3.7 发行。

安装器、便携包和 npm 包的最终 SHA-256 以 GitHub Release 附件 `SHA256SUMS.txt` 为准，避免将文件自己的散列嵌回自身造成循环依赖。安装、分发复核结果在发布完成后由开发交接报告记录。

## Windows 安装与启动

0.11.0 安装器在当前用户模式完成 0.10.0→0.11.0 程序升级，退出码 0，无需重启。安装后的三个二进制均与 release 构建 SHA-256 一致：

| 文件 | SHA-256 |
| --- | --- |
| wonderland.exe | `a2cb2455af78cd14a968c835e7cd9ce23515f4ca267da4cbe75554e242ecf139` |
| wonderland-desktop.exe | `66f15a0103e350f8a5e3dd6d0a634eebf5959f1db9e35c91d9f2f0aa89873944` |
| wonderland-cli.exe | `e5621f9ac0fd3a537b33b07a253f447726e91124ab5b60ad7690e3e56ff17582` |

安装后的后端用独立空数据目录、临时 localhost 端口和 offline API provider 启动，health 正常；安装版 CLI 连接该服务成功，版本为 0.11.0。应用目录按 `native_controls.managed` 确认四项：Codex、Claude、Kimi Python、DeepSeek。安装版桌面启动后观察 3 秒仍存活；这是启动冒烟，不替代全面界面操作测试。所有测试进程结束后清理，没有启动官方模型任务，也没有用新版打开用户正式数据。

首次安装冒烟脚本误把“应用目录总数”当作“受管应用数”，断言未通过；按服务合同改为检查 managed 字段后通过，未因此改动产品代码。后续最终打包仅纳入完成的交接/验证文档，二进制保持以上身份。

## 验证边界

本轮没有新增真实模型调用。Kimi 官方订阅团队的 12 项真实验收见 [0.9.0 记录](VALIDATION-0.9.0.md)，Claude 官方 CLI 本地合成 turn 与 DeepSeek 无密钥握手见 [0.10.0 记录](VALIDATION-0.10.0.md)。没有真实双厂商协作、订阅额度与精确计费校验，未证明任何节省成本比例。

Automatic、USD 硬预算、原生 resume/fork、更多受管厂商及插件/定时/远程/PR/网站面板仍未完成。Windows 安装器未代码签名；未制作 Linux/macOS 安装包。

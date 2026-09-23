# Windows 0.11.1 安装包工作报告

日期：2026-09-23

## 完成的工作

1. 将 Rust 根包、锁文件根包和 Inno Setup 安装器版本统一为 `0.11.1`，使发行二进制、安装器和本地 npm tarball 使用同一版本号。
2. 使用 `packaging/windows/build-windows-package.ps1` 完成 release 构建、CLIProxyAPI 7.3.7 校验下载、Inno Setup 7.1 打包和 npm 打包。
3. 生成 `dist/Wonderland-Setup-0.11.1-x64.exe`、`dist/Wonderland-0.11.1-windows-x64.zip` 和 `dist/rust-ai-wonderland-cli-0.11.1.tgz`，并在每次构建结束后写入 `dist/SHA256SUMS.txt`。该清单是当前产物的权威校验值，避免内嵌文档改动影响安装器自身哈希。
4. 检查便携 ZIP 的清单：后端、桌面、Rust CLI、CLIProxyAPI、启动器、文档、许可证和运行时 DLL 均在包内；没有账号、OAuth token 或测试数据。
5. 在 `dist` 下的独立临时目录静默安装安装器，确认三个主程序均为 `0.11.1`。已运行安装后 `wonderland-cli --help` 与 `wonderland-cli schedule --help`；以 `AGENT_PROVIDER=offline`、独立数据目录启动安装后的后端，`GET /health` 返回 `status=ok`、`provider=offline`。静默卸载后再次确认临时安装目录不存在。
6. 已创建 [GitHub v0.11.1 Release](https://github.com/Alice-Marx/Rust_for_AI-agent/releases/tag/v0.11.1)，目标提交为 `c8882d3`，并上传安装器、便携 ZIP、npm tarball 和 `SHA256SUMS.txt`。远端 release 元数据确认四个资产均为 uploaded；下载远端清单后与本地清单逐字一致。

## 文件说明

| 文件或目录 | 说明 |
| --- | --- |
| `Cargo.toml` / `Cargo.lock` | Rust 根包版本为 `0.11.1`。 |
| `packaging/windows/wonderland.iss` | Inno Setup 安装器的后备版本和文件清单。 |
| `packaging/windows/build-windows-package.ps1` | 构建 release 二进制、校验 CLIProxyAPI、生成 ZIP、安装器和 npm tarball 的脚本。 |
| `dist/Wonderland-Setup-0.11.1-x64.exe` | 用户用于本机真实账号验证的 Windows 安装器。 |
| `dist/Wonderland-0.11.1-windows-x64.zip` | 无安装权限时可解压使用的便携版本。 |
| `dist/rust-ai-wonderland-cli-0.11.1.tgz` | 本地 npm 安装或审计用的 CLI 包。 |
| `dist/SHA256SUMS.txt` | 三个交付物的 SHA-256 校验值。 |

## 已执行测试

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --locked --all-targets --features ui-snapshots` | 466 个库测试通过、8 个外部条件测试忽略、6 个 Rust CLI 测试通过、20 个桌面测试通过、7 个协议集成测试通过 |
| `npm test --prefix packaging/npm/wonderland-cli` | 16/16 通过 |
| Windows 打包脚本 | 通过；release 二进制、Inno Setup 7.1、ZIP 和 npm tarball 均生成 |
| ZIP 文件清单与 SHA-256 | 通过；清单符合预期，校验值见 `dist/SHA256SUMS.txt` |
| 静默安装、安装后 CLI 和离线后端健康检查、静默卸载 | 通过 |
| GitHub `v0.11.1` Release 与远端清单下载 | 通过；四个资产 uploaded，远端 `SHA256SUMS.txt` 与本地一致 |

安装器当前没有 Authenticode 签名，Windows 可能显示发布者提示；二进制版本和 SHA-256 已如上核对。

## 剩余任务与推进方式

1. **真实账号登录验收**：由你在 `Wonderland-Setup-0.11.1-x64.exe` 安装后启动桌面端，分别执行需要验证的 OAuth/API key 登录、账号列表、模型列表、一次 Work/Chat/Teams 请求和退出登录。记录渠道、模型、时间、预期和实际结果，再将结果写入下一份总项目报告。
2. **真实桌面窗口检查**：在你的显示缩放下查看 Teams 路由卡、长 blocker、滚动、空数据和长中文换行；若发现截断或重叠，附截图和复现步骤后修复。
3. **后续版本发布**：本次 `v0.11.1` 已推送 main、创建 GitHub Release 并完成远端清单核验。npm `0.11.1` 已存在，后续改版先递增版本、重跑完整回归和离线安装验收，再上传对应 tag、资产与清单。

## 2026-09-23 晚间修复：安装目录可选与启动器可视化

用户实测 0.11.1 安装包反馈两个问题，均已修复并重新构建：

1. **安装目录不能自选**：安装器显式 `DisableDirPage=no`，并新增「选择数据目录」向导页（默认 `%LOCALAPPDATA%\WonderlandData`；安装目录选择非系统盘时默认自动跟随到该盘根下 `WonderlandData`，仍可修改）；所选数据目录写入 `data-location.txt`，启动器按 安装器配置 > `AGENT_DATA_DIR` > 默认值 的顺序读取。程序与数据可完全避开 C 盘；卸载不删数据目录。
2. **启动后只剩空白终端**：原启动器全程无输出、失败即闪退，用户无法辨别状态。重写为全程中文状态输出（启动中/数据目录/服务就绪/打开桌面），任一失败保留窗口 15 秒并给出原因与日志路径；桌面经 `cmd start` 分离启动。另修复脚本 BOM 双写导致首行解析报错的问题。

已验证：ISS 编译通过；静默安装到 `F:\WLFinal`（数据目录自动 `F:\WonderlandData`）；启动器输出干净、后端 `/health` ok、桌面进程拉起；测试安装已卸载清理。新安装器 SHA-256 已更新至 `dist/SHA256SUMS.txt`（`c890435cb6c117c3d7beec6640c09338098dacb1930a2f09a4a761d17b072229`）。
# Windows 0.11.1 安装包工作报告

日期：2026-09-23

## 完成的工作

1. 将 Rust 根包、锁文件根包和 Inno Setup 安装器版本统一为 `0.11.1`，使发行二进制、安装器和本地 npm tarball 使用同一版本号。
2. 使用 `packaging/windows/build-windows-package.ps1` 完成 release 构建、CLIProxyAPI 7.3.7 校验下载、Inno Setup 7.1 打包和 npm 打包。
3. 生成 `dist/Wonderland-Setup-0.11.1-x64.exe`、`dist/Wonderland-0.11.1-windows-x64.zip` 和 `dist/rust-ai-wonderland-cli-0.11.1.tgz`，并在每次构建结束后写入 `dist/SHA256SUMS.txt`。该清单是当前产物的权威校验值，避免内嵌文档改动影响安装器自身哈希。
4. 检查便携 ZIP 的清单：后端、桌面、Rust CLI、CLIProxyAPI、启动器、文档、许可证和运行时 DLL 均在包内；没有账号、OAuth token 或测试数据。
5. 在 `dist` 下的独立临时目录静默安装安装器，确认三个主程序均为 `0.11.1`。已运行安装后 `wonderland-cli --help` 与 `wonderland-cli schedule --help`；以 `AGENT_PROVIDER=offline`、独立数据目录启动安装后的后端，`GET /health` 返回 `status=ok`、`provider=offline`。静默卸载后再次确认临时安装目录不存在。
6. 已创建 [GitHub v0.11.1 Release](https://github.com/Alice-Marx/Rust_for_AI-agent/releases/tag/v0.11.1)，目标提交为 `c8882d3`，并上传安装器、便携 ZIP、npm tarball 和 `SHA256SUMS.txt`。远端 release 元数据确认四个资产均为 uploaded；下载远端清单后与本地清单逐字一致。

## 文件说明

| 文件或目录 | 说明 |
| --- | --- |
| `Cargo.toml` / `Cargo.lock` | Rust 根包版本为 `0.11.1`。 |
| `packaging/windows/wonderland.iss` | Inno Setup 安装器的后备版本和文件清单。 |
| `packaging/windows/build-windows-package.ps1` | 构建 release 二进制、校验 CLIProxyAPI、生成 ZIP、安装器和 npm tarball 的脚本。 |
| `dist/Wonderland-Setup-0.11.1-x64.exe` | 用户用于本机真实账号验证的 Windows 安装器。 |
| `dist/Wonderland-0.11.1-windows-x64.zip` | 无安装权限时可解压使用的便携版本。 |
| `dist/rust-ai-wonderland-cli-0.11.1.tgz` | 本地 npm 安装或审计用的 CLI 包。 |
| `dist/SHA256SUMS.txt` | 三个交付物的 SHA-256 校验值。 |

## 已执行测试

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --locked --all-targets --features ui-snapshots` | 466 个库测试通过、8 个外部条件测试忽略、6 个 Rust CLI 测试通过、20 个桌面测试通过、7 个协议集成测试通过 |
| `npm test --prefix packaging/npm/wonderland-cli` | 16/16 通过 |
| Windows 打包脚本 | 通过；release 二进制、Inno Setup 7.1、ZIP 和 npm tarball 均生成 |
| ZIP 文件清单与 SHA-256 | 通过；清单符合预期，校验值见 `dist/SHA256SUMS.txt` |
| 静默安装、安装后 CLI 和离线后端健康检查、静默卸载 | 通过 |
| GitHub `v0.11.1` Release 与远端清单下载 | 通过；四个资产 uploaded，远端 `SHA256SUMS.txt` 与本地一致 |

安装器当前没有 Authenticode 签名，Windows 可能显示发布者提示；二进制版本和 SHA-256 已如上核对。

## 剩余任务与推进方式

1. **真实账号登录验收**：由你在 `Wonderland-Setup-0.11.1-x64.exe` 安装后启动桌面端，分别执行需要验证的 OAuth/API key 登录、账号列表、模型列表、一次 Work/Chat/Teams 请求和退出登录。记录渠道、模型、时间、预期和实际结果，再将结果写入下一份总项目报告。
2. **真实桌面窗口检查**：在你的显示缩放下查看 Teams 路由卡、长 blocker、滚动、空数据和长中文换行；若发现截断或重叠，附截图和复现步骤后修复。
3. **后续版本发布**：本次 `v0.11.1` 已推送 main、创建 GitHub Release 并完成远端清单核验。npm `0.11.1` 已存在，后续改版先递增版本、重跑完整回归和离线安装验收，再上传对应 tag、资产与清单。

## 2026-09-23 晚间修复：安装目录可选与启动器可视化

用户实测 0.11.1 安装包反馈两个问题，均已修复并重新构建：

1. **安装目录不能自选**：安装器显式 `DisableDirPage=no`，并新增「选择数据目录」向导页（默认 `%LOCALAPPDATA%\WonderlandData`；安装目录选择非系统盘时默认自动跟随到该盘根下 `WonderlandData`，仍可修改）；所选数据目录写入 `data-location.txt`，启动器按 安装器配置 > `AGENT_DATA_DIR` > 默认值 的顺序读取。程序与数据可完全避开 C 盘；卸载不删数据目录。
2. **启动后只剩空白终端**：原启动器全程无输出、失败即闪退，用户无法辨别状态。重写为全程中文状态输出（启动中/数据目录/服务就绪/打开桌面），任一失败保留窗口 15 秒并给出原因与日志路径；桌面经 `cmd start` 分离启动。另修复脚本 BOM 双写导致首行解析报错的问题。

已验证：ISS 编译通过；静默安装到 `F:\WLFinal`（数据目录自动 `F:\WonderlandData`）；启动器输出干净、后端 `/health` ok、桌面进程拉起；测试安装已卸载清理。新安装器 SHA-256 已更新至 `dist/SHA256SUMS.txt`（`c890435cb6c117c3d7beec6640c09338098dacb1930a2f09a4a761d17b072229`）。

## 2026-09-23 晚间第二次更新：GitHub Release 资产替换

便携 ZIP 以修复后的启动器重新打包；安装器、便携 ZIP 与 `SHA256SUMS.txt` 已替换上传到 [v0.11.1 Release](https://github.com/Alice-Marx/Rust_for_AI-agent/releases/tag/v0.11.1)（npm tarball 未变，四个资产均为 uploaded），Release 说明追加本次修复条目；远端下载的清单与本地逐字一致。修复代码提交为 `518df54`，已推送 main。

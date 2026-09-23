# H09 ZCode 发行物与协议审计工作报告

日期：2026-09-23
范围：只读核对 ZCode 的可验证发行物和传输协议；没有登录账号、没有调用模型、没有把第三方同名包接入 Wonderland。

## 一、已完成的工作

1. 核对了仓库外固定源码归档 `F:/everyAI/all/upstream-study/ZCode`（提交 `872ad960de7ec172591f7e1952f7849229f94521`）。根包为 `zcode@3.14.0`、`apps/zcode-cli` 为 `zcode-cli@0.16.9`、`packages/zcode-server-cli` 为 `@zcode/server-cli`；三者均标记 `private: true`，不能把源码中的名称和版本当作已发布 npm 包。
2. 对 `npm view @zcode/cli@3.14.0 version dist.tarball dist.integrity repository --json` 及 `npm view zcode@3.14.0 version dist.tarball dist.integrity repository --json` 的核对均返回 `E404`。因此没有官方 npm tarball、integrity 或文件清单可与固定源码比对。
3. 官方 [ZCode Releases 页面](https://github.com/zai-org/ZCode/releases) 显示没有 release，`v3.14.0` tag release URL 返回 404。没有可验证的 GitHub release 资产、版本或摘要可写入适配器双闸。
4. 深读源码确认其并非 ACP 或 Codex app-server JSON 合同：`packages/server/src/entry-stdio.ts` 先通过 stdout 发送一行 JSON `zcode-hello`，等待 stdin 的一行 `hello-ack`，随后 `packages/server/src/stdio.ts` 使用 `@zcode/rpc` 的 `SocketProtocol`。`packages/rpc/src/protocol.ts` 定义 13 字节二进制帧头（type、id、ack、payload length）。源码中的 HTTP/WebSocket 入口同样使用这套自有 RPC。
5. 未新增 `src/native_executor` 代码、未写猜测性协议 profile、未使用搜索结果中的第三方同名 npm 包。发行物身份和可执行入口缺失时，Wonderland 保持不受管、失败关闭状态。

## 二、本次文件说明

| 文件 | 说明 |
| --- | --- |
| `docs/WORK-REPORT-2026-09-23-ZCODE-AUDIT.md` | 本次发行物、协议、测试证据及后续方法的完整记录。 |
| `docs/ANYTOOL-ADAPTER-ASSESSMENT-2026-09-23.md` | 更正 ZCode 为“未接入”，并记录实际的握手和二进制 RPC 事实。 |
| `docs/ROADMAP-2026-09-23.md`、`docs/DEVELOPER_HANDOFF.md`、`docs/REVIEW-AND-PLAN-2026-09-23.md` | 把“下载 tarball 后实现”改为“等官方可验证发行物后再继续”。 |
| `docs/PROJECT-TASK-REPORT-2026-09-23.md`、`docs/DOCUMENT-GUIDE-2026-09-23.md`、`docs/README.md`、`README.md`、`docs/DESKTOP_PRODUCT_PLAN.md` | 同步总进度、文档入口和产品能力边界。 |
| `src/desktop_bridge.rs` | 将 ZCode 应用卡和 CLI 安装提示改为“用户已独立核验的手动终端候选”，避免显示未证实的官方安装/受管入口。 |
| `F:/everyAI/all/upstream-study/PROVENANCE.md` | 保存仓库外只读研究副本的审计证据。 |

## 三、已执行的核对与后续测试

| 核对 | 结果 |
| --- | --- |
| 固定源码 manifest 和入口静态检查 | 通过；三个候选包均为 private，发现 stdio 的 JSON 初始握手和随后的二进制 RPC 分帧。 |
| npm 精确版本查询 | 两个候选包均为 `E404`，没有可复核的官方 tarball。 |
| 官方 GitHub release 检查 | 无 release，也无 `v3.14.0` release 页。 |
| Rust/npm/桌面回归 | 本次仅更正应用卡与 CLI 提示文案；后续提交前运行 `desktop_bridge` 相关测试和格式检查。真正接入时必须先增加离线协议 fixture，再纳入全量回归。 |

在有官方发行物后，仍需依次测试：下载物的来源、签名或 SHA-256、`--version` 和实际入口；与该固定协议 profile 的离线握手/分帧/取消/权限/usage fixture；最后由账号持有人执行真实登录、推理、取消和失败恢复测试。真实账号测试不能替代前两层发行物与离线合同验证。

## 四、未完成事项与完成思路

| 事项 | 当前阻塞 | 完成方式 |
| --- | --- | --- |
| ZCode 受管适配器 | 没有官方可验证安装包、npm 包或 GitHub release，不能锁定可执行命令、版本与摘要。 | 等待厂商发布官方 tarball、签名 release 资产或安装器；或由用户提供可验证的官方安装包及来源。先记录版本/哈希和启动命令，再按实际协议写独立 transport。 |
| 离线协议覆盖 | 源码可说明帧格式，但不能替代发行物的行为合同。 | 以经验证的程序捕获脱敏握手、会话、授权、取消和 usage 帧，写固定 fixture；字段或语义不符时新建版本 profile 并保持旧 profile 拒绝。 |
| 真实账号闭环 | 需账号持有人完成登录。 | 适配器与离线测试完成后，由你使用安装包登录并执行最小推理、取消和失败路径；只归档脱敏事件与二进制摘要。 |

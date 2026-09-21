# 0.9.0 验证记录

日期：2026-09-21，Windows x64。该版本验证协作执行闭环，不是模型效率相等的对照实验，也不宣称已完成所有厂商的自动适配。

## Kimi 真实协作

使用已获授权的 Kimi 订阅账号，官方 Python `kimi-cli==1.50.0`，精确模型 `kimi-for-coding`，通过原生 Wire 协议。没有修改官方工具源码。

在主仓库外建立独立 JavaScript Git 项目，两个未实现模块为发票金额计算和 CSV 单元格转义。任务调用前提交已有测试和 README。最初 10 项测试有 8 项失败；复核后补充负数量和累加溢出两项测试，再在新的独立基线中执行最终验证。

最终任务：`team_7aa249db2ff1465891f2de61f2323e86`。

1. Kimi 原生规划器只读检查项目，输出两个并行实现节点，以及依赖二者的只读评审节点。主机严格解析 JSON 并校验 DAG。
2. 两个实现节点使用各自 detached Git worktree，分别只有 `src/money.js`、`src/csv.js` 的声明写范围。具体写入请求经 Wonderland 的一次性权限接口确认。
3. 主机检查补丁范围、保护既有测试，记录提交和补丁散列，再串行集成。
4. Kimi 只读评审返回 `accepted:true`、空阻断发现列表和解释。评审纠正了规划文本中测试数量写错的问题。
5. 主机以显式 argv 执行 `node --test`，**12 passed / 0 failed**。在服务外再次执行相同测试也是 12 项通过。
6. 只有验收成功且 revision 未改变后，父任务及实际被集成的子工作流才记录 succeeded。

已测试提交：`b5139d836965408c5a7b60a6f2ab212903b353fd`。原始基线：`d29c8fd`。最终改动仅 `src/money.js` 与 `src/csv.js`。原始 checkout 保持干净、HEAD 未变。原始和最终 `test/invoice.test.js` Git blob 均为 `b0338a91c09384e0ebb51c49240623eb13e0c095`；Windows 工作树换行方式可不同，不能用原始字节散列代替 Git 内容身份。

结果目录：`E:\harness\toolchains\wonderland-team-data-090\team-workspaces\team_7aa249db2ff1465891f2de61f2323e86\integration`。完整本地记录保存在专用测试数据目录，不把账号配置或会话凭据打进发布包。

此前一次探索运行在多轮授权后达到总时限，正确记录 failed 并停止只读评审子进程，没有声称验收完成。这轮实测还修正了 Windows verbatim Git 路径、全局换行设置造成的错误 dirty 判断，以及评审报告必须显式通过的问题。

## 在线证据

LiveBench 连续两次在线刷新与重载测试通过。REST 限流时从同一官方 GitHub 仓库在线读取 git-upload-pack advertised refs，严格校验帧、服务类型及唯一 `refs/heads/main`，再按不可变 SHA 读取源文件，不回退到旧缓存。

- 公布版本：2026-06-25，58 个模型。
- new-livebench：`bc7d9c1787d85ce521304fc0472fd8c25b117f75`。
- LiveBench：`1263ee472f4b9ac3833c0d2f6ad50dd3747fd1df`。
- 2026-09-21 官方价格在线解析取得 41 条可匹配 API 报价。OpenAI/Kimi 解析通过；Anthropic/DeepSeek 原始来源已保存，但未验证的精确 ID 或时段条件保持 blocked。
- Automatic 请求会刷新两类证据后受阻，不会把订阅计为零成本，也不会调用候选模型来掩盖缺失的计费/benchmark 映射。

## 本地回归与界面

363 项库测试、3 项 Rust CLI 测试、17 项桌面测试、7 项协议集成测试通过；5 项需要外部条件的测试默认忽略，其中 LiveBench 在线测试另行显式运行并通过。npm 12 项测试通过。库回归覆盖状态 CAS、预算精度/并发预留、重复领取、DAG 循环、固定与逐节点绑定、恢复中断、写范围、测试保护、补丁篡改、冲突恢复、并行补丁集成、Windows 换行与路径、验收退出码与取消。

桌面 Teams 详情和新建页已渲染检查，包括 940×620 窗口。样例截图明确标记为展示数据。模型真实任务证据来自服务记录，不来自 UI fixture。

## 发布构建与安装

`cargo build --locked --release --bins` 通过。Windows x64 安装器执行返回 0；安装后的后端、桌面、Rust CLI 与打包二进制 SHA-256 一致，CLI 报告 0.9.0，已安装桌面进程成功启动。便携 ZIP 的 31 个条目经过检查，未包含账号配置或测试数据库；npm tarball 只有许可证、README、两个脚本与 package.json。

停止测试后端后使用 release 二进制重启同一独立数据目录，真实 Kimi Team 仍为 succeeded，12 项验收输出与 `verification_artifact` 中的已测试 Git 提交均可通过 Rust CLI 读回。发布包的安装器、ZIP 和 npm tarball 均生成 SHA-256 校验文件。

## 未验证部分

本次真实协作是同一 Kimi 模型的多任务执行；跨厂商 Assigned 已有绑定/协议回归，但没有双厂商账号同时成功的实测记录。Codex 之前只验证握手和任务协议，模型调用曾失败；Claude、DeepSeek 等当前仍主要通过直接终端入口。预算硬限制、自动质量/价格权重和“与各官方客户端同等效率”均未被证明。

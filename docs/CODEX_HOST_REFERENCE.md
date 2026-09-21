# CodexHost 技术参考记录

审阅日期：2026-09-21。上游：[BytePioneer-AI/codex-host](https://github.com/BytePioneer-AI/codex-host)，提交 `fb36f2dfee08cb8db68d3eee362f02a7ca61d3ae`，MIT。只读参考目录位于主项目之外；未改写上游源码，未复制其桌面资源或分发其构建产物。

CodexHost 的主要目标是在 Codex Desktop 的交互环境内接入多个 Harness。其 Rust workspace 提供 launcher、platform、shim、updater；任务适配与大部分共享协议位于 TypeScript packages。Wonderland 自有 Rust/egui 桌面，因此适合借鉴协议边界与验证方法，不能直接套用它的桌面注入和 renderer 绑定。

## 已核对的接口

| 上游位置 | 设计与 Wonderland 的采用方向 |
|---|---|
| `packages/harness-adapter/src/text-session.ts` | 明确区分 Host 与 Native session/turn/checkpoint，接口分别表达 create/resume/fork/rollback、权限、问题、取消和有效模型。Wonderland 继续保存官方 session ID，并将后续恢复/分叉设计为显式能力，不能以重跑冒充恢复。 |
| `packages/harness-adapter/src/plugin.ts` | 每连接工厂接收环境、启动入口与远程主机上下文，避免全局注册副作用。适用于后续 Rust trait 或进程协议插件边界；当前不在 UI 中显示未实现的插件安装能力。 |
| `packages/harness-adapter/src/usage.ts` | token、实际 USD、native credits、五小时/七日订阅额度分别表达，未知字段不填零。Wonderland 价格来源与原生用量继续分离；未验证的订阅成本阻止自动成本排序。 |
| `packages/adapters/claude-code/manifest.json` | manifestVersion、adapterApiVersion、入口和安装来源分开版本化。后续 Claude 受管适配应拥有独立协议版本和回归门槛。 |
| `packages/adapters/deepseek-harness/src/profiles/` | 对不同 DeepSeek Harness 版本用不同协议 profile 严格解码；Modern Web 的连接、身份和命令能力分别处理。不能把它统一当成无认证 WebSocket 或普通聊天 API。 |
| `packages/adapters/*/test/` 与 `tools/gate-*` | 本地协议回归和真实工具验证区分。Wonderland 保持模拟测试、官方握手、真实模型结果与独立验收四类证据；不会把协议测试等同于同等效率。 |

## 本轮实际关联

0.9.0 的 Teams 沿用同样的职责分离：调度层处理任务 DAG 和生命周期，官方适配器负责原生调用，界面只展示与提交明确控制。用户指定的工具和模型绑定贯穿规划、节点、评审；权限请求保留官方标识，子任务的执行结束不自动代表整体验收通过。

新增只读评审节点返回结构化 `accepted/findings/summary`；有阻断问题的评审不能靠非空文本被当作完成。最终仍需要模型之外的主机检查命令通过。该设计是 Wonderland 自行实现，不是已导入 CodexHost 的全部适配器。

0.10.0 已新增自有 Rust Claude Code 双向 stream-json transport 和 DeepSeek Harness ACP transport。分别固定经过验证的官方发行版，检查生效模型与推理档位，并区分原生用量、上下文占用和实际计费证据。应用目录使用结构化能力报告，新增安装版本/入口/指纹诊断；安装检查不推测认证或模型可用性。

Claude 已完成官方 CLI 离线协议轨迹和本地合成响应测试，DeepSeek 已完成真实官方 CLI 无密钥握手，均未完成真实付费推理。两个适配器的恢复、分叉保持不可用。模型列表、原生额度、远程主机和插件清单仍需由适配器逐项实现及报告，不能从 CodexHost 的能力列表推断 Wonderland 已支持。

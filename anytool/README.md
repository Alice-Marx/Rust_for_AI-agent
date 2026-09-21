# 上游 AI 编码工具

本目录收录各家的上游仓库。`Claude/` 以普通文件保存从 Gitee 下载的项目快照，其余目录通过 Git 子模块记录上游来源和固定提交。这些工具独立维护，不参与 Wonderland 的编译。

```text
anytool/
├── kimi/
│   ├── kimi-cli/
│   └── kimi-code/
├── ChatGPT/
│   └── codex/
├── Claude/
├── DeepSeek/
│   └── deepseek-harness/
├── minimax/
│   ├── minimax-code/
│   └── cli/
├── mimo/
│   └── MiMo-Code/
└── zai/
    ├── ZCode/
    └── zcode-plugins/
```

| 目录 | 上游仓库 |
| --- | --- |
| `kimi/kimi-cli/` | [MoonshotAI/kimi-cli](https://github.com/MoonshotAI/kimi-cli) |
| `kimi/kimi-code/` | [MoonshotAI/kimi-code](https://github.com/MoonshotAI/kimi-code) |
| `ChatGPT/codex/` | [openai/codex](https://github.com/openai/codex) |
| `Claude/` | [Alice-Marx/claudecode（Gitee，源码快照）](https://gitee.com/Alice-Marx/claudecode) |
| `DeepSeek/deepseek-harness/` | [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) |
| `minimax/minimax-code/` | [MiniMax-AI/minimax-code](https://github.com/MiniMax-AI/minimax-code) |
| `minimax/cli/` | [MiniMax-AI/cli](https://github.com/MiniMax-AI/cli) |
| `mimo/MiMo-Code/` | [XiaomiMiMo/MiMo-Code](https://github.com/XiaomiMiMo/MiMo-Code) |
| `zai/ZCode/` | [zai-org/ZCode](https://github.com/zai-org/ZCode) |
| `zai/zcode-plugins/` | [zai-org/zcode-plugins](https://github.com/zai-org/zcode-plugins) |

## Claude 项目快照

`Claude/` 完整保留 Gitee 项目在提交 [`a1d261f9f79e4075a94406bbefa06d7b9f61a0e2`](https://gitee.com/Alice-Marx/claudecode/commit/a1d261f9f79e4075a94406bbefa06d7b9f61a0e2) 中的 6,702 个已跟踪文件，包括源码、发布物、归档包、文档和许可证。原项目的目录结构与文件内容保持不变，入口见 [Claude/README.md](Claude/README.md)。

该目录直接随主仓库提交，普通克隆和 GitHub ZIP 下载均包含其内容，无需初始化子模块；后续上游更新需要重新导入。

## 获取源码

首次克隆时，一并下载子模块：

```sh
git clone --recurse-submodules --shallow-submodules https://github.com/Alice-Marx/Rust_for_AI-agent.git
```

已有本仓库时，在仓库根目录执行：

```sh
git pull
git submodule update --init --recursive --depth 1
```

以上命令检出主仓库记录的版本。GitHub 的 ZIP 下载包含 `Claude/` 快照，但不包含其余子模块源码；需要全部工具源码时请使用 Git 获取。各工具的安装、运行方式和许可证以对应上游仓库中的说明为准。

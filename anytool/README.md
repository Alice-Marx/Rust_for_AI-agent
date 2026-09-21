# 上游 AI 编码工具

本目录通过 Git 子模块收录各家的上游仓库。主仓库记录每个子模块的来源和固定提交；这些工具独立维护，不参与 Wonderland 的编译。

```text
anytool/
├── kimi/
│   ├── kimi-cli/
│   └── kimi-code/
├── ChatGPT/
│   └── codex/
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
| `minimax/minimax-code/` | [MiniMax-AI/minimax-code](https://github.com/MiniMax-AI/minimax-code) |
| `minimax/cli/` | [MiniMax-AI/cli](https://github.com/MiniMax-AI/cli) |
| `mimo/MiMo-Code/` | [XiaomiMiMo/MiMo-Code](https://github.com/XiaomiMiMo/MiMo-Code) |
| `zai/ZCode/` | [zai-org/ZCode](https://github.com/zai-org/ZCode) |
| `zai/zcode-plugins/` | [zai-org/zcode-plugins](https://github.com/zai-org/zcode-plugins) |

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

以上命令检出主仓库记录的版本。GitHub 的 ZIP 下载不包含子模块源码，请使用 Git 获取。各工具的安装、运行方式和许可证以对应上游仓库中的说明为准。

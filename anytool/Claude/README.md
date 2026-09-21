# Claude Code 源码合集

本仓库收集整理 Claude Code（`@anthropic-ai/claude-code`，版本 2.1.88）相关的源码与发布物，按来源和形态分目录存放。

## 目录结构

| 目录 | 内容 | 来源 |
| --- | --- | --- |
| [`official-repo/`](official-repo/) | 官方 `anthropics/claude-code` 仓库镜像：官方 README、CHANGELOG、插件（`plugins/`）、示例（`examples/`）、内置 mods 源码（`mods/`）、issue 管理脚本（`scripts/`）及 `.github/`、`.devcontainer/` 等配置 | GitHub 官方仓库 |
| [`npm-package/`](npm-package/) | 官方 npm 包 `@anthropic-ai/claude-code@2.1.88` 解压后的内容：`cli.js`、`cli.js.map`、`vendor/`、`sdk-tools.d.ts` 等 | npm  registry |
| [`deobfuscated-src/`](deobfuscated-src/) | 由 `cli.js` + source map 还原的反混淆 TypeScript 源码（约 1900 个文件） | 从 npm 包还原 |
| [`claude-code-best/`](claude-code-best/) | CCB（Claude Code Best V5）完整复原的工程化项目，含文档、构建脚本与扩展特性，可独立浏览，见其自身 README | 第三方复原工程 |
| [`archives/`](archives/) | 原始归档文件：npm 包 `.tgz`、拆分存放的 `.tar.part01/part02`（还原方法见 [`archives/ARCHIVE_PARTS.md`](archives/ARCHIVE_PARTS.md)）、反混淆源码压缩包 `src.zip` | 发布物备份 |
| [`tools/`](tools/) | 辅助脚本，如 Windows 下通过 Docker/Podman 启动 DevContainer 运行 Claude Code 的 PowerShell 脚本 | 自建 |

## 说明

- 想快速了解 Claude Code 官方用法，从 [`official-repo/README.md`](official-repo/README.md) 开始。
- 想阅读实现源码，看 [`deobfuscated-src/`](deobfuscated-src/)（反混淆）或 [`claude-code-best/`](claude-code-best/)（工程化复原）。
- `archives/` 中的 `.tar` 因 Gitee 不支持 Git LFS 且单文件过大而拆分为两个 part，合并方式与 SHA-256 校验值见 [`archives/ARCHIVE_PARTS.md`](archives/ARCHIVE_PARTS.md)。
- 本仓库仅用于学习与研究，Claude Code 的相关权利归 Anthropic 所有。

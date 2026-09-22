# 工作报告：上游源码获取回退

日期：2026-09-22  
工作目录：`F:\harness\Codex\Rust_for_AI-agent-0.11.0\Rust_for_AI-agent`

## 本轮已完成

1. 排查直接 Git 获取失败原因：本机没有 Git `http.proxy`/`https.proxy` 配置，也没有 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 或 `NO_PROXY` 环境变量。
2. 曾成功读取远端分支：`main` 指向 `24dbf549450c78681db231d9b5e6870f67202b41`，与 0.11.0 交接源码 pin 一致。
3. 复现并确认普通克隆与浅克隆均在连接 `github.com:443` 时超时，随后三次 `git ls-remote` 也在约 21 秒超时；问题不在仓库 URL、权限或 Git clone 参数。
4. 验证 `codeload.github.com` 可访问，并新增 `tools/Get-UpstreamSource.ps1`。它会先做普通浅克隆；若 Git Smart HTTP 失败，则以完整提交 SHA 从官方 codeload 下载源码，写入包含 SHA-256 的 `.wonderland-source.json`，且明确标识结果不是 Git 历史克隆。
5. 更新开发者交接指南和文档索引，提供不依赖代理地址或用户凭据的获取命令。

## 验证结果

- `https://github.com` 的 HTTPS 连接超时；Git Smart HTTP 目前不可用。
- `https://codeload.github.com/Alice-Marx/Rust_for_AI-agent/zip/24dbf549450c78681db231d9b5e6870f67202b41` 可建立下载并持续传输。
- GitHub REST 端点可达，但共享出口的匿名 REST 配额在本轮已耗尽；没有使用或要求任何用户令牌。
- `tools/Get-UpstreamSource.ps1` 已通过 PowerShell 语法解析检查。

## 本轮尚未完成

1. 主机到 `github.com:443` 的直接 Git 网络故障仍未恢复；只有网络管理员或可用代理/网络路径才能从根本修复该连接。
2. 归档回退不含 Git 提交历史。网络恢复后仍应真实 clone，随后把本地提交移植到真实历史。
3. 为避免重复下载 194 MB 的交接归档，本轮没有完成第二份完整归档的 SHA-256 字节级比对；已保留并继续使用交接目录中标注为同一提交的完整源码归档。
4. 本轮未推送 GitHub，也未创建 PR；当前开发继续在已恢复的本地源码仓库中进行。

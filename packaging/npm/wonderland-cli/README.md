# rust-ai-wonderland-cli

Wonderland 的 npm 终端客户端。该包不要求安装 Rust，使用 Node.js 调用已经启动的 Rust Agent HTTP 服务。

## 安装

发布到 npm 后：

```powershell
npm install --global rust-ai-wonderland-cli
wonderland-cli --help
```

在本地从源码安装：

```powershell
Set-Location F:\codex\Rust_for_AI-agent\packaging\npm\wonderland-cli
npm install --global .
```

## 使用

Rust Agent 后端默认地址是 `http://127.0.0.1:8080`：

```powershell
wonderland-cli
wonderland-cli run "分析我的 Rust 项目"
wonderland-cli health
wonderland-cli models
wonderland-cli verify --model gpt-5.4
wonderland-cli login codex --wait
```

可以通过 `AGENT_SERVER_URL`、`AGENT_USER_ID` 和 `AGENT_SESSION_ID` 配置服务、用户和会话。

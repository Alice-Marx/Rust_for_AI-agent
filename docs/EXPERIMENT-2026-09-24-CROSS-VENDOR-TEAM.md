# 图文实验记录：跨厂商分工实验（Kimi × DeepSeek）

日期：2026-09-24。本文是**真实执行**的实验记录：Kimi（订阅账号登录）与 DeepSeek（API Key）两个厂商的官方工具在同一个团队里各实现一个模块，独立验证收尾。全部命令与输出都来自本机实际运行，可直接照做。

实验结论先行：**成功**。Kimi 实现 `src/strings.js`、DeepSeek 实现 `src/math.js`，各自通过节点验收后，合并工作区跑独立验收命令，4 个测试全部通过，团队状态 `succeeded`，全程约 52 秒（不含权限等待）。

---

## 0. 实验前的事实核对

先确认两个模型通道与官方 CLI 的真实状态（这是排错的起点）：

```powershell
# 服务健康：当前连接与内置 agent 通道
curl http://127.0.0.1:8080/health
# → {"model":"deepseek-flash","provider":"deepseek","cliproxyapi_configured":true,...}

# CLIProxyAPI 订阅通道（Kimi 登录在这里）：10 个 kimi 模型可用
curl -X POST http://127.0.0.1:8080/v1/providers/cliproxyapi/verify -H "Content-Type: application/json" -d "{}"
# → {"reachable":true,"model_count":10,"models":[{"id":"kimi-k2",...,"id":"kimi-k3"},...]}

# 但受管执行器需要的官方 CLI 一个都没装：
curl -X POST http://127.0.0.1:8080/api/v1/apps/kimi-code/probe   # → installed: false
curl -X POST http://127.0.0.1:8080/api/v1/apps/deepseek/probe    # → installed: false
```

结论：两条**模型通道**都通（订阅/API Key），但 Teams 受管分工需要**官方 CLI 程序**，当时未安装——这正是本次开发「一键安装」功能的原因。

## 1. 一键安装两个官方工具

新功能（本次开发）：`POST /api/v1/apps/{id}/install` 使用**服务端注册表里固定的安装命令**，客户端只传应用名；装完自动跑版本探测确认。

```powershell
# 安装 Kimi Code（命令固定为 npm install -g @moonshot-ai/kimi-code@2.0.2，版本=适配器核对版）
curl -X POST http://127.0.0.1:18081/api/v1/apps/kimi-code/install
# → {"status":"running","command":"npm install -g @moonshot-ai/kimi-code@2.0.2",...}

# 轮询结果（约 40 秒后）
curl http://127.0.0.1:18081/api/v1/apps/kimi-code/install/status
# → {"status":"succeeded","exit_code":0,
#    "post_install_probe":{"installed":true,"version":"2.0.2",
#      "path":"...\\npm\\node_modules\\@moonshot-ai\\kimi-code\\dist\\main.mjs"}}

# 安装 DeepSeek Harness（dsh@0.1.6-alpha.2）
curl -X POST http://127.0.0.1:18081/api/v1/apps/deepseek/install
# → 第一次因 npm 网络抖动失败（错误与 npm 原始输出保留在 output_tail，可重试）
# → 重试成功：post_install_probe.installed=true, version=0.1.6-alpha.2

# 不支持的会被明确拒绝（fail-closed，绝不猜）：
curl -X POST http://127.0.0.1:18081/api/v1/apps/grok/install
# → HTTP 409 {"error":"该应用不支持一键安装：Grok Build 1.0.38 由 xAI 官方渠道分发，..."}
```

桌面端「应用」页同时新增了「一键安装（命令明文显示）」按钮、安装中的旋转指示、失败原因与「安装输出（最近 12 行）」折叠区。

## 2. 准备实验仓库

建一个只有失败测试的干净 Git 仓库（团队要求干净仓库，每个节点在独立工作区执行）：

```text
wl-exp-repo/
├─ package.json          # {"type":"module"}
├─ test/math.test.js     # 断言 add(2,3)=5、multiply(2,3)=6 …… 引用尚不存在的 src/math.js
└─ test/strings.test.js  # 断言 capitalize('hello')='Hello'、reverse('abc')='cba' ……
```

基线验证：`node --test test/math.test.js test/strings.test.js` → 4 个测试全部失败（找不到模块）——这就是两个执行器要完成的工作。

## 3. 创建分工团队（Assigned 策略）

```json
{
  "title": "分工实验：Kimi × DeepSeek 各实现一个模块",
  "prompt": "本仓库已有失败的测试。把它变绿：实现 src/math.js 和 src/strings.js，使用 ES Module。两个模块分别由不同执行器实现，不要修改 test/ 下的测试文件。",
  "cwd": "F:/everyAI/all/wl-exp-repo",
  "strategy": "assigned",
  "planner": {"app_id": "kimi-code", "model": "kimi-for-coding"},
  "nodes": [
    {"id": "math",
     "objective": "创建 src/math.js：导出 add(a,b) 与 multiply(a,b)……",
     "write_paths": ["src/math.js"],
     "executor": {"app_id": "deepseek", "model": "deepseek-chat"}},
    {"id": "strings",
     "objective": "创建 src/strings.js：导出 capitalize(s) 与 reverse(s)……",
     "write_paths": ["src/strings.js"],
     "executor": {"app_id": "kimi-code", "model": "kimi-for-coding"}}
  ],
  "checks": [{"program": "node", "args": ["--test", "test/math.test.js", "test/strings.test.js"], "timeout_secs": 120}],
  "max_parallel": 2
}
```

要点：`strategy: "assigned"` 允许**每个节点指定不同厂商的执行器**——这就是跨厂商分工；`write_paths` 声明节点的写权限范围；`checks` 是团队级独立验收命令（在合并后的 integration 工作区跑，不属于任何执行器）。

```powershell
curl -X POST http://127.0.0.1:18081/api/v1/teams -H "Content-Type: application/json" -d @team.json
# → {"id":"team_f518...","status":"planned"}
curl -X POST http://127.0.0.1:18081/api/v1/teams/team_f518.../start
# → {"status":"running"}
```

## 4. 执行过程（真实事件流）

两个节点并行启动，各自在独立 Git 工作区里执行：

```text
12:41:27  status: planned → running；strings、math 两个 child 同时挂起
12:41:57  DeepSeek(math) 请求权限：
             "node --test spawns test child processes over piped stdio, which the
              confined sandbox blocks with EPERM; danger-full-access is needed to
              run the required acceptance command."
          （它要跑验收测试，受限沙箱阻止子进程，申请一次性完全访问——附带了明确理由）
          → 操作者批准：POST /api/v1/workflows/{child}/approve {"request_id":"...","approve":true}
12:42:10  strings(Kimi)：running → verifying → succeeded
12:42:15  math(DeepSeek)：running → verifying → succeeded
12:42:19  团队进入 verifying：integration 工作区合并两份变更，跑独立验收
          verification: checks[0].exit_code=0, success=true
          status: verifying → succeeded
```

## 5. 结果核对

两份产物的**风格差异肉眼可辨**——确实是两家不同的模型各自写的：

```javascript
// src/math.js —— DeepSeek 写的（完整 JSDoc 注释风格）
/**
 * Return the sum of two numbers.
 * @param {number} a
 * @param {number} b
 * @returns {number}
 */
export function add(a, b) {
  return a + b;
}
export function multiply(a, b) {
  return a * b;
}

// src/strings.js —— Kimi 写的（简洁实现风格）
export function capitalize(s) {
  if (s.length === 0) return s;
  return s[0].toUpperCase() + s.slice(1).toLowerCase();
}
export function reverse(s) {
  return [...s].reverse().join('');
}
```

在 integration 工作区复跑验收：

```text
ℹ tests 4
ℹ pass 4
ℹ fail 0
```

## 6. 实验过程中修掉的真实缺陷（8 轮迭代）

实验不是一次跑通的。以下问题全部来自真实运行，已修复并带回归测试（471 项库测试全绿）：

| # | 症状 | 根因 | 修复 |
| --- | --- | --- | --- |
| 1 | dsh 安装成功但探测不到 | 用户的 npm 全局目录搬到了 D 盘且不在 PATH | 服务启动时执行 `npm config get prefix` 并注入本进程 PATH |
| 2 | `npm is required... program not found` | Windows 上 `Command::new("npm")` 解析到 npm.cmd，std Command 无法直接执行 | 抽共享 `desktop_bridge::npm_root_global()`（PowerShell shim），kimi/minimax/mimo 三处统一 |
| 3 | `nested DeepSeek package overrides are unsupported` | 适配器只认"平坦兄弟包"布局，npm 11 实际把 dsh-* 嵌套装进 `dsh/node_modules/@deepseek-ai/`（三个内容哈希与常量完全一致） | 适配器接受嵌套/平坦两种布局，混合布局仍拒绝 |
| 4 | `Kimi Code ACP stream closed before response` | sessionId 强制 UUID 解析，而 kimi 返回 `session_<uuid>` 前缀格式，校验失败杀进程掩盖了真实错误 | 校验放宽为非空/无控制字符（ACP 规范本就 opaque） |
| 5 | 同上（第二层） | Windows `canonicalize` 产生 `\\?\` verbatim 路径，node 把它当相对模块路径，CLI 秒退且 stderr 被丢弃 | 共享 `node_path()` 把 verbatim 归一化回普通盘符路径（deepseek 原有逻辑提升为公共），kimi/minimax 接入 |
| 6 | `provider/model drift` | kimi 模型值是 `kimi-code/` 前缀全名（`kimi-code/kimi-for-coding`），请求用裸名 | `model_value` 确定性加前缀；初始校验只要求"请求模型在目录中"，设置后仍强校验相等 |
| 7 | `unsupported ... session update` | 真实服务器会发 `session_info_update`（会话标题元数据） | 加入 kimi 方言透传列表 |
| 8 | `invalid tool start/result status` | kimi 的工具调用首状态是 `pending`，且中间态 `in_progress` 被当非法终态报错 | kimi 接受 pending 起始；终态判定改为三态（中间态返回 false） |

另修一个实验自身的问题：node v24 的 `node --test test/`（目录形式）在本环境解析失败，验收命令改为显式文件列表。

## 7. 你自己重做这个实验的最短路径

1. 安装 0.11.2+ 版本（含一键安装），桌面「应用」页对未安装的 Kimi Code / DeepSeek 点「一键安装」，完成后点「检测安装」确认版本；
2. DeepSeek 需要在「工作区设置」里连好 API Key（受管执行器会自动复用同一凭据，无需另设环境变量）；Kimi Code 需要在终端跑一次 `kimi login`（设备码流程）；
3. 桌面「Teams → 创建协作计划」：策略选「逐节点指定模型」，规划器填 kimi-code + `kimi-for-coding`，两个节点分别指定 kimi-code 与 deepseek 执行器（模型 `kimi-for-coding` / `deepseek-chat`）；
4. 启动后留意**权限请求**：执行器要跑命令/写文件时会挂起等你批准（每次一批准一次）；
5. 结束后看团队详情的路由与验收事件；`数据目录\team-workspaces\<team id>\integration` 里是合并后的最终产物。

> 边界说明：费用由各厂商按你的账号套餐计费；Wonderland 记录的 usage 是观察值，不当作厂商账单。

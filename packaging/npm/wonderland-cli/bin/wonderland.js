#!/usr/bin/env node

const fs = require("node:fs");
const path = require("node:path");
const readline = require("node:readline/promises");
const { stdin, stdout } = require("node:process");
const { consumeStream } = require("./stream");

const defaultServer = process.env.AGENT_SERVER_URL || "http://127.0.0.1:8080";
const defaultUser = process.env.AGENT_USER_ID || "local-user";

function usage() {
  console.log(`Wonderland npm CLI

Usage:
  wonderland-cli [options] chat [prompt]
  wonderland-cli [options] run <input>
  wonderland-cli [options] health
  wonderland-cli [options] models
  wonderland-cli [options] verify [--model <model>]
  wonderland-cli [options] login <provider> [--wait]
  wonderland-cli [options] sessions
  wonderland-cli [options] session <id>
  wonderland-cli [options] tools
  wonderland-cli [options] mcp
  wonderland-cli [options] commands
  wonderland-cli [options] search <text>
  wonderland-cli [options] accounts
  wonderland-cli [options] profile
  wonderland-cli [options] mcp-login <name> [--wait]
  wonderland-cli [options] mcp-reload

Options:
  --server <url>       Rust Agent URL (default: ${defaultServer})
  --user-id <id>       User ID (default: ${defaultUser})
  --session-id <id>    Session ID
  --continue           复用该用户最近一次会话
  --cwd <dir>          工作目录（权限规则与自定义命令都从这里加载）
  --model <model>      覆盖本次请求的模型
  --mode <mode>        Permission mode: default / plan / acceptEdits / bypassPermissions / dontAsk
  --reasoning <level>  模型支持的推理档位（profile 查看能力）
  --no-stream         等待完整响应
  -V, --version       显示版本
  -h, --help           Show that help

聊天内置命令：/help /exit /health /models /skills /sessions /session /cost /tools /mcp /commands
自定义命令：项目 .claude/commands/**/*.md 或 .wonderland/commands/，用法 /<name> <args>
`);
}

function parseArgs(argv) {
  const options = {
    server: defaultServer,
    userId: defaultUser,
    sessionId: process.env.AGENT_SESSION_ID || null,
    command: null,
    args: [],
    wait: false,
    model: process.env.AGENT_MODEL || null,
    mode: process.env.AGENT_PERMISSION_MODE || null,
    cwd: process.env.AGENT_CWD || process.cwd(),
    resume: false,
    stream: true,
    reasoning: process.env.AGENT_REASONING_EFFORT || null,
  };
  const valueFlags = {
    "--server": "server",
    "--user-id": "userId",
    "--session-id": "sessionId",
    "--cwd": "cwd",
    "--model": "model",
    "--mode": "mode",
    "--reasoning": "reasoning",
  };
  let index = 0;
  while (index < argv.length) {
    const value = argv[index];
    if (valueFlags[value]) {
      if (!argv[index + 1] || argv[index + 1].startsWith("--")) throw new Error(`${value} requires a value`);
      options[valueFlags[value]] = argv[++index];
    }
    else if (value === "--no-stream") options.stream = false;
    else if (value === "--version" || value === "-V") options.version = true;
    else if (value === "--wait") options.wait = true;
    else if (value === "--continue") options.resume = true;
    else if (value === "-h" || value === "--help") options.help = true;
    else if (!options.command) options.command = value;
    else options.args.push(value);
    index += 1;
  }
  return options;
}

function cryptoRandomId() {
  return `npm-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function endpoint(server, pathname) {
  return `${server.replace(/\/$/, "")}${pathname}`;
}

async function request(options, pathname, init = {}) {
  let response;
  try {
    response = await fetch(endpoint(options.server, pathname), {
      ...init,
      headers: { "content-type": "application/json", ...(process.env.WONDERLAND_SERVER_TOKEN ? {Authorization: `Bearer ${process.env.WONDERLAND_SERVER_TOKEN}`} : {}), ...(init.headers || {}) },
    });
  } catch (error) {
    throw new Error(`无法连接 Rust Agent 服务：${error.message}`);
  }
  const text = await response.text();
  let body;
  try {
    body = text ? JSON.parse(text) : {};
  } catch {
    body = { raw: text };
  }
  if (!response.ok) {
    throw new Error(body.error || `HTTP ${response.status}`);
  }
  return body;
}

// 自定义斜杠命令：与 Rust 端 src/commands.rs 的语义保持一致。
function parseFrontmatter(text) {
  const meta = {};
  for (const line of text.split(/\r?\n/)) {
    const at = line.indexOf(":");
    if (at <= 0 || line.trim().startsWith("#")) continue;
    const key = line.slice(0, at).trim();
    const value = line
      .slice(at + 1)
      .trim()
      .replace(/^["']|["']$/g, "");
    if (key) meta[key] = value;
  }
  return meta;
}

function firstLine(body) {
  const line = body
    .split(/\r?\n/)
    .map((entry) => entry.trim())
    .find((entry) => entry && !entry.startsWith("#"));
  if (!line) return "（无描述）";
  return line.length > 80 ? `${line.slice(0, 77)}...` : line;
}

function parseCommand(name, raw) {
  let content = raw.replace(/^\uFEFF/, "");
  let meta = {};
  const match = content.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/);
  if (match) {
    meta = parseFrontmatter(match[1]);
    content = content.slice(match[0].length);
  }
  const body = content.trim();
  if (!name || !body) return null;
  return {
    name,
    description: meta.description || firstLine(body),
    hint: meta["argument-hint"] || null,
    body,
  };
}

function collectCommands(root, directory, found) {
  let entries;
  try {
    entries = fs.readdirSync(directory, { withFileTypes: true });
  } catch {
    return;
  }
  entries.sort((left, right) => left.name.localeCompare(right.name));
  for (const entry of entries) {
    const full = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      collectCommands(root, full, found);
      continue;
    }
    if (!entry.name.endsWith(".md")) continue;
    const name = path.relative(root, full).replace(/\.md$/, "").split(path.sep).join(":");
    try {
      const command = parseCommand(name, fs.readFileSync(full, "utf8"));
      if (command) found.set(name, command);
    } catch {
      // 读不了的文件跳过即可，不影响其它命令。
    }
  }
}

function loadCommands(cwd) {
  const found = new Map();
  for (const directory of [
    path.join(cwd, ".wonderland", "commands"),
    path.join(cwd, ".claude", "commands"),
  ]) {
    collectCommands(directory, directory, found);
  }
  return [...found.values()].sort((left, right) => left.name.localeCompare(right.name));
}

function expandCommand(command, args) {
  const trimmed = (args || "").trim();
  const positional = trimmed ? trimmed.split(/\s+/) : [];
  let rendered = command.body;
  for (let index = 9; index >= 1; index -= 1) {
    rendered = rendered.split(`$${index}`).join(positional[index - 1] || "");
  }
  rendered = rendered.split("$ARGUMENTS").join(trimmed);
  if (!/\$ARGUMENTS|\$[1-9]/.test(command.body) && trimmed) {
    rendered += `\n\n${trimmed}`;
  }
  return rendered.trim();
}

function renderCommandList(commands) {
  if (!commands.length) {
    return "（当前项目没有自定义命令；在 .claude/commands/*.md 中添加）";
  }
  return commands
    .map(
      (command) =>
        `/${command.name}${command.hint ? ` ${command.hint}` : ""}  ${command.description}`,
    )
    .join("\n");
}

async function resolveSession(options) {
  if (options.sessionId) return options.sessionId;
  if (options.resume) {
    const sessions = await request(
      options,
      `/v1/sessions?user_id=${encodeURIComponent(options.userId)}`,
    );
    if (sessions.length) {
      const latest = sessions[0];
      console.log(
        `继续会话 ${latest.id}（messages=${latest.message_count} updated=${latest.updated_at}）`,
      );
      return latest.id;
    }
    console.log("没有可继续的会话，新建一个。");
  }
  return cryptoRandomId();
}

async function runAgent(options, input) {
  const body = {
    session_id: options.sessionId,
    user_id: options.userId,
    input,
  };
  if (options.mode) body.mode = options.mode;
  if (options.cwd) body.cwd = options.cwd;
  if (options.model) body.model = options.model;
  if (options.reasoning) body.reasoning_effort = options.reasoning;
  if (options.stream) {
    const response = await fetch(endpoint(options.server, "/v1/agent/stream"), {
      method: "POST", headers: { "Content-Type": "application/json", "Accept": "text/event-stream", "x-wonderland-interactive":stdin.isTTY?"true":"false", ...(process.env.WONDERLAND_SERVER_TOKEN?{Authorization:`Bearer ${process.env.WONDERLAND_SERVER_TOKEN}`}:{}) }, body: JSON.stringify(body),
    });
    let printed = false;
    const result = await consumeStream(response, async event => {
      if (event.type === "permission_request") {
        process.stderr.write(`\n${event.tool}\n${JSON.stringify(event.input,null,2)}\n`);
        const rl=options.promptInterface || readline.createInterface({input:stdin,output:process.stderr});
        let answer="";
        try { answer=await rl.question("允许这一次？[y/N] "); } finally { if (!options.promptInterface) rl.close(); }
        await request(options,`/v1/permissions/${encodeURIComponent(event.id)}`,{method:"POST",body:JSON.stringify({allow:/^y(es)?$/i.test(answer.trim())})});
      }
      if (event.type === "text_delta") { stdout.write(event.text); printed = true; }
      if (event.type === "tool_call") process.stderr.write(`\n[tool: ${event.name}]\n`);
    });
    if (printed) { stdout.write("\n"); result._streamed = true; }
    return result;
  }
  return request(options, "/v1/agent/run", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

function renderTodos(todos) {
  if (!Array.isArray(todos) || !todos.length) return "";
  const lines = todos.map((todo, index) => {
    const active = todo.active_form || todo.content;
    return `${index + 1}. [${todo.status}] ${active}`;
  });
  return `\ntodos:\n${lines.map((line) => `  ${line}`).join("\n")}\n`;
}

function printUsage(usage = {}) {
  const cached = usage.cache_read_tokens || 0;
  const total = (usage.input_tokens || 0) + cached + (usage.cache_creation_tokens || 0);
  const rate = total === 0 ? 0 : Math.round((cached * 100) / total);
  console.log(
    `tokens: ${usage.input_tokens || 0} in (${cached} cached read / ${
      usage.cache_creation_tokens || 0
    } cache write) / ${usage.output_tokens || 0} out | cache 命中 ${rate}%`,
  );
}

function printResponse(response) {
  if (!response._streamed) console.log(response.output);
  console.log(renderTodos(response.todos));
  console.log(
    `[session=${response.session_id} execution=${response.execution_id} score=${Number(
      response.evaluation?.total_score || 0,
    ).toFixed(1)} turns=${response.turns ?? 0} tool_calls=${response.tool_calls ?? 0} tokens=${
      response.usage?.input_tokens ?? 0
    }in/${response.usage?.output_tokens ?? 0}out]`,
  );
}

async function printTools(options) {
  for (const tool of await request(options, "/v1/tools")) {
    console.log(
      `${String(tool.name).padEnd(26)} ${String(tool.source || "builtin").padEnd(9)} ${
        tool.read_only ? "read-only" : "mutating"
      }`,
    );
  }
}

async function printMcpServers(options) {
  const servers = await request(options, "/v1/mcp/servers");
  if (!servers.length) {
    console.log("（没有已连接的 MCP 服务器；在 .mcp.json 或 settings.json 的 mcpServers 中配置）");
    return;
  }
  for (const server of servers) {
    console.log(`${server.name}:`);
    for (const tool of server.tools || []) console.log(`  - ${tool}`);
  }
}

async function printSessions(options) {
  const sessions = await request(
    options,
    `/v1/sessions?user_id=${encodeURIComponent(options.userId)}`,
  );
  if (!sessions.length) {
    console.log("（暂无会话）");
    return;
  }
  for (const session of sessions) {
    console.log(
      `${session.id}  messages=${session.message_count}  updated=${session.updated_at}`,
    );
  }
}

async function chat(options, prompt, commands) {
  if (prompt) {
    printResponse(await runAgent(options, prompt));
    return;
  }
  const rl = readline.createInterface({ input: stdin, output: stdout });
  options.promptInterface=rl;
  console.log(`Wonderland npm CLI | session=${options.sessionId} | cwd=${options.cwd}`);
  console.log("输入消息开始对话，输入 /help 查看命令，输入 /exit 退出。");
  if (commands.length) {
    console.log(`可用自定义命令：${commands.map((command) => `/${command.name}`).join(" ")}`);
  }
  console.log();
  try {
    while (true) {
      const input = (await rl.question("you> ")).trim();
      if (!input) continue;
      if (input === "/exit" || input === "/quit") break;
      if (input === "/help") {
        console.log(
          "/exit 退出；/health 检查服务；/models 查看模型；/skills 列出技能；/sessions 列出会话；" +
            "/session 或 /cost 查看当前会话用量；/tools 列出全部工具；/mcp 列出 MCP 服务器；/commands 列出自定义命令。\n",
        );
        continue;
      }
      if (input === "/health") {
        console.log(JSON.stringify(await request(options, "/health"), null, 2));
        continue;
      }
      if (input === "/models") {
        const models = await request(options, "/v1/models");
        console.log(models.map((model) => `- ${model.id}`).join("\n"));
        continue;
      }
      if (input === "/skills") {
        console.log(JSON.stringify(await request(options, "/v1/skills"), null, 2));
        continue;
      }
      if (input === "/sessions") {
        await printSessions(options);
        continue;
      }
      if (input === "/session" || input === "/cost") {
        const session = await request(
          options,
          `/v1/sessions/${encodeURIComponent(options.sessionId)}`,
        );
        console.log(`session: ${session.id}`);
        printUsage(session.usage);
        for (const [index, todo] of (session.todos || []).entries()) {
          console.log(`${index + 1}. [${todo.status}] ${todo.active_form || todo.content}`);
        }
        continue;
      }
      if (input === "/tools") {
        await printTools(options);
        continue;
      }
      if (input === "/mcp") {
        await printMcpServers(options);
        continue;
      }
      if (input === "/commands") {
        console.log(renderCommandList(commands));
        continue;
      }
      if (input.startsWith("/")) {
        const [name, ...rest] = input.split(/\s+/);
        const command = commands.find((entry) => `/${entry.name}` === name);
        if (!command) {
          console.log(`未知命令 ${name}；输入 /help 查看内置命令，或 /commands 查看项目自定义命令。`);
          continue;
        }
        console.log(`[命令 /${command.name}] ${command.description}`);
        printResponse(await runAgent(options, expandCommand(command, rest.join(" "))));
        continue;
      }
      printResponse(await runAgent(options, input));
    }
  } finally {
    delete options.promptInterface;
    rl.close();
  }
}

async function login(options, provider) {
  const started = await request(options, "/v1/providers/cliproxyapi/login", {
    method: "POST",
    body: JSON.stringify({ provider }),
  });
  console.log(`provider: ${provider}`);
  console.log(`state: ${started.state || "<missing>"}`);
  console.log(`OAuth URL:\n${started.url || "<missing>"}`);
  if (!options.wait || !started.state) return;
  while (true) {
    await new Promise((resolve) => setTimeout(resolve, 2000));
    const status = await request(
      options,
      `/v1/providers/cliproxyapi/login/status?state=${encodeURIComponent(started.state)}`,
    );
    console.log(`status=${status.status} authenticated=${status.authenticated}`);
    if (status.status !== "wait") break;
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.version) { console.log(require("../package.json").version); return; }
  if (options.help) {
    usage();
    return;
  }
  const commands = loadCommands(options.cwd);
  options.sessionId = await resolveSession(options);

  if (!options.command) {
    await chat(options, null, commands);
    return;
  }
  switch (options.command) {
    case "chat":
      await chat(options, options.args.join(" "), commands);
      break;
    case "run":
      if (!options.args.length) throw new Error("run 需要输入任务文本");
      printResponse(await runAgent(options, options.args.join(" ")));
      break;
    case "health":
      console.log(JSON.stringify(await request(options, "/health"), null, 2));
      break;
    case "models":
      console.log(
        JSON.stringify(await request(options, "/v1/models"), null, 2),
      );
      break;
    case "verify":
      console.log(
        JSON.stringify(
          await request(options, "/v1/providers/cliproxyapi/verify", {
            method: "POST",
            body: JSON.stringify({ model: options.model }),
          }),
          null,
          2,
        ),
      );
      break;
    case "login":
      if (!options.args[0]) throw new Error("login 需要 provider，例如 codex");
      await login(options, options.args[0]);
      break;
    case "sessions":
      await printSessions(options);
      break;
    case "tools":
      await printTools(options);
      break;
    case "mcp":
      await printMcpServers(options);
      break;
    case "accounts":
      console.log(JSON.stringify(await request(options, "/v1/providers/cliproxyapi/accounts"), null, 2));
      break;
    case "search":
      if (!options.args.length) throw new Error("search 需要关键词");
      console.log(JSON.stringify(await request(options, `/v1/sessions/search?q=${encodeURIComponent(options.args.join(" "))}&user_id=${encodeURIComponent(options.userId)}`), null, 2));
      break;
    case "profile":
      console.log(JSON.stringify(await request(options, `/v1/models/profile?model=${encodeURIComponent(options.model || "")}`), null, 2));
      break;
    case "mcp-reload":
      console.log(JSON.stringify(await request(options, "/v1/mcp/reload", { method: "POST", body: JSON.stringify({cwd:options.cwd}) }), null, 2));
      break;
    case "mcp-login": {
      if (!options.args[0]) throw new Error("mcp-login 需要服务器名");
      const started = await request(options, `/v1/mcp/${encodeURIComponent(options.args[0])}/login`, {method:"POST",body:JSON.stringify({cwd:options.cwd})});
      console.log(started.url);
      if (options.wait) {
        for (let attempt = 0; attempt < 150; attempt++) {
          await new Promise(resolve => setTimeout(resolve,2000));
          const status = await request(options, `/v1/mcp/login/status?state=${encodeURIComponent(started.state)}`);
          if (status.status === "error") throw new Error(status.error);
          if (status.status === "ok") { console.log(await request(options,"/v1/mcp/reload",{method:"POST",body:JSON.stringify({cwd:options.cwd})})); break; }
        }
      }
      break;
    }
    case "commands":
      console.log(renderCommandList(commands));
      break;
    case "session": {
      if (!options.args[0]) throw new Error("session 需要会话 ID");
      const session = await request(options, `/v1/sessions/${encodeURIComponent(options.args[0])}`);
      console.log(`session: ${session.id}`);
      printUsage(session.usage);
      if (Array.isArray(session.todos) && session.todos.length) {
        console.log("todos:");
        session.todos.forEach((todo, index) => {
          console.log(`  ${index + 1}. [${todo.status}] ${todo.active_form || todo.content}`);
        });
      }
      break;
    }
    default:
      throw new Error(`未知命令：${options.command}`);
  }
}

main().catch((error) => {
  console.error(`错误：${error.message}`);
  process.exitCode = 1;
});

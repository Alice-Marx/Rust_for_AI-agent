#!/usr/bin/env node

const readline = require("node:readline/promises");
const { stdin, stdout } = require("node:process");

const defaultServer = process.env.AGENT_SERVER_URL || "http://127.0.0.1:8080";
const defaultUser = process.env.AGENT_USER_ID || "local-user";

function usage() {
  console.log(`Rust AI Agent npm CLI

Usage:
  agent-cli [options] chat [prompt]
  agent-cli [options] run <input>
  agent-cli [options] health
  agent-cli [options] models
  agent-cli [options] verify [--model <model>]
  agent-cli [options] login <provider> [--wait]
  agent-cli [options] sessions
  agent-cli [options] session <id>

Options:
  --server <url>       Rust Agent URL (default: ${defaultServer})
  --user-id <id>       User ID (default: ${defaultUser})
  --session-id <id>    Session ID
  --mode <mode>        Permission mode: default / plan / acceptEdits / bypassPermissions / dontAsk
  -h, --help           Show that help
`);
}

function parseArgs(argv) {
  const options = {
    server: defaultServer,
    userId: defaultUser,
    sessionId: process.env.AGENT_SESSION_ID || cryptoRandomId(),
    command: null,
    args: [],
    wait: false,
    model: null,
    mode: process.env.AGENT_PERMISSION_MODE || null,
  };
  let index = 0;
  while (index < argv.length) {
    const value = argv[index];
    if (value === "--server") options.server = argv[++index];
    else if (value === "--user-id") options.userId = argv[++index];
    else if (value === "--session-id") options.sessionId = argv[++index];
    else if (value === "--model") options.model = argv[++index];
    else if (value === "--wait") options.wait = true;
    else if (value === "--mode") options.mode = argv[++index];
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

function endpoint(server, path) {
  return `${server.replace(/\/$/, "")}${path}`;
}

async function request(options, path, init = {}) {
  let response;
  try {
    response = await fetch(endpoint(options.server, path), {
      ...init,
      headers: { "content-type": "application/json", ...(init.headers || {}) },
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

async function runAgent(options, input) {
  const body = {
    session_id: options.sessionId,
    user_id: options.userId,
    input,
  };
  if (options.mode) body.mode = options.mode;
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

function printResponse(response) {
  console.log(response.output);
  console.log(renderTodos(response.todos));
  console.log(`[session=${response.session_id} execution=${response.execution_id} score=${Number(response.evaluation?.total_score || 0).toFixed(1)} turns=${response.turns ?? 0} tool_calls=${response.tool_calls ?? 0} tokens=${response.usage?.input_tokens ?? 0}in/${response.usage?.output_tokens ?? 0}out]`);
}

async function chat(options, prompt) {
  if (prompt) {
    printResponse(await runAgent(options, prompt));
    return;
  }
  const rl = readline.createInterface({ input: stdin, output: stdout });
  console.log(`Rust AI Agent npm CLI | session=${options.sessionId}`);
  console.log("输入消息开始对话，输入 /help 查看命令，输入 /exit 退出。\n");
  try {
    while (true) {
      const input = (await rl.question("you> ")).trim();
      if (!input) continue;
      if (input === "/exit" || input === "/quit") break;
      if (input === "/help") {
        console.log("/exit 退出；/health 检查服务；/models 查看模型；/sessions 列出会话；其他文本发送给 Agent。\n");
        continue;
      }
      if (input === "/health") {
        console.log(JSON.stringify(await request(options, "/health"), null, 2));
        continue;
      }
      if (input === "/models") {
        const models = await request(options, "/v1/providers/cliproxyapi/models");
        console.log(models.map((model) => `- ${model.id}`).join("\n"));
        continue;
      }
      if (input === "/sessions") {
        const sessions = await request(options, "/v1/sessions");
        for (const session of sessions) {
          console.log(`${session.id}  messages=${session.message_count}  updated=${session.updated_at}`);
        }
        continue;
      }
      printResponse(await runAgent(options, input));
    }
  } finally {
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
    const status = await request(options, `/v1/providers/cliproxyapi/login/status?state=${encodeURIComponent(started.state)}`);
    console.log(`status=${status.status} authenticated=${status.authenticated}`);
    if (status.status !== "wait") break;
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    usage();
    return;
  }
  if (!options.command) {
    await chat(options);
    return;
  }
  switch (options.command) {
    case "chat":
      await chat(options, options.args.join(" "));
      break;
    case "run":
      if (!options.args.length) throw new Error("run 需要输入任务文本");
      printResponse(await runAgent(options, options.args.join(" ")));
      break;
    case "health":
      console.log(JSON.stringify(await request(options, "/health"), null, 2));
      break;
    case "models":
      console.log(JSON.stringify(await request(options, "/v1/providers/cliproxyapi/models"), null, 2));
      break;
    case "verify":
      console.log(JSON.stringify(await request(options, "/v1/providers/cliproxyapi/verify", {
        method: "POST",
        body: JSON.stringify({ model: options.model }),
      }), null, 2));
      break;
    case "login":
      if (!options.args[0]) throw new Error("login 需要 provider，例如 codex");
      await login(options, options.args[0]);
      break;
    case "sessions": {
      const sessions = await request(options, "/v1/sessions");
      if (!sessions.length) console.log("（暂无会话）");
      for (const session of sessions) {
        console.log(`${session.id}  messages=${session.message_count}  updated=${session.updated_at}`);
      }
      break;
    }
    case "session": {
      if (!options.args[0]) throw new Error("session 需要会话 ID");
      const session = await request(options, `/v1/sessions/${encodeURIComponent(options.args[0])}`);
      console.log(`session: ${session.id}`);
      console.log(`tokens: ${session.usage?.input_tokens ?? 0} in / ${session.usage?.output_tokens ?? 0} out`);
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

const { test } = require("node:test");
const assert = require("node:assert/strict");
const http = require("node:http");
const { spawn } = require("node:child_process");
const path = require("node:path");

async function invoke(args, response = { id: "task-1", status: "draft" }, status = 200) {
  const requests = [];
  const server = http.createServer(async (req, res) => {
    let raw = "";
    for await (const chunk of req) raw += chunk;
    requests.push({ path: req.url, method: req.method, body: raw ? JSON.parse(raw) : undefined });
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(response));
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  try {
    const child = spawn(process.execPath, [path.join(__dirname, "../bin/wonderland.js"), "--server", `http://127.0.0.1:${server.address().port}`, ...args], { env: { ...process.env, AGENT_SESSION_ID: "test-session" } });
    let output = "", error = "";
    child.stdout.on("data", data => output += data);
    child.stderr.on("data", data => error += data);
    const code = await new Promise((resolve, reject) => { child.once("error", reject); child.once("exit", resolve); });
    return { requests, code, output, error };
  } finally { await new Promise(resolve => server.close(resolve)); }
}

test("managed task creation preserves explicit tool/model and does not auto-start", async () => {
  const result = await invoke(["--model", "kimi-k2.5", "work", "create", "kimi-cli", "Fix", "a", "test"]);
  assert.equal(result.code, 0);
  assert.equal(result.requests.length, 1);
  assert.equal(result.requests[0].path, "/api/v1/workflows");
  assert.equal(result.requests[0].body.app_id, "kimi-cli");
  assert.equal(result.requests[0].body.model, "kimi-k2.5");
  assert.equal(result.requests[0].body.prompt, "Fix a test");
});
test("approval requires an explicit decision and sends scoped request identity", async () => {
  const result = await invoke(["work", "approve", "task-1", "rpc-2", "deny"]);
  assert.equal(result.code, 0);
  assert.deepEqual(result.requests[0].body, { request_id: "rpc-2", approve: false });
  const invalid = await invoke(["work", "approve", "task-1", "rpc-2", "yes"]);
  assert.equal(invalid.code, 1);
  assert.equal(invalid.requests.length, 0);
});
test("work persists a reasoning choice at creation and rejects a start override", async () => {
  const result = await invoke(["--model", "claude-sonnet-4-6", "--reasoning", "high", "work", "create", "claude", "Review"]);
  assert.equal(result.code, 0);
  assert.equal(result.requests.length, 1);
  assert.equal(result.requests[0].body.reasoning_effort, "high");
  assert.equal(result.requests[0].body.model, "claude-sonnet-4-6");
  const start = await invoke(["--reasoning", "low", "work", "start", "task-1"]);
  assert.equal(start.code, 1);
  assert.equal(start.requests.length, 0);
  assert.match(start.error, /reasoning_effort/);
});
test("accept sends human evidence and server conflicts fail the command", async () => {
  const result = await invoke(["work", "accept", "task-1", "Tests", "passed"], { error: "task is still running" }, 409);
  assert.equal(result.code, 1);
  assert.deepEqual(result.requests[0].body, { evidence: "Tests passed" });
});
test("intelligence refresh is a new online operation", async () => {
  const result = await invoke(["intelligence", "refresh"]);
  assert.equal(result.requests[0].method, "POST");
  assert.equal(result.requests[0].path, "/api/v1/intelligence/refresh");
});

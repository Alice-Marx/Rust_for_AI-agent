const { test } = require("node:test");
const assert = require("node:assert/strict");
const http = require("node:http");
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

async function invoke(args, response = { id: "team-1", status: "planned" }, status = 200) {
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
    const env = { ...process.env, AGENT_SESSION_ID: "", AGENT_MODEL: "" };
    const child = spawn(process.execPath, [path.join(__dirname, "../bin/wonderland.js"), "--server", `http://127.0.0.1:${server.address().port}`, ...args], { env });
    let output = "", error = "";
    child.stdout.on("data", data => output += data);
    child.stderr.on("data", data => error += data);
    const code = await new Promise((resolve, reject) => { child.once("error", reject); child.once("exit", resolve); });
    return { requests, code, output, error };
  } finally { await new Promise(resolve => server.close(resolve)); }
}

test("team create preserves the reviewed JSON without auto-start or chat session lookup", async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "wonderland-team-"));
  try {
    const plan = {
      title: "改进测试", prompt: "修复失败测试", cwd: "E:/projects/demo", strategy: "fixed",
      planner: { app_id: "kimi-cli", model: "kimi-for-coding" }, candidates: [],
      nodes: [{ id: "fix", objective: "修复边界条件", dependencies: [], write_paths: ["src"], acceptance: ["相关测试通过"] }],
      checks: [{ program: "cargo", args: ["test", "--locked"], timeout_secs: 300 }],
      max_parallel: 2, max_duration_secs: 1800, max_attempts: 2, budget_usd: null,
    };
    const file = path.join(directory, "team plan.json");
    fs.writeFileSync(file, JSON.stringify(plan));
    const result = await invoke(["--continue", "team", "create", "--file", file]);
    assert.equal(result.code, 0, result.error);
    assert.deepEqual(result.requests, [{ path: "/api/v1/teams", method: "POST", body: plan }]);
    assert.equal(JSON.parse(result.output).status, "planned");
  } finally { fs.rmSync(directory, { recursive: true, force: true }); }
});

test("team lifecycle routes keep IDs in a single encoded path segment", async () => {
  for (const [args, expectedPath, method] of [
    [["team", "list"], "/api/v1/teams", "GET"],
    [["team", "get", "team/a?x=1"], "/api/v1/teams/team%2Fa%3Fx%3D1", "GET"],
    [["team", "start", "team-1"], "/api/v1/teams/team-1/start", "POST"],
    [["team", "cancel", "team-1"], "/api/v1/teams/team-1/cancel", "POST"],
    [["team", "events", "team-1", "--after", "42"], "/api/v1/teams/team-1/events?after=42", "GET"],
  ]) {
    const result = await invoke(args);
    assert.equal(result.code, 0, result.error);
    assert.equal(result.requests.length, 1);
    assert.equal(result.requests[0].path, expectedPath);
    assert.equal(result.requests[0].method, method);
  }
});

test("invalid team arguments and malformed plans perform no requests", async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "wonderland-team-invalid-"));
  try {
    const malformed = path.join(directory, "bad.json"); fs.writeFileSync(malformed, "{broken");
    const array = path.join(directory, "array.json"); fs.writeFileSync(array, "[]");
    for (const args of [
      ["team", "start"], ["team", "destroy", "team-1"], ["team", "get", ".."],
      ["team", "events", "team-1", "--after", "-1"],
      ["team", "events", "team-1", "--after", "9007199254740992"],
      ["team", "create"], ["team", "create", "--file", malformed], ["team", "create", "--file", array],
      ["team", "start", "team-1", "--file", malformed], ["team", "list", "extra"],
    ]) {
      const result = await invoke(args);
      assert.equal(result.code, 1, args.join(" "));
      assert.equal(result.requests.length, 0, args.join(" "));
    }
  } finally { fs.rmSync(directory, { recursive: true, force: true }); }
});

test("team routing passthrough keeps IDs encoded and validates the policy file", async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "wonderland-routing-"));
  try {
    const policy = { required_categories: ["coding"], minimum_quality: 50, budget_usd: 1.5 };
    const file = path.join(directory, "policy.json");
    fs.writeFileSync(file, JSON.stringify(policy));
    const preview = await invoke(["team", "routing", "preview", "team/a?x=1", "--file", file], { status: "previewed" });
    assert.equal(preview.code, 0, preview.error);
    assert.deepEqual(preview.requests, [{ path: "/api/v1/teams/team%2Fa%3Fx%3D1/routing/preview", method: "POST", body: policy }]);
    const saved = await invoke(["team", "routing", "saved", "team-1"]);
    assert.equal(saved.code, 0, saved.error);
    assert.deepEqual(saved.requests, [{ path: "/api/v1/teams/team-1/routing/preview/saved", method: "POST", body: {} }]);
    const replay = await invoke(["team", "routing", "replay", "team-1"]);
    assert.equal(replay.code, 0, replay.error);
    assert.deepEqual(replay.requests, [{ path: "/api/v1/teams/team-1/routing/preview", method: "GET", body: undefined }]);
    const malformed = path.join(directory, "bad.json"); fs.writeFileSync(malformed, "{broken");
    for (const args of [
      ["team", "routing"], ["team", "routing", "guess", "team-1"], ["team", "routing", "replay", ".."],
      ["team", "routing", "preview", "team-1"], ["team", "routing", "preview", "team-1", "--file", malformed],
      ["team", "routing", "saved", "team-1", "--file", file], ["team", "routing", "saved", "team-1", "extra"],
    ]) {
      const result = await invoke(args);
      assert.equal(result.code, 1, args.join(" "));
      assert.equal(result.requests.length, 0, args.join(" "));
    }
  } finally { fs.rmSync(directory, { recursive: true, force: true }); }
});

test("pricing status reads cache and refresh is an explicit POST", async () => {
  const status = await invoke(["--continue", "pricing", "status"]);
  assert.equal(status.code, 0, status.error);
  assert.deepEqual(status.requests, [{ path: "/api/v1/pricing", method: "GET", body: undefined }]);
  const refresh = await invoke(["pricing", "refresh"]);
  assert.equal(refresh.code, 0, refresh.error);
  assert.deepEqual(refresh.requests, [{ path: "/api/v1/pricing/refresh", method: "POST", body: {} }]);
});

test("app diagnostics request a registered tool probe without a model call", async () => {
  const diagnostic = { app_id: "claude", installed: true, authentication: { status: "unknown" } };
  const result = await invoke(["apps", "--probe", "claude"], diagnostic);
  assert.equal(result.code, 0, result.error);
  assert.deepEqual(result.requests, [{ path: "/api/v1/apps/claude/probe", method: "POST", body: {} }]);
  assert.deepEqual(JSON.parse(result.output), diagnostic);
  const failure = await invoke(["apps", "--probe", "unknown"], { error: "application ID is not registered" }, 409);
  assert.equal(failure.code, 1);
  assert.match(failure.error, /application ID is not registered/);
});

test("pricing quote preserves exact identities and explicit subscription semantics", async () => {
  const blocked = { status: "blocked", reason: "subscription quota is not a zero-cost API price", quote: null };
  const result = await invoke(["pricing", "quote", "--app", "kimi-cli", "--model", "model+variant&reason=high", "--billing", "subscription"], blocked);
  assert.equal(result.code, 0, result.error);
  assert.equal(result.requests.length, 1);
  assert.equal(result.requests[0].method, "GET");
  const url = new URL(result.requests[0].path, "http://localhost");
  assert.equal(url.pathname, "/api/v1/pricing/quote");
  assert.deepEqual([...url.searchParams], [["app_id", "kimi-cli"], ["model", "model+variant&reason=high"], ["billing_channel", "subscription"]]);
  assert.deepEqual(JSON.parse(result.output), blocked);
});

test("pricing refuses incomplete quotes and surfaces backend failures", async () => {
  for (const args of [["pricing", "quote", "--app", "codex", "--model", "gpt-test"], ["pricing", "guess"], ["pricing", "status", "--billing", "api"]]) {
    const result = await invoke(args); assert.equal(result.code, 1); assert.equal(result.requests.length, 0);
  }
  const failure = await invoke(["team", "start", "team-1"], { error: "planner identity has not been verified" }, 409);
  assert.equal(failure.code, 1);
  assert.match(failure.error, /planner identity has not been verified/);
  const refresh = await invoke(["pricing", "refresh"], { error: "official pricing unavailable" }, 503);
  assert.equal(refresh.code, 1);
  assert.match(refresh.error, /official pricing unavailable/);
});

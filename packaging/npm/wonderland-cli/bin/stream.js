"use strict";
async function consumeStream(response, onEvent) {
  if (!response.ok) throw new Error(`HTTP ${response.status}: ${await response.text()}`);
  if (!response.body) throw new Error("Empty SSE response");
  const decoder = new TextDecoder();
  let pending = "", data = [], final;
  const line = async (value) => {
    if (value === "") {
      if (!data.length) return;
      const frame = JSON.parse(data.join("\n"));
      data = [];
      if (frame.frame === "error") throw new Error(frame.message);
      if (frame.frame === "event") await onEvent(frame.event);
      if (frame.frame === "response") final = frame.response;
    } else if (value.startsWith("data:")) data.push(value.slice(5).replace(/^ /, ""));
  };
  for await (const bytes of response.body) {
    pending += decoder.decode(bytes, { stream: true });
    let end;
    while ((end = pending.indexOf("\n")) >= 0) {
      await line(pending.slice(0, end).replace(/\r$/, ""));
      pending = pending.slice(end + 1);
      if (final) return final;
    }
  }
  throw new Error("连接中断：未收到完整响应，请检查会话后重试");
}
module.exports = { consumeStream };

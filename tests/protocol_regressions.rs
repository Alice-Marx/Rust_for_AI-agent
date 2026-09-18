use serde_json::json;
use wonderland::{provider::*, responses::*};

fn request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        system: "instructions".into(),
        messages: vec![],
        tools: vec![],
        max_tokens: 8192,
        temperature: Some(0.2),
        reasoning_effort: Some("off".into()),
        prompt_cache_key: Some("stable-session".into()),
    }
}

#[test]
fn sse_survives_every_utf8_chunk_boundary_and_preserves_spaces() {
    let wire = "data:  中文🦀\r\ndata: next\r\n\r\n:data: ignored\r\rdata: 完成\n\n";
    for split in 0..=wire.len() {
        let mut decoder = SseBuffer::new();
        let mut events = decoder.push_bytes(&wire.as_bytes()[..split]);
        events.extend(decoder.push_bytes(&wire.as_bytes()[split..]));
        assert_eq!(events, vec![" 中文🦀\nnext", "完成"], "split {split}");
    }
}

#[test]
fn thinking_and_cache_fields_follow_vendor_protocol() {
    for model in ["deepseek-reasoner", "kimi-k2.5"] {
        let mut req = request(model);
        req.messages.push(ChatMessage {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking("reasoning for the tool call", None),
                ContentBlock::tool_use("call-1", "Grep", json!({"pattern":"test"})),
            ],
        });
        let body = build_openai_request(&req);
        assert_eq!(
            body["messages"][1]["reasoning_content"],
            "reasoning for the tool call"
        );
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("prompt_cache_key").is_none());
    }
    let body = build_responses_request(&request("gpt-5.1"));
    assert_eq!(body["reasoning"]["effort"], "none");
    assert_eq!(body["max_output_tokens"], 8192);
    assert_eq!(body["prompt_cache_key"], "stable-session");
    let body = wonderland::anthropic::build_anthropic_request(&request("claude-sonnet-4-5"));
    assert!(body.get("thinking").is_none());
    let mut kimi = request("kimi-k2.8");
    kimi.reasoning_effort = Some("high".into());
    let body = build_openai_request(&kimi);
    assert_eq!(
        body["thinking"],
        json!({"type":"enabled","effort":"high","keep":"all"})
    );
    assert!(body.get("reasoning_effort").is_none());
    let mut claude = request("claude-opus-4-6");
    claude.reasoning_effort = Some("max".into());
    let body = wonderland::anthropic::build_anthropic_request(&claude);
    assert_eq!(body["thinking"], json!({"type":"adaptive"}));
    assert_eq!(body["output_config"]["effort"], "max");
}

#[test]
fn responses_interleaved_items_do_not_erase_later_tool_calls() {
    let mut acc = ResponsesStreamAccumulator::default();
    for index in 0..2 {
        acc.apply(&json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","call_id":format!("call-{index}"),"name":"Grep","arguments":""}}),None).unwrap();
    }
    for index in [1, 0] {
        acc.apply(&json!({"type":"response.function_call_arguments.delta","output_index":index,"delta":"{}"}),None).unwrap();
    }
    acc.apply(
        &json!({"type":"response.completed","response":{"status":"completed"}}),
        None,
    )
    .unwrap();
    let response = acc.finish(None).unwrap();
    assert_eq!(response.tool_uses().count(), 2);
    assert!(ResponsesStreamAccumulator::default().finish(None).is_err());
}

#[tokio::test]
async fn http_mcp_accepts_url_only_and_returns_before_sse_connection_closes() {
    use axum::{http::HeaderMap, response::IntoResponse, routing::post, Json};
    async fn handler(
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        if body["method"] == "initialize" {
            return ([("mcp-session-id","test-session")],Json(json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":"2025-03-26","capabilities":{"resources":{}}}}))).into_response();
        }
        assert_eq!(headers["mcp-session-id"], "test-session");
        assert_eq!(headers["mcp-protocol-version"], "2025-03-26");
        if body.get("id").is_none() {
            return axum::http::StatusCode::ACCEPTED.into_response();
        }
        let payload = format!(
            "data: {}\n\n",
            json!({"jsonrpc":"2.0","id":body["id"],"result":{"resources":[{"uri":"test:///one"}]}})
        );
        let stream =
            futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(payload) });
        let never = futures_util::stream::pending();
        use futures_util::StreamExt;
        axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(stream.chain(never)))
            .unwrap()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().route("/mcp", post(handler)))
            .await
            .unwrap()
    });
    let config = serde_json::from_value(json!({"type":"http","url":url})).unwrap();
    let client = wonderland::mcp::McpHttpClient::connect("http-test", &config)
        .await
        .unwrap();
    use wonderland::mcp::McpSession;
    assert!(client.list_tools().await.unwrap().is_empty());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.request("resources/list", json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["resources"][0]["uri"], "test:///one");
    server.abort();
}

#[tokio::test]
async fn legacy_sse_handshake_and_prompt_request() {
    use axum::{
        extract::State,
        response::{sse::Event, Sse},
        routing::{get, post},
        Json,
    };
    use std::{convert::Infallible, sync::Arc};
    type Bus = Arc<tokio::sync::broadcast::Sender<serde_json::Value>>;
    async fn events(
        State(bus): State<Bus>,
    ) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
        let rx = bus.subscribe();
        let endpoint = futures_util::stream::once(async {
            Ok(Event::default().event("endpoint").data("/messages"))
        });
        let messages = futures_util::stream::unfold(rx, |mut rx| async move {
            let message = rx.recv().await.ok()?;
            Some((
                Ok(Event::default().event("message").data(message.to_string())),
                rx,
            ))
        });
        use futures_util::StreamExt;
        Sse::new(endpoint.chain(messages))
    }
    async fn messages(
        State(bus): State<Bus>,
        Json(body): Json<serde_json::Value>,
    ) -> axum::http::StatusCode {
        if body.get("id").is_some() {
            let result = if body["method"] == "initialize" {
                json!({"protocolVersion":"2024-11-05","capabilities":{"prompts":{}}})
            } else {
                assert_eq!(body["method"], "prompts/list");
                json!({"prompts":[{"name":"review"}]})
            };
            bus.send(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
                .unwrap();
        }
        axum::http::StatusCode::ACCEPTED
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/sse", listener.local_addr().unwrap());
    let (bus, _) = tokio::sync::broadcast::channel(8);
    let app = axum::Router::new()
        .route("/sse", get(events))
        .route("/messages", post(messages))
        .with_state(Arc::new(bus));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = serde_json::from_value(json!({"type":"sse","url":url})).unwrap();
    let client = wonderland::mcp_sse::connect("legacy", &config)
        .await
        .unwrap();
    assert!(client.list_tools().await.unwrap().is_empty());
    assert_eq!(
        client.request("prompts/list", json!({})).await.unwrap()["prompts"][0]["name"],
        "review"
    );
    drop(client);
    server.abort();
}

#[test]
fn opaque_reasoning_is_not_sent_to_another_provider() {
    let payload = json!({"type":"reasoning","id":"rs_123","summary":[{"type":"summary_text","text":"checked"}],"encrypted_content":"encrypted"});
    let mut req = request("gpt-5.1");
    req.messages.push(ChatMessage::assistant_blocks(vec![
        ContentBlock::ProviderReasoning {
            provider: "openai-responses".into(),
            summary: "checked".into(),
            payload: payload.clone(),
        },
    ]));
    assert_eq!(build_responses_request(&req)["input"][0], payload);
    let anthropic = wonderland::anthropic::build_anthropic_request(&req);
    assert!(!anthropic.to_string().contains("encrypted"));
    req.messages = vec![ChatMessage::assistant_blocks(vec![ContentBlock::thinking(
        "claude",
        Some("anthropic-signature".into()),
    )])];
    assert!(!build_responses_request(&req)
        .to_string()
        .contains("anthropic-signature"));
}

#[cfg(windows)]
#[tokio::test]
async fn appcontainer_denies_host_files_and_network_but_can_work_locally() {
    use wonderland::{
        model::SandboxRequest,
        sandbox::{SandboxExecutor, SandboxPolicy},
    };
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("private.txt");
    std::fs::write(&secret, "not-for-sandbox").unwrap();
    let path = serde_json::to_string(&secret.display().to_string()).unwrap();
    let code=format!("import socket, pathlib\np={path}\ntry:\n open(p).read()\n print('READ_LEAK')\nexcept OSError:\n print('READ_DENIED')\ntry:\n open(p,'w').write('changed')\n print('WRITE_LEAK')\nexcept OSError:\n print('WRITE_DENIED')\ns=socket.socket()\ns.settimeout(1)\ntry:\n s.connect(('1.1.1.1',443))\n print('NETWORK_LEAK')\nexcept OSError:\n print('NETWORK_DENIED')\npathlib.Path('local.txt').write_text('ok')\nprint('LOCAL_OK')\n");
    let sandbox = SandboxExecutor::new(SandboxPolicy {
        enabled: true,
        timeout_ms: 10_000,
        ..Default::default()
    });
    let result = sandbox
        .execute(SandboxRequest {
            language: "python".into(),
            code,
            timeout_ms: None,
        })
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    for marker in ["READ_DENIED", "WRITE_DENIED", "NETWORK_DENIED", "LOCAL_OK"] {
        assert!(result.stdout.contains(marker), "{result:?}");
    }
    assert!(!result.stdout.contains("LEAK"), "{result:?}");
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "not-for-sandbox");
}

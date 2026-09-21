// Included by the Claude module so protocol fixtures can exercise private
// boundaries without adding a public test injection path to production.
#[cfg(test)]
mod claude_protocol_tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    fn request(read_only: bool) -> NativeRequest {
        NativeRequest {
            app_id: "claude".into(),
            model: "claude-sonnet-4-6".into(),
            cwd: std::env::temp_dir(),
            prompt: "Offline protocol fixture".into(),
            read_only,
            max_duration_secs: 5,
            reasoning_effort: None,
            config_path: None,
        }
    }

    fn runner(
        read_only: bool,
    ) -> (
        Runner,
        mpsc::Receiver<NativeEvent>,
        tokio::io::DuplexStream,
        mpsc::Sender<NativeControl>,
    ) {
        let (writer, reader) = tokio::io::duplex(65536);
        let (_, frames) = mpsc::channel(64);
        let (controls_tx, controls) = mpsc::channel(64);
        let (events, receiver) = mpsc::channel(64);
        (
            Runner::new(
                request(read_only),
                "session-fixture".into(),
                Box::new(writer),
                frames,
                controls,
                events,
            ),
            receiver,
            reader,
            controls_tx,
        )
    }

    fn init(read_only: bool) -> Value {
        let mut tools = READ_TOOLS.to_vec();
        if !read_only {
            tools.extend_from_slice(WRITE_TOOLS);
        }
        json!({"type":"system", "subtype":"init", "session_id":"session-fixture", "model":"claude-sonnet-4-6",
            "claude_code_version":SUPPORTED_VERSION, "permissionMode":"default", "tools":tools,
            "mcp_servers":[], "plugins":[], "skills":[]})
    }

    fn event(event: Value) -> Value {
        json!({"type":"stream_event", "session_id":"session-fixture", "parent_tool_use_id":null, "event":event})
    }

    fn start(id: &str, model: &str) -> Value {
        event(json!({"type":"message_start", "message":{"id":id, "model":model}}))
    }

    fn assistant(id: &str, text: &str, thinking: &str) -> Value {
        json!({"type":"assistant", "session_id":"session-fixture", "parent_tool_use_id":null,
            "message":{"id":id, "model":"claude-sonnet-4-6", "content":[
                {"type":"thinking", "thinking":thinking}, {"type":"text", "text":text}]}})
    }

    fn terminal() -> Value {
        json!({"type":"result", "session_id":"session-fixture", "subtype":"success", "is_error":false,
            "result":"Hello", "modelUsage":{"claude-sonnet-4-6":{"inputTokens":3,"outputTokens":1}}, "usage":{"input_tokens":3}})
    }

    fn permission(id: &str, name: &str, input: Value) -> Value {
        json!({"type":"control_request", "request_id":id, "request":{"subtype":"can_use_tool",
            "tool_name":name, "tool_use_id":"tool-1", "input":input}})
    }

    #[test]
    fn exact_models_effort_and_config_are_checked() {
        let mut req = request(true);
        assert!(validate(&req).is_ok());
        for model in [
            "sonnet",
            "default",
            "claude-foo\n",
            "gpt-5.4",
            "claude-sonnet-4-6[1m]",
        ] {
            req.model = model.into();
            assert!(validate(&req).is_err(), "{model}");
        }
        req = request(true);
        req.reasoning_effort = Some("none".into());
        assert!(validate(&req).is_err());
        req = request(true);
        req.config_path = Some(PathBuf::from("settings.json"));
        assert!(validate(&req).is_err());
    }

    #[test]
    fn settings_require_exact_applied_model_effort_and_no_policy_overlay() {
        let mut req = request(true);
        req.reasoning_effort = Some("high".into());
        let valid = json!({"applied":{"model":req.model,"effort":"high"}, "effective":settings(),
            "sources":[{"source":"flagSettings","settings":settings()}]});
        assert!(validate_settings(&valid, &req).is_ok());
        for (pointer, changed) in [
            ("/applied/model", json!("claude-opus-4-6")),
            ("/applied/effort", json!("low")),
            ("/effective/fallbackModel", json!(["claude-haiku-4-5"])),
            (
                "/effective/permissions/defaultMode",
                json!("bypassPermissions"),
            ),
        ] {
            let mut bad = valid.clone();
            *bad.pointer_mut(pointer).unwrap() = changed;
            assert!(validate_settings(&bad, &req).is_err(), "{pointer}");
        }
        let mut policy = valid;
        policy["sources"]
            .as_array_mut()
            .unwrap()
            .push(json!({"source":"policySettings","settings":{"hooks":{}}}));
        assert!(validate_settings(&policy, &req).is_err());
    }

    #[tokio::test]
    async fn stream_and_full_messages_do_not_duplicate_text_or_reasoning() {
        let (mut runner, mut events, _, _controls) = runner(true);
        runner.frame(init(true)).await.unwrap();
        runner
            .frame(start("message-1", "claude-sonnet-4-6"))
            .await
            .unwrap();
        runner.frame(event(json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}))).await.unwrap();
        runner.frame(event(json!({"type":"content_block_delta","index":0, "delta":{"type":"thinking_delta", "thinking":"Consider"}}))).await.unwrap();
        let mut thinking_snapshot = assistant("message-1", "", "Consider");
        thinking_snapshot["message"]["content"]
            .as_array_mut()
            .unwrap()
            .pop();
        runner.frame(thinking_snapshot).await.unwrap();
        runner
            .frame(event(json!({"type":"content_block_stop","index":0})))
            .await
            .unwrap();
        runner.frame(event(json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}))).await.unwrap();
        runner
            .frame(event(
                json!({"type":"content_block_delta","index":1, "delta":{"type":"text_delta", "text":"Hel"}}),
            ))
            .await
            .unwrap();
        let mut text_snapshot = assistant("message-1", "Hello", "");
        text_snapshot["message"]["content"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        runner.frame(text_snapshot.clone()).await.unwrap();
        runner.frame(text_snapshot).await.unwrap();
        runner
            .frame(event(json!({"type":"content_block_stop","index":1})))
            .await
            .unwrap();
        runner
            .frame(event(json!({"type":"message_stop"})))
            .await
            .unwrap();
        assert_eq!(runner.result.output, "Hello");
        assert!(runner.frame(terminal()).await.unwrap());
        let mut text = String::new();
        let mut reasoning = String::new();
        while let Ok(event) = events.try_recv() {
            match event {
                NativeEvent::TextDelta { text: delta } => text.push_str(&delta),
                NativeEvent::ReasoningDelta { text: delta } => reasoning.push_str(&delta),
                _ => {}
            }
        }
        assert_eq!(text, "Hello");
        assert_eq!(reasoning, "Consider");
    }

    #[tokio::test]
    async fn official_offline_trace_preserves_thinking_then_text_blocks() {
        let (mut runner, mut events, _, _controls) = runner(true);
        let mut complete = false;
        for line in include_str!("official_2_1_193.jsonl").lines() {
            let frame: Value = serde_json::from_str(line).unwrap();
            complete = runner.frame(frame).await.unwrap();
        }
        assert!(complete);
        assert_eq!(runner.result.output, "Offline fixture OK");
        let mut thinking = String::new();
        while let Ok(event) = events.try_recv() {
            if let NativeEvent::ReasoningDelta { text } = event {
                thinking.push_str(&text);
            }
        }
        assert_eq!(thinking, "Offline reasoning");
    }

    #[tokio::test]
    async fn unfinished_streams_cannot_report_success() {
        // Start from actual official CLI output, then independently omit the
        // message stop, leave an earlier message incomplete, or omit the last
        // block stop. All other evidence still describes a successful turn.
        for fault in [
            "missing_message_stop",
            "orphaned_message",
            "missing_block_stop",
        ] {
            let (mut runner, _events, _reader, _controls) = runner(true);
            let mut terminal = None;
            for line in include_str!("official_2_1_193.jsonl").lines() {
                let frame: Value = serde_json::from_str(line).unwrap();
                if frame["type"] == "result" {
                    terminal = Some(frame);
                    break;
                }
                if fault == "missing_message_stop" && frame["event"]["type"] == "message_stop" {
                    continue;
                }
                if fault == "missing_block_stop"
                    && frame["event"]["type"] == "content_block_stop"
                    && frame["event"]["index"] == 1
                {
                    continue;
                }
                if fault == "orphaned_message" && frame["event"]["type"] == "message_start" {
                    runner
                        .frame(start("unfinished-earlier-message", "claude-sonnet-4-6"))
                        .await
                        .unwrap();
                }
                assert!(!runner.frame(frame).await.unwrap());
            }
            assert_eq!(runner.result.output, "Offline fixture OK");
            let error = runner
                .frame(terminal.expect("official success result"))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("unfinished assistant stream"),
                "{fault}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn model_switch_is_rejected_before_text_is_emitted() {
        let (mut runner, mut events, _, _controls) = runner(true);
        runner.frame(init(true)).await.unwrap();
        assert!(runner
            .frame(start("message-1", "claude-opus-4-6"))
            .await
            .is_err());
        assert!(runner.result.output.is_empty());
        assert!(events.try_recv().is_err());
        assert!(runner
            .frame(event(
                json!({"type":"content_block_delta", "delta":{"type":"text_delta", "text":"bad"}})
            ))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn chat_denies_mutation_even_when_ui_can_approve() {
        let (mut runner, _, reader, _controls) = runner(true);
        runner.frame(init(true)).await.unwrap();
        assert!(runner
            .frame(permission("p1", "Bash", json!({"command":"touch file"})))
            .await
            .is_err());
        let mut lines = BufReader::new(reader).lines();
        let response: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            response.pointer("/response/response/behavior").unwrap(),
            "deny"
        );
        assert!(runner.pending.is_empty());
    }

    #[tokio::test]
    async fn agent_mutation_waits_for_single_explicit_permission() {
        let (mut runner, mut events, reader, _controls) = runner(false);
        runner.frame(init(false)).await.unwrap();
        runner
            .frame(permission(
                "p1",
                "Write",
                json!({"file_path":"fixture.txt","content":"ok"}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(NativeEvent::PermissionRequested { .. })
        ));
        assert_eq!(runner.pending.len(), 1);
        runner
            .control(NativeControl::Permission {
                request_id: "p1".into(),
                approve: true,
            })
            .await
            .unwrap();
        let mut lines = BufReader::new(reader).lines();
        let response: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            response.pointer("/response/response/behavior").unwrap(),
            "allow"
        );
        assert_eq!(
            response
                .pointer("/response/response/updatedInput/content")
                .unwrap(),
            "ok"
        );
        assert!(runner.pending.is_empty());
        assert!(runner
            .frame(permission("p1", "Write", json!({})))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn questions_preserve_option_shape_and_validate_answer_keys() {
        let (mut runner, mut events, reader, _controls) = runner(true);
        runner.frame(init(true)).await.unwrap();
        let input = json!({"questions":[{"question":"Choose scope?","header":"Scope",
            "multiSelect":true,"options":[{"label":"Core","description":"Core files"}]}]});
        runner
            .frame(permission("q1", "AskUserQuestion", input))
            .await
            .unwrap();
        let NativeEvent::QuestionRequested { data, .. } = events.recv().await.unwrap() else {
            panic!("expected question");
        };
        assert_eq!(data["questions"][0]["multi_select"], true);
        assert!(runner
            .control(NativeControl::Answer {
                request_id: "q1".into(),
                answers: json!({"unknown":"Core"})
            })
            .await
            .is_err());
        assert!(runner.pending.contains_key("q1"));
        runner
            .control(NativeControl::Answer {
                request_id: "q1".into(),
                answers: json!({"Choose scope?":"Core"}),
            })
            .await
            .unwrap();
        let mut lines = BufReader::new(reader).lines();
        let response: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            response
                .pointer("/response/response/updatedInput/answers/Choose scope?")
                .unwrap(),
            "Core"
        );
    }

    #[tokio::test]
    async fn failed_mixed_model_or_incomplete_terminals_never_complete() {
        for kind in 0..4 {
            let (mut runner, _events, _reader, _controls) = runner(true);
            runner.frame(init(true)).await.unwrap();
            runner
                .frame(assistant("message-1", "Hello", ""))
                .await
                .unwrap();
            let mut result = terminal();
            match kind {
                0 => result["subtype"] = json!("error_max_turns"),
                1 => result["modelUsage"]["claude-haiku-4-5"] = json!({}),
                2 => result["session_id"] = json!("other-session"),
                _ => result["terminal_reason"] = json!("aborted_tools"),
            }
            assert!(runner.frame(result).await.is_err(), "case {kind}");
        }
    }

    #[tokio::test]
    async fn custom_tools_and_version_changes_fail_closed_at_init() {
        for kind in 0..4 {
            let (mut runner, _, _, _controls) = runner(true);
            let mut init = init(true);
            match kind {
                0 => init["tools"].as_array_mut().unwrap().push(json!("Agent")),
                1 => init["claude_code_version"] = json!("2.1.999"),
                2 => init["plugins"] = json!([{"name":"unknown"}]),
                _ => init["permissionMode"] = json!("bypassPermissions"),
            }
            assert!(runner.frame(init).await.is_err());
        }
    }

    #[tokio::test]
    async fn closed_approval_channel_denies_and_cancellation_releases_pending() {
        let (mut runner, _events, reader, controls) = runner(false);
        runner.frame(init(false)).await.unwrap();
        drop(controls);
        runner
            .frame(permission("p1", "Write", json!({"content":"no"})))
            .await
            .unwrap();
        let mut lines = BufReader::new(reader).lines();
        let response: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            response.pointer("/response/response/behavior").unwrap(),
            "deny"
        );
        assert!(runner.pending.is_empty());
    }

    #[tokio::test]
    async fn invalid_handshake_never_sends_the_user_prompt() {
        let (mut runner, _, reader, _controls) = runner(true);
        let (tx, frames) = mpsc::channel(2);
        runner.frames = frames;
        tx.send(Ok(json!({"type":"control_response", "response":{"subtype":"success", "request_id":"host-initialize",
            "response":{"account":{"apiProvider":"bedrock"}}}}))).await.unwrap();
        assert!(runner.run().await.is_err());
        drop(runner.stdin);
        let mut lines = BufReader::new(reader).lines();
        let mut sent = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            sent.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["type"], "control_request");
    }
}

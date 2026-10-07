use super::*;
use crate::ir::{Finish, Reasoning, Usage};
use serde_json::json;

#[test]
fn cache_writes_and_inclusive_reasoning_survive_full_and_stream_translation() {
    let expected = Usage { input: 70, cache_read: 20, cache_write: 10, output: 40, reasoning: 30 };
    let req = Request { include_usage: true, ..Default::default() };
    for format in [Format::Chat, Format::Responses, Format::Claude] {
        let mut agg = Aggregate::default();
        agg.push(&Event::Usage(expected.clone()));
        agg.push(&Event::Finish(Finish::Stop));
        let body = render_full(format, &agg, "model", &req);
        let mut full = Aggregate::default();
        for event in full_to_events(format, &body) {
            full.push(&event);
        }
        assert_eq!(full.usage, expected, "full {format:?}");
        let mut renderer = renderer(format, "model", &req);
        let mut frames = Vec::new();
        renderer.push(&Event::Usage(expected.clone()), &mut frames);
        renderer.push(&Event::Finish(Finish::Stop), &mut frames);
        renderer.finish(&mut frames);
        let mut parser = parser(format);
        let mut events = Vec::new();
        for frame in frames {
            parser
                .feed(&SseEvent { event: frame.event.map(|event| event.into_owned()), data: frame.data }, &mut events);
        }
        let mut streamed = Aggregate::default();
        for event in events {
            streamed.push(&event);
        }
        assert_eq!(streamed.usage, expected, "stream {format:?}");
        assert_eq!(streamed.usage.prompt_total() + streamed.usage.output, 140);
    }
}

#[test]
fn manual_thinking_preserves_caps_and_rejects_impossible_budgets() {
    let model = "claude-sonnet-4-5";
    for cap in [0, 64, 1024, 1025, 1500, 2047, 2048, 4096] {
        let body = json!({"messages":[{"role":"user","content":"hello"}],"max_tokens":cap,"reasoning_effort":"low"});
        let req = chat::parse_request(&body).unwrap();
        let built = claude::build_request(&req, model);
        assert_eq!(built["max_tokens"], cap, "explicit caps must never grow");
        if cap <= 1024 {
            assert!(claude::validate_thinking_budget(&req, model).is_err());
            assert!(claude::validate_native_thinking_budget(&built, model).is_err());
        } else {
            claude::validate_thinking_budget(&req, model).unwrap();
            claude::validate_native_thinking_budget(&built, model).unwrap();
            let budget = built["thinking"]["budget_tokens"].as_u64().unwrap();
            assert!(budget >= 1024 && budget < cap);
        }
    }
    let mut req = chat::parse_request(&json!({"messages":[{"role":"user","content":"hello"}],
        "max_tokens":64,"reasoning_effort":"none"}))
    .unwrap();
    claude::validate_thinking_budget(&req, model).unwrap();
    req.reasoning = Some(Reasoning { effort: Some("high".into()), ..Default::default() });
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![Part::ToolCall { id: "call_1".into(), name: "lookup".into(), args: "{}".into(), sig: None }],
    });
    claude::validate_thinking_budget(&req, model).unwrap();
    let built = claude::build_request(&req, model);
    assert_eq!(built["thinking"]["type"], "disabled");
    assert_eq!(built["max_tokens"], 64);
}

#[test]
fn translated_cache_defaults_respect_explicit_controls() {
    let original = json!({"input":"hello"});
    let mut body = json!({"system":[{"type":"text","text":"stable instructions"}],
        "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]});
    claude::apply_translated_cache_policy(&original, &mut body);
    assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
    let automatic = body.clone();
    claude::apply_translated_cache_policy(&original, &mut body);
    assert_eq!(body, automatic);
    for mut body in [
        json!({"cache_control":{"type":"ephemeral","ttl":"1h"}}),
        json!({"tools":[{"name":"tool","cache_control":{"type":"ephemeral","ttl":"1h"}}],
        "system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}],
        "messages":[{"role":"user","content":[
            {"type":"text","text":"first","cache_control":{"type":"ephemeral"}},
            {"type":"text","text":"second","cache_control":{"type":"ephemeral"}}
        ]}]}),
    ] {
        let explicit = body.clone();
        claude::apply_translated_cache_policy(&original, &mut body);
        assert_eq!(body, explicit, "do not add a fifth marker or change TTL ordering");
    }
    let mut body = json!({"messages":[]});
    claude::apply_translated_cache_policy(&json!({"prompt_cache_options":{"mode":"explicit"}}), &mut body);
    assert!(body["cache_control"].is_null());
    claude::apply_translated_cache_policy(
        &json!({"input":[{"role":"user","content":[
            {"type":"input_text","text":"prefix","prompt_cache_breakpoint":{"mode":"explicit"}}
        ]}]}),
        &mut body,
    );
    assert!(body["cache_control"].is_null());
}

#[test]
fn gemini_native_and_translated_efforts_agree_and_preserve_summary_choice() {
    for (effort, expected) in [("none", "low"), ("minimal", "low"), ("medium", "medium"), ("high", "high")] {
        let r = Reasoning { effort: Some(effort.into()), disabled: effort == "none", ..Default::default() };
        let mut native = json!({"generation_config":{"max_output_tokens":4000,
            "thinking_config":{"thinking_budget":1234,"include_thoughts":false}}});
        gemini::apply_native_reasoning(&mut native, &r, "gemini-3.8-flash");
        let translated =
            gemini::build_request(&Request { reasoning: Some(r), ..Default::default() }, "gemini-3.8-flash");
        assert_eq!(native["generationConfig"]["thinkingConfig"]["thinkingLevel"], expected);
        assert_eq!(translated["generationConfig"]["thinkingConfig"]["thinkingLevel"], expected);
        assert_eq!(native["generationConfig"]["max_output_tokens"], 4000);
        assert_eq!(native["generationConfig"]["thinkingConfig"]["includeThoughts"], false);
        assert!(native["generationConfig"]["thinkingConfig"]["thinking_budget"].is_null());
        assert!(native.get("generation_config").is_none());
    }
    let r = Reasoning { budget: Some(100_000), ..Default::default() };
    let mut native = json!({});
    gemini::apply_native_reasoning(&mut native, &r, "gemini-2.5-pro");
    assert_eq!(native["generationConfig"]["thinkingConfig"]["thinkingBudget"], 32_768);
    gemini::apply_native_reasoning(&mut native, &r, "gemini-3-image");
    assert!(native["generationConfig"]["thinkingConfig"].is_null());
}

#[test]
fn chat_done_requires_finish_reason_but_allows_usage_after_finish() {
    let mut parser = chat::Parser::default();
    let mut events = Vec::new();
    parser.feed(&SseEvent { event: None, data: "[DONE]".into() }, &mut events);
    assert!(matches!(events.as_slice(), [Event::Error { status: 502, .. }]));
    let mut parser = chat::Parser::default();
    let mut events = Vec::new();
    for data in [
        json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}).to_string(),
        json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}).to_string(),
        "[DONE]".into(),
    ] {
        parser.feed(&SseEvent { event: None, data }, &mut events);
    }
    assert!(events.iter().any(|event| matches!(event, Event::Finish(Finish::Stop))));
    assert!(events.iter().any(|event| matches!(event, Event::Usage(Usage { output: 3, .. }))));
    assert!(!events.iter().any(|event| matches!(event, Event::Error { .. })));
}

#[test]
fn failed_tool_streams_never_complete_partial_calls() {
    for format in [Format::Responses, Format::Gemini] {
        let mut renderer = renderer(format, "model", &Request::default());
        let mut frames = Vec::new();
        for event in [
            Event::ToolStart { key: 0, id: "call_1".into(), name: "run".into() },
            Event::ToolArgs { key: 0, delta: "{\"command\":".into() },
            Event::Usage(Usage { input: 3, output: 2, ..Default::default() }),
            Event::Error { status: 502, message: "truncated upstream".into() },
        ] {
            renderer.push(&event, &mut frames);
        }
        renderer.finish(&mut frames);
        for frame in &frames {
            let value: Value = serde_json::from_str(&frame.data).unwrap();
            assert!(value["candidates"][0]["content"]["parts"][0]["functionCall"].is_null());
            assert!(
                !frame.event.as_deref().is_some_and(|event| event.ends_with(".done") || event == "response.completed")
            );
        }
        if format == Format::Responses {
            let failed: Value = serde_json::from_str(&frames.last().unwrap().data).unwrap();
            assert_eq!(failed["response"]["status"], "failed");
            assert_eq!(failed["response"]["usage"]["output_tokens"], 2);
            assert_eq!(failed["response"]["output"], json!([]));
        }
    }
}

#[test]
fn gemini_blocked_prompts_are_terminal_safety_responses() {
    for feedback in [json!({"blockReason":"SAFETY"}), json!({"block_reason":"PROHIBITED_CONTENT"})] {
        let body = json!({"promptFeedback":feedback,"usageMetadata":{"promptTokenCount":10}});
        let events = gemini::full_to_events(&body);
        assert!(events.iter().any(|event| matches!(event, Event::Finish(Finish::Filter))));
        assert!(events.iter().any(|event| matches!(event, Event::Usage(Usage { input: 10, .. }))));
        let mut parser = gemini::Parser::default();
        let mut streamed = Vec::new();
        parser.feed(&SseEvent { event: None, data: body.to_string() }, &mut streamed);
        assert!(streamed.iter().any(|event| matches!(event, Event::Finish(Finish::Filter))));
    }
    assert!(
        !gemini::full_to_events(&json!({"promptFeedback":{"blockReason":"BLOCK_REASON_UNSPECIFIED"}}))
            .iter()
            .any(|event| matches!(event, Event::Finish(_)))
    );
}

#[test]
fn failed_responses_terminals_keep_usage_without_successful_finish() {
    for kind in ["response.failed", "response.done", "response.completed"] {
        let mut parser = responses::Parser::default();
        let mut events = Vec::new();
        parser.feed(
            &SseEvent {
                event: None,
                data: json!({"type":kind,"response":{
                    "status":"failed","error":{"code":"server_is_overloaded","message":"busy"},
                    "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":80},"output_tokens":4}
                }})
                .to_string(),
            },
            &mut events,
        );
        assert!(
            matches!(
                events.as_slice(),
                [Event::Usage(Usage { input: 20, output: 4, cache_read: 80, .. }), Event::Error { status: 503, .. },]
            ),
            "{kind}: {events:?}"
        );
    }
}

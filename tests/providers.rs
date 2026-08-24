//! The provider seam: one harness, every dialect, the catalog, the registry.
//!
//! Adding a dialect is a case in the table, not a new file. The mock server,
//! `E_HOME` lock, and request collector live in `common/` so the pins stay
//! about wire shape and event grammar — not fixture boilerplate.

mod common;

use common::{clear_env_keys, env_lock, read_tool, request_json, serve_sse, test_model, Home};

use e::core::provider::catalog::{self, Api, Model, Thinking};
use e::core::provider::{self, ChatMessage, Event, Request, SseSplitter, ToolCall};

// ---------------------------------------------------------------------------
// Shared framing
// ---------------------------------------------------------------------------

#[test]
fn sse_splitter_handles_fragmentation_and_crlf() {
    let mut s = SseSplitter::new();
    assert!(s.feed("data: {\"a\":").is_empty());
    assert_eq!(s.feed("1}\n\n"), vec!["{\"a\":1}"]);
    assert_eq!(s.feed("data: x\r\n\r\ndata: y\n\n"), vec!["x", "y"]);
    assert_eq!(s.feed("data: a\ndata: b\n\n"), vec!["a\nb"]);
}

#[test]
fn sse_splitter_preserves_utf8_across_byte_chunks() {
    let event = "data: {\"text\":\"é\"}\n\n";
    let bytes = event.as_bytes();
    let split = bytes.iter().position(|byte| *byte == 0xc3).unwrap() + 1;
    let mut splitter = SseSplitter::new();
    assert!(splitter.feed_bytes(&bytes[..split]).is_empty());
    assert_eq!(
        splitter.feed_bytes(&bytes[split..]),
        vec![r#"{"text":"é"}"#]
    );
}

// ---------------------------------------------------------------------------
// Dialects — one table, one pin per wire contract
// ---------------------------------------------------------------------------

struct DialectCase {
    name: &'static str,
    provider: &'static str,
    auth: &'static str,
    api: Api,
    thinking: Thinking,
    effort: Option<&'static str>,
    /// Anthropic is the only dialect that reshapes tool results on the way
    /// out; the others still get a user turn so the request isn't empty.
    history: bool,
    sse: &'static str,
    text: &'static str,
    reasoning: &'static str,
    usage: Option<(u64, u64, u64)>,
    tool: Option<(&'static str, &'static str)>,
}

fn dialects() -> Vec<DialectCase> {
    vec![
        DialectCase {
            name: "completions",
            provider: "mock",
            auth: r#"{"mock":{"key":"k"}}"#,
            api: Api::Completions,
            thinking: Thinking::Manual,
            effort: None,
            history: false,
            sse: concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
                "\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,",
                "\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
                "data: [DONE]\n\n",
            ),
            text: "Hello",
            reasoning: "hmm",
            usage: Some((12, 3, 4)),
            tool: Some(("c1", "{\"path\":\"a.txt\"}")),
        },
        DialectCase {
            name: "anthropic",
            provider: "anthropic",
            auth: r#"{"anthropic":{"key":"sk-ant-test"}}"#,
            api: Api::Anthropic,
            thinking: Thinking::Manual,
            effort: Some("high"),
            history: true,
            sse: concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":40}}}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"read\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.txt\\\"}\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
                "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":25}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ),
            text: "hello world",
            reasoning: "hmm",
            usage: Some((100, 25, 40)),
            tool: Some(("tu_1", "{\"path\":\"a.txt\"}")),
        },
        DialectCase {
            name: "responses",
            provider: "openai",
            auth: r#"{"openai":{"key":"sk-test"}}"#,
            api: Api::Responses,
            thinking: Thinking::Manual,
            effort: Some("medium"),
            history: false,
            sse: concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"hmm\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":0}}}}\n\n",
            ),
            text: "hi",
            reasoning: "hmm",
            usage: Some((10, 2, 0)),
            tool: Some(("c1", "{\"path\":\"a.txt\"}")),
        },
    ]
}

fn assert_wire(name: &str, sent: &str) {
    let body = request_json(sent);
    match name {
        "completions" => {
            assert!(
                body["tools"][0].get("function").is_some(),
                "completions must keep the nested function shape: {body}"
            );
            assert_eq!(body["tools"][0]["function"]["name"], "read");
        }
        "anthropic" => {
            assert!(sent.contains("x-api-key: sk-ant-test"));
            assert!(sent.contains("anthropic-version: 2023-06-01"));
            assert!(
                body["tools"][0]["input_schema"].is_object(),
                "tool schema not converted"
            );
            assert!(
                sent.contains("\"tool_use_id\":\"tu_0\""),
                "tool result not in anthropic shape"
            );
            assert!(
                sent.contains("\"budget_tokens\":24000"),
                "high effort budget missing"
            );
            assert!(body["max_tokens"].is_number());
        }
        "responses" => {
            // The regression that 400'd the first live turn: tools[0].name
            // at top level, not nested under function.
            assert_eq!(body["tools"][0]["type"], "function");
            assert_eq!(body["tools"][0]["name"], "read");
            assert!(
                body["tools"][0].get("function").is_none(),
                "nested shape leaked"
            );
            assert!(body["tools"][0]["parameters"].is_object());
            assert!(body["parallel_tool_calls"].as_bool().unwrap());
            let request_line = sent.lines().next().unwrap();
            assert!(
                request_line.starts_with("POST /responses "),
                "wrong mount: {request_line}"
            );
            assert!(!sent.contains("chatgpt-account-id"));
            assert!(
                sent.contains("authorization: Bearer sk-test")
                    || sent.contains("Authorization: Bearer sk-test")
            );
        }
        other => panic!("unknown dialect {other}"),
    }
}

async fn collect_stream(
    request: Request,
) -> (String, String, Vec<ToolCall>, Option<(u64, u64, u64)>) {
    let (mut rx, _handle) = provider::stream(request);
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls = Vec::new();
    let mut usage = None;
    while let Some(event) = rx.recv().await {
        match event {
            Event::TextDelta(d) => text.push_str(&d),
            Event::ReasoningDelta(d) => reasoning.push_str(&d),
            Event::ToolCall(c) => calls.push(c),
            Event::Usage {
                input,
                output,
                cache_read,
            } => usage = Some((input, output, cache_read)),
            Event::Error(err) => panic!("stream errored: {}", err.message),
            Event::Done => break,
            Event::ReasoningItem(_) => {}
        }
    }
    (text, reasoning, calls, usage)
}

// The env lock is held across awaits: E_HOME must stay ours and each
// #[tokio::test] runs on its own runtime.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn each_dialect_streams_text_tools_and_usage() {
    let _lock = env_lock();
    for case in dialects() {
        let (port, server) = serve_sse(&[case.sse]);
        let home = Home::new(case.name);
        home.auth(case.auth);

        let mut model = test_model(case.provider, port, case.api);
        model.thinking = case.thinking;
        let messages = if case.history {
            vec![
                ChatMessage::user("read a.txt"),
                ChatMessage::assistant(
                    "",
                    vec![ToolCall {
                        id: "tu_0".into(),
                        name: "read".into(),
                        arguments: "{\"path\":\"old.txt\"}".into(),
                    }],
                ),
                ChatMessage::tool_result("tu_0", "old contents"),
            ]
        } else {
            vec![ChatMessage::user("hello")]
        };
        let request = Request {
            model,
            system: "sys".into(),
            messages,
            effort: case.effort.map(str::to_string),
            tools: vec![read_tool()],
        };

        let (text, reasoning, calls, usage) = collect_stream(request).await;
        assert_eq!(text, case.text, "{}", case.name);
        assert_eq!(reasoning, case.reasoning, "{}", case.name);
        match case.tool {
            Some((id, args)) => {
                assert_eq!(calls.len(), 1, "{}", case.name);
                assert_eq!(calls[0].id, id, "{}", case.name);
                assert_eq!(calls[0].arguments, args, "{}", case.name);
            }
            None => assert!(calls.is_empty(), "{}", case.name),
        }
        assert_eq!(usage, case.usage, "{}", case.name);

        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 1, "{}", case.name);
        assert_wire(case.name, &sent[0]);
    }
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn adaptive_models_take_effort_through_output_config() {
    let _lock = env_lock();
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (port, server) = serve_sse(&[body]);
    let home = Home::new("adaptive");
    home.auth(r#"{"anthropic":{"key":"sk-ant-test"}}"#);

    let mut model = test_model("anthropic", port, Api::Anthropic);
    model.thinking = Thinking::Adaptive;
    let request = Request {
        model,
        system: "be helpful".into(),
        messages: vec![ChatMessage::user("hi")],
        effort: Some("high".into()),
        tools: Vec::new(),
    };
    let (_text, _reasoning, _calls, _usage) = collect_stream(request).await;

    let sent = server.join().unwrap().remove(0);
    assert!(
        sent.contains("\"thinking\":{\"type\":\"adaptive\"}"),
        "adaptive shape missing"
    );
    assert!(
        sent.contains("\"output_config\":{\"effort\":\"high\"}"),
        "effort not carried through output_config"
    );
    assert!(
        !sent.contains("budget_tokens"),
        "legacy budget_tokens must not reach an adaptive model"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn responses_replays_reasoning_items_ahead_of_their_calls() {
    use e::core::agent::{Agent, SessionEvent};

    let _lock = env_lock();
    let first = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"SECRETBLOB\",\"summary\":[]}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"f.txt\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
    );
    let second = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
    );
    let (port, server) = serve_sse(&[first, second]);
    let home = Home::new("reasoning");
    home.auth(r#"{"openai":{"key":"k"}}"#);

    let ws = home.dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("f.txt"), "data\n").unwrap();
    std::env::set_current_dir(ws.canonicalize().unwrap()).unwrap();

    let (mut agent, mut rx) = Agent::new(test_model("openai", port, Api::Responses));
    agent.submit("read f.txt".into(), "sys".into());
    let mut ended = false;
    while let Some(event) = rx.recv().await {
        if let SessionEvent::TurnEnd { .. } = event {
            ended = true;
            break;
        }
    }
    assert!(ended);

    let sent = server.join().unwrap();
    assert_eq!(sent.len(), 2);
    let body = request_json(&sent[1]);
    let input = body["input"].as_array().unwrap();
    let kinds: Vec<&str> = input.iter().filter_map(|i| i["type"].as_str()).collect();
    let reasoning_pos = kinds.iter().position(|k| *k == "reasoning");
    let call_pos = kinds.iter().position(|k| *k == "function_call");
    assert!(
        reasoning_pos.is_some(),
        "reasoning item not replayed: {kinds:?}"
    );
    assert!(
        reasoning_pos.unwrap() < call_pos.expect("function_call replayed"),
        "reasoning must precede its call: {kinds:?}"
    );
    let item = &input[reasoning_pos.unwrap()];
    assert_eq!(item["encrypted_content"], "SECRETBLOB", "item not verbatim");
    assert_eq!(item["id"], "rs_1");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn unexpected_eof_is_an_error_not_a_silent_done() {
    let _lock = env_lock();
    // A partial stream that closes without [DONE].
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let (port, _server) = serve_sse(&[body]);
    let home = Home::new("eof");
    home.auth(r#"{"mock":{"key":"k"}}"#);

    let request = Request {
        model: test_model("mock", port, Api::Completions),
        system: "sys".into(),
        messages: vec![ChatMessage::user("hi")],
        effort: None,
        tools: Vec::new(),
    };
    let (mut rx, _handle) = provider::stream(request);
    let mut saw_text = false;
    let mut terminal = None;
    while let Some(event) = rx.recv().await {
        match event {
            Event::TextDelta(_) => saw_text = true,
            Event::Error(err) => {
                terminal = Some(err.message);
                break;
            }
            Event::Done => panic!("unexpected EOF must not finish as Done"),
            _ => {}
        }
    }
    assert!(saw_text);
    let message = terminal.expect("error terminal missing");
    assert!(
        message.contains("unexpected"),
        "unexpected EOF wording missing: {message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unanswered_request_fails_instead_of_hanging() {
    use e::core::provider::{http, send_request_within, FailureCause};
    use std::time::Duration;

    // The gap between connect (client-bounded) and body reads (chunk-bounded):
    // the server accepts the request and never sends response headers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::Read;
        let (mut sock, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 8192];
        let _ = sock.read(&mut buffer);
        std::thread::sleep(Duration::from_secs(30));
    });

    let builder = http()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .body("{}");
    let err = send_request_within(builder, Duration::from_millis(300))
        .await
        .expect_err("headers never arrive — the send must time out");
    assert!(
        err.message.contains("no response"),
        "stall wording missing: {}",
        err.message
    );
    assert_eq!(err.cause, FailureCause::Stalled);
    assert!(err.cause.is_retryable());
}

// ---------------------------------------------------------------------------
// Catalog + registry — walk the data, pin the seams that have drifted
// ---------------------------------------------------------------------------

fn catalog_with_models_json(json: &str) -> Vec<Model> {
    let home = Home::new("models");
    home.write("models.json", json);
    catalog::catalog()
}

#[test]
fn registry_and_catalog_match_the_embedded_data() {
    let _lock = env_lock();
    clear_env_keys();
    let _home = Home::new("registry");

    let all = e::core::provider::registry::all();
    assert!(all.len() >= 6);
    let catalog = catalog::catalog();

    for provider in all {
        assert!(
            !provider.display.is_empty(),
            "{} has no display name",
            provider.name
        );
        assert!(provider.base_url.starts_with("https://"));
        assert!(
            provider.auth.oauth.is_some() || provider.auth.key,
            "{} has no way to sign in",
            provider.name
        );
        if provider.auth.key {
            assert!(
                provider.auth.key_env.is_some(),
                "{} key without env var",
                provider.name
            );
        }
        assert_eq!(catalog::display_name(&provider.name), provider.display);

        for decl in &provider.models {
            let model = catalog
                .iter()
                .find(|m| m.provider == provider.name && m.id == decl.id)
                .unwrap_or_else(|| {
                    panic!("{} / {} missing from the catalog", provider.name, decl.id)
                });
            assert_eq!(model.base_url, provider.base_url, "{}", decl.id);
            assert_eq!(model.api, provider.api(), "{}", decl.id);
            assert_eq!(model.context_window, decl.context_window, "{}", decl.id);
            assert_eq!(model.efforts, decl.efforts, "{}", decl.id);
            let thinking = match decl.thinking.as_deref() {
                Some("adaptive") => Thinking::Adaptive,
                _ => Thinking::Manual,
            };
            assert_eq!(model.thinking, thinking, "{}", decl.id);
        }
    }

    // Panel contents, from data: two account flows, six key providers.
    assert_eq!(e::core::provider::registry::oauth_providers().len(), 2);
    assert_eq!(e::core::provider::registry::key_providers().len(), 6);

    let vercel = e::core::provider::registry::find("vercel").expect("vercel is a built-in");
    assert_eq!(vercel.auth.key_env.as_deref(), Some("AI_GATEWAY_API_KEY"));
    assert!(vercel.auth.oauth.is_none(), "gateway is API-key only");

    assert!(
        !catalog.iter().any(|m| m.id == "grok-build-0.1"),
        "grok-build-0.1 was culled from the built-ins"
    );
    let sonnet = catalog
        .iter()
        .find(|m| m.provider == "vercel" && m.id == "anthropic/claude-sonnet-5")
        .unwrap();
    assert_eq!(catalog::slug(sonnet), "vercel/anthropic/claude-sonnet-5");

    let go = catalog
        .iter()
        .find(|m| m.provider == "opencode-go")
        .unwrap();
    let zen = catalog
        .iter()
        .find(|m| m.provider == "opencode-zen")
        .unwrap();
    assert_ne!(go.base_url, zen.base_url, "Go and Zen must stay distinct");
}

#[test]
fn models_json_windows_and_overrides() {
    let _lock = env_lock();
    let catalog = catalog_with_models_json(
        r#"{"providers":{"local":{"base_url":"http://localhost:9999","context_window":64000,
            "models":["small", {"id":"big","context_window":1000000}]}}}"#,
    );
    let find = |id: &str| {
        catalog
            .iter()
            .find(|m| m.provider == "local" && m.id == id)
            .unwrap()
    };
    assert_eq!(
        find("small").context_window,
        64_000,
        "provider default applies"
    );
    assert_eq!(
        find("big").context_window,
        1_000_000,
        "per-model wins over provider default"
    );

    let catalog = catalog_with_models_json(
        r#"{"providers":{"opencode-go":{"models":[{"id":"kimi-k3","context_window":131072}]}}}"#,
    );
    let matches: Vec<_> = catalog
        .iter()
        .filter(|m| m.provider == "opencode-go" && m.id == "kimi-k3")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "the file entry replaces the built-in, not duplicates it"
    );
    assert_eq!(matches[0].context_window, 131_072);
}

#[test]
fn partial_override_inherits_the_builtin() {
    let _lock = env_lock();
    // One field corrected; transport, window, efforts, and thinking stay
    // with the built-in. Three former tests, one reproduction.
    let catalog = catalog_with_models_json(
        r#"{"providers":{"anthropic":{"models":[
            {"id":"claude-opus-5","context_window":123456},
            "claude-sonnet-5"
        ]}}}"#,
    );
    let opus = catalog
        .iter()
        .find(|m| m.provider == "anthropic" && m.id == "claude-opus-5")
        .unwrap();
    assert_eq!(opus.base_url, "https://api.anthropic.com");
    assert_eq!(opus.api, Api::Anthropic);
    assert_eq!(opus.context_window, 123_456);
    assert_eq!(opus.thinking, Thinking::Adaptive);

    let sonnet = catalog
        .iter()
        .find(|m| m.provider == "anthropic" && m.id == "claude-sonnet-5")
        .unwrap();
    assert_eq!(sonnet.context_window, 1_000_000);
    assert_eq!(
        sonnet.efforts,
        vec!["low".to_string(), "medium".to_string(), "high".to_string()]
    );
    assert_eq!(sonnet.thinking, Thinking::Adaptive);
}

#[test]
fn a_custom_provider_without_base_url_is_rejected_with_a_warning() {
    let _lock = env_lock();
    let home = Home::new("base-required");
    home.write(
        "models.json",
        r#"{"providers":{"custom":{"models":["secret-target"]}}}"#,
    );

    let catalog = catalog::catalog();
    assert!(
        !catalog
            .iter()
            .any(|model| model.provider == "custom" && model.id == "secret-target"),
        "a provider with no endpoint must not inherit an unrelated host"
    );
    assert_eq!(
        catalog::config_warnings(),
        vec!["models.json: provider custom requires an explicit base_url"]
    );
}

#[test]
fn only_signed_in_providers_are_available() {
    let _lock = env_lock();
    clear_env_keys();
    let home = Home::new("avail");

    assert!(catalog::available().is_empty());
    assert!(catalog::resolve("grok-4.6").is_none());

    home.auth(r#"{"anthropic":{"key":"k"}}"#);
    let available = catalog::available();
    assert!(available.iter().all(|m| m.provider == "anthropic"));
    assert!(catalog::resolve("claude-fable-5").is_some());
    assert!(catalog::resolve("grok-4.6").is_none());

    home.write(
        "settings.json",
        r#"{"model":"opencode-go/deepseek-v4-flash"}"#,
    );
    assert_eq!(catalog::default_model().provider, "anthropic");
}

#[test]
fn cycle_pool_follows_the_scope() {
    let _lock = env_lock();
    clear_env_keys();
    let home = Home::new("scope");
    home.auth(r#"{"anthropic":{"key":"k"},"xai":{"key":"k"}}"#);

    assert_eq!(catalog::cycle_pool().len(), catalog::available().len());

    catalog::set_scope(&[
        "anthropic/claude-fable-5".into(),
        "xai/grok-4.6".into(),
        "openai/gpt-5.5".into(),
    ]);
    let pool: Vec<String> = catalog::cycle_pool().iter().map(catalog::slug).collect();
    assert_eq!(pool, vec!["xai/grok-4.6", "anthropic/claude-fable-5"]);
}

#[test]
fn env_keys_sign_providers_in() {
    let _lock = env_lock();
    let home = Home::new("envkey");
    clear_env_keys();

    assert!(catalog::available().is_empty());

    // Every key_env the registry declares is a real sign-in, not just
    // Anthropic's. Walk them so a new provider is covered by this test.
    for provider in e::core::provider::registry::all() {
        let Some(env) = &provider.auth.key_env else {
            continue;
        };
        std::env::set_var(env, "sk-from-env");
        let available = catalog::available();
        assert!(
            available.iter().any(|m| m.provider == provider.name),
            "{env} did not sign {} in",
            provider.name
        );
        if provider.name == "vercel" {
            assert!(catalog::resolve("anthropic/claude-sonnet-5").is_some());
            assert!(catalog::resolve("vercel/anthropic/claude-sonnet-5").is_some());
        }
        std::env::remove_var(env);
        clear_env_keys();
    }

    // auth.json still wins over the environment.
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-env");
    home.auth(r#"{"anthropic":{"key":"sk-file"}}"#);
    match e::core::auth::load().get("anthropic").unwrap() {
        e::core::auth::Credential::ApiKey { key } => assert_eq!(key, "sk-file"),
        _ => panic!("wrong credential kind"),
    }
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[test]
fn legacy_opencode_auth_keys_still_sign_in() {
    let _lock = env_lock();
    let home = Home::new("legacy-auth");
    clear_env_keys();

    home.auth(r#"{"opencode":{"key":"sk-old"}}"#);
    let auth = e::core::auth::load();
    assert!(
        matches!(auth.get("opencode-zen"), Some(e::core::auth::Credential::ApiKey { key }) if key == "sk-old"),
        "legacy key not honored"
    );
    assert!(catalog::available()
        .iter()
        .any(|m| m.provider == "opencode-zen"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn provider_reported_models_appear_without_a_release() {
    use std::io::{Read, Write};
    let _lock = env_lock();
    clear_env_keys();
    let home = Home::new("live");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut a, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8192];
        let n = a.read(&mut buf).unwrap();
        let sent = String::from_utf8_lossy(&buf[..n]).to_string();
        let body = r#"{"data":[
            {"id":"brand-new-model","context_length":64000},
            {"id":"small","context_length":123456},
            {"id":"text-embedding-large"},
            {"id":"brand-new-model-20260101"},
            {"id":"fine-looking-embed","type":"embedding","context_length":8192},
            {"id":"typed-chat","type":"language","context_window":8000}
        ]}"#;
        let _ = a.write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
        sent
    });

    home.auth(r#"{"mock":{"key":"sk-live"}}"#);
    home.write(
        "models.json",
        format!(
            r#"{{"providers":{{"mock":{{"base_url":"http://127.0.0.1:{port}","api":"anthropic","models":["small"]}}}}}}"#
        ),
    );

    catalog::refresh_remote().await;
    let sent = server.join().unwrap();
    assert!(sent.contains("GET /models"));
    assert!(sent.contains("Bearer sk-live") || sent.contains("bearer sk-live"));

    let catalog = catalog::catalog();
    let fresh = catalog
        .iter()
        .find(|m| m.provider == "mock" && m.id == "brand-new-model")
        .expect("gateway model appears");
    assert_eq!(fresh.context_window, 64_000);
    assert_eq!(fresh.api, Api::Anthropic);
    let known = catalog
        .iter()
        .find(|m| m.provider == "mock" && m.id == "small")
        .expect("declared model stays listed");
    assert_eq!(known.context_window, 123_456);
    assert!(!catalog.iter().any(|m| m.id == "text-embedding-large"));
    assert!(!catalog.iter().any(|m| m.id == "brand-new-model-20260101"));
    assert!(
        !catalog.iter().any(|m| m.id == "fine-looking-embed"),
        "a non-language type is dropped even when the id looks like chat"
    );
    let typed = catalog
        .iter()
        .find(|m| m.provider == "mock" && m.id == "typed-chat")
        .expect("language type is kept");
    assert_eq!(typed.context_window, 8_000);
    assert!(catalog::available()
        .iter()
        .any(|m| m.id == "brand-new-model"));

    catalog::refresh_remote().await;
}

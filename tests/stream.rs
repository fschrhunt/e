//! The agent session stream: provider events fold into one `SessionEvent`
//! channel, failures still end the turn exactly once, Esc aborts a stall,
//! and a retryable campaign either recovers or gives up. Dialect parsing
//! lives in `providers.rs`.

mod common;

use std::io::{Read, Write};
use std::time::Duration;

use common::{env_lock, serve_raw, serve_sse, sse_response, test_model, Home};

use e::core::agent::{Agent, SessionEvent};
use e::core::provider::catalog::Api;

fn mock_home() -> Home {
    let home = Home::new("stream");
    home.auth(r#"{"mock":{"key":"k"}}"#);
    home
}

// The env lock is held across awaits: E_HOME must stay ours and each
// #[tokio::test] runs on its own runtime.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn agent_folds_provider_events_into_one_session_stream() {
    let _lock = env_lock();
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n",
    );
    let (port, _server) = serve_sse(&[body]);
    let _home = mock_home();

    let (mut agent, mut rx) = Agent::new(test_model("mock", port, Api::Completions));
    agent.submit("hi".into(), "sys".into());

    let mut kinds = Vec::new();
    while let Some(event) = rx.recv().await {
        let done = matches!(event, SessionEvent::TurnEnd { .. });
        kinds.push(match event {
            SessionEvent::TurnStart => "start",
            SessionEvent::TextDelta(_) => "text",
            SessionEvent::Usage { .. } => "usage",
            SessionEvent::TurnEnd { .. } => "end",
            SessionEvent::ReasoningDelta(_) => "reasoning",
            SessionEvent::Error(_) => "error",
            _ => "other",
        });
        if done {
            break;
        }
    }
    assert_eq!(kinds.first(), Some(&"start"));
    assert_eq!(kinds.last(), Some(&"end"));
    assert!(kinds.contains(&"text") && kinds.contains(&"usage"));
    assert!(!kinds.contains(&"error"));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn agent_reports_errors_and_still_ends_the_turn_exactly_once() {
    let _lock = env_lock();
    // A 401 is an auth-class failure: never retried, so one request, one
    // deterministic ending.
    let (port, _server) = serve_raw(vec![
        "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
    ]);
    let _home = mock_home();

    let (mut agent, mut rx) = Agent::new(test_model("mock", port, Api::Completions));
    agent.submit("hi".into(), "sys".into());

    let mut saw_error = false;
    let mut endings = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            SessionEvent::Error(_) => saw_error = true,
            SessionEvent::TurnEnd { aborted } => {
                endings.push(aborted);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_error, "error event missing");
    assert_eq!(endings, vec![false]);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn agent_interrupt_ends_a_stalled_stream() {
    let _lock = env_lock();
    // Headers arrive, then the body never does — the pre-fix hang: Esc set
    // the cancel flag but the turn only checked it after the next SSE event.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let _home = mock_home();

    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 8192];
        let _ = sock.read(&mut buffer);
        let _ = sock.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
        );
        std::thread::sleep(Duration::from_secs(30));
    });

    let (mut agent, mut rx) = Agent::new(test_model("mock", port, Api::Completions));
    agent.submit("hi".into(), "sys".into());

    while let Some(event) = rx.recv().await {
        if matches!(event, SessionEvent::TurnStart) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    agent.interrupt();

    let aborted = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = rx.recv().await {
            if let SessionEvent::TurnEnd { aborted } = event {
                return aborted;
            }
        }
        panic!("turn ended without TurnEnd");
    })
    .await
    .expect("interrupt must end a stalled turn promptly");
    assert!(aborted, "stalled interrupt should abort the turn");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn agent_interrupt_ends_a_turn_stuck_before_headers() {
    let _lock = env_lock();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let _home = mock_home();

    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 8192];
        let _ = sock.read(&mut buffer);
        std::thread::sleep(Duration::from_secs(30));
    });

    let (mut agent, mut rx) = Agent::new(test_model("mock", port, Api::Completions));
    agent.submit("hi".into(), "sys".into());

    while let Some(event) = rx.recv().await {
        if matches!(event, SessionEvent::TurnStart) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    agent.interrupt();

    let aborted = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = rx.recv().await {
            if let SessionEvent::TurnEnd { aborted } = event {
                return aborted;
            }
        }
        panic!("turn ended without TurnEnd");
    })
    .await
    .expect("interrupt must end a pre-headers stall promptly");
    assert!(aborted, "pre-headers interrupt should abort the turn");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn retry_recovers_after_a_transient_failure() {
    use e::core::provider::FailureCause;

    let _lock = env_lock();
    let ok = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (port, _server) = serve_raw(vec![
        "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
        sse_response(ok),
    ]);
    let _home = mock_home();

    let (mut agent, mut rx) = Agent::new(test_model("mock", port, Api::Completions));
    agent.submit("hi".into(), "sys".into());

    let mut saw_retry = false;
    let mut saw_recovered = false;
    let mut saw_error = false;
    let mut text = String::new();
    let mut aborted = None;
    while let Some(event) = rx.recv().await {
        match event {
            SessionEvent::Retry {
                attempt,
                limit,
                cause,
                ..
            } => {
                saw_retry = true;
                assert_eq!(attempt, 2);
                assert_eq!(limit, e::core::agent::retry::MAX_ATTEMPTS);
                assert_eq!(cause, FailureCause::ProviderUnavailable);
            }
            SessionEvent::Recovered { attempt, limit } => {
                saw_recovered = true;
                assert_eq!(attempt, 2);
                assert_eq!(limit, e::core::agent::retry::MAX_ATTEMPTS);
            }
            SessionEvent::TextDelta(d) => text.push_str(&d),
            SessionEvent::Error(_) => saw_error = true,
            SessionEvent::TurnEnd { aborted: a } => {
                aborted = Some(a);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_retry, "Retry event missing");
    assert!(saw_recovered, "Recovered event missing");
    assert!(!saw_error, "should not surface an error after recovering");
    assert_eq!(text, "ok");
    assert_eq!(aborted, Some(false));
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn retry_campaign_gives_up_after_max_attempts() {
    use e::core::agent::retry::MAX_ATTEMPTS;

    let _lock = env_lock();
    let fail =
        "HTTP/1.1 503 Service Unavailable\r\nretry-after: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    let (port, _server) = serve_raw(vec![fail.to_string(); MAX_ATTEMPTS]);
    let _home = mock_home();

    let (mut agent, mut rx) = Agent::new(test_model("mock", port, Api::Completions));
    agent.submit("hi".into(), "sys".into());

    let mut retries = Vec::new();
    let mut error_message = None;
    let aborted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("stream ended without TurnEnd");
            match event {
                SessionEvent::Retry { attempt, .. } => retries.push(attempt),
                SessionEvent::Error(message) => error_message = Some(message),
                SessionEvent::TurnEnd { aborted } => return aborted,
                _ => {}
            }
        }
    })
    .await
    .expect("the campaign must give up, not hang");

    assert_eq!(retries, (2..=MAX_ATTEMPTS).collect::<Vec<_>>());
    let message = error_message.expect("exhausted campaign must report an error");
    assert!(
        message.contains(&format!("{MAX_ATTEMPTS}/{MAX_ATTEMPTS}")),
        "attempt count missing: {message}"
    );
    assert!(
        message.contains("gave up"),
        "exhaustion wording missing: {message}"
    );
    assert!(!aborted);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn a_blank_successful_stream_surfaces_an_error_not_silence() {
    let _lock = env_lock();
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":0}}\n\n",
        "data: [DONE]\n\n",
    );
    let (port, _server) = serve_sse(&[body]);
    let _home = mock_home();

    let (mut agent, mut rx) = Agent::new(test_model("mock", port, Api::Completions));
    agent.submit("hi".into(), "sys".into());

    let mut saw_error = false;
    while let Some(event) = rx.recv().await {
        match event {
            SessionEvent::Error(message) => {
                saw_error = true;
                assert!(message.contains("empty"), "unexpected error: {message}");
            }
            SessionEvent::TurnEnd { .. } => break,
            _ => {}
        }
    }
    assert!(saw_error, "a blank success must surface an error");
}

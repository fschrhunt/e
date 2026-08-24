//! Live Vercel AI Gateway tests. Ignored by default so `cargo test` (and CI)
//! never spends tokens. Run only when asked:
//!
//! ```sh
//! cargo test --test gateway -- --ignored --nocapture
//! ```
//!
//! Needs `AI_GATEWAY_API_KEY` (Vercel's conventional name) or `AI_GATEWAY`
//! (an alias some environments inject). Hits a cheap model.

#![allow(clippy::await_holding_lock)]

use std::sync::Mutex;

use e::core::provider::catalog::{self, Api, Model};
use e::core::provider::{stream, ChatMessage, Event, Request};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Cheap, non-reasoning, tool-capable — enough to prove the wire without
/// burning a frontier model's token budget.
const CHEAP_MODEL: &str = "openai/gpt-4.1-nano";

fn gateway_key() -> String {
    std::env::var("AI_GATEWAY_API_KEY")
        .or_else(|_| std::env::var("AI_GATEWAY"))
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| {
            panic!("live gateway tests need AI_GATEWAY_API_KEY (or AI_GATEWAY) in the environment")
        })
}

fn isolate_home() -> (std::path::PathBuf, std::sync::MutexGuard<'static, ()>) {
    let lock = ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!(
        "e-gateway-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("E_HOME", &dir);
    for provider in e::core::provider::registry::all() {
        if let Some(env) = &provider.auth.key_env {
            std::env::remove_var(env);
        }
    }
    std::env::set_var("AI_GATEWAY_API_KEY", gateway_key());
    (dir, lock)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live Vercel AI Gateway — run with cargo test --test gateway -- --ignored"]
async fn gateway_lists_language_models() {
    let (dir, _lock) = isolate_home();

    catalog::refresh_remote_within(0).await;

    let listed: Vec<_> = catalog::catalog()
        .into_iter()
        .filter(|m| m.provider == "vercel")
        .collect();
    assert!(
        listed.len() > 3,
        "live /models should add gateway ids beyond the seed; got {}",
        listed.len()
    );
    assert!(
        listed.iter().any(|m| m.id == CHEAP_MODEL),
        "{CHEAP_MODEL} missing from the live catalog"
    );
    assert!(
        listed.iter().any(|m| m.id.contains('/')),
        "gateway ids are provider/model"
    );
    assert!(
        !listed.iter().any(|m| m.id.to_lowercase().contains("embed")),
        "embedding models must stay out of the picker"
    );
    let cheap = listed.iter().find(|m| m.id == CHEAP_MODEL).unwrap();
    assert!(
        cheap.context_window > 0,
        "the gateway reports a context window"
    );
    assert_eq!(cheap.api, Api::Completions);
    assert_eq!(cheap.base_url, "https://ai-gateway.vercel.sh/v1");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live Vercel AI Gateway — run with cargo test --test gateway -- --ignored"]
async fn cheap_model_streams_text() {
    let (dir, _lock) = isolate_home();

    let request = Request {
        model: Model {
            provider: "vercel".into(),
            id: CHEAP_MODEL.into(),
            base_url: "https://ai-gateway.vercel.sh/v1".into(),
            api: Api::Completions,
            efforts: Vec::new(),
            thinking: catalog::Thinking::Manual,
            context_window: 1_000_000,
        },
        system: "Reply with a single word. No punctuation.".into(),
        messages: vec![ChatMessage::user("Reply with the single word pong.")],
        effort: None,
        tools: Vec::new(),
    };
    let (mut rx, handle) = stream(request);

    let mut text = String::new();
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(45);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(Event::TextDelta(delta))) => text.push_str(&delta),
            Ok(Some(Event::ReasoningDelta(_))) => {}
            Ok(Some(Event::Usage { .. })) => {}
            Ok(Some(Event::Done)) => {
                saw_done = true;
                break;
            }
            Ok(Some(Event::Error(err))) => {
                panic!("gateway stream failed: {} ({})", err.message, err.short)
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => panic!("gateway stream timed out after 45s"),
        }
    }
    let _ = handle.await;

    assert!(saw_done, "stream must end with Done");
    assert!(
        !text.trim().is_empty(),
        "cheap model should stream some text"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

//! The chat-completions dialect: `POST {base}/chat/completions`, Bearer key,
//! SSE deltas at `choices[0].delta`, streamed tool-call argument fragments
//! accumulated by index, a final usage frame, `[DONE]` sentinel.

use serde_json::json;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

use crate::core::providers::runtime::Authorization;
use crate::core::providers::{
    http, require_success, retry_after_seconds, send_request, Event, FinishReason, ProviderError,
    Request, SseStream, StreamEnd, ToolCall,
};

/// Models that rejected our `reasoning_effort` at least once this run. Effort
/// levels are declared data (a seed, a `models.json` override), and a
/// realtime-discovered gateway can accept a set we never see advertised — so
/// a declared level can be wrong for the actual backend. Rather than hard-fail
/// on that, the request self-heals (retries without the field) and records the
/// model here, so we stop paying a failed round-trip for it every step. Keyed
/// `provider/id`; process-scoped, fail-open (a poisoned lock just re-probes).
fn reasoning_rejected() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(Default::default)
}

fn reasoning_is_rejected(model: &crate::core::providers::catalog::Model) -> bool {
    let key = format!("{}/{}", model.provider, model.id);
    reasoning_rejected()
        .lock()
        .map(|set| set.contains(&key))
        .unwrap_or(false)
}

fn mark_reasoning_rejected(model: &crate::core::providers::catalog::Model) {
    let key = format!("{}/{}", model.provider, model.id);
    if let Ok(mut set) = reasoning_rejected().lock() {
        set.insert(key);
    }
}

/// One chat-completions request. The caller retries it once without an
/// optional field (`stream_options`, `reasoning_effort`) when a strict gateway
/// names that field — or the reasoning knob — in its validation error.
async fn send(
    request: &Request,
    authorization: &Authorization,
    body: &serde_json::Value,
) -> Result<reqwest::Response, ProviderError> {
    send_request(
        http()?
            .post(format!("{}/chat/completions", request.model.base_url))
            .bearer_auth(&authorization.bearer)
            .header("accept", "text/event-stream")
            .json(body),
    )
    .await
}

pub async fn run(
    request: &Request,
    authorization: &Authorization,
    tx: &mpsc::Sender<Event>,
) -> Result<StreamEnd, ProviderError> {
    let mut messages = vec![json!({"role": "system", "content": request.system})];
    for m in &request.messages {
        match m.role.as_str() {
            "assistant" if !m.tool_calls.is_empty() => {
                let calls: Vec<_> = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        json!({"id": c.id, "type": "function",
                               "function": {"name": c.name, "arguments": c.arguments}})
                    })
                    .collect();
                let mut msg = json!({"role": "assistant", "tool_calls": calls});
                if !m.content.is_empty() {
                    msg["content"] = json!(m.content);
                }
                messages.push(msg);
            }
            "tool" => messages.push(json!({
                "role": "tool",
                "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": m.content,
            })),
            // Responses-dialect reasoning items mean nothing here.
            "reasoning" => {}
            role => {
                if m.images.is_empty() {
                    messages.push(json!({"role": role, "content": m.content}));
                } else {
                    let mut content = Vec::new();
                    if !m.content.is_empty() {
                        content.push(json!({"type": "text", "text": m.content}));
                    }
                    content.extend(m.images.iter().map(|image| {
                        json!({
                            "type": "image_url",
                            "image_url": {"url": image.data_url()},
                        })
                    }));
                    messages.push(json!({"role": role, "content": content}));
                }
            }
        }
    }

    let mut body = json!({
        "model": request.model.id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    // Reasoning effort rides the OpenAI-standard field; only sent when the
    // model declares a knob, so gateways that never heard of it never see
    // it. `off` has no wire encoding here — absence is the closest thing.
    // Skipped for a model already known to reject it (see `reasoning_rejected`).
    if let Some(effort) = request.effort.as_deref().filter(|e| *e != "off") {
        if !reasoning_is_rejected(&request.model) {
            body["reasoning_effort"] = json!(effort);
        }
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools);
    }

    let first = send(request, authorization, &body).await?;
    let response = if matches!(first.status().as_u16(), 400 | 422) {
        let status = first.status();
        let retry_after = retry_after_seconds(&first);
        let text = first.text().await.unwrap_or_default();
        let lower = text.to_ascii_lowercase();
        // A validation rejection over an optional field we added is recoverable:
        // drop the offending field(s) and retry once, rather than carry a
        // provider-specific compatibility table. A rejected request generated
        // no content, so the retry is safe. The body is our own typed object;
        // a non-object can't be repaired, so it falls through to the error.
        let repaired = body.as_object_mut().map(|object| {
            let mut changed = false;
            // Usage is useful but not essential; strict OpenAI-compatible
            // gateways commonly name it in the validation error.
            if lower.contains("stream_options") && object.remove("stream_options").is_some() {
                changed = true;
            }
            // The declared effort level is wrong for this backend (an
            // unadvertised set, a stale override). Drop the knob so the model
            // runs at its own default instead of hard-failing, and remember so
            // we stop re-sending it. The error names the field, or — as the Go
            // gateway does for GLM — says the model's thinking can't be tuned.
            if object.contains_key("reasoning_effort")
                && (lower.contains("reasoning") || lower.contains("thinking"))
            {
                object.remove("reasoning_effort");
                mark_reasoning_rejected(&request.model);
                changed = true;
            }
            changed
        });
        if repaired == Some(true) {
            require_success(send(request, authorization, &body).await?).await?
        } else {
            return Err(ProviderError::from_status(status, &text).with_retry_after(retry_after));
        }
    } else {
        require_success(first).await?
    };

    // Tool-call fragments accumulate per stream index until [DONE].
    let mut pending: BTreeMap<u64, ToolCall> = BTreeMap::new();
    let flush = |pending: &mut BTreeMap<u64, ToolCall>, tx: &mpsc::Sender<Event>| {
        let calls: Vec<(u64, ToolCall)> = std::mem::take(pending).into_iter().collect();
        let tx = tx.clone();
        async move {
            for (index, call) in calls {
                if !call.name.is_empty() {
                    let _ = tx
                        .send(Event::ToolCallEnd {
                            key: index.to_string(),
                        })
                        .await;
                    let _ = tx.send(Event::ToolCall(call)).await;
                }
            }
        }
    };

    let mut sse = SseStream::new(response.bytes_stream());
    let mut finish = FinishReason::Normal;
    loop {
        let payload = sse.next().await?;
        {
            if payload == "[DONE]" {
                flush(&mut pending, tx).await;
                return Ok(sse.end(finish));
            }
            let value: serde_json::Value = match serde_json::from_str(&payload) {
                Ok(v) => v,
                Err(_) => {
                    sse.malformed();
                    continue;
                }
            };
            if let Some(delta) = value["choices"][0]["delta"].as_object() {
                if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        let _ = tx.send(Event::TextDelta(text.into())).await;
                    }
                }
                for key in ["reasoning_content", "reasoning"] {
                    if let Some(text) = delta.get(key).and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            let _ = tx.send(Event::ReasoningDelta(text.into())).await;
                        }
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for fragment in calls {
                        let index = fragment["index"].as_u64().unwrap_or(0);
                        if !pending.contains_key(&index) {
                            let _ = tx
                                .send(Event::ToolCallStart {
                                    key: index.to_string(),
                                })
                                .await;
                        }
                        let entry = pending.entry(index).or_insert_with(|| ToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                            signature: None,
                        });
                        if let Some(id) = fragment["id"].as_str() {
                            entry.id = id.into();
                        }
                        if let Some(name) = fragment["function"]["name"].as_str() {
                            entry.name.push_str(name);
                        }
                        if let Some(args) = fragment["function"]["arguments"].as_str() {
                            entry.arguments.push_str(args);
                            if !args.is_empty() {
                                let _ = tx
                                    .send(Event::ToolArgumentsDelta {
                                        key: index.to_string(),
                                        delta: args.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                }
            }
            if let Some(reason) = value["choices"][0]["finish_reason"].as_str() {
                finish = match reason {
                    "stop" => FinishReason::Normal,
                    "tool_calls" => FinishReason::ToolCalls,
                    "length" => FinishReason::Length,
                    "content_filter" => FinishReason::ContentFilter,
                    other => FinishReason::Other(other.to_string()),
                };
                // A finish_reason of tool_calls closes the accumulation early.
                if finish == FinishReason::ToolCalls {
                    flush(&mut pending, tx).await;
                }
            }
            if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
                let cached = usage["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                let _ = tx
                    .send(Event::Usage {
                        input: usage["prompt_tokens"].as_u64().unwrap_or(0),
                        output: usage["completion_tokens"].as_u64().unwrap_or(0),
                        cache_read: cached,
                    })
                    .await;
            }
        }
    }
}

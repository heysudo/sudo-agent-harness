//! Orchestrator integration tests against a stub Cerebras endpoint.
//!
//! A real HTTP server is stood up on loopback rather than mocking the client, so the
//! SSE decoding, streaming tool-call reassembly and round accounting are all
//! exercised end to end. No network access and no API keys are required.

use hermit::config::Config;
use hermit::llm::CerebrasClient;
use hermit::memory::{Store, prompt::Layers};
use hermit::metrics::TurnTimings;
use hermit::music::{MpvClient, MusicController};
use hermit::orchestrator::{Orchestrator, TurnEvent};
use hermit::tools::{ToolContext, research};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One scripted response from the stub model.
#[derive(Clone)]
enum Scripted {
    /// Emit these text tokens, then finish.
    Text(Vec<&'static str>),
    /// Emit these tool calls: (id, name, arguments).
    Tools(Vec<(&'static str, &'static str, &'static str)>),
}

/// Serve scripted SSE responses in order, counting requests.
async fn stub_llm(script: Vec<Scripted>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_task = calls.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let script = script.clone();
            let calls = calls_for_task.clone();
            tokio::spawn(async move {
                // Read the request (headers + body); we only need to know it arrived.
                let mut buf = vec![0u8; 65536];
                let _ = sock.read(&mut buf).await;

                let n = calls.fetch_add(1, Ordering::SeqCst);
                let step = script
                    .get(n)
                    .cloned()
                    .unwrap_or(Scripted::Text(vec!["done."]));

                let mut body = String::new();
                match step {
                    Scripted::Text(tokens) => {
                        for t in tokens {
                            body.push_str(&format!(
                                "data: {}\n\n",
                                serde_json::json!({
                                    "choices":[{"delta":{"content": t}}]
                                })
                            ));
                        }
                        body.push_str(&format!(
                            "data: {}\n\n",
                            serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
                        ));
                    }
                    Scripted::Tools(calls) => {
                        // Split arguments across frames, the way the real API does.
                        for (i, (id, name, args)) in calls.iter().enumerate() {
                            body.push_str(&format!(
                                "data: {}\n\n",
                                serde_json::json!({"choices":[{"delta":{"tool_calls":[
                                    {"index": i, "id": id, "function": {"name": name, "arguments": ""}}
                                ]}}]})
                            ));
                            let mid = args.len() / 2;
                            for part in [&args[..mid], &args[mid..]] {
                                body.push_str(&format!(
                                    "data: {}\n\n",
                                    serde_json::json!({"choices":[{"delta":{"tool_calls":[
                                        {"index": i, "function": {"arguments": part}}
                                    ]}}]})
                                ));
                            }
                        }
                        body.push_str(&format!(
                            "data: {}\n\n",
                            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})
                        ));
                    }
                }
                body.push_str("data: [DONE]\n\n");

                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });

    (format!("http://{addr}"), calls)
}

struct Harness {
    orch: Orchestrator,
    cfg: Config,
    calls: Arc<AtomicUsize>,
    _tmp: tempfile::TempDir,
}

async fn harness(script: Vec<Scripted>) -> Harness {
    let (base_url, calls) = stub_llm(script).await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("identity.md"), "You are Hermit.").unwrap();

    let mut cfg = Config::default();
    cfg.llm.base_url = base_url.clone();
    cfg.paths.config_dir = tmp.path().to_path_buf();
    cfg.paths.data_dir = tmp.path().to_path_buf();

    let cfg = Arc::new(cfg);
    let http = hermit::http::build_client().unwrap();
    let llm = Arc::new(CerebrasClient::new(
        http.clone(),
        &base_url,
        "test-key",
        "gpt-oss-120b",
        Duration::from_secs(10),
    ));

    let store = Arc::new(Store::open_in_memory().unwrap());
    let layers = Arc::new(Layers::load(tmp.path(), tmp.path(), 600));
    let music = Arc::new(MusicController::new(
        MpvClient::new(tmp.path().join("mpv.sock")),
        None,
        Default::default(),
        70,
        -12.0,
    ));
    let (research_tx, _research_rx) = tokio::sync::mpsc::channel(research::QUEUE_DEPTH);

    let tools = ToolContext {
        cfg: cfg.clone(),
        // No API keys: web_search/fetch_page report a clean error to the model,
        // which is exactly what we want to observe being fed back.
        search: None,
        fetch: None,
        http,
        llm: llm.clone(),
        music,
        research: research_tx,
        news_style: Arc::new(String::new()),
    };

    Harness {
        orch: Orchestrator {
            llm,
            tools,
            store,
            layers,
        },
        cfg: (*cfg).clone(),
        calls,
        _tmp: tmp,
    }
}

fn collect(rx: &mut tokio::sync::mpsc::UnboundedReceiver<TurnEvent>) -> Vec<TurnEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn plain_answer_streams_tokens_and_makes_one_llm_call() {
    let h = harness(vec![Scripted::Text(vec![
        "High tide ",
        "in Bergen ",
        "is at ",
        "twenty past two.",
    ])])
    .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timings = TurnTimings::new(1);
    let answer = h
        .orch
        .run_turn(&h.cfg, "when is high tide", &tx, &mut timings, None)
        .await
        .unwrap();
    drop(tx);

    assert_eq!(answer, "High tide in Bergen is at twenty past two.");
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        1,
        "no tools => exactly one call"
    );
    assert_eq!(timings.tool_rounds, 0);
    assert!(timings.ttft_ms.is_some(), "TTFT must be recorded");

    let events = collect(&mut rx);
    assert!(events.iter().any(|e| matches!(e, TurnEvent::Token(_))));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::SpeechChunk(_))),
        "the answer must reach the speech path"
    );
    assert!(
        !events.iter().any(|e| matches!(e, TurnEvent::Ack)),
        "no acknowledgment should play when no tool runs"
    );
}

#[tokio::test]
async fn tool_round_plays_an_acknowledgment_then_answers() {
    let h = harness(vec![
        Scripted::Tools(vec![("c1", "web_search", r#"{"query":"tide bergen"}"#)]),
        Scripted::Text(vec!["Twenty ", "past two."]),
    ])
    .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timings = TurnTimings::new(1);
    let answer = h
        .orch
        .run_turn(
            &h.cfg,
            "when is high tide in bergen",
            &tx,
            &mut timings,
            None,
        )
        .await
        .unwrap();
    drop(tx);

    assert_eq!(answer, "Twenty past two.");
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        2,
        "one tool round => two calls"
    );
    assert_eq!(timings.tool_rounds, 1);

    let events = collect(&mut rx);
    assert!(
        events.iter().any(|e| matches!(e, TurnEvent::Ack)),
        "the user must hear something the instant a tool round starts"
    );
    assert!(events.iter().any(
        |e| matches!(e, TurnEvent::ToolRound(names) if names.contains(&"web_search".to_string()))
    ));
    assert_eq!(timings.tool_ms.len(), 1, "tool duration must be recorded");
}

#[tokio::test]
async fn parallel_tool_calls_all_execute_in_one_round() {
    let h = harness(vec![
        Scripted::Tools(vec![
            ("c1", "web_search", r#"{"query":"tide bergen"}"#),
            ("c2", "web_search", r#"{"query":"weather bergen"}"#),
            ("c3", "music", r#"{"action":"status"}"#),
        ]),
        Scripted::Text(vec!["All done."]),
    ])
    .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timings = TurnTimings::new(1);
    h.orch
        .run_turn(
            &h.cfg,
            "tide and weather in bergen",
            &tx,
            &mut timings,
            None,
        )
        .await
        .unwrap();
    drop(tx);

    assert_eq!(
        timings.tool_rounds, 1,
        "three calls are ONE round, not three"
    );
    assert_eq!(timings.tool_ms.len(), 3, "every call must run");
    let events = collect(&mut rx);
    let round = events
        .iter()
        .find_map(|e| match e {
            TurnEvent::ToolRound(n) => Some(n.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(round.len(), 3);
}

#[tokio::test]
async fn interactive_tool_rounds_are_hard_capped_at_two() {
    // The model asks for tools every single time. The cap must still hold, and the
    // final round must be forced to answer.
    let h = harness(vec![
        Scripted::Tools(vec![("c1", "web_search", r#"{"query":"a"}"#)]),
        Scripted::Tools(vec![("c2", "web_search", r#"{"query":"b"}"#)]),
        Scripted::Tools(vec![("c3", "web_search", r#"{"query":"c"}"#)]),
        Scripted::Tools(vec![("c4", "web_search", r#"{"query":"d"}"#)]),
    ])
    .await;

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timings = TurnTimings::new(1);
    h.orch
        .run_turn(&h.cfg, "keep digging forever", &tx, &mut timings, None)
        .await
        .unwrap();

    assert_eq!(
        timings.tool_rounds, 2,
        "spec §4.3: at most two interactive tool rounds"
    );
    // Two tool rounds plus the forced final answer = three LLM calls, never more.
    assert_eq!(h.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn tool_errors_are_fed_back_rather_than_failing_the_turn() {
    // search is None in the harness, so web_search returns a tool error.
    let h = harness(vec![
        Scripted::Tools(vec![("c1", "web_search", r#"{"query":"anything"}"#)]),
        Scripted::Text(vec!["I couldn't look that up."]),
    ])
    .await;

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timings = TurnTimings::new(1);
    let answer = h
        .orch
        .run_turn(&h.cfg, "look something up", &tx, &mut timings, None)
        .await
        .expect("a failing tool must not abort the turn");

    assert_eq!(answer, "I couldn't look that up.");
    assert_eq!(timings.tool_rounds, 1);
}

#[tokio::test]
async fn unknown_tool_names_are_rejected_and_reported() {
    let h = harness(vec![
        Scripted::Tools(vec![("c1", "delete_everything", r#"{}"#)]),
        Scripted::Text(vec!["I can't do that."]),
    ])
    .await;

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut timings = TurnTimings::new(1);
    let answer = h
        .orch
        .run_turn(&h.cfg, "delete everything", &tx, &mut timings, None)
        .await
        .unwrap();

    assert_eq!(answer, "I can't do that.");
    assert_eq!(timings.tool_ms.len(), 1);
    assert_eq!(timings.tool_ms[0].0, "delete_everything");
}

#[tokio::test]
async fn local_harness_overhead_stays_inside_the_fifteen_millisecond_gate() {
    let h = harness(vec![Scripted::Text(vec!["ok."])]).await;

    // Seed a realistic archive so recall is doing real work.
    let batch = hermit::reflect::parse_extraction(
        r#"{"facts":[
            {"text":"the user lives in Bergen","importance":0.9},
            {"text":"the user prefers metric units","importance":0.8},
            {"text":"the user's dog is named Ada","importance":0.7}
        ]}"#,
    )
    .unwrap();
    h.orch.store.apply_reflection(&batch, 0.8).unwrap();

    // Warm caches, then measure.
    for _ in 0..3 {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut t = TurnTimings::new(0);
        let _ = h
            .orch
            .run_turn(&h.cfg, "what units do I use", &tx, &mut t, None)
            .await;
    }

    let mut overheads = Vec::new();
    for _ in 0..10 {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut t = TurnTimings::new(0);
        h.orch
            .run_turn(&h.cfg, "what units do I use", &tx, &mut t, None)
            .await
            .unwrap();
        overheads.push(t.local_overhead_ms());
    }
    overheads.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = overheads[overheads.len() / 2];

    assert!(
        p50 < 15.0,
        "local overhead p50 was {p50:.2}ms (recall + assemble); gate is 15ms"
    );
}

#[tokio::test]
async fn a_turn_records_nothing_to_memory_by_itself() {
    // The orchestrator must not write memory; only the gateway records messages and
    // only reflection writes facts. This keeps the firewall's surface small.
    let h = harness(vec![Scripted::Text(vec!["hello."])]).await;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut t = TurnTimings::new(0);
    h.orch
        .run_turn(&h.cfg, "hi", &tx, &mut t, None)
        .await
        .unwrap();

    assert_eq!(h.orch.store.fact_count(), 0);
    assert!(h.orch.store.recent_messages(10).is_empty());
}

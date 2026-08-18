//! stdin/stdout front end.
//!
//! Useful for bring-up and for `scripts/bench.sh`, which drives the daemon through
//! this interface and parses the `hermit_timing` lines it emits.

use super::Gateway;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The operator explicitly asked the daemon to stop.
    Quit,
    /// stdin disappeared (normal under systemd); only the CLI front end is gone.
    Eof,
}

/// Read lines from stdin, answer each, stream tokens to stdout.
pub async fn run(gateway: Arc<Gateway>) -> Exit {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    let _ = stdout
        .write_all(b"hermit ready. type a question, /listen to speak, or /quit\n> ")
        .await;
    let _ = stdout.flush().await;

    let exit = loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => break Exit::Eof,
            Err(e) => {
                tracing::warn!(error = %e, "stdin read failed");
                break Exit::Eof;
            }
        };

        let input = line.trim();
        if input.is_empty() {
            let _ = stdout.write_all(b"> ").await;
            let _ = stdout.flush().await;
            continue;
        }
        if matches!(input, "/quit" | "/exit") {
            break Exit::Quit;
        }
        // Start a voice turn without the wake word. Useful when no Picovoice key is
        // configured, and for testing the microphone path on demand.
        if matches!(input, "/listen") {
            let ok = gateway.trigger_listen();
            let msg: &[u8] = if ok {
                b"listening... speak now\n> "
            } else {
                b"voice pipeline is not running (no microphone, or built without ALSA)\n> "
            };
            let _ = stdout.write_all(msg).await;
            let _ = stdout.flush().await;
            continue;
        }
        // `/say` speaks the answer too; plain input is text-only so the bench
        // harness is not gated on audio hardware.
        let (utterance, speak) = match input.strip_prefix("/say ") {
            Some(rest) => (rest.trim(), true),
            None => (input, false),
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let printer = tokio::spawn(async move {
            let mut out = tokio::io::stdout();
            while let Some(tok) = rx.recv().await {
                let _ = out.write_all(tok.as_bytes()).await;
                let _ = out.flush().await;
            }
        });

        match gateway.handle(utterance, speak, Some(tx), None).await {
            Ok(_) => {}
            Err(e) => tracing::error!(error = ?e, "turn failed"),
        }
        let _ = printer.await;

        let _ = stdout.write_all(b"\n> ").await;
        let _ = stdout.flush().await;
    };

    tracing::info!(?exit, "cli front end closed");
    exit
}

//! HERMIT — ultra-low-latency voice agent daemon.
//!
//! Two subcommands:
//!   `hermit run`         — the daemon (systemd `hermit.service`)
//!   `hermit consolidate` — one-shot nightly memory consolidation (systemd timer)

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hermit::{
    audio::AudioPlayer,
    config::{self},
    gateway::Gateway,
    http,
    llm::CerebrasClient,
    memory::{Store, prompt::Layers},
    music::{MpvClient, MusicController, SpotifyClient},
    orchestrator::Orchestrator,
    reflect::{ReflectSignal, ReflectionWorker},
    speech::{acks::AckBank, tts::Tts, wake},
    tools::{self, ToolContext, research},
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "hermit", version, about = "Voice agent harness for Raspberry Pi")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon.
    Run {
        #[arg(long, default_value = "/opt/hermit/config/hermit.toml")]
        config: PathBuf,
    },
    /// Run memory consolidation once and exit.
    Consolidate {
        #[arg(long, default_value = "/opt/hermit/config/hermit.toml")]
        config: PathBuf,
    },
    /// Validate the config file and exit.
    Check {
        #[arg(long, default_value = "/opt/hermit/config/hermit.toml")]
        config: PathBuf,
    },
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    // A two-worker runtime is right for a 4-core Pi shared with librespot and mpv;
    // the audio threads are separate OS threads anyway.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    match cli.command {
        Command::Run { config } => runtime.block_on(run(config)),
        Command::Consolidate { config } => runtime.block_on(consolidate(config)),
        Command::Check { config } => {
            let cfg = config::load(&config)?;
            println!("config OK: {}", cfg.source_path.display());
            println!("  model            {}", cfg.llm.model);
            println!("  search mode      {}", cfg.search.mode);
            println!("  tool rounds      {}", cfg.llm.max_tool_rounds);
            println!("  playback pcm     {}", cfg.audio.playback_pcm);
            println!("  capture pcm      {}", cfg.audio.capture_pcm);
            println!("  sample rate      {}", cfg.audio.sample_rate);
            println!("  core token cap   {}", cfg.memory.core_token_cap);
            Ok(())
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env("HERMIT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,hermit=info,hermit_timing=info"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(false) // journald, not a terminal
        // stderr, so the CLI front end's answers on stdout stay clean and
        // scripts/bench.sh can parse timing lines without stripping prose.
        .with_writer(std::io::stderr)
        .init();
}

/// Wire everything up and serve.
async fn run(config_path: PathBuf) -> Result<()> {
    let cfg = config::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    let cfg = Arc::new(cfg);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        "hermit starting"
    );

    // Hot-reloadable config (spec §3: iterate without a rebuild).
    let (cfg_tx, cfg_rx) = tokio::sync::watch::channel(cfg.clone());
    if let Err(e) = config::watch(config_path.clone(), cfg_tx) {
        tracing::warn!(error = %e, "config hot reload unavailable");
    }

    // ---- HTTP + upstreams -------------------------------------------------
    let http_client = http::build_client()?;
    let llm = Arc::new(CerebrasClient::new(
        http_client.clone(),
        &cfg.llm.base_url,
        http::secret("CEREBRAS_API_KEY")?,
        &cfg.llm.model,
        cfg.llm_timeout(),
    ));

    let search = http::secret_opt("PARALLEL_API_KEY").map(|k| {
        Arc::new(tools::search::SearchClient::new(http_client.clone(), &cfg.search, k))
    });
    if search.is_none() {
        tracing::warn!("PARALLEL_API_KEY not set; web_search will return an error to the model");
    }
    let fetch = http::secret_opt("FIRECRAWL_API_KEY")
        .map(|k| Arc::new(tools::fetch::FetchClient::new(http_client.clone(), &cfg.fetch, k)));

    // ---- storage + prompt layers -----------------------------------------
    let store = Arc::new(Store::open(&cfg.paths.data_dir)?);
    let layers = Arc::new(Layers::load(
        &cfg.config_dir(),
        &cfg.paths.data_dir,
        cfg.memory.core_token_cap,
    ));
    let skills_dir = cfg.config_dir().join("skills");
    if let Err(e) = store.reindex_skills(&skills_dir) {
        tracing::warn!(error = %e, "skill indexing failed");
    }

    // ---- audio ------------------------------------------------------------
    let player = AudioPlayer::spawn(&cfg.audio)?;
    let tts = Arc::new(Tts::from_config(&cfg));

    // ---- music ------------------------------------------------------------
    let stations = MusicController::load_stations(&cfg.resolve(&cfg.music.stations_file));
    let spotify = SpotifyClient::from_env(
        http_client.clone(),
        &cfg.music.spotify_api_base,
        &cfg.music.librespot_device_name,
    );
    if spotify.is_none() {
        tracing::info!("Spotify not configured; radio and local commands still work");
    }
    let music = Arc::new(MusicController::new(
        MpvClient::new(&cfg.music.mpv_socket),
        spotify.clone(),
        stations,
        cfg.music.default_volume,
        cfg.audio.duck_db,
    ));

    // ---- prompts ----------------------------------------------------------
    let prompts = cfg.config_dir().join("prompts");
    let news_style = Arc::new(read_prompt(&prompts, "news_briefing.md"));
    let research_prompt = Arc::new(read_prompt(&prompts, "research.md"));
    let extract_prompt = Arc::new(read_prompt(&prompts, "reflect_extract.md"));
    let skill_prompt = Arc::new(read_prompt(&prompts, "reflect_skill.md"));
    let consolidate_prompt = Arc::new(read_prompt(&prompts, "consolidate.md"));

    // ---- tools + research worker -----------------------------------------
    let (research_tx, research_rx) =
        tokio::sync::mpsc::channel::<research::ResearchJob>(research::QUEUE_DEPTH);
    let (announce_tx, mut announce_rx) =
        tokio::sync::mpsc::channel::<research::Announcement>(research::QUEUE_DEPTH);

    let tool_ctx = ToolContext {
        cfg: cfg.clone(),
        search,
        fetch,
        http: http_client.clone(),
        llm: llm.clone(),
        music: music.clone(),
        research: research_tx,
        news_style,
    };

    let researcher = research::ResearchWorker::new(
        tool_ctx.clone(),
        research_prompt,
        cfg.research.max_rounds,
        cfg.research.timeout_secs,
    );
    tokio::spawn(researcher.run(research_rx, announce_tx));

    // ---- reflection worker ------------------------------------------------
    let (reflect_tx, reflect_rx) = tokio::sync::mpsc::channel::<ReflectSignal>(32);
    let reflection = ReflectionWorker {
        llm: llm.clone(),
        store: store.clone(),
        layers: layers.clone(),
        cfg_rx: cfg_rx.clone(),
        extract_prompt,
        skill_prompt,
        consolidate_prompt,
    };
    tokio::spawn(reflection.run(reflect_rx));

    // ---- orchestrator + gateway ------------------------------------------
    let orch = Arc::new(Orchestrator {
        llm: llm.clone(),
        tools: tool_ctx.clone(),
        store: store.clone(),
        layers: layers.clone(),
    });

    // Pre-warm before anything can ask a question (spec §5).
    http::prewarm(&http_client, &http::hot_endpoints(&cfg)).await;
    tts.prewarm().await;
    if let Some(sp) = &spotify {
        sp.prewarm().await;
    }
    let acks = Arc::new(AckBank::load_or_build(&cfg.paths.ack_dir, &tts, &player).await);

    let gateway = Arc::new(Gateway::new(
        cfg_rx.clone(),
        orch,
        store.clone(),
        music.clone(),
        tts.clone(),
        player.clone(),
        acks,
        reflect_tx.clone(),
    ));

    // ---- background upkeep ------------------------------------------------
    tokio::spawn(http::prewarm_loop(
        http_client.clone(),
        cfg_rx.clone(),
        Duration::from_secs(240),
    ));
    tokio::spawn(hermit::metrics::rss_watchdog(100.0));

    // Deliver finished background research.
    {
        let gw = gateway.clone();
        tokio::spawn(async move {
            while let Some(a) = announce_rx.recv().await {
                gw.announce(a).await;
            }
        });
    }

    // ---- front ends -------------------------------------------------------
    if cfg.wake.enabled || cfg.stt.url.is_empty() {
        let detector = wake::build(&cfg.wake);
        match hermit::gateway::voice::spawn_capture(&cfg.audio) {
            Ok(mic_rx) => {
                let gw = gateway.clone();
                tokio::spawn(hermit::gateway::voice::run(gw, mic_rx, detector));
            }
            Err(e) => tracing::error!(error = %e, "microphone capture unavailable; voice disabled"),
        }
    }

    if !cfg.server.ws_bind.is_empty() {
        let gw = gateway.clone();
        let bind = cfg.server.ws_bind.clone();
        tokio::spawn(async move {
            if let Err(e) = hermit::gateway::ws::run(gw, bind).await {
                tracing::error!(error = %e, "websocket gateway stopped");
            }
        });
    }

    notify_ready();
    tokio::spawn(watchdog_loop());
    tracing::info!("hermit ready");

    if cfg.server.cli {
        hermit::gateway::cli::run(gateway.clone()).await;
    } else {
        shutdown_signal().await;
    }

    tracing::info!("shutting down");
    player.stop().await;
    Ok(())
}

/// One-shot consolidation (systemd timer at 04:00).
async fn consolidate(config_path: PathBuf) -> Result<()> {
    let cfg = Arc::new(config::load(&config_path)?);
    tracing::info!("running one-shot consolidation");

    let http_client = http::build_client()?;
    let llm = Arc::new(CerebrasClient::new(
        http_client,
        &cfg.llm.base_url,
        http::secret("CEREBRAS_API_KEY")?,
        &cfg.llm.model,
        cfg.llm_timeout(),
    ));
    let store = Arc::new(Store::open(&cfg.paths.data_dir)?);
    let layers = Arc::new(Layers::load(
        &cfg.config_dir(),
        &cfg.paths.data_dir,
        cfg.memory.core_token_cap,
    ));
    let prompts = cfg.config_dir().join("prompts");
    let (_tx, cfg_rx) = tokio::sync::watch::channel(cfg.clone());

    let worker = ReflectionWorker {
        llm,
        store,
        layers,
        cfg_rx,
        extract_prompt: Arc::new(read_prompt(&prompts, "reflect_extract.md")),
        skill_prompt: Arc::new(read_prompt(&prompts, "reflect_skill.md")),
        consolidate_prompt: Arc::new(read_prompt(&prompts, "consolidate.md")),
    };
    worker.consolidate(&cfg).await
}

fn read_prompt(dir: &std::path::Path, name: &str) -> String {
    let path = dir.join(name);
    match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "prompt file missing; using built-in default");
            String::new()
        }
    }
}

// ---------------------------------------------------------------------------
// systemd integration
// ---------------------------------------------------------------------------

#[cfg(feature = "systemd")]
fn notify_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed (not running under systemd?)");
    }
}

#[cfg(not(feature = "systemd"))]
fn notify_ready() {}

/// Ping the watchdog at half the configured interval.
///
/// `hermit.service` sets `WatchdogSec=30`; systemd expects pings well inside that,
/// so 10 s leaves room for a slow tick without a spurious restart.
#[cfg(feature = "systemd")]
async fn watchdog_loop() {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
    }
}

#[cfg(not(feature = "systemd"))]
async fn watchdog_loop() {}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

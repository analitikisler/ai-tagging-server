mod admin;
mod ai;
mod alerts;
mod checkup;
mod container;
mod container_file;
mod counts;
mod drafts;
mod eventlog;
mod features;
mod google;
mod history;
mod integrations;
mod metrics;
mod mpv1;
mod mpv2;
mod permissions;
mod replay;
mod request;
mod sandbox;
mod server;
mod state;
mod store;
mod tenants;
mod traces;
mod transform;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use container::{Container, Settings};
use container_file::ContainerFile;
use integrations::Integrations;
use sandbox::{Engine, Storage};
use server::{App, HttpTransport, Pool, Target};
use tenants::{Resolved, TenantStats, Tenants};

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    // Plain text when not on a terminal, so Cloud Logging shows no color codes.
    tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let port: u16 = env("PORT", 8080);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // A worker waits for its hit's outgoing sends, so most of its time is
    // spent idle on the network rather than on the CPU.
    let workers: usize = env("WORKERS", cpus * 8);
    let budget = Duration::from_millis(env("RUN_TIMEOUT_MS", 5000u64).min(30_000));
    let memory = env("JS_MEMORY_MB", 64usize).clamp(4, 512) * 1024 * 1024;
    let allow_private = env("OUTGOING_ALLOW_PRIVATE", 0u8) == 1;
    let debug_token = secret_env("DEBUG_TOKEN").filter(|t| t.len() >= 16);
    let proxy_secret = secret_env("PROXY_SECRET");
    if proxy_secret.as_ref().is_some_and(|s| s.len() < 16) {
        eprintln!("PROXY_SECRET must be at least 16 characters");
        std::process::exit(1);
    }
    if proxy_secret.is_none() {
        tracing::warn!(
            "PROXY_SECRET is not set: the visitor IP sent to vendors comes from \
             cf-connecting-ip, x-real-ip or x-forwarded-for, which clients can fake \
             unless a proxy in front of this server overwrites them"
        );
    }

    let tenants_file = std::env::var("TENANTS_FILE").ok().filter(|p| !p.is_empty());
    let admin_token = secret_env("ADMIN_TOKEN");
    if tenants_file.is_some() {
        for var in [
            "CONTAINER_FILE",
            "SCRIPT_IDS",
            "MEASUREMENT_IDS",
            "SITE_DOMAINS",
        ] {
            if std::env::var(var).is_ok_and(|v| !v.is_empty()) {
                eprintln!(
                    "{var} can't be combined with TENANTS_FILE: each tenant carries its own settings"
                );
                std::process::exit(1);
            }
        }
    }
    let integrations_file = std::env::var("INTEGRATIONS_FILE")
        .ok()
        .filter(|p| !p.is_empty());
    if integrations_file.is_some() && tenants_file.is_some() {
        eprintln!("INTEGRATIONS_FILE can't be combined with TENANTS_FILE yet");
        std::process::exit(1);
    }
    if admin_token.as_ref().is_some_and(|t| t.len() < 32) {
        eprintln!("ADMIN_TOKEN must be at least 32 characters");
        std::process::exit(1);
    }
    let storage: Storage = Default::default();
    let (target, single) = match &tenants_file {
        Some(path) => {
            let tenants = Tenants::open(&PathBuf::from(path)).unwrap_or_else(|e| {
                eprintln!("tenants: {e}");
                std::process::exit(1);
            });
            // Pick up edits other instances (or a person) made to the file.
            let watched = tenants.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tick.tick().await;
                    watched.reload_if_changed();
                }
            });
            (Target::Multi(tenants), None)
        }
        None => {
            let stats = Arc::new(TenantStats::default());
            let base = match std::env::var("CONTAINER_FILE") {
                Ok(path) => {
                    let file = ContainerFile::open(&PathBuf::from(path), storage.clone(), stats)
                        .unwrap_or_else(|e| {
                            eprintln!("container: {e}");
                            std::process::exit(1);
                        });
                    // Pick up saves from the admin page on other instances, and
                    // edits made to the file by hand.
                    let watched = file.clone();
                    tokio::spawn(async move {
                        let mut tick = tokio::time::interval(Duration::from_secs(3));
                        loop {
                            tick.tick().await;
                            watched.reload_if_changed();
                        }
                    });
                    file
                }
                Err(_) => {
                    // Comma-separated container and tag ids whose scripts this server
                    // serves, e.g. GTM-ABC123,G-XYZ789.
                    let script_ids = list_env("SCRIPT_IDS");
                    // GA4 properties hits are accepted for; by default the G- ids above.
                    // Without any, the server refuses to start rather than relay hits
                    // for every property; MEASUREMENT_IDS=* accepts any on purpose.
                    let mut measurement_ids = list_env("MEASUREMENT_IDS");
                    if measurement_ids.is_empty() {
                        measurement_ids = script_ids
                            .iter()
                            .filter(|id| id.starts_with("G-"))
                            .cloned()
                            .collect();
                    }
                    if measurement_ids.is_empty() {
                        eprintln!(
                            "set MEASUREMENT_IDS (or a G- id in SCRIPT_IDS) to the GA4 ids to accept, or MEASUREMENT_IDS=* to accept any"
                        );
                        std::process::exit(1);
                    }
                    // Domains whose pages may send hits; by default the request's host.
                    let site_domains = list_env("SITE_DOMAINS");
                    let origin_check =
                        std::env::var("ORIGIN_CHECK").unwrap_or_else(|_| "log".into());
                    if !matches!(origin_check.as_str(), "enforce" | "log" | "off") {
                        eprintln!("ORIGIN_CHECK must be enforce, log or off");
                        std::process::exit(1);
                    }
                    let features = features::Features::from_env(|k| std::env::var(k).ok())
                        .unwrap_or_else(|e| {
                            eprintln!("{e}");
                            std::process::exit(1);
                        });
                    let container = Container::builtin_with(&Settings {
                        script_ids,
                        measurement_ids,
                        site_domains,
                        origin_check,
                        strip_user_data: env("STRIP_USER_DATA", 0u8) == 1,
                        features,
                    });
                    ContainerFile::fixed(container, storage.clone(), stats)
                }
            };
            if let Some(path) = &integrations_file {
                let integrations =
                    Integrations::open(&PathBuf::from(path), base).unwrap_or_else(|e| {
                        eprintln!("integrations: {e}");
                        std::process::exit(1);
                    });
                let watched = integrations.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_secs(5));
                    loop {
                        tick.tick().await;
                        watched.reload_if_changed();
                    }
                });
                let resolved = integrations.resolved();
                (Target::Integrated(integrations), Some(resolved))
            } else {
                let resolved = base.resolved();
                (Target::Single(base), Some(resolved))
            }
        }
    };
    let google = google_auth().unwrap_or_else(|e| {
        eprintln!("GOOGLE_AUTH: {e}");
        std::process::exit(1);
    });
    if let Some(g) = &google {
        tracing::info!("BigQuery inserts use {}", g.describe());
    }
    let transport = Arc::new(
        HttpTransport::new(
            tokio::runtime::Handle::current(),
            allow_private,
            env("SEND_RETRIES", 1u32).min(5),
        )
        .with_google(google.map(Arc::new)),
    );
    metrics::init();

    // A dry preview of a draft sends nothing.
    let dry: Arc<dyn sandbox::Transport> = Arc::new(replay::Dry);
    let pool = Pool::new(workers, workers * 128, move |t: &Resolved| {
        let transport: Arc<dyn sandbox::Transport> = if t.id == "preview-dry" {
            dry.clone()
        } else {
            transport.clone()
        };
        Engine::new(
            t.container.clone(),
            t.storage.clone(),
            transport,
            budget,
            memory,
        )
        .expect("javascript runtime")
    });

    // Recent hits for the console's debugger, kept only while the admin page is on.
    let trace_capacity = env("TRACE_BUFFER", traces::DEFAULT_CAPACITY);
    if trace_capacity > traces::MAX_CAPACITY {
        tracing::warn!(
            "TRACE_BUFFER {trace_capacity} is more than {}; keeping {0}",
            traces::MAX_CAPACITY
        );
    }
    // Counts, history, alerts and pinned hits: shared by every instance with
    // STATE_DIR, else kept in this process only.
    let state_dir = std::env::var("STATE_DIR").ok().filter(|p| !p.is_empty());
    let state = Arc::new(match &state_dir {
        Some(p) => state::State::dir(&PathBuf::from(p)).unwrap_or_else(|e| {
            eprintln!("STATE_DIR {p}: {e}");
            std::process::exit(1);
        }),
        None => state::State::memory(),
    });
    let history = Arc::new(history::History::new(state.clone()));
    let counts = admin_token
        .is_some()
        .then(|| Arc::new(counts::Recorder::new(state.clone())));

    // Alerts when tracking breaks: posted to Slack, shown in the console.
    let slack = secret_env("SLACK_WEBHOOK_URL");
    if slack.as_ref().is_some_and(|u| !u.starts_with("https://")) {
        eprintln!("SLACK_WEBHOOK_URL must be an https:// address");
        std::process::exit(1);
    }
    let check_token = secret_env("CHECK_TOKEN");
    if check_token.as_ref().is_some_and(|t| t.len() < 16) {
        eprintln!("CHECK_TOKEN must be at least 16 characters");
        std::process::exit(1);
    }
    let alerts = Arc::new(alerts::Alerts::new(
        state.clone(),
        history.clone(),
        counts.clone(),
        slack.clone(),
        std::env::var("ADMIN_URL")
            .ok()
            .filter(|u| !u.trim().is_empty()),
    ));

    // The AI behind the console: only with the console, and never in the hit path.
    let ai_key = secret_env("ANTHROPIC_API_KEY");
    if ai_key.is_some() && admin_token.is_none() {
        tracing::warn!("ANTHROPIC_API_KEY is set but ADMIN_TOKEN isn't; the AI stays off");
    }
    let ai = ai_key.filter(|_| admin_token.is_some()).map(|key| {
        Arc::new(ai::Ai::new(
            key,
            std::env::var("AI_MODEL")
                .ok()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| ai::DEFAULT_MODEL.into()),
            std::env::var("AI_BASE_URL")
                .ok()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| ai::DEFAULT_BASE_URL.into()),
            env("AI_DAILY_LIMIT", ai::DEFAULT_DAILY_LIMIT),
        ))
    });
    let pool = Arc::new(pool);
    let log = Arc::new(eventlog::EventLog::default());
    let admin = admin_token.map(|token| admin::Admin {
        tenants: match &target {
            Target::Multi(t) => Some(t.clone()),
            _ => None,
        },
        integrations: match &target {
            Target::Integrated(i) => Some(i.clone()),
            _ => None,
        },
        token,
        traces: Arc::new(traces::Traces::new(trace_capacity)),
        target: target.clone(),
        workers,
        ai: ai.clone(),
        state: state.clone(),
        history: history.clone(),
        alerts: alerts.clone(),
        check_token,
        pool: pool.clone(),
        log: log.clone(),
        drafts: match &target {
            Target::Single(f) => Some(f.clone()),
            Target::Integrated(i) => Some(i.base().clone()),
            Target::Multi(_) => None,
        }
        .filter(|f| f.is_file())
        .map(|f| Arc::new(drafts::Drafts::new(state.clone(), f))),
    });
    // On a server that keeps running, a timer runs the alert checks; on
    // Cloud Run, which pauses idle instances, a Cloud Scheduler job does.
    let alert_every = env("ALERT_INTERVAL_S", 300u64);
    if let Some(a) = admin.clone().filter(|_| alert_every > 0) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(alert_every.max(60)));
            tick.tick().await;
            loop {
                tick.tick().await;
                let r = admin::run_alert_check(&a).await;
                if r["opened"].as_u64().unwrap_or(0) > 0 || r["resolved"].as_u64().unwrap_or(0) > 0
                {
                    tracing::info!("alerts: {r}");
                }
            }
        });
    }
    // Only the integrations page shows the event log.
    let log = admin
        .as_ref()
        .filter(|a| a.integrations.is_some())
        .map(|_| log);
    let app = App {
        pool,
        target: target.clone(),
        admin: admin.clone(),
        debug_token: debug_token.clone(),
        proxy_secret: proxy_secret.clone(),
        counts: counts.clone(),
        log,
    };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    let serving = match (&target, &single) {
        (Target::Multi(t), _) => format!("{} tenants", t.list().len()),
        (_, Some(r)) => format!(
            "container {} ({} clients, {} tags)",
            r.container.version,
            r.container.clients.len(),
            r.container.tags.len()
        ),
        _ => String::new(),
    };
    tracing::info!(
        "ai-tagging-server {} listening on {addr}: {serving}, {workers} workers, debug {}, admin {}, AI {}, state {}, Slack {}, proxy secret {}",
        env!("CARGO_PKG_VERSION"),
        if debug_token.is_some() { "on" } else { "off" },
        match &admin {
            Some(a) if a.traces.is_on() => format!("on (traces {})", a.traces.capacity()),
            Some(_) => "on (traces off)".to_string(),
            None => "off".to_string(),
        },
        match &ai {
            Some(ai) => format!("on ({})", ai.model()),
            None => "off".to_string(),
        },
        if state_dir.is_some() {
            "shared"
        } else {
            "in memory"
        },
        if slack.is_some() { "on" } else { "off" },
        if proxy_secret.is_some() {
            "required"
        } else {
            "off"
        },
    );
    axum::serve(
        listener,
        server::router(app).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("server");
    // The quarter so far, so a stopping instance's last minute isn't lost.
    if let Some(c) = &counts {
        c.flush();
    }
}

/// The Google credentials BigQuery inserts are sent with. GOOGLE_AUTH picks
/// them: `key` reads the service account key file at
/// GOOGLE_APPLICATION_CREDENTIALS, `metadata` asks the metadata server of the
/// Cloud Run service or VM, `off` sends without. The default, `auto`, uses the
/// key file when the variable is set, else the metadata server on Cloud Run
/// (K_SERVICE is set there), else nothing.
fn google_auth() -> Result<Option<google::GoogleAuth>, String> {
    let mode = std::env::var("GOOGLE_AUTH").unwrap_or_default();
    let key_file = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .ok()
        .filter(|p| !p.is_empty());
    let on_cloud_run = std::env::var("K_SERVICE").is_ok_and(|v| !v.is_empty());
    let read_key = |path: &str| {
        let json = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        google::GoogleAuth::from_key_json(&json)
            .map(Some)
            .map_err(|e| format!("{path}: {e}"))
    };
    match mode.trim() {
        "off" => Ok(None),
        "metadata" => Ok(Some(google::GoogleAuth::metadata())),
        "key" => read_key(
            key_file
                .as_deref()
                .ok_or("key needs GOOGLE_APPLICATION_CREDENTIALS")?,
        ),
        "" | "auto" => match key_file {
            Some(path) => read_key(&path),
            None if on_cloud_run => Ok(Some(google::GoogleAuth::metadata())),
            None => Ok(None),
        },
        other => Err(format!("{other}: use auto, key, metadata or off")),
    }
}

/// Reads a comma-separated list, dropping empty entries.
fn list_env(name: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Reads a secret, ignoring surrounding whitespace such as the trailing newline
/// a secret store keeps when the value was piped in from a command.
fn secret_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Cloud Run and Kubernetes stop containers with SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

mod api;
mod config;
mod miner;
mod pubsub;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

use miner::{Miner, Streamer};

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        println!("[{}] {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), format!($($arg)*))
    };
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        log!("Error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let path = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "config.toml".into()),
    );
    let cfg = config::Config::load(&path)?;
    let api = api::Twitch::login(&cfg.username).await?;

    let mut logins = cfg.streamers.clone();
    if cfg.followers {
        let follows = api.followers().await?;
        log!("Loaded {} followed channels", follows.len());
        logins.extend(follows);
    }
    let mut seen = std::collections::HashSet::new();
    logins.retain(|l| !cfg.blacklist.contains(l) && seen.insert(l.clone()));

    log!("Loading {} streamers...", logins.len());
    let ids = api.channel_ids(&logins).await?;
    let mut streamers = Vec::new();
    for (login, id) in logins.into_iter().zip(ids) {
        match id {
            Some(channel_id) => streamers.push(Streamer::new(login, channel_id)),
            None => log!("Streamer {login} does not exist"),
        }
    }
    if streamers.is_empty() {
        anyhow::bail!("No valid streamers");
    }

    let miner = Arc::new(Miner::new(api, cfg, streamers));
    // Warm up the spade URL in the background; only watching needs it
    let m = miner.clone();
    tokio::spawn(async move { m.api.spade_url(&m.streamers[0].login).await });
    miner.start().await;

    pubsub::spawn_all(&miner);
    tokio::spawn(miner.clone().watch_loop());
    tokio::spawn(miner.clone().refresh_loop());
    if miner.cfg.claim_drops {
        tokio::spawn(miner.clone().drops_loop());
    }
    log!("Miner running. Press Ctrl+C to stop.");

    shutdown_signal().await;
    println!();
    miner.print_summary();
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;

use crate::notify::Event;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub streamers: Vec<String>,
    #[serde(default)]
    pub followers: bool,
    #[serde(default)]
    pub blacklist: Vec<String>,
    #[serde(default = "yes")]
    pub follow_raids: bool,
    #[serde(default = "yes")]
    pub claim_moments: bool,
    #[serde(default = "yes")]
    pub watch_streak: bool,
    #[serde(default = "yes")]
    pub claim_drops: bool,
    #[serde(default)]
    pub discord_webhook: Option<String>,
    #[serde(default = "Event::defaults")]
    pub discord_events: Vec<Event>,
}

fn yes() -> bool {
    true
}

/// Empty variables count as unset, so panels like Pterodactyl can leave fields blank.
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_list(name: &str, target: &mut Vec<String>) {
    if let Some(v) = env(name) {
        *target = v
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
}

fn env_bool(name: &str, target: &mut bool) -> Result<()> {
    if let Some(v) = env(name) {
        *target = match v.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => bail!("{name} must be true or false, got '{v}'"),
        };
    }
    Ok(())
}

impl Config {
    /// Reads the TOML file if it exists, then applies environment overrides.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
        };
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("Invalid config {}", path.display()))?;

        if let Some(u) = env("TWITCH_USERNAME") {
            cfg.username = u;
        }
        env_list("STREAMERS", &mut cfg.streamers);
        env_list("BLACKLIST", &mut cfg.blacklist);
        env_bool("FOLLOWERS", &mut cfg.followers)?;
        env_bool("FOLLOW_RAIDS", &mut cfg.follow_raids)?;
        env_bool("CLAIM_MOMENTS", &mut cfg.claim_moments)?;
        env_bool("WATCH_STREAK", &mut cfg.watch_streak)?;
        env_bool("CLAIM_DROPS", &mut cfg.claim_drops)?;
        if let Some(url) = env("DISCORD_WEBHOOK") {
            cfg.discord_webhook = Some(url);
        }
        if let Some(v) = env("DISCORD_EVENTS") {
            cfg.discord_events = v
                .split([',', ' '])
                .filter(|s| !s.is_empty())
                .map(|s| {
                    Event::parse(s).with_context(|| format!("Unknown DISCORD_EVENTS entry '{s}'"))
                })
                .collect::<Result<_>>()?;
        }
        cfg.discord_webhook = cfg
            .discord_webhook
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty());
        if let Some(u) = &cfg.discord_webhook
            && !u.starts_with("https://")
        {
            bail!("discord_webhook must be an https:// URL");
        }

        cfg.username = cfg.username.trim().to_lowercase();
        if cfg.username.is_empty() || cfg.username == "your-twitch-username" {
            bail!("Set 'username' in {} or TWITCH_USERNAME", path.display());
        }
        for list in [&mut cfg.streamers, &mut cfg.blacklist] {
            for s in list.iter_mut() {
                *s = s.trim().to_lowercase();
            }
        }
        if cfg.streamers.is_empty() && !cfg.followers {
            bail!("No streamers configured and 'followers' is disabled");
        }
        Ok(cfg)
    }
}

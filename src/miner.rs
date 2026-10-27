//! Streamer state and all point-earning actions.

use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::api::{self, Twitch};
use crate::config::Config;
use crate::log;

/// Twitch only credits watch time for two channels at the same time.
const MAX_WATCHING: usize = 2;

#[derive(Default)]
pub struct Streamer {
    pub login: String,
    pub channel_id: String,
    pub points: i64,
    pub start_points: i64,
    pub online: bool,
    pub online_at: Option<Instant>,
    pub offline_at: Option<Instant>,
    pub stream_up_at: Option<Instant>,
    pub broadcast_id: Option<String>,
    /// (game id, game name) of the current stream, needed for drop progress
    pub game: Option<(String, String)>,
    pub last_update: Option<Instant>,
    pub watch_streak_missing: bool,
    pub minutes_watched: f64,
    pub last_watch: Option<Instant>,
    pub last_raid: Option<String>,
    /// reason -> (count, points)
    pub history: BTreeMap<String, (u32, i64)>,
}

/// 1234567 -> "1,234,567"
pub fn fmt_points(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Human readable source of a point gain.
fn reason_text(code: &str) -> String {
    match code {
        "WATCH" => "watching".into(),
        "CLAIM" => "bonus chest".into(),
        "WATCH_STREAK" => "watch streak".into(),
        "RAID" => "raid".into(),
        other => other.to_lowercase().replace('_', " "),
    }
}

impl Streamer {
    fn game_name(&self) -> &str {
        self.game.as_ref().map_or("unknown game", |g| g.1.as_str())
    }

    fn set_online(&mut self, announce: bool) {
        if !self.online {
            self.online = true;
            self.online_at = Some(Instant::now());
            self.watch_streak_missing = true;
            self.minutes_watched = 0.0;
            self.last_watch = None;
            if announce {
                log!(
                    "ONLINE   {} is now live ({}) - {} points",
                    self.login,
                    self.game_name(),
                    fmt_points(self.points)
                );
            }
        }
    }

    fn set_offline(&mut self, announce: bool) {
        if self.online {
            self.online = false;
            self.offline_at = Some(Instant::now());
            if announce {
                log!(
                    "OFFLINE  {} went offline - {} points",
                    self.login,
                    fmt_points(self.points)
                );
            }
        }
    }
}

pub struct Miner {
    pub api: Twitch,
    pub cfg: Config,
    pub streamers: Vec<Mutex<Streamer>>,
    /// Status changes are only announced after the startup overview
    pub started: AtomicBool,
}

fn elapsed(t: Option<Instant>) -> Option<Duration> {
    t.map(|t| t.elapsed())
}

impl Miner {
    pub fn find(&self, channel_id: &str) -> Option<usize> {
        self.streamers
            .iter()
            .position(|s| s.lock().unwrap().channel_id == channel_id)
    }

    fn login_of(&self, i: usize) -> String {
        self.streamers[i].lock().unwrap().login.clone()
    }

    pub fn set_offline(&self, i: usize) {
        let announce = self.started.load(Ordering::Relaxed);
        self.streamers[i].lock().unwrap().set_offline(announce);
    }

    fn is_online(&self, i: usize) -> bool {
        self.streamers[i].lock().unwrap().online
    }

    /// Loads the balance and claims a pending bonus chest.
    pub async fn load_points(&self, i: usize) -> Result<()> {
        let login = self.login_of(i);
        let r = self
            .api
            .gql(&api::POINTS_CONTEXT, json!({"channelLogin": login}))
            .await?;
        let cp = &r["data"]["community"]["channel"]["self"]["communityPoints"];
        if let Some(balance) = cp["balance"].as_i64() {
            self.streamers[i].lock().unwrap().points = balance;
        }
        if let Some(claim_id) = cp["availableClaim"]["id"].as_str() {
            self.claim_bonus(i, claim_id).await;
        }
        Ok(())
    }

    pub async fn claim_bonus(&self, i: usize, claim_id: &str) {
        let (login, channel_id) = {
            let s = self.streamers[i].lock().unwrap();
            (s.login.clone(), s.channel_id.clone())
        };
        log!("Claiming bonus chest on {login}");
        let vars = json!({"input": {"channelID": channel_id, "claimID": claim_id}});
        if let Err(e) = self.api.gql(&api::CLAIM_POINTS, vars).await {
            log!("Bonus chest on {login} failed: {e:#}");
        }
    }

    pub async fn claim_moment(&self, i: usize, moment_id: &str) {
        let login = self.login_of(i);
        log!("Claiming moment on {login}");
        let vars = json!({"input": {"momentID": moment_id}});
        if let Err(e) = self.api.gql(&api::CLAIM_MOMENT, vars).await {
            log!("Moment on {login} failed: {e:#}");
        }
    }

    pub async fn join_raid(&self, i: usize, raid_id: &str, target: &str) {
        let login = {
            let mut s = self.streamers[i].lock().unwrap();
            if s.last_raid.as_deref() == Some(raid_id) {
                return;
            }
            s.last_raid = Some(raid_id.to_owned());
            s.login.clone()
        };
        log!("Joining raid from {login} to {target}");
        let vars = json!({"input": {"raidID": raid_id}});
        if let Err(e) = self.api.gql(&api::JOIN_RAID, vars).await {
            log!("Joining raid failed: {e:#}");
        }
    }

    pub async fn claim_drop(&self, instance_id: &str, name: &str) {
        let vars = json!({"input": {"dropInstanceID": instance_id}});
        match self.api.gql(&api::CLAIM_DROP, vars).await {
            Ok(r) => match r["data"]["claimDropRewards"]["status"].as_str() {
                Some("ELIGIBLE_FOR_ALL" | "DROP_INSTANCE_ALREADY_CLAIMED") => {
                    log!("Claimed drop: {name}")
                }
                status => log!(
                    "Claiming drop {name} failed: {}",
                    status.unwrap_or("no status")
                ),
            },
            Err(e) => log!("Claiming drop {name} failed: {e:#}"),
        }
    }

    /// Claims every finished but unclaimed drop in the inventory.
    pub async fn claim_inventory_drops(&self) -> Result<()> {
        let r = self
            .api
            .gql(&api::INVENTORY, json!({"fetchRewardCampaigns": true}))
            .await?;
        let campaigns = &r["data"]["currentUser"]["inventory"]["dropCampaignsInProgress"];
        for campaign in campaigns.as_array().into_iter().flatten() {
            for drop in campaign["timeBasedDrops"].as_array().into_iter().flatten() {
                let progress = &drop["self"];
                let Some(instance_id) = progress["dropInstanceID"].as_str() else {
                    continue;
                };
                if progress["isClaimed"].as_bool() != Some(false) {
                    continue;
                }
                let reward = drop["benefitEdges"][0]["benefit"]["name"]
                    .as_str()
                    .or(drop["name"].as_str())
                    .unwrap_or("unknown");
                let campaign_name = campaign["name"].as_str().unwrap_or("unknown campaign");
                self.claim_drop(instance_id, &format!("{reward} ({campaign_name})"))
                    .await;
                tokio::time::sleep(Duration::from_secs(rand::random_range(5..10))).await;
            }
        }
        Ok(())
    }

    pub async fn drops_loop(self: Arc<Self>) {
        loop {
            if let Err(e) = self.claim_inventory_drops().await {
                log!("Checking drop inventory failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        }
    }

    pub fn points_earned(&self, i: usize, balance: Option<i64>, earned: Option<(i64, &str)>) {
        let mut s = self.streamers[i].lock().unwrap();
        if let Some(b) = balance {
            s.points = b;
        }
        if let Some((amount, reason)) = earned {
            let h = s.history.entry(reason.to_owned()).or_default();
            h.0 += 1;
            h.1 += amount;
            if reason == "WATCH_STREAK" {
                s.watch_streak_missing = false;
            }
            log!(
                "POINTS   +{amount} on {} from {} - now {} points",
                s.login,
                reason_text(reason),
                fmt_points(s.points)
            );
        }
    }

    /// Re-checks online state via GQL. Online streamers are refreshed at most every 2 minutes.
    pub async fn check_online(&self, i: usize) {
        let (login, online) = {
            let s = self.streamers[i].lock().unwrap();
            // Avoid flapping right after going offline
            if elapsed(s.offline_at).is_some_and(|d| d < Duration::from_secs(60)) {
                return;
            }
            if s.online && elapsed(s.last_update).is_some_and(|d| d < Duration::from_secs(120)) {
                return;
            }
            (s.login.clone(), s.online)
        };

        let r = match self
            .api
            .gql(&api::STREAM_INFO, json!({"channel": login}))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                log!("Stream info for {login} failed: {e:#}");
                return;
            }
        };
        let user = &r["data"]["user"];
        let Some(broadcast_id) = user["stream"]["id"].as_str() else {
            self.set_offline(i);
            return;
        };
        if !online && let Err(e) = self.api.spade_url(&login).await {
            log!("Cannot get spade_url: {e:#}");
            return;
        }
        let game = &user["broadcastSettings"]["game"];
        let mut s = self.streamers[i].lock().unwrap();
        s.broadcast_id = Some(broadcast_id.to_owned());
        s.game = match (game["id"].as_str(), game["name"].as_str()) {
            (Some(id), Some(name)) => Some((id.to_owned(), name.to_owned())),
            _ => None,
        };
        s.last_update = Some(Instant::now());
        s.set_online(self.started.load(Ordering::Relaxed));
    }

    /// Picks which streamers get watched this round: missing watch streaks first, then config order.
    fn pick(&self) -> Vec<usize> {
        let candidates: Vec<usize> = (0..self.streamers.len())
            .filter(|&i| {
                let s = self.streamers[i].lock().unwrap();
                s.online && elapsed(s.online_at).is_some_and(|d| d > Duration::from_secs(30))
            })
            .collect();

        let mut chosen = Vec::new();
        if self.cfg.watch_streak {
            for &i in &candidates {
                let s = self.streamers[i].lock().unwrap();
                // A streak needs ~6 minutes watched and 30 minutes since the previous stream ended
                if s.watch_streak_missing
                    && s.minutes_watched < 7.0
                    && elapsed(s.offline_at).is_none_or(|d| d > Duration::from_secs(30 * 60))
                    && chosen.len() < MAX_WATCHING
                {
                    chosen.push(i);
                }
            }
        }
        for &i in &candidates {
            if chosen.len() >= MAX_WATCHING {
                break;
            }
            if !chosen.contains(&i) {
                chosen.push(i);
            }
        }
        chosen
    }

    async fn watch(&self, i: usize) -> Result<()> {
        let (login, payload) = {
            let s = self.streamers[i].lock().unwrap();
            let user_id = self
                .api
                .user_id
                .parse::<u64>()
                .map(Value::from)
                .unwrap_or_else(|_| Value::from(self.api.user_id.clone()));
            let mut props = json!({
                "channel_id": s.channel_id,
                "broadcast_id": s.broadcast_id,
                "player": "site",
                "user_id": user_id,
                "live": true,
                "channel": s.login,
            });
            if self.cfg.claim_drops
                && let Some((game_id, game)) = &s.game
            {
                props["game"] = game.clone().into();
                props["game_id"] = game_id.clone().into();
            }
            (
                s.login.clone(),
                json!([{"event": "minute-watched", "properties": props}]),
            )
        };

        self.api.touch_stream(&login).await?;
        let spade = self.api.spade_url(&login).await?;
        if self.api.send_minute_watched(&spade, &payload).await? {
            let mut s = self.streamers[i].lock().unwrap();
            if let Some(d) = elapsed(s.last_watch) {
                s.minutes_watched += d.as_secs_f64() / 60.0;
            }
            s.last_watch = Some(Instant::now());
        } else {
            // Maybe the endpoint moved; fetch it again next time
            self.api.reset_spade_url();
        }
        Ok(())
    }

    pub async fn watch_loop(self: Arc<Self>) {
        loop {
            for i in 0..self.streamers.len() {
                let stale = {
                    let s = self.streamers[i].lock().unwrap();
                    s.online && elapsed(s.last_update).is_some_and(|d| d > Duration::from_secs(600))
                };
                if stale {
                    self.check_online(i).await;
                }
            }

            let chosen = self.pick();
            if chosen.is_empty() {
                tokio::time::sleep(Duration::from_secs(20)).await;
                continue;
            }
            let slot = Duration::from_secs(20) / chosen.len() as u32;
            for i in chosen {
                let next = Instant::now() + slot;
                if let Err(e) = self.watch(i).await {
                    log!("Watching {} failed: {e:#}", self.login_of(i));
                }
                tokio::time::sleep_until(next.into()).await;
            }
        }
    }

    /// Periodically re-syncs balances, as a fallback for missed PubSub events.
    pub async fn refresh_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
            for i in 0..self.streamers.len() {
                if !self.is_online(i) {
                    continue;
                }
                if let Err(e) = self.load_points(i).await {
                    log!("Loading points for {} failed: {e:#}", self.login_of(i));
                }
            }
        }
    }

    /// Prints every streamer with its online state and balance.
    pub fn print_status(&self, title: &str) {
        log!("{title}");
        let width = self
            .streamers
            .iter()
            .map(|s| s.lock().unwrap().login.len())
            .max()
            .unwrap_or(0);
        for s in &self.streamers {
            let s = s.lock().unwrap();
            let state = if s.online { "online " } else { "offline" };
            let game = if s.online {
                format!("  ({})", s.game_name())
            } else {
                String::new()
            };
            println!(
                "    {state}  {:<width$}  {:>12} points{game}",
                s.login,
                fmt_points(s.points)
            );
        }
    }

    pub fn print_summary(&self) {
        self.print_status("Current status:");
        log!("Earned this session:");
        let mut total = 0;
        for s in &self.streamers {
            let s = s.lock().unwrap();
            let gained = s.points - s.start_points;
            total += gained;
            if s.history.is_empty() && gained == 0 {
                continue;
            }
            let details: Vec<String> = s
                .history
                .iter()
                .map(|(reason, (n, pts))| {
                    format!("{} {n}x +{}", reason_text(reason), fmt_points(*pts))
                })
                .collect();
            println!(
                "    {}: {}{} ({})",
                s.login,
                if gained >= 0 { "+" } else { "" },
                fmt_points(gained),
                details.join(", ")
            );
        }
        println!(
            "    Total: {}{}",
            if total >= 0 { "+" } else { "" },
            fmt_points(total)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_points() {
        assert_eq!(fmt_points(0), "0");
        assert_eq!(fmt_points(999), "999");
        assert_eq!(fmt_points(1000), "1,000");
        assert_eq!(fmt_points(1234567), "1,234,567");
        assert_eq!(fmt_points(-45000), "-45,000");
    }

    #[test]
    fn readable_reasons() {
        assert_eq!(reason_text("CLAIM"), "bonus chest");
        assert_eq!(reason_text("WATCH_STREAK"), "watch streak");
        assert_eq!(reason_text("SOME_NEW_CODE"), "some new code");
    }
}

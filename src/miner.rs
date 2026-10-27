//! Streamer state and all point-earning actions.

use anyhow::Result;
use futures_util::future::join_all;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::api::{self, Twitch};
use crate::config::Config;
use crate::log;

/// Twitch only credits watch time for two channels at the same time.
const MAX_WATCHING: usize = 2;

pub struct Streamer {
    pub login: String,
    pub channel_id: String,
    state: Mutex<State>,
}

#[derive(Default)]
pub struct State {
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

impl Streamer {
    pub fn new(login: String, channel_id: String) -> Self {
        Self {
            login,
            channel_id,
            state: Mutex::default(),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
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

fn fmt_signed(n: i64) -> String {
    if n >= 0 {
        format!("+{}", fmt_points(n))
    } else {
        fmt_points(n)
    }
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

impl State {
    fn game_name(&self) -> &str {
        self.game.as_ref().map_or("unknown game", |g| g.1.as_str())
    }
}

fn elapsed(t: Option<Instant>) -> Option<Duration> {
    t.map(|t| t.elapsed())
}

pub struct Miner {
    pub api: Twitch,
    pub cfg: Config,
    pub streamers: Vec<Streamer>,
    /// channel id -> index into `streamers`
    index: HashMap<String, usize>,
    /// Status changes are only announced after the startup overview
    started: AtomicBool,
}

impl Miner {
    pub fn new(api: Twitch, cfg: Config, streamers: Vec<Streamer>) -> Self {
        let index = streamers
            .iter()
            .enumerate()
            .map(|(i, s)| (s.channel_id.clone(), i))
            .collect();
        Self {
            api,
            cfg,
            streamers,
            index,
            started: AtomicBool::new(false),
        }
    }

    pub fn find(&self, channel_id: &str) -> Option<usize> {
        self.index.get(channel_id).copied()
    }

    fn announce(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// Loads balances and stream status for every streamer, prints the overview
    /// and enables live status messages.
    pub async fn start(&self) {
        let all: Vec<usize> = (0..self.streamers.len()).collect();
        self.refresh(&all, true).await;
        for s in &self.streamers {
            let mut st = s.state();
            st.start_points = st.points;
        }
        self.print_status("Streamers:");
        self.started.store(true, Ordering::Relaxed);
    }

    /// Batch-loads balances (claiming pending bonus chests) and optionally stream status.
    async fn refresh(&self, ids: &[usize], with_stream: bool) {
        let mut reqs = Vec::new();
        for &i in ids {
            let login = &self.streamers[i].login;
            reqs.push((&api::POINTS_CONTEXT, json!({"channelLogin": login})));
            if with_stream {
                reqs.push((&api::STREAM_INFO, json!({"channel": login})));
            }
        }
        let results = match self.api.gql_batch(reqs).await {
            Ok(r) => r,
            Err(e) => {
                log!("Loading streamers failed: {e:#}");
                return;
            }
        };
        let per_streamer = if with_stream { 2 } else { 1 };

        let mut claims = Vec::new();
        for (&i, r) in ids.iter().zip(results.chunks(per_streamer)) {
            let cp = &r[0]["data"]["community"]["channel"]["self"]["communityPoints"];
            match cp["balance"].as_i64() {
                Some(balance) => self.streamers[i].state().points = balance,
                None => log!("Cannot load points for {}", self.streamers[i].login),
            }
            if let Some(claim) = cp["availableClaim"]["id"].as_str() {
                claims.push(self.claim_bonus(i, claim.to_owned()));
            }
            if with_stream {
                self.apply_stream_info(i, &r[1]);
            }
        }
        join_all(claims).await;
    }

    pub async fn claim_bonus(&self, i: usize, claim_id: String) {
        let s = &self.streamers[i];
        log!("Claiming bonus chest on {}", s.login);
        let vars = json!({"input": {"channelID": s.channel_id, "claimID": claim_id}});
        if let Err(e) = self.api.gql(&api::CLAIM_POINTS, vars).await {
            log!("Bonus chest on {} failed: {e:#}", s.login);
        }
    }

    pub async fn claim_moment(&self, i: usize, moment_id: String) {
        let login = &self.streamers[i].login;
        log!("Claiming moment on {login}");
        let vars = json!({"input": {"momentID": moment_id}});
        if let Err(e) = self.api.gql(&api::CLAIM_MOMENT, vars).await {
            log!("Moment on {login} failed: {e:#}");
        }
    }

    pub async fn join_raid(&self, i: usize, raid_id: String, target: String) {
        let s = &self.streamers[i];
        {
            let mut st = s.state();
            if st.last_raid.as_deref() == Some(raid_id.as_str()) {
                return;
            }
            st.last_raid = Some(raid_id.clone());
        }
        log!("Joining raid from {} to {target}", s.login);
        let vars = json!({"input": {"raidID": raid_id}});
        if let Err(e) = self.api.gql(&api::JOIN_RAID, vars).await {
            log!("Joining raid failed: {e:#}");
        }
    }

    pub async fn claim_drop(&self, instance_id: String, name: String) {
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
    async fn claim_inventory_drops(&self) -> Result<()> {
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
                self.claim_drop(
                    instance_id.to_owned(),
                    format!("{reward} ({campaign_name})"),
                )
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
        let s = &self.streamers[i];
        let mut st = s.state();
        if let Some(b) = balance {
            st.points = b;
        }
        if let Some((amount, reason)) = earned {
            let h = st.history.entry(reason.to_owned()).or_default();
            h.0 += 1;
            h.1 += amount;
            if reason == "WATCH_STREAK" {
                st.watch_streak_missing = false;
            }
            log!(
                "POINTS   +{amount} on {} from {} - now {} points",
                s.login,
                reason_text(reason),
                fmt_points(st.points)
            );
        }
    }

    pub fn set_stream_up(&self, i: usize) {
        self.streamers[i].state().stream_up_at = Some(Instant::now());
    }

    /// stream-up arrives before the API knows the stream; give it two minutes.
    pub fn stream_ready(&self, i: usize) -> bool {
        self.streamers[i]
            .state()
            .stream_up_at
            .is_none_or(|t| t.elapsed() > Duration::from_secs(120))
    }

    pub fn set_offline(&self, i: usize) {
        let s = &self.streamers[i];
        let mut st = s.state();
        if st.online {
            st.online = false;
            st.offline_at = Some(Instant::now());
            if self.announce() {
                log!(
                    "OFFLINE  {} went offline - {} points",
                    s.login,
                    fmt_points(st.points)
                );
            }
        }
    }

    /// Applies a STREAM_INFO response: updates broadcast/game and the online state.
    fn apply_stream_info(&self, i: usize, r: &Value) {
        let user = &r["data"]["user"];
        if user.is_null() {
            return; // request failed, keep the current state
        }
        let Some(broadcast_id) = user["stream"]["id"].as_str() else {
            self.set_offline(i);
            return;
        };
        let s = &self.streamers[i];
        let game = &user["broadcastSettings"]["game"];
        let mut st = s.state();
        st.broadcast_id = Some(broadcast_id.to_owned());
        st.game = match (game["id"].as_str(), game["name"].as_str()) {
            (Some(id), Some(name)) => Some((id.to_owned(), name.to_owned())),
            _ => None,
        };
        st.last_update = Some(Instant::now());
        if !st.online {
            st.online = true;
            st.online_at = Some(Instant::now());
            st.watch_streak_missing = true;
            st.minutes_watched = 0.0;
            st.last_watch = None;
            if self.announce() {
                log!(
                    "ONLINE   {} is now live ({}) - {} points",
                    s.login,
                    st.game_name(),
                    fmt_points(st.points)
                );
            }
        }
    }

    /// Re-checks online state via GQL. Online streamers are refreshed at most every 2 minutes.
    pub async fn check_online(&self, i: usize) {
        let s = &self.streamers[i];
        {
            let st = s.state();
            // Avoid flapping right after going offline
            if elapsed(st.offline_at).is_some_and(|d| d < Duration::from_secs(60)) {
                return;
            }
            if st.online && elapsed(st.last_update).is_some_and(|d| d < Duration::from_secs(120)) {
                return;
            }
        }
        match self
            .api
            .gql(&api::STREAM_INFO, json!({"channel": s.login}))
            .await
        {
            Ok(r) => self.apply_stream_info(i, &r),
            Err(e) => log!("Stream info for {} failed: {e:#}", s.login),
        }
    }

    /// Picks which streamers get watched this round: missing watch streaks first, then config order.
    fn pick(&self) -> Vec<usize> {
        let candidates: Vec<usize> = (0..self.streamers.len())
            .filter(|&i| {
                let st = self.streamers[i].state();
                st.online && elapsed(st.online_at).is_some_and(|d| d > Duration::from_secs(30))
            })
            .collect();

        let mut chosen = Vec::new();
        if self.cfg.watch_streak {
            for &i in &candidates {
                let st = self.streamers[i].state();
                // A streak needs ~6 minutes watched and 30 minutes since the previous stream ended
                if st.watch_streak_missing
                    && st.minutes_watched < 7.0
                    && elapsed(st.offline_at).is_none_or(|d| d > Duration::from_secs(30 * 60))
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
        let s = &self.streamers[i];
        let payload = {
            let st = s.state();
            let user_id = self
                .api
                .user_id
                .parse::<u64>()
                .map(Value::from)
                .unwrap_or_else(|_| Value::from(self.api.user_id.clone()));
            let mut props = json!({
                "channel_id": s.channel_id,
                "broadcast_id": st.broadcast_id,
                "player": "site",
                "user_id": user_id,
                "live": true,
                "channel": s.login,
            });
            if self.cfg.claim_drops
                && let Some((game_id, game)) = &st.game
            {
                props["game"] = game.clone().into();
                props["game_id"] = game_id.clone().into();
            }
            json!([{"event": "minute-watched", "properties": props}])
        };

        let (touched, spade) = tokio::join!(
            self.api.touch_stream(&s.login),
            self.api.spade_url(&s.login)
        );
        touched?;
        if self.api.send_minute_watched(&spade?, &payload).await? {
            let mut st = s.state();
            if let Some(d) = elapsed(st.last_watch) {
                st.minutes_watched += d.as_secs_f64() / 60.0;
            }
            st.last_watch = Some(Instant::now());
        } else {
            // Maybe the endpoint moved; fetch it again next time
            self.api.reset_spade_url();
        }
        Ok(())
    }

    pub async fn watch_loop(self: Arc<Self>) {
        loop {
            let stale: Vec<usize> = (0..self.streamers.len())
                .filter(|&i| {
                    let st = self.streamers[i].state();
                    st.online
                        && elapsed(st.last_update).is_some_and(|d| d > Duration::from_secs(600))
                })
                .collect();
            join_all(stale.into_iter().map(|i| self.check_online(i))).await;

            let chosen = self.pick();
            if chosen.is_empty() {
                tokio::time::sleep(Duration::from_secs(20)).await;
                continue;
            }
            let slot = Duration::from_secs(20) / chosen.len() as u32;
            for i in chosen {
                let next = Instant::now() + slot;
                if let Err(e) = self.watch(i).await {
                    log!("Watching {} failed: {e:#}", self.streamers[i].login);
                }
                tokio::time::sleep_until(next.into()).await;
            }
        }
    }

    /// Periodic background work: balances as fallback for missed PubSub events,
    /// and the Client-Version header.
    pub async fn refresh_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
            let online: Vec<usize> = (0..self.streamers.len())
                .filter(|&i| self.streamers[i].state().online)
                .collect();
            tokio::join!(
                self.refresh(&online, false),
                self.api.refresh_client_version()
            );
        }
    }

    /// Prints every streamer with its online state and balance.
    pub fn print_status(&self, title: &str) {
        log!("{title}");
        let width = self
            .streamers
            .iter()
            .map(|s| s.login.len())
            .max()
            .unwrap_or(0);
        for s in &self.streamers {
            let st = s.state();
            let (state, game) = if st.online {
                ("online ", format!("  ({})", st.game_name()))
            } else {
                ("offline", String::new())
            };
            println!(
                "    {state}  {:<width$}  {:>12} points{game}",
                s.login,
                fmt_points(st.points)
            );
        }
    }

    pub fn print_summary(&self) {
        self.print_status("Current status:");
        log!("Earned this session:");
        let mut total = 0;
        for s in &self.streamers {
            let st = s.state();
            let gained = st.points - st.start_points;
            total += gained;
            if st.history.is_empty() && gained == 0 {
                continue;
            }
            let details: Vec<String> = st
                .history
                .iter()
                .map(|(reason, (n, pts))| {
                    format!("{} {n}x +{}", reason_text(reason), fmt_points(*pts))
                })
                .collect();
            println!(
                "    {}: {} ({})",
                s.login,
                fmt_signed(gained),
                details.join(", ")
            );
        }
        println!("    Total: {}", fmt_signed(total));
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
        assert_eq!(fmt_signed(1500), "+1,500");
        assert_eq!(fmt_signed(-20), "-20");
    }

    #[test]
    fn readable_reasons() {
        assert_eq!(reason_text("CLAIM"), "bonus chest");
        assert_eq!(reason_text("WATCH_STREAK"), "watch streak");
        assert_eq!(reason_text("SOME_NEW_CODE"), "some new code");
    }
}

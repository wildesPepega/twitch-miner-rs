//! Twitch PubSub: live events for points, bonus chests, raids, moments, drops and stream status.

use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

use crate::api::{ALNUM, random_string};
use crate::log;
use crate::miner::Miner;

const URL: &str = "wss://pubsub-edge.twitch.tv/v1";
/// Twitch allows at most 50 topics per connection.
const TOPICS_PER_CONN: usize = 50;

pub fn topics(miner: &Miner) -> Vec<String> {
    let mut t = vec![format!("community-points-user-v1.{}", miner.api.user_id)];
    if miner.cfg.claim_drops {
        t.push(format!("user-drop-events.{}", miner.api.user_id));
    }
    for s in &miner.streamers {
        let id = s.lock().unwrap().channel_id.clone();
        t.push(format!("video-playback-by-id.{id}"));
        if miner.cfg.follow_raids {
            t.push(format!("raid.{id}"));
        }
        if miner.cfg.claim_moments {
            t.push(format!("community-moments-channel-v1.{id}"));
        }
    }
    t
}

pub fn spawn_all(miner: &Arc<Miner>) {
    for (n, chunk) in topics(miner).chunks(TOPICS_PER_CONN).enumerate() {
        tokio::spawn(run(miner.clone(), n, chunk.to_vec()));
    }
}

async fn run(miner: Arc<Miner>, n: usize, topics: Vec<String>) {
    loop {
        let delay = match session(&miner, &topics).await {
            Ok(()) => Duration::from_secs(5),
            Err(e) => {
                log!("PubSub #{n}: {e:#}, retrying in 30s");
                Duration::from_secs(30)
            }
        };
        tokio::time::sleep(delay).await;
    }
}

/// One connection lifetime. Ok(()) means Twitch asked us to reconnect.
async fn session(miner: &Arc<Miner>, topics: &[String]) -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(URL).await?;

    for topic in topics {
        let mut data = json!({"topics": [topic]});
        // Topics about our own account need the auth token
        if topic.ends_with(&format!(".{}", miner.api.user_id)) {
            data["auth_token"] = miner.api.token.clone().into();
        }
        let msg = json!({"type": "LISTEN", "nonce": random_string(30, ALNUM), "data": data});
        ws.send(Message::text(msg.to_string())).await?;
    }

    let ping = json!({"type": "PING"}).to_string();
    let mut last_pong = Instant::now();
    let mut last_msg = String::new();
    loop {
        let wait = Duration::from_secs_f64(rand::random_range(25.0..30.0));
        tokio::select! {
            frame = ws.next() => {
                let text = match frame {
                    None => bail!("connection closed"),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) => bail!("connection closed"),
                    Some(Ok(_)) => continue,
                };
                let v: Value = serde_json::from_str(&text)?;
                match v["type"].as_str() {
                    Some("PONG") => last_pong = Instant::now(),
                    Some("RECONNECT") => return Ok(()),
                    Some("RESPONSE") => {
                        let err = v["error"].as_str().unwrap_or_default();
                        if err == "ERR_BADAUTH" {
                            log!("PubSub: token rejected. Delete {}.token.json and restart", miner.cfg.username);
                        } else if !err.is_empty() {
                            log!("PubSub LISTEN error: {err}");
                        }
                    }
                    Some("MESSAGE") => {
                        let (Some(topic), Some(message)) =
                            (v["data"]["topic"].as_str(), v["data"]["message"].as_str())
                        else {
                            continue;
                        };
                        // Twitch occasionally delivers the same message twice
                        if message == last_msg {
                            continue;
                        }
                        last_msg = message.to_owned();
                        if let Ok(msg) = serde_json::from_str::<Value>(message) {
                            handle(miner, topic, msg);
                        }
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(wait) => {
                if last_pong.elapsed() > Duration::from_secs(5 * 60) {
                    bail!("no PONG for 5 minutes");
                }
                ws.send(Message::text(ping.clone())).await?;
            }
        }
    }
}

fn handle(miner: &Arc<Miner>, topic: &str, msg: Value) {
    let (topic_name, topic_id) = topic.split_once('.').unwrap_or((topic, ""));
    let kind = msg["type"].as_str().unwrap_or_default();
    let data = &msg["data"];

    if topic_name == "user-drop-events" {
        if kind == "drop-claim"
            && let Some(id) = data["drop_instance_id"].as_str()
        {
            let (m, id) = (miner.clone(), id.to_owned());
            tokio::spawn(async move { m.claim_drop(&id, "from live event").await });
        }
        return;
    }

    let channel_id = [
        &data["claim"]["channel_id"],
        &data["channel_id"],
        &data["balance"]["channel_id"],
    ]
    .into_iter()
    .find_map(|v| v.as_str())
    .unwrap_or(topic_id);
    let Some(i) = miner.find(channel_id) else {
        return;
    };

    match (topic_name, kind) {
        ("community-points-user-v1", "points-earned" | "points-spent") => {
            let balance = data["balance"]["balance"].as_i64();
            let earned = (kind == "points-earned")
                .then(|| {
                    let g = &data["point_gain"];
                    Some((g["total_points"].as_i64()?, g["reason_code"].as_str()?))
                })
                .flatten();
            miner.points_earned(i, balance, earned);
        }
        ("community-points-user-v1", "claim-available") => {
            if let Some(id) = data["claim"]["id"].as_str() {
                let (m, id) = (miner.clone(), id.to_owned());
                tokio::spawn(async move { m.claim_bonus(i, &id).await });
            }
        }
        ("video-playback-by-id", "stream-up") => {
            miner.streamers[i].lock().unwrap().stream_up_at = Some(Instant::now());
        }
        ("video-playback-by-id", "stream-down") => {
            miner.streamers[i].lock().unwrap().set_offline();
        }
        ("video-playback-by-id", "viewcount") => {
            // stream-up arrives before the API knows the stream; give it two minutes
            let ready = miner.streamers[i]
                .lock()
                .unwrap()
                .stream_up_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(120));
            if ready {
                let m = miner.clone();
                tokio::spawn(async move { m.check_online(i).await });
            }
        }
        ("raid", "raid_update_v2") => {
            let raid = &msg["raid"];
            if let (Some(id), Some(target)) = (raid["id"].as_str(), raid["target_login"].as_str()) {
                let (m, id, target) = (miner.clone(), id.to_owned(), target.to_owned());
                tokio::spawn(async move { m.join_raid(i, &id, &target).await });
            }
        }
        ("community-moments-channel-v1", "active") => {
            if let Some(id) = data["moment_id"].as_str() {
                let (m, id) = (miner.clone(), id.to_owned());
                tokio::spawn(async move { m.claim_moment(i, &id).await });
            }
        }
        _ => {}
    }
}

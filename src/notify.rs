//! Optional Discord webhook notifications.
//!
//! Messages are queued and sent in order by a single background task,
//! so callers never wait on Discord and rate limits are respected.

use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::log;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Event {
    Online,
    Offline,
    Points,
    Bonus,
    Raid,
    Moment,
    Drop,
    Login,
    Error,
}

impl Event {
    pub const ALL: [(&'static str, Event); 9] = [
        ("online", Event::Online),
        ("offline", Event::Offline),
        ("points", Event::Points),
        ("bonus", Event::Bonus),
        ("raid", Event::Raid),
        ("moment", Event::Moment),
        ("drop", Event::Drop),
        ("login", Event::Login),
        ("error", Event::Error),
    ];

    pub fn parse(s: &str) -> Option<Event> {
        let s = s.trim().to_lowercase();
        Self::ALL.iter().find(|(n, _)| *n == s).map(|(_, e)| *e)
    }

    pub fn defaults() -> Vec<Event> {
        use Event::*;
        vec![Online, Offline, Drop, Raid, Login, Error]
    }
}

struct Notifier {
    events: HashSet<Event>,
    tx: Mutex<Option<UnboundedSender<String>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

static NOTIFIER: OnceLock<Notifier> = OnceLock::new();

/// Starts the sender task. Without a webhook URL, notifications are disabled.
pub fn init(webhook: Option<String>, events: &[Event]) {
    let Some(url) = webhook else {
        return;
    };
    let (tx, mut rx) = unbounded_channel::<String>();
    let worker = tokio::spawn(async move {
        let http = reqwest::Client::new();
        while let Some(msg) = rx.recv().await {
            post(&http, &url, &msg).await;
        }
    });
    let _ = NOTIFIER.set(Notifier {
        events: events.iter().copied().collect(),
        tx: Mutex::new(Some(tx)),
        worker: Mutex::new(Some(worker)),
    });
}

/// Queues a message if the event type is enabled.
pub fn send(event: Event, msg: impl Into<String>) {
    if let Some(n) = NOTIFIER.get()
        && n.events.contains(&event)
        && let Some(tx) = n.tx.lock().unwrap().as_ref()
    {
        let _ = tx.send(msg.into());
    }
}

/// Delivers queued messages before the process exits (at most 5 seconds).
pub async fn flush() {
    let Some(n) = NOTIFIER.get() else {
        return;
    };
    n.tx.lock().unwrap().take();
    let worker = n.worker.lock().unwrap().take();
    if let Some(w) = worker {
        let _ = tokio::time::timeout(Duration::from_secs(5), w).await;
    }
}

async fn post(http: &reqwest::Client, url: &str, msg: &str) {
    let body = json!({"username": "Twitch Miner", "content": msg});
    for _ in 0..3 {
        match http.post(url).json(&body).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                let wait = r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["retry_after"].as_f64())
                    .unwrap_or(2.0);
                tokio::time::sleep(Duration::from_secs_f64(wait.min(60.0))).await;
            }
            Ok(r) if !r.status().is_success() => {
                log!("Discord webhook failed: HTTP {}", r.status());
                return;
            }
            Ok(_) => return,
            Err(e) => {
                log!("Discord webhook failed: {}", e.without_url());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivers_enabled_events_on_flush() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            while let Ok(Ok((mut sock, _))) =
                tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
            {
                let mut buf = vec![0; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                bodies.push(String::from_utf8_lossy(&buf[..n]).to_string());
                sock.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
            }
            bodies
        });

        init(Some(format!("http://{addr}/hook")), &[Event::Drop]);
        send(Event::Drop, "got a drop");
        send(Event::Points, "filtered out");
        flush().await;

        let bodies = server.await.unwrap();
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].contains("got a drop"));
    }

    #[test]
    fn parses_events() {
        assert_eq!(Event::parse(" Drop "), Some(Event::Drop));
        assert_eq!(Event::parse("nope"), None);
    }
}

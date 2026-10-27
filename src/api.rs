//! Twitch HTTP/GQL client and the device-code login.

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant};

use crate::log;

pub const CLIENT_ID: &str = "ue6666qo983tsx6so1t0vnawi233wa"; // Android TV
const DEFAULT_CLIENT_VERSION: &str = "ef928475-9403-42f2-8a34-55784bd08e16";
const GQL_URL: &str = "https://gql.twitch.tv/gql";
const TV_UA: &str = "Mozilla/5.0 (Linux; Android 7.1; Smart Box C1) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/108.0.0.0 Safari/537.36";
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:85.0) Gecko/20100101 Firefox/85.0";

/// Persisted GQL query: (operationName, sha256Hash).
pub struct Op(&'static str, &'static str);

pub const PLAYBACK_ACCESS_TOKEN: Op = Op(
    "PlaybackAccessToken",
    "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9",
);
pub const STREAM_INFO: Op = Op(
    "VideoPlayerStreamInfoOverlayChannel",
    "198492e0857f6aedead9665c81c5a06d67b25b58034649687124083ff288597d",
);
pub const CLAIM_POINTS: Op = Op(
    "ClaimCommunityPoints",
    "46aaeebe02c99afdf4fc97c7c0cba964124bf6b0af229395f1f6d1feed05b3d0",
);
pub const CLAIM_MOMENT: Op = Op(
    "CommunityMomentCallout_Claim",
    "e2d67415aead910f7f9ceb45a77b750a1e1d9622c936d832328a0689e054db62",
);
pub const POINTS_CONTEXT: Op = Op(
    "ChannelPointsContext",
    "7fe050e3761eb2cf258d70ee1a21cbd76fa8cf3d7e7b12fc437e7029d446b5e3",
);
pub const JOIN_RAID: Op = Op(
    "JoinRaid",
    "c6a332a86d1087fbbb1a8623aa01bd1313d2386e7c63be60fdb2d1901f01a4ae",
);
pub const ID_FROM_LOGIN: Op = Op(
    "GetIDFromLogin",
    "94e82a7b1e3c21e186daa73ee2afc4b8f23bade1fbbff6fe8ac133f50a2f58ca",
);
pub const INVENTORY: Op = Op(
    "Inventory",
    "d86775d0ef16a63a33ad52e80eaff963b2d5b72fada7c991504a57496e1d8e4b",
);
pub const CLAIM_DROP: Op = Op(
    "DropsPage_ClaimDropRewards",
    "a455deea71bdc9015b78eb49f4acfbce8baa7ccbedd28e549bb025bd0f751930",
);
pub const CHANNEL_FOLLOWS: Op = Op(
    "ChannelFollows",
    "eecf815273d3d949e5cf0085cc5084cd8a1b5b7b6f7990cf43cb0beadf546907",
);

static BUILD_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"window\.__twilightBuildID\s*=\s*"([0-9a-fA-F-]{36})""#).unwrap()
});
static SETTINGS_JS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(https://static\.twitchcdn\.net/config/settings.*?js|https://assets\.twitch\.tv/config/settings.*?\.js)").unwrap()
});
static SPADE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""spade_url":"(.*?)""#).unwrap());

#[derive(Serialize, Deserialize)]
struct TokenFile {
    auth_token: String,
}

pub struct Twitch {
    pub http: reqwest::Client,
    pub token: String,
    pub user_id: String,
    device_id: String,
    session_id: String,
    client_version: RwLock<(String, Option<Instant>)>,
    spade_url: RwLock<Option<String>>,
}

pub fn random_string(len: usize, charset: &[u8]) -> String {
    (0..len)
        .map(|_| charset[rand::random_range(0..charset.len())] as char)
        .collect()
}

pub const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const HEX: &[u8] = b"0123456789abcdef";

impl Twitch {
    /// Loads the saved token (or runs the device-code login) and validates it.
    pub async fn login(username: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        let device_id = random_string(32, ALNUM);
        let token_path = PathBuf::from(format!("{username}.token.json"));

        let mut token = std::fs::read_to_string(&token_path)
            .ok()
            .and_then(|s| serde_json::from_str::<TokenFile>(&s).ok())
            .map(|t| t.auth_token);

        let user_id = loop {
            let t = match token.take() {
                Some(t) => t,
                None => {
                    let t = device_login(&http, &device_id).await?;
                    save_token(&token_path, &t)?;
                    t
                }
            };
            match validate(&http, &t).await? {
                Some((login, id)) => {
                    if login != username {
                        log!("Warning: token belongs to '{login}', not '{username}'");
                    }
                    token = Some(t);
                    break id;
                }
                None => {
                    log!("Saved token is invalid, logging in again");
                    let _ = std::fs::remove_file(&token_path);
                }
            }
        };

        Ok(Self {
            http,
            token: token.unwrap(),
            user_id,
            device_id,
            session_id: random_string(32, HEX),
            client_version: RwLock::new((DEFAULT_CLIENT_VERSION.into(), None)),
            spade_url: RwLock::new(None),
        })
    }

    /// Twitch wants a recent web build id as Client-Version; refreshed hourly.
    async fn client_version(&self) -> String {
        {
            let cv = self.client_version.read().unwrap();
            if cv
                .1
                .is_some_and(|t| t.elapsed() < Duration::from_secs(3600))
            {
                return cv.0.clone();
            }
        }
        let fetched = async {
            let html = self
                .http
                .get("https://www.twitch.tv")
                .send()
                .await
                .ok()?
                .text()
                .await
                .ok()?;
            Some(BUILD_ID_RE.captures(&html)?[1].to_string())
        }
        .await;
        let mut cv = self.client_version.write().unwrap();
        if let Some(v) = fetched {
            cv.0 = v;
        }
        cv.1 = Some(Instant::now());
        cv.0.clone()
    }

    pub async fn gql(&self, op: &Op, variables: Value) -> Result<Value> {
        let body = json!({
            "operationName": op.0,
            "variables": variables,
            "extensions": {"persistedQuery": {"version": 1, "sha256Hash": op.1}},
        });
        let resp: Value = self
            .http
            .post(GQL_URL)
            .header("Authorization", format!("OAuth {}", self.token))
            .header("Client-Id", CLIENT_ID)
            .header("Client-Session-Id", &self.session_id)
            .header("Client-Version", self.client_version().await)
            .header("User-Agent", TV_UA)
            .header("X-Device-Id", &self.device_id)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("GQL {}", op.0))?
            .json()
            .await
            .with_context(|| format!("GQL {} response", op.0))?;
        if resp.get("data").is_none_or(Value::is_null) {
            bail!("GQL {} failed: {resp}", op.0);
        }
        Ok(resp)
    }

    pub async fn channel_id(&self, login: &str) -> Result<Option<String>> {
        let r = self.gql(&ID_FROM_LOGIN, json!({"login": login})).await?;
        Ok(r["data"]["user"]["id"].as_str().map(str::to_owned))
    }

    pub async fn followers(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let r = self
                .gql(
                    &CHANNEL_FOLLOWS,
                    json!({"limit": 100, "order": "ASC", "cursor": cursor}),
                )
                .await?;
            let follows = &r["data"]["user"]["follows"];
            let edges = follows["edges"]
                .as_array()
                .ok_or_else(|| anyhow!("Follows: {r}"))?;
            for e in edges {
                if let Some(login) = e["node"]["login"].as_str() {
                    out.push(login.to_lowercase());
                }
                if let Some(c) = e["cursor"].as_str() {
                    cursor = c.to_owned();
                }
            }
            if !follows["pageInfo"]["hasNextPage"]
                .as_bool()
                .unwrap_or(false)
                || edges.is_empty()
            {
                return Ok(out);
            }
        }
    }

    /// Spade is Twitch's analytics endpoint that receives the minute-watched events.
    pub async fn spade_url(&self, login: &str) -> Result<String> {
        if let Some(u) = self.spade_url.read().unwrap().clone() {
            return Ok(u);
        }
        let page = self
            .http
            .get(format!("https://www.twitch.tv/{login}"))
            .header("User-Agent", BROWSER_UA)
            .send()
            .await?
            .text()
            .await?;
        let settings_url = SETTINGS_JS_RE
            .captures(&page)
            .ok_or_else(|| anyhow!("settings.js not found"))?[1]
            .to_string();
        let js = self
            .http
            .get(settings_url)
            .header("User-Agent", BROWSER_UA)
            .send()
            .await?
            .text()
            .await?;
        let url = SPADE_RE
            .captures(&js)
            .ok_or_else(|| anyhow!("spade_url not found"))?[1]
            .to_string();
        *self.spade_url.write().unwrap() = Some(url.clone());
        Ok(url)
    }

    pub fn reset_spade_url(&self) {
        *self.spade_url.write().unwrap() = None;
    }

    /// Fetches the HLS playlist and requests the newest segment, like a real player would.
    pub async fn touch_stream(&self, login: &str) -> Result<()> {
        let r = self
            .gql(
                &PLAYBACK_ACCESS_TOKEN,
                json!({
                    "login": login, "isLive": true, "isVod": false, "vodID": "",
                    "playerType": "site", "platform": "web",
                }),
            )
            .await?;
        let pat = &r["data"]["streamPlaybackAccessToken"];
        let (Some(sig), Some(value)) = (pat["signature"].as_str(), pat["value"].as_str()) else {
            bail!("No PlaybackAccessToken for {login}");
        };

        let master = self
            .http
            .get(format!(
                "https://usher.ttvnw.net/api/channel/hls/{login}.m3u8"
            ))
            .query(&[("sig", sig), ("token", value)])
            .header("User-Agent", TV_UA)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        // The last variant in the master playlist is the lowest quality.
        let variant = last_url(&master).ok_or_else(|| anyhow!("No stream variant"))?;
        let media = self
            .http
            .get(variant)
            .header("User-Agent", TV_UA)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let segment = last_url(&media).ok_or_else(|| anyhow!("No stream segment"))?;
        self.http
            .head(segment)
            .header("User-Agent", TV_UA)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn send_minute_watched(&self, spade_url: &str, payload: &Value) -> Result<bool> {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(payload.to_string());
        let resp = self
            .http
            .post(spade_url)
            .header("User-Agent", TV_UA)
            .form(&[("data", data)])
            .send()
            .await?;
        Ok(resp.status() == reqwest::StatusCode::NO_CONTENT)
    }
}

fn last_url(playlist: &str) -> Option<&str> {
    playlist
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("http"))
}

fn save_token(path: &Path, token: &str) -> Result<()> {
    let data = serde_json::to_string(&TokenFile {
        auth_token: token.to_owned(),
    })?;
    std::fs::write(path, data).with_context(|| format!("Cannot write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    log!("Token saved to {}", path.display());
    Ok(())
}

/// Returns (login, user_id) for a valid token, None if Twitch rejects it.
async fn validate(http: &reqwest::Client, token: &str) -> Result<Option<(String, String)>> {
    let resp = http
        .get("https://id.twitch.tv/oauth2/validate")
        .header("Authorization", format!("OAuth {token}"))
        .send()
        .await
        .context("Token validation")?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    let v: Value = resp.error_for_status()?.json().await?;
    match (v["login"].as_str(), v["user_id"].as_str()) {
        (Some(l), Some(id)) => Ok(Some((l.to_owned(), id.to_owned()))),
        _ => bail!("Unexpected token validation response: {v}"),
    }
}

async fn oauth_post(
    http: &reqwest::Client,
    device_id: &str,
    url: &str,
    form: &[(&str, &str)],
) -> Result<(reqwest::StatusCode, Value)> {
    let resp = http
        .post(url)
        .header("Client-Id", CLIENT_ID)
        .header("Origin", "https://android.tv.twitch.tv")
        .header("Referer", "https://android.tv.twitch.tv/")
        .header("User-Agent", TV_UA)
        .header("X-Device-Id", device_id)
        .form(form)
        .send()
        .await?;
    let status = resp.status();
    Ok((status, resp.json().await.unwrap_or(Value::Null)))
}

async fn device_login(http: &reqwest::Client, device_id: &str) -> Result<String> {
    const SCOPES: &str =
        "channel_read chat:read user_blocks_edit user_blocks_read user_follows_edit user_read";
    loop {
        let (status, r) = oauth_post(
            http,
            device_id,
            "https://id.twitch.tv/oauth2/device",
            &[("client_id", CLIENT_ID), ("scopes", SCOPES)],
        )
        .await?;
        let (Some(device_code), Some(user_code)) =
            (r["device_code"].as_str(), r["user_code"].as_str())
        else {
            bail!("Login failed ({status}): {r}");
        };
        let interval = Duration::from_secs(r["interval"].as_u64().unwrap_or(5));
        let expires =
            Instant::now() + Duration::from_secs(r["expires_in"].as_u64().unwrap_or(1800));

        log!("Open https://www.twitch.tv/activate and enter this code: {user_code}");

        while Instant::now() < expires {
            tokio::time::sleep(interval).await;
            let (status, r) = oauth_post(
                http,
                device_id,
                "https://id.twitch.tv/oauth2/token",
                &[
                    ("client_id", CLIENT_ID),
                    ("device_code", device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .await?;
            // 400 = code not entered yet
            if status.is_success()
                && let Some(t) = r["access_token"].as_str()
            {
                log!("Login successful");
                return Ok(t.to_owned());
            }
        }
        log!("Code expired, requesting a new one...");
    }
}

# twitch-miner

A lightweight Rust port of [Twitch Channel Points Miner v2](https://github.com/rdavydov/Twitch-Channel-Points-Miner-v2).
It does one thing: farm channel points.

- Watches streams (minute-watched events)
- Claims bonus chests automatically
- Joins raids
- Claims moments
- Claims finished drops from your inventory (live via PubSub plus a check every 30 minutes)
- Prefers channels whose watch streak is still missing
- Optional Discord webhook notifications

Not included: predictions, web UI/analytics, log files, chat, drop campaign prioritization.

### Built to run unattended

- Balances, bonus chests and stream status are also polled every 5 minutes, so the miner keeps
  working even if PubSub events stop arriving.
- Network failures at startup are retried with growing delays instead of exiting.
- If Twitch rejects the token, the miner starts a new login by itself. The activation code
  shows up in the console and, if configured, in Discord.

### Why only 2 channels at once?

Twitch only credits watch time for two channels at the same time. Watching more would not earn more points.
Higher channels in your list get priority. If one of them goes offline, the next live channel takes its place.
Raids and moments are handled on **all** configured live channels.

## Usage

Download a binary from [Releases](https://github.com/wildesPepega/twitch-miner-rs/releases), or build it yourself:

```sh
cargo build --release
cp config.example.toml config.toml   # edit it
./target/release/twitch-miner        # or: twitch-miner path/to/config.toml
```

On first start, the miner prints a code. Open https://www.twitch.tv/activate and enter it.
The token is saved to `<username>.token.json` in the working directory. Keep this file private.
Press Ctrl+C for a short summary.

## Configuration

See [`config.example.toml`](config.example.toml). Every option can also be set with an environment
variable, which overrides the file. The config file is optional if `TWITCH_USERNAME` is set.

| Option            | Environment variable | Default |
|-------------------|----------------------|---------|
| `username`        | `TWITCH_USERNAME`    | –       |
| `streamers`       | `STREAMERS` (comma separated) | `[]` |
| `followers`       | `FOLLOWERS`          | `false` |
| `blacklist`       | `BLACKLIST` (comma separated) | `[]` |
| `follow_raids`    | `FOLLOW_RAIDS`       | `true`  |
| `claim_moments`   | `CLAIM_MOMENTS`      | `true`  |
| `watch_streak`    | `WATCH_STREAK`       | `true`  |
| `claim_drops`     | `CLAIM_DROPS`        | `true`  |
| `discord_webhook` | `DISCORD_WEBHOOK`    | off     |
| `discord_events`  | `DISCORD_EVENTS` (comma separated) | `online, offline, drop, raid, login, error` |

Discord events: `online`, `offline`, `points`, `bonus`, `raid`, `moment`, `drop`, `login`, `error`.
`points` sends a message for every point gain, which is a lot.

## Docker

```sh
docker run -d --name twitch-miner --restart unless-stopped \
  -v ./data:/data \
  -e TWITCH_USERNAME=your-twitch-username \
  -e STREAMERS=streamer1,streamer2 \
  ghcr.io/wildespepega/twitch-miner-rs:latest
docker logs -f twitch-miner   # shows the activation code on first start
```

Or use [`docker-compose.yml`](docker-compose.yml). To build the image yourself: `docker build -t twitch-miner .`
The token file is stored in `/data`, so keep that volume. A `config.toml` placed there is picked up too.

## Pterodactyl

Import [`egg-twitch-miner.json`](egg-twitch-miner.json) in your panel (Admin → Nests → Import Egg).
Create a server, then fill in your username and streamers. On first start, the activation code shows up in the console.
To update, reinstall the server.

## License

GPL-3.0, like the original project.

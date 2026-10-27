# twitch-miner

A lightweight Rust port of [Twitch Channel Points Miner v2](https://github.com/rdavydov/Twitch-Channel-Points-Miner-v2).
It does one thing: farm channel points.

- Watches streams (minute-watched events)
- Claims bonus chests automatically
- Joins raids
- Claims moments
- Claims finished drops from your inventory (live via PubSub plus a check every 30 minutes)
- Prefers channels whose watch streak is still missing

Not included: predictions, web UI/analytics, log files, notifications, chat, drop campaign prioritization.

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
If the token becomes invalid, the miner logs in again automatically. Press Ctrl+C for a short summary.

## Configuration

See [`config.example.toml`](config.example.toml). Every option can also be set with an environment
variable, which overrides the file. The config file is optional if `TWITCH_USERNAME` is set.

| Option          | Environment variable | Default |
|-----------------|----------------------|---------|
| `username`      | `TWITCH_USERNAME`    | –       |
| `streamers`     | `STREAMERS` (comma separated) | `[]` |
| `followers`     | `FOLLOWERS`          | `false` |
| `blacklist`     | `BLACKLIST` (comma separated) | `[]` |
| `follow_raids`  | `FOLLOW_RAIDS`       | `true`  |
| `claim_moments` | `CLAIM_MOMENTS`      | `true`  |
| `watch_streak`  | `WATCH_STREAK`       | `true`  |
| `claim_drops`   | `CLAIM_DROPS`        | `true`  |

## Pterodactyl

Import [`egg-twitch-miner.json`](egg-twitch-miner.json) in your panel (Admin → Nests → Import Egg).
Create a server, then fill in your username and streamers. On first start, the activation code shows up in the console.
To update, reinstall the server.

## License

GPL-3.0, like the original project.

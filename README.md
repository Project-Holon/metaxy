# metaxy

The relay server for multiplayer in
[eidos](https://github.com/Project-Holon/eidos), the native PC port of
Pokémon Black and White.

Players connect out to the relay, so nobody has to open ports or use a VPN.
The host opens a session and gets a short code; the others join with it.
The relay forwards messages between the players and keeps nothing. It holds
no game code and no game data.

metaxy also runs world sessions: eidos-only arkhipelagos (randomizer) games
that it makes, keeps on disk and routes. See "World sessions" below.

The protocol and the multiplayer design are in [docs/SPEC.md](docs/SPEC.md).

## Mystery Gift events

`data/mystery_events.json` lists the Mystery Gift events of Black and
White as they were given out, for each cartridge language: English,
French, German, Italian, Spanish, Japanese and Korean. That is 168 events.
Each entry gives:

- how the gift was given out: Nintendo WFC, local wireless (shops and
  events) or infrared
- the gifts and the games (Black, White or both)
- the original dates, one range for each time the event ran
- where each Wonder Card file is in Project Pokémon's EventsGallery, with
  its size and SHA-256
- the text each Wonder Card shows

The Wonder Card files are in `data/cards`, named by their SHA-256; the
relay checks each one against the list when it starts. eidos asks the relay
which events are up on the player's date (a `G` frame, docs/SPEC.md section
4b) and gets each one with its card. Every event comes back every month: it
starts on the day of the month it first started and stays up for a week.
`--events` names another list, and `--events ""` turns this off.

### Sources

- Event dates, methods, gifts and the cards' texts: Bulbapedia's lists of Generation V event
  distributions, Wi-Fi and local, for each language, and its list of other
  event distributions (the Liberty Pass and the Master Ball).
  <https://bulbapedia.bulbagarden.net/>. Bulbapedia's text is under
  CC BY-NC-SA 2.5; this list takes only dates and names from it.
- Wonder Card files: Project Pokémon's EventsGallery, copied unchanged.
  <https://github.com/projectpokemon/EventsGallery>
- Dates checked against Serebii's event pages
  (<https://www.serebii.net/events/>) and PKMN.NET.

Local events often ran on different dates in different countries. The list
keeps the dates of the language's home country (the United States for
English when listed there, France, Germany, Italy, Spain, Japan, Korea),
and otherwise the first country named. Four events have no card file in
EventsGallery and list no card.

## Build

You need a stable Rust toolchain.

```
cargo build --release
```

The binary is `target/release/metaxy`. `cargo test` runs the unit tests and
the integration tests, which use real TCP on local ports.

## Run

```
metaxy
```

This listens on plain TCP, port 4141, on all addresses. Stop it with
Ctrl+C: every client gets `E shutdown` first. Sessions live in memory only,
so a restart forgets them.

To serve TLS (TLS 1.3, port 443), give a certificate chain and a key, both
PEM files. Plain TCP stays on unless you turn it off:

```
metaxy --tls-cert fullchain.pem --tls-key privkey.pem --plain false
```

## Configure

Every setting is a command-line flag and a key in an optional TOML file
(`--config metaxy.toml`). Flags win over the file, and the file wins over
the defaults. `metaxy --help` lists them all. The defaults are the limits in
section 6 of the spec.

```toml
bind = "0.0.0.0"
port = 4141
plain = true
tls_cert = ""          # set both to serve TLS
tls_key = ""
tls_port = 443

max_payload = 4224     # bytes in one frame
max_players = 16
msg_rate = 60.0        # D frames per second per client
msg_burst = 256.0
byte_rate = 524288.0   # D payload bytes per second per client
byte_burst = 1048576.0
rate_factor = 4.0      # over this many times the rate ...
rate_window_ms = 1000
rate_close_windows = 5 # ... for this many windows: E rate and close
queue_cap = 2097152    # queued bytes to a slow client: E too-slow
conn_per_window = 10   # new connections per IP address ...
conn_window_ms = 60000 # ... per window
sessions_per_ip = 4
max_sessions = 100000
handshake_ms = 10000
idle_ms = 60000
session_expiry_ms = 600000

log_level = "info"     # error, warn, info, debug, trace or off
log_json = false       # true: JSON lines
```

## World sessions

metaxy also runs eidos-only arkhipelagos games: it makes a world with
arkhipelagos' generator (linked in, no Python), keeps it on disk, sends each
player their data when they join, and passes items and positions between
the players. The frames and rules are in Part C of the spec
([docs/SPEC.md](docs/SPEC.md)). This role keeps state on disk, unlike the
relay.

### The world store

Each world is a folder: `world.json` (its configuration, seed, generator
version and hash) and `log` (one line per claim, check, goal). metaxy picks
the folder in this order:

1. `--world-store <dir>` (or `world_store` in the TOML file);
2. the environment variable `METAXY_WORLD_STORE`;
3. `metaxy-worlds` in the system temp folder.

metaxy makes the folder if it is missing. If it cannot write there, it logs
an error and uses the temp folder. A temp folder is not persistent: worlds
are lost when the service restarts or redeploys, and the log warns about
it at start. The relay never stops over the store. `--world-store off`
turns world sessions off.

### Run it locally

```
cargo run --release -- --bind 127.0.0.1 --port 20609 --world-store worlds
```

Any folder works for `--world-store`; leave it out to use the temp folder.
Then point eidos at `127.0.0.1:20609`.

To play a whole two-player world against it (create, start, both players
join, every check, every item with its index, positions, both goals, then
delete):

```
cargo run --release --example world_play -- 127.0.0.1:20609
```

`--keep` keeps the world and prints its code and keys; `--config file.json`
plays another two-player configuration.

### Settings

| Setting | Default | What |
|---|---|---|
| `world_store` | (see above) | the store folder, `temp` or `off` |
| `gen_workers` | cores minus one | threads that make worlds |
| `gen_queue` | 64 | generations that may wait |
| `gen_time_ms` | 10000 | time limit of one generation |
| `gen_per_hour` | 6 | generations per IP address per hour |
| `worlds_per_ip` | 20 | open worlds made from one IP address |
| `world_config_max` | 65536 | largest configuration in bytes |
| `world_data_max` | 1048576 | largest player data in bytes |
| `pos_rate`, `pos_burst` | 8, 16 | tile positions per second per player |
| `world_unload_ms` | 600000 | a made world leaves memory after this long without players |
| `world_idle_s` | 7776000 (90 days) | a world with no join for this long is deleted |
| `world_done_s` | 2592000 (30 days) | a finished world is deleted this long after its end |
| `world_sweep_ms` | 60000 | how often unloading and expiry are checked |

### The generator

`Cargo.toml` pins `arkh-gen` (arkhipelagos' `crates/gen`) to one commit. A
stored world names the generator version that made it, and metaxy makes the
world again from its configuration when a player joins. If the hash
differs, the world is not served (`E gen-mismatch`). The test
`golden_hash_of_the_pinned_generator` checks arkhipelagos' golden hash for
its two-player test world. Keep `serde_json` without extra features: a
feature such as `preserve_order` changes every world's hash.

## The Archipelago bridge

With `--bridge` (or `bridge = true`), eidos can play in an Archipelago
multiworld through metaxy: the game sends a room link or `host:port` in an
`L` frame, and metaxy opens a WebSocket to that Archipelago server and
passes its messages both ways (Part D of the spec). metaxy checks that the
server's first message is `RoomInfo` and connects only to public addresses
unless `bridge_private = true`. It is off by default.

## Logs

The relay writes one line per event to standard error, with a UTC time (RFC
3339, to the millisecond), a level, and named fields. The session code, the
peer number and the client address are there wherever they apply.

- `--log-level LEVEL` (or `log_level`): `error`, `warn`, `info` (the
  default), `debug`, `trace` or `off`. A filter such as `metaxy=debug` also
  works.
- `--log-json` (or `log_json = true`): JSON lines instead of text.

At `info` you see connections opening and closing (with the cause and the
duration), sessions created and forgotten, joins, leaves, `Q` removals, and
every `E` the relay sends, with its detail text. At `debug` you also get
per-peer frame and byte counts every 60 seconds and at disconnect, rate
limit drops, and queue high-water marks.

```
2026-10-22T12:33:40.611Z  INFO session created conn=1 code=SJA37C peer=1 host_addr=203.0.113.7 host_name="Ann" listener_port=4141 mode="plain"
2026-10-22T12:33:41.002Z  INFO join conn=2 code=SJA37C peer=2 name="Bob" addr=198.51.100.4 kind="new"
2026-10-22T12:33:41.812Z  INFO E sent conn=3 addr=198.51.100.9 reason="unknown-code" detail="no session has code ZZZZZZ"
2026-10-22T12:35:02.120Z  INFO leave conn=2 code=SJA37C peer=2 name="Bob" addr=198.51.100.4 how="drop" cause="idle timeout (60 s idle)"
2026-10-22T12:35:02.121Z  INFO connection closed conn=2 code=SJA37C peer=2 addr=198.51.100.4 duration_s=61.119 cause="idle timeout (60 s idle)"
2026-10-22T12:36:00.000Z DEBUG peer traffic conn=1 code=SJA37C peer=1 addr=203.0.113.7 frames_in=480 bytes_in=61250 frames_out=455 bytes_out=58120 d_dropped=0 d_dropped_bytes=0 queue_high=4310
```

The relay never logs message contents (`D` keys and bytes) or session
tokens. It does log player names and IP addresses. Keep logs 7 days with
your log tool (journald, logrotate).

`E` frames carry a short text after the reason word, for example
`E full the session has 16 of 16 players`. The list is in section 7 of the
spec.

## Self-hosting

Players need only the relay's host name and port in eidos. Open the port in
your firewall (4141, or 443 with TLS).

### systemd

```ini
[Unit]
Description=metaxy relay
After=network-online.target

[Service]
ExecStart=/usr/local/bin/metaxy --config /etc/metaxy.toml
Restart=on-failure
DynamicUser=yes
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

`CAP_NET_BIND_SERVICE` lets the relay bind port 443 without root.

For Mystery Gift, copy the repository's `data` folder to the server (for
example `/usr/local/share/metaxy/data`) and set
`events = "/usr/local/share/metaxy/data/mystery_events.json"` in
`metaxy.toml`. The default path, `data/mystery_events.json`, is relative to
the working directory. The Docker image has the folder built in.

### Docker

```
docker build -t metaxy .
docker run -d --restart unless-stopped -p 4141:4141 metaxy
```

For TLS, mount the certificate and key and pass the flags:

```
docker run -d -p 443:443 -v /etc/letsencrypt/live/example.org:/certs:ro metaxy   --tls-cert /certs/fullchain.pem --tls-key /certs/privkey.pem
```

### The world store in a container

The world store must outlive the container. Never keep it in the
container's own layer, which a redeploy throws away.

Docker, with a named volume:

```
docker volume create metaxy-worlds
docker run -d --restart unless-stopped -p 4141:4141 \
  -v metaxy-worlds:/var/lib/metaxy/worlds metaxy --world-store /var/lib/metaxy/worlds
```

systemd: add `StateDirectory=metaxy/worlds` to the unit and
`world_store = "/var/lib/metaxy/worlds"` to `metaxy.toml`.

Railway (the public relay):

- Attach a Railway Volume to the metaxy service, mounted at `/data`.
- Set the variable `METAXY_WORLD_STORE=/data/worlds`.
- The image runs as the non-root user `metaxy`, and Railway mounts the
  volume owned by root. Set `RAILWAY_RUN_UID=0` on the service, or give the
  folder to the user `metaxy`.
- Until the volume and `RAILWAY_RUN_UID` exist, `/data/worlds` cannot be
  made or written. metaxy then logs an error and uses the temp folder:
  world sessions work, but worlds are lost at the next redeploy. Check the
  start log for "world store loaded" with `persistent=true`.
- A service with a volume runs as one instance. That fits metaxy: two
  processes must never share a store.
- Start the volume at 1 GB. A world takes tens of KB plus its log.

The store is plain files written with sync and rename, so copying the
folder while metaxy runs is a usable backup.

## License

MIT (`LICENSE`).

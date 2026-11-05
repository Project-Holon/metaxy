# metaxy

The relay server for multiplayer in
[eidos](https://github.com/Project-Holon/eidos), the native PC port of
Pokémon Black and White.

Players connect out to the relay, so nobody has to open ports or use a VPN.
The host opens a session and gets a short code; the others join with it.
The relay forwards messages between the players and keeps nothing. It holds
no game code and no game data.

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

The Wonder Card files are in `data/cards`, named by their SHA-256; the
relay checks each one against the list when it starts. eidos asks the relay
which events are up on the player's date (a `G` frame, docs/SPEC.md section
4b) and gets each one with its card. Every event comes back every month: it
starts on the day of the month it first started and stays up for a week.
`--events` names another list, and `--events ""` turns this off.

### Sources

- Event dates, methods and gifts: Bulbapedia's lists of Generation V event
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

## License

MIT (`LICENSE`).

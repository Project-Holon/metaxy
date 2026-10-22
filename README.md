# metaxy

The relay server for multiplayer in
[eidos](https://github.com/Project-Holon/eidos), the native PC port of
Pokémon Black and White.

Players connect out to the relay, so nobody has to open ports or use a VPN.
The host opens a session and gets a short code; the others join with it.
The relay forwards messages between the players and keeps nothing. It holds
no game code and no game data.

The protocol and the multiplayer design are in [docs/SPEC.md](docs/SPEC.md).

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
```

The relay logs connections, sessions and errors, with the IP address, to
standard error. It never logs message contents. Keep logs 7 days with your
log tool (journald, logrotate).

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

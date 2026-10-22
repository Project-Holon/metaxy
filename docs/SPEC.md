# metaxy: relay specification

Status: agreed with eidos (review of 2026-10-22). Protocol version 1 is the one eidos's
client speaks today (`eidos-net/1`, eidos `sdk/mod_net.c` and
`docs/mod-services.md`, "Peer sessions"). This document is the relay's side
of it, plus the rules the client relies on and the limits the relay applies.

## 1. What the relay does

- A player's game connects out to the relay. Nobody opens a port.
- One player hosts a session and gets a short code. Other players join with
  the code.
- The relay forwards each mod's messages between the players of one
  session. It does not read them, keep them, or know what game data they
  carry.
- It holds no game code and no game data.

It does not: run game logic, store saves or sessions, match players with
strangers, keep accounts, or keep message contents. It keeps nothing on
disk except its logs: a session exists in memory only. The host's save is
the source of truth for the session (section 5).

## 2. Transport

- One TCP connection per player, kept for the whole session.
- Plain TCP on port 4141 now. eidos's client speaks plain TCP, so the
  public relay accepts it.
- TLS later: eidos will bundle mbedTLS 3.6 with the Let's Encrypt roots
  built in. Then the public relay also serves TLS 1.3 on port 443 (so
  school and work firewalls that allow web traffic allow the relay), with a
  certificate for its domain. Protocol 1 does not change, only the port and
  the transport. The relay serves either mode, or both on two ports.

## 3. Frames

A connection carries a stream of frames in both directions:

```
<type> <length>\n<payload>
```

- `type`: one ASCII letter.
- `length`: the payload's length in bytes, in decimal ASCII, no sign, no
  leading zeros (except `0` itself).
- `payload`: exactly `length` bytes.

The relay refuses a frame (with `E bad-frame`, then closes) when the header
is longer than 16 bytes before the newline, the type is unknown, the length
is not a decimal number, or the length is over the limit in section 6.

### Client to relay

| Type | Payload | Meaning |
|---|---|---|
| `H` | `<version> <token> <name>` | host a new session |
| `R` | `<version> <token> <code> <name>` | reopen a session with the code the host's save keeps |
| `J` | `<version> <token> <code> <name>` | join a session |
| `D` | `<peer>\n<key>\n<bytes>` | a mod's message: to one peer, or to every other peer when `peer` is 0 |
| `B` | empty | leave the session |
| `K` | empty | keep-alive: sent after 20 seconds without any other frame; the relay never answers it |
| `Q` | `<peer>` | remove a player (the host only) |

### Relay to client

| Type | Payload | Meaning |
|---|---|---|
| `C` | `<code>` | the session's code (to the host only, before `W`, after `H` or `R`) |
| `W` | `<peer> <id> <name>` | you are in: your peer number, your player id and the name the relay kept |
| `P` | `<peer> <id> <name>` | a peer joined (also sent once for each player already in the session, after `W`) |
| `X` | `<peer>` | a peer left or was dropped |
| `D` | `<peer>\n<key>\n<bytes>` | a mod's message; `peer` is the sender |
| `E` | `<reason>` | an error; the relay closes the connection after it |

Every frame from the relay has a non-empty payload. Released eidos clients
treat a 0-length frame as a broken stream. The relay accepts 0-length
payloads from clients (`B`, `K`).

### Fields

- `version`: the protocol's name and version, exactly `eidos-net/1`. The
  relay refuses any other with `E version`.
- `token`: the client's identity in one session, 16 to 64 characters of
  `[0-9a-f]`. It lets a dropped player rejoin as the same peer. eidos makes
  32 hex digits from the OS random source, one per session code, kept in
  its settings, so the relay cannot link a player across sessions and a
  restarted game rejoins as the same peer.
- `name`: the display name, the rest of the payload after the fields before
  it, so it may contain spaces. 1 to 64 bytes of UTF-8, no control
  characters. The relay trims it to 64 bytes on a character boundary.
- `code`: see section 4.
- `id`: the player's id, 16 hex digits: the first 8 bytes of
  SHA-256(`<code>:<token>`). It is the same every time the same token joins
  the same code, so the host's save can keep its roster by id without the
  relay keeping anything, and players can check that peer 1 is the host
  their save knows. The token itself is never sent to other players.
- `peer`: a decimal number. Peers are 1 to 16; 0 means "everyone else" in a
  client's `D` and never names a player.
- `key`: the mod's namespace, 1 to 96 bytes, no newline. The relay forwards
  it unchanged; the client delivers a message only to the mod with the same
  key. An empty key is `E bad-frame`.
- `bytes`: the mod's message, 0 to 4096 bytes.

## 4. Sessions and codes

- A code is 6 characters from `23456789ABCDEFGHJKMNPQRSTUVWXYZ` (31
  characters, no 0/O, 1/I/L): about 887 million codes. Codes are not case
  sensitive: the relay upper-cases a code before it looks it up.
- `H` creates a session with a new code and makes the sender peer 1, the
  host. The relay answers `C <code>`, then `W 1 <id> <name>`. The host's
  save keeps the code.
- `R` reopens a session with the code the host's save keeps. If no session
  with that code is in memory, the relay creates it with that code and the
  sender as peer 1. If one is, and its peer 1 has the same token, the
  sender takes peer 1 back (a rejoin). Otherwise the relay answers
  `E code-taken`.
- A `J` with a code in memory and a free slot makes the player the lowest
  free peer number. The relay answers `W`, then a `P` for each player
  already in, and sends `P` for the
  new player to everyone else. A `J` with a code that is not in memory gets
  `E unknown-code` (the host has not opened it yet).
- At most 16 players in a session (eidos draws up to 16 avatars).
- A `D` to a peer number that is not in the session (it may have just
  left), or to the sender itself, is dropped silently. A `D` to peer 0 goes
  to every other player, never back to the sender.

## 5. The host's save is the truth

A session belongs to one multiplayer save, and the host's copy of it is the
source of truth, as in a Minecraft world opened to LAN. The host's save
keeps the session's code, its roster (player ids and names) and any data
the mods share. The relay only forwards and keeps nothing once everyone has
gone. To play again later, the host opens the save (`R` with its code), the
others pick their copy of it and join, and the host starts the game when
everyone on the roster is back.

The relay makes no decisions about the game. What it does, in memory:

- **Peer numbers:** a token keeps its number while the session is in
  memory, so a rejoin gets it back. After the session leaves memory, the
  next time it opens numbers can differ; player ids do not. Mods and the
  host key their data by id.
- **Joins and leaves:** it tells everyone with `P` and `X`. A drop is a
  connection that closes without `B`, or sends nothing (not even `K`) for
  60 seconds.
- **Forwarding:** `D` messages, in order, to one player or to everyone else.
- **Removing a player:** only peer 1 (the host) may send `Q <peer>`. The
  relay sends that player `E removed`, closes the connection, and sends
  `X <peer>` to the others. A `Q` from anyone else is ignored.
- **Leaving memory:** a session is forgotten 10 minutes after the last
  player disconnects. Nothing about it is written to disk.

What the host's game decides (eidos), and tells the others with `D`
messages under the port's own key:

- the lobby: who is connected, who is missing from the roster;
- the roster: set at the first start; ids not on it are removed with `Q`;
- starting: only when every roster id is connected;
- back to the lobby: when any player leaves, drops or rejoins during a
  game; every game pauses until the host starts again;
- the mods' shared data: kept in the host's save, sent to a joining player,
  and changed only by the host, which applies the other players' requests
  in the order it receives them, so there is one copy and no conflicts.

## 6. Limits

| Limit | Value | When reached |
|---|---|---|
| Frame payload | 4096 + 128 bytes | `E too-big`, close |
| Players in a session | 16 | `E full` on `J` |
| Sessions a connection can be in | 1 | `E bad-state` on a second `H` or `J` |
| Messages per client | 60 per second, bursts of 120 | excess `D` frames are dropped silently |
| Bytes per client | 256 KB per second | excess `D` frames are dropped silently |
| Sustained excess | over 4 times either rate for 5 seconds | `E rate`, close |
| Relay to a slow client: queued bytes | 1 MB | `E too-slow`, close (a drop) |
| New connections per IP address | 10 per minute | the TCP connection is refused |
| Open sessions per IP address | 4 | `E busy` on `H` |
| Handshake time (connect to `H` or `J`) | 10 seconds | close |

All limits are settings; the values above are a public relay's defaults.
Position updates every step (about 8 per second) fit well inside them.

## 7. Errors

`E` reasons are one word, then optional text:

| Reason | Meaning |
|---|---|
| `version` | the client's protocol version is not one the relay speaks |
| `unknown-code` | no session has that code |
| `full` | the session has 16 players |
| `code-taken` | `R` with a code another host has open |
| `removed` | the host removed this player |
| `bad-frame` | a frame the relay cannot read |
| `bad-state` | a frame that does not fit now (for example `D` before `W`) |
| `too-big` | a frame over the size limit |
| `rate` | over the message or byte rate |
| `too-slow` | the client does not read fast enough |
| `busy` | too many sessions from this address |
| `shutdown` | the relay is stopping |

## 8. Privacy and operation

- The relay never writes message contents (`D` bytes and keys) to logs or
  disk.
- Logs hold: connection opened and closed, session created and expired,
  errors sent, with the IP address. Logs are kept 7 days.
- A stats page (counts only: sessions, players, bytes per second) is
  optional and off by default.
- Players never see each other's IP addresses.
- A self-hosted relay is one binary with a config file or command-line
  flags: bind address, ports, TLS certificate paths or automatic
  certificates for a domain, and the limits in section 6.

## 9. Decisions with eidos (2026-10-22)

1. TLS: plain TCP now; eidos bundles mbedTLS 3.6 (hash-checked, Let's
   Encrypt ISRG Root X1 and X2 built in, optional system store) later.
2. Keep-alive: `K`, empty, client to relay only, after 20 seconds of client
   silence; part of version 1 because no relay had shipped.
3. Token: 32 hex digits from the OS random source, per session code.
4. Spectators: a later `eidos-net/2` adds `S eidos-net/2 <code>`, a
   read-only join with no peer number that receives every `D` with the
   sender's number, off unless the host allows it. Until then a mod
   forwards events with `D`.
5. Peer 0 is every other player, never the sender.

eidos's client changes for this spec: accept 0-length frames, send `K`,
the random per-code token, refuse an empty key, and clean up typed codes
(strip spaces and dashes, upper-case).

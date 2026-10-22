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

It does not: run game logic, store saves, match players with strangers,
keep accounts, or keep message contents.

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
| `J` | `<version> <token> <code> <name>` | join a session |
| `D` | `<peer>\n<key>\n<bytes>` | a mod's message: to one peer, or to every other peer when `peer` is 0 |
| `B` | empty | leave the session |
| `K` | empty | keep-alive: sent after 20 seconds without any other frame; the relay never answers it |
| `G` | `go` | start the game (the leader only, see section 5) |

### Relay to client

| Type | Payload | Meaning |
|---|---|---|
| `C` | `<code>` | the new session's code (to the host only, before `W`) |
| `W` | `<peer> <name>` | you are in: your peer number and the name the relay kept |
| `P` | `<peer> <name>` | a peer joined (also sent once for each player already in the session, after `W`) |
| `X` | `<peer>` | a peer left or was dropped |
| `D` | `<peer>\n<key>\n<bytes>` | a mod's message; `peer` is the sender |
| `G` | `go` | the game started |
| `L` | `lobby`, `back`, `not-leader` or `waiting <peer>...` | the session is in the lobby (`lobby`: on join; `back`: the game stopped because a player left, dropped or rejoined; `not-leader`: a `G` from someone other than the leader; `waiting`: a `G` while roster players are missing, with their numbers) |
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
- The host's `H` creates the session and makes the host peer 1. The relay
  answers `C <code>`, then `W 1 <name>`.
- A `J` with a known code and a free slot makes the player the lowest free
  peer number. The relay answers `W`, then a `P` for each player already
  in, and sends `P` for the new player to everyone else.
- The host has no special role after the session starts. The session lives
  as long as section 5 says (it is kept on disk), so players can come back
  to the same code.
- At most 16 players in a session (eidos draws up to 16 avatars).
- A `D` to a peer number that is not in the session (it may have just
  left), or to the sender itself, is dropped silently. A `D` to peer 0 goes
  to every other player, never back to the sender.

## 5. Lobby, leaving and rejoining

A session belongs to one multiplayer save. Its code is the save's
permanent id: to play again later, each player picks that save, lands in its
lobby, and the game starts when all of its players are back. Nothing is lost
when a player leaves: the session stays, and the player can come back.

- **Peer numbers are kept for the whole session.** A token keeps its
  number for as long as the session exists. A player who leaves with `B` or
  drops gets the same number back when they join the same code again.
  Numbers are keyed by (code, token). If the old connection is still open,
  the relay closes it first (no `X`, the player is the same).
- **A drop:** a connection that closes without `B`, or sends nothing (not
  even `K`) for 60 seconds. It counts as leaving.
- **States:** `lobby` (a new session starts here) and `game`.
- **The leader:** the connected player with the lowest peer number (the
  host, while the host is there).
- **The roster:** the first start locks the roster to the players
  connected at that moment. Before that, new players join freely in the
  lobby (up to 16). After it, a new token gets `E locked`, and the numbers
  of players who joined but were not there at the first start are released.
- **Starting:** the leader sends `G go` in the lobby. Before the roster is
  locked, the game starts with whoever is connected and locks them. After
  that, it starts only when every roster player is connected; otherwise the
  relay answers the leader `L waiting <peer> <peer> ...` (the missing
  numbers) and stays in the lobby. On a start the relay sets the state to
  `game` and sends `G go` to every connected player, the leader included. A `G` from anyone else gets `L not-leader`, to that player only.
  A `G` during a game is ignored.
- **Back to the lobby:** any roster change during a game (a player leaves,
  drops, or rejoins) sends everyone back to the lobby. The relay sends `X`
  (leave or drop) or `P` (rejoin) as usual, then `L back` to every
  connected player, and the state becomes `lobby`. The players start again
  when the leader sends `G`.
- **In the lobby,** players join and leave freely: `P` and `X` only.
- **Joining:** a new token may join in the lobby before the roster is
  locked, while fewer than 16 numbers are taken (kept numbers count). A
  known token may rejoin at any time.
- **On join,** after `W` and the `P` frames, the relay sends the state:
  `L lobby` or `G go`.
- `D` messages pass in both states, so mods can chat in the lobby.
- **Persistence:** the relay keeps sessions on disk, so they survive a
  relay restart. It stores only the code, the created and last-used times,
  the roster (number, token, last name) and the lock. When nobody is
  connected the session is in the lobby.
- **Expiry:** a session ends after 365 days without any connection (a
  setting).

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
| `locked` | a new player tried to join after the roster was locked |
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

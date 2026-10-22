# metaxy: specification

Part A is the relay: protocol version 1 (`eidos-net/1`), which eidos's
client speaks (eidos `sdk/mod_net.c`, `docs/mod-services.md`, "Peer
sessions"), and the limits the relay applies. Part B is the multiplayer
design the game follows on top of it (eidos's side). Both were agreed on
2026-10-22.

# Part A: the relay

## 1. What the relay does

- A player's game connects out to the relay. Nobody opens a port.
- The host opens a session and gets a short code. Other players join with
  the code.
- The relay forwards messages between the players of one session. It does
  not read them, keep them, or know what game data they carry.
- It holds no game code and no game data.

It does not: run game logic, store saves or sessions, match players with
strangers, keep accounts, or keep message contents. It keeps nothing on
disk except its logs: a session exists in memory only. The host's save is
the source of truth for the session (Part B).

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
| `D` | `<peer>\n<key>\n<bytes>` | a message: to one peer, or to every other peer when `peer` is 0 |
| `B` | empty | leave the session |
| `K` | empty | keep-alive: sent after 20 seconds without any other frame; the relay never answers it |
| `Q` | `<peer>` | remove a player (the host only) |

### Relay to client

| Type | Payload | Meaning |
|---|---|---|
| `C` | `<code>` | the session's code (to the host only, before `W`) |
| `W` | `<peer> <name>` | you are in: your peer number and the name the relay kept |
| `P` | `<peer> <name>` | a peer joined (also sent once for each player already in the session, after `W`) |
| `X` | `<peer>` | a peer left or was dropped |
| `D` | `<peer>\n<key>\n<bytes>` | a message; `peer` is the sender |
| `E` | `<reason>` | an error; the relay closes the connection after it |

Every frame from the relay has a non-empty payload. Released eidos clients
treat a 0-length frame as a broken stream. The relay accepts 0-length
payloads from clients (`B`, `K`).

### Fields

- `version`: the protocol's name and version, exactly `eidos-net/1`. The
  relay refuses any other with `E version`.
- `token`: a throwaway identity for one session, 16 to 64 characters of
  `[0-9a-f]`. It lets a dropped player (the host included) rejoin the same
  session as the same peer. eidos makes 32 hex digits from the OS random
  source for each session. A player's lasting identity is not the relay's
  business (Part B, section 12).
- `name`: the display name, the rest of the payload after the fields before
  it, so it may contain spaces. 1 to 64 bytes of UTF-8, no control
  characters. The relay trims it to 64 bytes on a character boundary.
- `code`: see section 4.
- `peer`: a decimal number. Peers are 1 to 16; 0 means "everyone else" in a
  client's `D` and never names a player.
- `key`: the namespace of the mod (or of the port) that sent the message, 1
  to 96 bytes, no newline. The relay forwards it unchanged; the client
  delivers a message only to the mod with the same key. An empty key is
  `E bad-frame`.
- `bytes`: the message, 0 to 4096 bytes.

## 4. Sessions and codes

- A code is 6 characters from `23456789ABCDEFGHJKMNPQRSTUVWXYZ` (31
  characters, no 0/O, 1/I/L): about 887 million codes. Codes are not case
  sensitive: the relay upper-cases a code before it looks it up.
- `H` creates a session with a new code and makes the sender peer 1, the
  host. The relay answers `C <code>`, then `W 1 <name>`. A host gets a new
  code every time it opens a session.
- A `J` with a code in memory and a free slot makes the player the lowest
  free peer number. The relay answers `W`, then a `P` for each player
  already in, and sends `P` for the new player to everyone else. A `J` with
  a code that is not in memory gets `E unknown-code`.
- At most 16 players in a session (eidos draws up to 16 avatars). Most
  sessions are expected to have 2 or 3.
- A `D` to a peer number that is not in the session (it may have just
  left), or to the sender itself, is dropped silently. A `D` to peer 0 goes
  to every other player, never back to the sender.

## 5. What the relay does in a session

The relay makes no decisions about the game. In memory only:

- **Peer numbers:** a token keeps its number while the session is in
  memory, so a dropped player, the host included, rejoins with the same
  code and token and gets the same number back. If the old connection is
  still open, the relay closes it first (no `X`, the player is the same).
- **Joins and leaves:** it tells everyone with `P` and `X`. A drop is a
  connection that closes without `B`, or sends nothing (not even `K`) for
  60 seconds.
- **Forwarding:** `D` messages, in order, to one player or to everyone else.
- **Removing a player:** only peer 1 (the host) may send `Q <peer>`. The
  relay sends that player `E removed`, closes the connection, and sends
  `X <peer>` to the others. A `Q` from anyone else is ignored.
- **Leaving memory:** a session is forgotten 10 minutes after the last
  player disconnects. Nothing about it is written to disk.

## 6. Limits

| Limit | Value | When reached |
|---|---|---|
| Frame payload | 4096 + 128 bytes | `E too-big`, close |
| Players in a session | 16 | `E full` on `J` |
| Sessions a connection can be in | 1 | `E bad-state` on a second `H` or `J` |
| Messages per client | 60 per second, bursts of 120 | excess `D` frames are dropped silently |
| Bytes per client | 512 KB per second, bursts of 1 MB | excess `D` frames are dropped silently |
| Sustained excess | over 4 times either rate for 5 seconds | `E rate`, close |
| Relay to a slow client: queued bytes | 2 MB | `E too-slow`, close (a drop) |
| New connections per IP address | 10 per minute | the TCP connection is refused |
| Open sessions per IP address | 4 | `E busy` on `H` |
| Handshake time (connect to `H` or `J`) | 10 seconds | close |

All limits are settings; the values above are a public relay's defaults.
Position updates every step (about 8 per second) fit well inside them, and
a 512 KB save moves in about a second.

## 7. Errors

`E` reasons are one word, then optional text:

| Reason | Meaning |
|---|---|
| `version` | the client's protocol version is not one the relay speaks |
| `unknown-code` | no session has that code |
| `full` | the session has 16 players |
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
  flags: bind address, ports, TLS certificate paths, and the limits in
  section 6.

## 9. Notes

- TLS: plain TCP now; eidos bundles mbedTLS 3.6 (hash-checked, Let's
  Encrypt ISRG Root X1 and X2 built in, optional system store) later, at
  medium-high priority, well before a public launch.
- Keep-alive: `K`, empty, client to relay only, after 20 seconds of client
  silence.
- Spectators: a later `eidos-net/2` may add `S eidos-net/2 <code>`, a
  read-only join with no peer number that receives every `D`, off unless
  the host allows it.

# Part B: the multiplayer design (the game's side)

The relay only forwards. Every multiplayer decision is made by the host's
game, as in a Minecraft world opened to LAN. eidos implements this part;
the messages between the games are `D` frames under a key the port
reserves, documented by eidos.

## 10. The host's save is the truth

- A multiplayer save belongs to the host only. It holds the host's own game
  save, every other player's game save, each player's mod data, the roster,
  and the data mods share. Joiners keep no save.
- The host keeps automatic backup copies of the multiplayer save (the last
  few), because everyone's progress is in it.
- Each player's save records which game it belongs to (Black or White). A
  player joining with the other game is refused. Black and White players
  may play together.

## 11. Menu and save slots

The original main menu stays as it is.

- **Continue:** the game's own Continue card gets left and right to flip
  between single-player saves. With one save it looks exactly as before.
- **New Game:** as before, but writes to a free slot. The original "this
  will delete your save" warning appears only when every slot is full.
- **Slots:** 8 in all; multiplayer saves count toward the 8.
- **Multiplayer:** one extra line in the main menu, next to the game's own
  extras. It opens the host's multiplayer saves (host a new one, open one)
  and Join (type a code). Multiplayer saves appear only here.

## 12. Players

- **Joining:** a joiner picks Multiplayer, then Join, types the code the
  host gave them, and lands in the lobby. The host opens a new code each
  time and shares it (by chat, for example).
- **Identity:** after joining, a joiner sends the host a secret it keeps
  for that host (in its eidos settings, one per host). The host's save
  keeps only a fingerprint of it, and knows the player by it. A joiner who
  lost their secret (a reinstall) is given their roster place again by the
  host, as a new player.
- **The roster:** set at the first start. Players not on it are removed
  with `Q`. Adding a player after the first start: low priority, later.
- **First time:** a new player starts a new game (trainer, intro, starter);
  the save is created in the host's multiplayer save.

## 13. Lobby, start and pause

- The lobby shows who is connected and who on the roster is missing.
- The host starts the game only when every roster player is connected and
  every player's multiplayer mods are compatible. Each mod states a
  multiplayer compatibility number; only that has to match, so mods may
  update freely. The lobby names any missing or mismatched mod (players
  install it themselves; downloading mods from the host comes later).
- At the start, the host sends each joiner their save and their mod data,
  and each joiner's game boots straight into it.
- **A player drops mid-game:** the others wait 15 seconds. If the player is
  back within that time, play goes on. Otherwise every game pauses (the
  game is held, with a "waiting for players" screen) until the host starts
  again.
- **The host drops:** everyone stays paused while the host rejoins the same
  session within its 10 minutes. If the host does not come back, joiners
  return to the menu; progress since the last save is lost.

## 14. Saving

- Only the host saves; joiners have no save option.
- When the host saves, every player's game saves too: each joiner's game
  writes its save at its next safe moment (not in a battle or a menu) and
  sends it to the host. The host writes each upload to a temporary file and
  replaces the old one only when it is complete and its checksum matches,
  then confirms. The host's "saved" message waits until every player has
  confirmed.
- Saves move in 4 KB pieces with a checksum, and a broken transfer resumes.

## 15. Time

- Each player's game uses its own computer's clock (time of day and
  season), as on a real DS, so night Pokemon appear at each player's night.

## 16. Built in, without a mod

- **Positions:** every player appears on the others' maps (field avatars),
  always on in a session.
- **Events:** every player's game events are forwarded to the host, all of
  them, for mods to use.

## 17. For mods

- Each player's own events and choices (catches, faints, a Nuzlocke's
  rules) are decided by that player's game, at once.
- Data shared between players (Soul Link pairs, shared route records) is
  decided by the host. eidos provides a shared-table service: the host's
  copy is the real one; other players send requests, the host applies them
  in the order they arrive and sends out the result.
- Rules mods cannot use multiplayer for now; multiplayer mods are code
  mods.

## 18. Order of work (eidos)

1. Session client changes for this spec.
2. Lobby, start and pause.
3. Save slots and the menu.
4. Save transfer and host-only saving.
5. Built-in positions and event forwarding.
6. The shared-table service.
7. TLS.
8. Adding players after the first start.

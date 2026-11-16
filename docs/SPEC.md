# metaxy: specification

Part A is the relay: protocol version 1 (`eidos-net/1`), which eidos's
client speaks (eidos `sdk/mod_net.c`, `docs/mod-services.md`, "Peer
sessions"), and the limits the relay applies. Part B is the multiplayer
design the game follows on top of it (eidos's side). Both were agreed on
2026-10-22. Part C is world sessions: eidos-only arkhipelagos games that
metaxy makes, keeps on disk and routes (protocol `eidos-net/2`), approved on
2026-11-16. Part D is the Archipelago bridge (off by default).

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
the source of truth for the session (Part B). (World sessions, Part C, are
another role of the same service and do keep worlds on disk.)

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

The relay refuses a frame (with `E bad-frame` and a text, then closes) when the header
is longer than 16 bytes before the newline, the type is unknown, the length
is not a decimal number, or the length is over the limit in section 6.

### Client to relay

| Type | Payload | Meaning |
|---|---|---|
| `H` | `<version> <token> <name>` | host a new session |
| `J` | `<version> <token> <code> <name>` | join a session |
| `A` | `<version> <token> <key> <name>` | join the shared air: a public room of `key` (section 4a) |
| `D` | `<peer>\n<key>\n<bytes>` | a message: to one peer, or to every other peer when `peer` is 0 |
| `B` | empty | leave the session |
| `K` | empty | keep-alive: sent after 20 seconds without any other frame; the relay never answers it |
| `Q` | `<peer>` | remove a player (the host only) |
| `G` | `<version> <language> <game> <date>` | the Mystery Gift events up on that date (section 4b); not in a session |

### Relay to client

| Type | Payload | Meaning |
|---|---|---|
| `C` | `<code>` | the session's code (to the host only, before `W`) |
| `W` | `<peer> <name>` | you are in: your peer number and the name the relay kept |
| `P` | `<peer> <name>` | a peer joined (also sent once for each player already in the session, after `W`) |
| `X` | `<peer>` | a peer left or was dropped |
| `D` | `<peer>\n<key>\n<bytes>` | a message; `peer` is the sender |
| `E` | `<reason>` | an error; the relay closes the connection after it |
| `M` | `<method>\n<title>\n<gifts>\n<text>\n<card>` | one Mystery Gift event that is up (section 4b) |
| `Z` | `<count>` | the end of the events; the relay closes the connection after it |

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

## 4a. The shared air (public rooms)

eidos's Link play (the DS's local wireless: the C-Gear, the Union Room, the
Entralink) works like a DS's radio, with no codes: every game whose radio is
on is in the air, and metaxy stands for the range of the radio.

- `A <version> <token> <key> <name>` puts the player in a public room of
  `key`: the fullest room of that key with a free place, or a new one. A
  room holds `air_room` players (default 8), which stands for the radio's
  range. `key` is 1 to 64 characters of `A-Z a-z 0-9 . : _ -`; eidos sends
  its version and the activity (`bw:<version>:<activity>`), so players meet
  others on a compatible version doing the same thing.
- The answers are a join's: `C <room code>`, `W`, a `P` for each player in
  the room, and `P` to the others. From there on the room is a session:
  `D`, `B` and `K` work as in any session. Nobody hosts a room and `Q` does
  nothing in it.
- A player of the air sends `A` again to move to another room (another
  activity): the old room sees `X`, the new one `P`.
- A room keeps no places: a player who drops is gone, and a room with no
  players is forgotten like an empty session.

## 4b. Mystery Gift events

The relay keeps the Mystery Gift events of Black and White
(`data/mystery_events.json` and the Wonder Card files in `data/cards`) and
tells eidos which ones are up on a given day. This is a request on its own
connection, outside any session.

- `G <version> <language> <game> <date>`: `language` is the cartridge's
  (`en`, `fr`, `de`, `it`, `es`, `ja`, `ko`), `game` is `black` or `white`,
  `date` is the player's own date, `YYYY-MM-DD` (each player keeps their own
  clock, section 15).
- The relay answers with one `M` frame for each event up on that date for
  that language and game, then `Z` with how many it sent, and closes the
  connection.
- An `M` payload is the method (`wfc`, `wireless` or `infrared`), the
  title, the gifts (comma-separated) and the Wonder Card's text (its line
  breaks written as tabs) on four lines, then the Wonder Card's bytes (204
  bytes for Black and White). The text is the card's own for that language
  and, for an event with a card per Pokemon, for the card sent. For a wireless event the relay
  sends the card's local wireless copy when the archive has one.
- When is an event up: each time it ran (a run) comes back every month on
  the day of the month it first started and stays up for seven days. A run
  that started on the 31st starts on the last day of a shorter month.
- A relay without the list answers `E unavailable`. A `G` with bad fields
  is `E bad-frame`; a `G` inside a session is `E bad-state`.

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
- **Details:**
  - A drop keeps the player's number; `B` and `Q` free it. A removed player
    who joins again gets a new number.
  - Numbers held by dropped players count toward the 16, so a session can
    be `full` with fewer than 16 connected.
  - On a rejoin, the name sent replaces the stored one.
  - A `Q` naming an unknown peer, peer 1 or a peer over 16 is ignored. A
    malformed `Q` from the host is `E bad-frame`; a `Q` before joining is
    `E bad-state`.
  - The per-address session cap counts sessions in memory created from that
    address, including ones with nobody connected.
  - Only `D` frames count toward the rate limits.
  - After sending `E`, the relay reads and discards input for up to 1
    second before closing, so the client receives the `E`.

## 6. Limits

| Limit | Value | When reached |
|---|---|---|
| Frame payload | 4096 + 128 bytes | `E too-big`, close |
| Players in a session | 16 | `E full` on `J` |
| Sessions a connection can be in | 1 | `E bad-state` on a second `H` or `J` |
| Messages per client | 60 per second, bursts of 256 (a whole save in 4 KB pieces) | excess `D` frames are dropped silently |
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

An `E` payload is the reason word, then optionally a space and a short
plain text that says what was wrong. The reason word is always first and
never changes: clients match on it, and show or log the text. The text is
for people. Its wording may change, so a client must not parse it. It never
holds message contents, keys or tokens.

| Reason | Meaning | Example |
|---|---|---|
| `version` | the client's protocol version is not one the relay speaks | `E version got "eidos-net/2", this relay speaks eidos-net/1` |
| `unknown-code` | no session has that code | `E unknown-code no session has code ABC123` |
| `full` | the session has 16 players | `E full the session has 16 of 16 players` |
| `removed` | the host removed this player | `E removed by the host` |
| `bad-frame` | a frame the relay cannot read | `E bad-frame length "12a" is not a number` |
| `bad-state` | a frame that does not fit now (for example `D` before `W`) | `E bad-state D before joining` |
| `too-big` | a frame over the size limit | `E too-big frame of 9000 bytes (limit 4224)` |
| `rate` | over the message or byte rate | `E rate over 4x the message rate for 5 s (240 per second, limit 60)` |
| `too-slow` | the client does not read fast enough | `E too-slow 2097152 bytes queued (limit 2097152)` |
| `busy` | too many sessions from this address | `E busy this address has 4 open sessions (limit 4)` |
| `shutdown` | the relay is stopping | `E shutdown the relay is stopping` |
| `unavailable` | the relay has no Mystery Gift event list | `E unavailable this relay has no event list` |

World sessions add their own reasons (section 27).

The text is at most a few hundred bytes. A client that reads only the first
word keeps working.

## 8. Privacy and operation

- The relay never writes message contents (`D` keys and bytes), session
  tokens or anything else from inside a `D` frame to logs or disk. It logs
  player names and IP addresses.
- Logs go to standard error, one event per line, each with a UTC time (RFC
  3339, millisecond), a level and named fields. The session code, the peer
  number and the client address are fields wherever they apply. Text is the
  default; `--log-json` (or `log_json = true`) writes JSON lines instead.
  `--log-level` (or `log_level`) picks the level: `error`, `warn`, `info`
  (the default), `debug`, `trace` or `off`.
- At `info` the logs hold:
  - connection opened (address, plain or TLS) and closed (address, session
    and peer if any, duration, cause: `eof`, `reset`, `idle timeout`,
    `handshake timeout`, `removed by the host`, `left (B)`, `server
    shutdown`, `replaced by a rejoin`, or the reason word of an `E` the
    relay sent, such as `rate`, `too-slow` or `bad-frame`);
  - session created (code, host address and name, listener port and mode)
    and session forgotten (code, age, why);
  - join (code, peer, name, address, new or rejoin), leave (`B`, or a drop
    with its cause), and `Q` (the removed player, or why it was ignored);
  - every `E` sent (reason, the detail text, code, peer, address), which
    also covers refusals before a session (unknown code, full, busy,
    version);
  - connections refused for the per-address rate (count and window).
- At `debug` it also holds, per peer: frame and byte counts in and out
  every 60 seconds and when the connection ends, how many `D` frames the
  rate limits dropped and which limit, and the most bytes ever queued to the
  client.
- Logs are kept 7 days.
- A stats page (counts only: sessions, players, bytes per second) is
  optional and off by default.
- Players never see each other's IP addresses.
- World sessions keep worlds on disk; what they hold about people is in
  section 28.
- A self-hosted relay is one binary with a config file or command-line
  flags: bind address, ports, TLS certificate paths, log settings, and the
  limits in section 6.

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

# Part C: world sessions

World sessions run eidos-only arkhipelagos games (eidos's randomizer
played online, with no Archipelago server). They are a role of the same
service, on the same listeners, beside the relay. The design was approved
on 2026-11-16 (arkhipelagos `docs/metaxy-proposal.md`, Part C).

Unlike the relay, this role keeps state on disk: each world's
configuration and an append-only log of what happened in it. The relay's
rule in section 1 stays as it was for relay sessions.

## 19. What it does

- Makes a world from a game configuration with arkhipelagos' generator
  (`arkh-gen`, a Rust crate linked into metaxy, pinned to one commit).
- Keeps each world and its state on disk until it expires.
- Lets players join any time, without waiting for anyone. There is no host.
- On a join, sends the player's whole data at once, then every item for
  them.
- Records checks and gives each item to its receiver now, or keeps it until
  they join.
- Passes positions between the players of a world.

## 20. Frames

The frame format is the relay's (section 3). `N`, `O` and `I` carry the
version `eidos-net/2`; a relay that speaks only version 1 answers
`E bad-frame` (an unknown frame type), and this relay answers `E version`
to any other version. `H`, `J`, `A` and `G` keep `eidos-net/1`.

### Client to relay

| Type | Payload | Meaning |
|---|---|---|
| `N` | `<version> <token>\n<configuration JSON>` | make a world |
| `O` | `<version> <code> <owner key> <action>` | owner action: `start`, `close` or `delete` |
| `I` | `<version> <token> <code> <name or player key>` | join a world |
| `V` | `<location id> ...`, `goal`, or empty | checks, the goal reached, or no checks yet |
| `U` | `<zone> <x> <y> <z> <dir>` | this player's position |
| `F` | `<message id> <last 0/1>\n<bytes>` | a piece of a big payload |
| `B` | empty | leave the world |
| `K` | empty | keep-alive, as in a relay session |

### Relay to client

| Type | Payload | Meaning |
|---|---|---|
| `R` | `wait <place>` | the configuration waits in the queue at this place (1 is next) |
| `R` | `<code> <owner key>` | the world is made and stored |
| `O` | `<code> <state>` | the owner action is done: `started`, `closed` or `deleted` |
| `S` | `<player> <player key>\n<player data JSON>` | the push on join |
| `Y` | `<index> <item id> <from player> ...` | items for this player, three numbers each |
| `W` | `<player> <name>` | you are in |
| `P` | `<player> <name>` | a player came (also once for each player already in, after `W`) |
| `X` | `<player>` | a player left |
| `U` | `<player> <zone> <x> <y> <z> <dir>` | another player's position |
| `F` | `<message id> <last 0/1>\n<bytes>` | a piece of a big payload |
| `E` | `<reason> <text>` | an error, then the relay closes the connection |

### Fields

- `token`: as in `H` (16 to 64 characters of `0-9 a-f`). It is checked and
  not used further: the player key is what lets a player back in.
- `code`: 6 characters from the session-code alphabet (section 4), not case
  sensitive. World codes and session codes are separate.
- `owner key`, `player key`: 32 characters of `0-9 a-f`, from the OS random
  source. metaxy keeps only their SHA-256.
- `name or player key`: a player's name from the configuration, or the key
  metaxy gave on the first join. A field of 32 hex digits that is not a
  name is taken as a key. A configuration may not use a name that looks
  like a key.
- `player`: 1 to 16, the player's place in the configuration (player 1
  first), the same as the generator's `slot`.
- `location id`, `item id`: arkhipelagos' permanent numbers
  (arkhipelagos `docs/generator.md`). A `V` holds up to 4096 bytes; a game
  sends its checks in as many `V` frames as it needs.
- `zone`, `dir`: whole numbers from 0; `x`, `y`, `z`: whole numbers that
  may be negative.

### Big payloads

A configuration or a player's data can be larger than a frame. It then
travels as `F` frames with one message id, each with up to 4096 bytes, the
last one with `1`. The frame that uses it comes right after, with
`@<message id>` in place of its body: `N <version> <token>\n@7`,
`S <player> <key>\n@1`. A body that fits in one frame is sent in the frame
itself. Message ids are counted by each side on its own connection.

Limits: a configuration 64 KB, a player's data 1 MB. A client may hold at
most 64 KB of unused pieces; more is `E too-big`.

## 21. A world's life

1. **Make.** `N` sends the configuration (below). metaxy checks it and puts
   it in the queue. While it waits behind others, the client gets
   `R wait <place>` each time its place changes. When the world is made,
   metaxy stores it and answers `R <code> <owner key>`. Nobody can join
   yet.
2. **Start.** `O <version> <code> <owner key> start` makes it joinable. The
   answer is `O <code> started`.
3. **First join.** `I` with a name that is not claimed yet. metaxy makes a
   player key, stores its hash, and sends the push (below) with the new
   key. From now on that name is claimed: a join with it is
   `E name-taken`.
4. **Confirming the claim.** The claim is provisional until the game shows
   it kept the key: its first `V` (an empty `V` will do) or `U` on that
   connection, or a later join with the key. If the connection ends before
   that (a drop, `B`, a stop of metaxy), the claim is released: the name is
   free again and the old key no longer works. A game sends `V` with every
   check it has (or an empty `V`) right after it stored its key, on every
   connect. A game whose join with a stored key gets `E bad-key` (its claim
   was released before it confirmed) joins once more by its name and keeps
   the new key.
5. **Later joins.** `I` with the player key. The push carries the same key.
   A join while the same player's old connection is still open closes the
   old one first, without `X`.
6. **Play.** `V` checks and `U` positions (sections 22 and 23).
7. **Leave.** `B`, a closed connection, or 60 s without a frame. The others
   get `X <player>`. metaxy keeps the world and its state.
8. **Close.** `O ... close`: no more joins, by name or key. Players already
   in play on. `O ... start` opens it again.
9. **Delete.** `O ... delete`: the world is gone at once. Players in it get
   `E no-world`, and its folder is removed.

`N`, `O` and `I` are refused with `E bad-state` inside a relay session, and
`H`, `J`, `A`, `D` and `Q` inside a world session. A connection holds at
most one world session. A connection may send `N`, then `O`, then `I`.
After an accepted `N`, `O` or `I`, the 60 s idle limit applies instead of
the 10 s handshake limit.

### The configuration

It is arkhipelagos' configuration (`docs/generator.md` there), as JSON:

```json
{"schema": 1, "seed": 4242,
 "options": {"goal": "ghetsis"},
 "players": [{"name": "Ash", "version": "black"},
             {"name": "Misty", "version": "white"}]}
```

metaxy checks: valid JSON, an object, `schema` 1, 1 to 16 players, each
with a name of 1 to 64 bytes and no control characters, names all
different, at most 64 KB. When `seed` is missing, metaxy draws one. The
generator checks the rest (versions, options). Anything wrong is
`E gen-failed` with the reason.

### The push on join

In this order:

1. `S <player> <player key>\n<player data JSON>`, in `F` pieces when big;
2. `Y` frames with every item for this player, from index 1, in order;
3. `W <player> <name>`;
4. `P <player> <name>` for each other player connected now;
5. `U` with the last position of each other player connected now.

The other players get `P <player> <name>`.

The player data is a JSON object:

| Key | What |
|---|---|
| `world` | the world's code |
| `player` | the player's number |
| `players` | every player: `player`, `name`, `version` |
| `generator` | `crate`, `version` and `data_hash` of the generator |
| `seed` | the world's seed |
| `world_hash` | the world's hash |
| `data` | this player's part of the generated world (arkhipelagos `PlayerOut`: `slot`, `name`, `version`, `options`, `locations` as `[location, item, receiver]`, `left_out`, `starters`, `picks`, `tables`) |

Every join sends the whole item list again, from index 1. The game keeps
the last index it applied in its save and skips the items up to it, so a
repeat does no harm, and an item handed over just before a crash is never
lost.

## 22. Checks, items and goals

- `V <location id> ...`: for each location of this player's game that is
  not recorded yet, metaxy writes one log line and syncs the log to disk,
  then sends the item: to the receiver at once as `Y <index> <item> <from>`
  if they are connected, or with the next push when they join.
- Each receiver's items are numbered 1, 2, 3 ... in the order they were
  found. The number never changes.
- A check for the player's own item is recorded, but it is not put in any
  list and sends no `Y`: the game gave the item already.
- A location already recorded, a location twice in one frame, and a
  location that is not in the player's world are ignored. So a game sends
  every check it made again on every connect, and nothing is counted
  twice.
- An empty `V` records nothing; it confirms a new claim (section 21).
- `V goal`: the player reached the goal. It is recorded once. When every
  player of the world reached it, the world is finished.
- `V` has no answer. Lines are on disk before any `Y` they cause is sent.

## 23. Positions

- A `U` with another zone than the player's last one (or the player's first
  `U`) is a zone change. It goes to every other connected player of the
  world, for the Town Map.
- A `U` in the same zone is a tile position. It goes only to the other
  players whose last zone is the same, for field avatars.
- Tile positions are limited to 8 per second per player (bursts of 16); the
  excess is dropped. Zone changes are not dropped.
- metaxy keeps each connected player's last position in memory, so a joiner
  sees everyone at once (step 5 of the push). Positions are not written to
  disk. A player who leaves is forgotten (`X`).

## 24. Generation

- Worlds are made on a few threads of their own (`gen_workers`, default the
  number of cores minus one, at least 1), never on the threads that serve
  connections. The queue holds `gen_queue` configurations (64).
- A generation has a time limit (`gen_time_ms`, 10 s). It counts from when a
  thread takes the job, not while it waits. A generation over the limit
  gets `E gen-failed`; the thread finishes it and drops the result (a
  running generation cannot be stopped).
- The same configuration and seed give the same world. A world's hash is
  FNV-1a over its JSON, as `arkh-gen hash` prints it.

## 25. The store

- A folder per world, named by its code, under the store folder:
  - `world.json`, written once (through a temporary file and a rename): the
    format (`metaxy-world/1`), the code, the creation time, the generator
    (`crate`, `version`, `data_hash`), the seed, the world hash, the
    SHA-256 of the owner key, the configuration as checked on `N` (with its
    seed), and the players' names;
  - `log`, one JSON line per change, written and synced to disk before
    metaxy answers: `start`, `close`, `claim` (player and key hash),
    `confirm`, `release`, `join`, `check` (player, location, item,
    receiver, index), `goal`. Each line has its time.
- The generated world is not stored. When it is first needed (a join),
  metaxy makes it again with the stored configuration and checks the hash.
  The same hash: the world is served. Another hash, another generator
  version or data hash: the world is not served (`E gen-mismatch`), and the
  log says which world and which versions. A made world stays in memory
  while it has players and leaves 10 minutes after the last one
  (`world_unload_ms`).
- At start, metaxy reads every folder and rebuilds each world's state from
  its log. A claim with no `confirm` line is released (with a `release`
  line): its connection ended with the last run. A cut last line (a crash during a write) is cut off the file.
  A folder that does not read is logged and left alone. A log with many
  repeated joins is compacted (one join line per player kept).
- Expiry: a world is deleted after 90 days with no join
  (`world_idle_s`), or 30 days after every player reached the goal
  (`world_done_s`), never while a player is connected. Only the owner key
  deletes earlier.
- Shutting down: every line is on disk before it is answered, so a stop
  loses nothing that was confirmed. Clients send their checks again when
  they come back.

### Where the store is

- `--world-store <dir>` (or `world_store` in the file), then the
  environment variable `METAXY_WORLD_STORE`, then a temporary folder
  (`metaxy-worlds` in the system temp folder).
- A folder that cannot be made or written falls back to the temporary
  folder, with an error in the log. A temporary store is not persistent:
  worlds are lost when the service restarts or redeploys, and the log says
  so at start.
- If even the temporary folder cannot be written, world sessions are off
  and their frames get `E unavailable`. The relay always starts.
- `--world-store off` turns world sessions off.
- One store per metaxy process. Two processes must never share a store.

## 26. Limits

| Limit | Value | When reached |
|---|---|---|
| Generations per IP address | 6 per hour | `E busy` |
| Generations waiting | 64 | `E gen-busy` |
| Open worlds made from one IP address | 20 (closed and deleted ones do not count) | `E busy` |
| Players in a world | 16 (and at most `max_players`) | `E gen-failed` |
| Configuration | 64 KB | `E too-big` |
| Player data | 1 MB | `E gen-failed` |
| Tile positions | 8 per second, bursts of 16 | excess dropped |

The per-address counts are kept in memory only (no addresses on disk), so
they start again when metaxy restarts. `N`, `O`, `I`, `V`, `U` and `F`
frames count toward the client's rates of section 6 together with `D`
frames: they are never dropped, but a sustained excess is `E rate`.

## 27. Errors

| Reason | Meaning |
|---|---|
| `gen-failed` | the configuration cannot make a world (the text says why) |
| `gen-busy` | the generation queue is full |
| `no-world` | no world has that code (or it was deleted) |
| `not-started` | the world is not joinable yet |
| `closed` | the owner closed the world |
| `not-owner` | the owner key is wrong |
| `name-taken` | that player is claimed; join with the player key |
| `bad-key` | no player of the world has that key (or that name) |
| `gen-mismatch` | the world made again does not match its stored hash or generator |
| `unavailable` | world sessions are off on this relay, or the store cannot be written |

`version`, `bad-frame`, `bad-state`, `too-big`, `busy` and `rate` keep their
meaning from section 7.

## 28. Privacy

The store holds the names in the configuration, the SHA-256 of the owner
and player keys, and the times of joins and checks. It holds no addresses
and no keys. The logs hold world codes, player numbers and names, and
addresses, as for the relay; never keys.

## 29. Decisions where the proposal was silent

- The answer to `O` is a relay `O <code> <state>`. `V` has no answer.
- A name that is not in the world is `bad-key` (there is no reason word for
  it in the proposal).
- The push order is `S`, `Y`, `W`, `P`, `U`; every join resends the whole
  item list from index 1.
- Own items are recorded as checks but are not numbered.
- `start` on a closed world opens it again.
- The proposal's `gen-v0.1.0` tag does not exist yet; metaxy pins
  arkhipelagos by commit (`rev`) in `Cargo.toml`.
- A world made again for a join goes to the front of the queue and is not
  refused by the queue limit; its time limit is three times `gen_time_ms`.
- Without a store setting, metaxy uses a temporary folder and warns, and an
  unusable folder falls back to it (the proposal said to refuse the world
  role; the operator asked for this so that world sessions can be tried
  before the volume exists).
- A claim is provisional until the game's first `V` or `U` (or a join with
  the key), and is released if the connection ends before that. So a drop
  between the claim and the game storing its key does not lock the player
  out. An empty `V` is allowed for this, so a game with no checks yet can
  confirm too. There is no separate confirm frame.

# Part D: the Archipelago bridge

The bridge lets an eidos game play in an Archipelago multiworld through
metaxy: one outbound WebSocket per bridged player to an Archipelago server.
The design was approved on 2026-11-16 (arkhipelagos
`docs/metaxy-proposal.md`, Part D). It is off unless the operator turns it
on (`--bridge`, below).

## 30. What it does

- Opens one WebSocket (`ws` or `wss`) per bridged player to an Archipelago
  server, and passes Archipelago messages (JSON text) both ways as `T`
  frames.
- Reads nothing in them, except the server's first message: it must be a
  JSON array whose first command is `RoomInfo`. A server that does not
  answer the WebSocket upgrade, or sends something else first, is closed
  (no open proxy). Nothing the player sends passes before that.
- Keeps nothing. The Archipelago server sees metaxy's address, not the
  player's.

## 31. Frames

| Direction | Type | Payload | Meaning |
|---|---|---|---|
| client | `L` | `<version> <token>\n<link>` | open the link |
| relay | `L` | `ok` | linked: the server sent `RoomInfo` |
| both | `T` | `<JSON text>` or `@<message id>` | one Archipelago message |
| both | `F` | `<message id> <last 0/1>\n<bytes>` | a piece of a big message (section 20) |

- `version` is `eidos-net/2`; `token` as in section 20.
- After `L ok` the first `T` is the server's `RoomInfo`, as it came. Then
  every message, in order.
- A message that does not fit in a frame travels as `F` pieces, then
  `T @<message id>`. Messages are at most 2 MB either way.
- metaxy answers a server's ping with a pong.

### The link

| Form | Example | What metaxy does |
|---|---|---|
| room link | `https://archipelago.gg/room/<id>` (the scheme may be left out) | opens the room page (it wakes a sleeping room), reads `/api/room_status/<id>` for `last_port`, then connects to `wss://<host>:<last_port>` |
| host and port | `archipelago.gg:38281` | `wss://`, then `ws://` if that fails |
| WebSocket URL | `ws://host:port`, `wss://host:port/path` | exactly that |

An `http://` room link uses `ws://` for the room (for local servers).

## 32. Rules

- One link per connection. `L` is refused in a world session, `I` with a
  link, and a second `L` (`E bad-state`). A relay session and a link may
  share a connection (a co-op host in Archipelago mode).
- The link is set up within 15 s (`bridge_setup_ms`): connect, upgrade,
  `RoomInfo`. When a link form has two tries (`wss`, then `ws`), the first
  has a third of that time.
- By default the bridge connects only to public addresses: loopback,
  private, link-local and other special ranges are refused
  (`bridge_private` lets a test or a private network use them).
- When the server closes the link, or it breaks, the player gets
  `E ap-closed` and the connection closes. When the player leaves, metaxy
  closes the WebSocket.
- A message to the player waits while the player's queue is over half full,
  instead of dropping or closing; after 30 s the link ends.

## 33. Limits and errors

| Limit | Value | When reached |
|---|---|---|
| Bridge links per IP address | 4 | `E busy` |
| Archipelago message | 2 MB, either way | `E ap-closed` (from the server), `E too-big` (from the player) |
| Link set-up time | 15 s | `E ap-unreachable` |

| Reason | Meaning |
|---|---|
| `ap-unreachable` | no connection, no WebSocket, or no `RoomInfo` in time |
| `ap-closed` | the server closed the link, or it broke |
| `unavailable` | the bridge is off on this relay |

## 34. Settings

`bridge` (off), `bridge_per_ip` (4), `bridge_setup_ms` (15000),
`bridge_msg_max` (2097152), `bridge_private` (off). The public relay keeps
the bridge off until archipelago.gg's operators have been asked about many
players arriving from one address (proposal D6).

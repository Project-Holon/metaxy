# metaxy: relay specification

Status: draft for review with eidos. Protocol version 1 is the one eidos's
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
- Public relays use TLS (TLS 1.3, certificate for the relay's domain, for
  example from Let's Encrypt). A relay for a LAN or for testing may run
  without TLS. The relay serves either mode, or both on two ports.
- Default port: 443 with TLS, so school and work firewalls that allow web
  traffic allow the relay too; 4141 without TLS. Both are settings.
- See section 9, question 1: eidos's client speaks plain TCP today.

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

### Relay to client

| Type | Payload | Meaning |
|---|---|---|
| `C` | `<code>` | the new session's code (to the host only, before `W`) |
| `W` | `<peer> <name>` | you are in: your peer number and the name the relay kept |
| `P` | `<peer> <name>` | a peer joined (also sent once for each player already in the session, after `W`) |
| `X` | `<peer>` | a peer left or was dropped |
| `D` | `<peer>\n<key>\n<bytes>` | a mod's message; `peer` is the sender |
| `E` | `<reason>` | an error; the relay closes the connection after it |

### Fields

- `version`: the protocol version, the decimal `1`. The relay refuses any
  other with `E version`.
- `token`: the client's identity for this session, 16 to 64 characters of
  `[0-9a-f]`. It lets a dropped player rejoin as the same peer.
- `name`: the display name, the rest of the payload after the fields before
  it, so it may contain spaces. 1 to 64 bytes of UTF-8, no control
  characters. The relay trims it to 64 bytes on a character boundary.
- `code`: see section 4.
- `peer`: a decimal number. Peers are 1 to 16; 0 means "everyone else" in a
  client's `D` and never names a player.
- `key`: the mod's namespace, 1 to 96 bytes, no newline. The relay forwards
  it unchanged; the client delivers a message only to the mod with the same
  key.
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
  while at least one player is connected, and for 10 minutes after the last
  one leaves or drops, so players can come back to the same code.
- At most 16 players in a session (eidos draws up to 16 avatars).

## 5. Leaving, drops and rejoining

- `B` from a client: the relay sends `X <peer>` to the others and closes the
  connection. The peer number is freed.
- A connection that closes without `B`, or sends nothing for 60 seconds:
  a drop. The relay sends `X <peer>` to the others and keeps the peer
  number reserved for that token for 10 minutes.
- A `J` with the same code and the same token within those 10 minutes gets
  the same peer number back. If the old connection is still open, the relay
  closes it first (no `X`, the player is the same).
- After 10 minutes the reserved number is freed.

Keeping a connection alive: see section 9, question 2.

## 6. Limits

| Limit | Value | When reached |
|---|---|---|
| Frame payload | 4096 + 128 bytes | `E too-big`, close |
| Players in a session | 16 | `E full` on `J` |
| Sessions a connection can be in | 1 | `E bad-state` on a second `H` or `J` |
| Messages per client | 60 per second, bursts of 120 | `E rate`, close |
| Bytes per client | 256 KB per second | `E rate`, close |
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

## 9. Open questions for eidos

1. **TLS in the client.** eidos's transport is plain TCP. Options: (a) the
   client gains TLS through the OS (Schannel on Windows, Secure Transport on
   macOS, OpenSSL or GnuTLS on Linux); (b) a bundled small TLS library
   (mbedTLS); (c) protocol 1 stays plain and the public relay accepts it
   until (a) or (b) lands. The relay supports all three.
2. **Keep-alive.** Protocol 1 has no ping. A quiet player (standing still,
   no mod messages) would hit the 60 second idle drop. Proposal: version 1
   adds an optional frame `K` with an empty payload, sent by the client
   every 20 seconds when it has sent nothing else; the relay answers
   nothing and resets the idle timer. A relay ignores the idle limit for
   clients that never send `K` only if configured to.
3. **The token.** eidos makes it once per process from the clock (16 hex
   digits), so a player who restarts the game cannot rejoin as the same
   peer, and the token is guessable. Proposal: 32 hex digits from the OS
   random source, kept in the port's settings per session code, so a
   restart rejoins as the same peer.
4. **Watching without playing.** The user wants "every player's game events
   at once". Mods can forward events as `D` messages today. A spectator
   (for example a web page showing every player's events) would need a
   join that takes no peer number and receives every `D`. Proposal: a later
   version adds `S <version> <code>` for a read-only spectator, off unless
   the host allows it.
5. **Peer 0 echo.** A `D` to peer 0 goes to every other player, not back to
   the sender. Confirm the client expects no echo.

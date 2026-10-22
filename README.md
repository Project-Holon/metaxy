# metaxy

The relay server for multiplayer in
[eidos](https://github.com/Project-Holon/eidos), the native PC port of
Pokémon Black and White.

Players connect out to the relay, so nobody has to open ports or use a VPN.
The host opens a session and gets a short code; the others join with it.
The relay forwards messages between the players and keeps nothing. It holds
no game code and no game data.

The protocol and the multiplayer design are in [docs/SPEC.md](docs/SPEC.md).

## License

MIT or Apache-2.0, whichever you prefer: [LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE).

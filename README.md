# metaxy

The relay server for multiplayer sessions in
[eidos](https://github.com/Project-Holon/eidos), the native PC port of
Pokémon Black and White.

Players connect out to the relay, so nobody has to open ports or use a VPN.
One player hosts a session and gets a short code; the others join with that
code. The relay then forwards the mods' messages between the players. It
holds no game code and no game data.

## Status

Not started. The client side and the protocol live in eidos
(`docs/mod-services.md`, "Peer sessions"; `sdk/mod_net.c`).

## Self-hosting

eidos lets a player set the relay's address in its settings, so anyone can
run their own relay. Instructions will be here once the server exists.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

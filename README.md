# P4kTools

Rust crates that read the files of a Star Citizen installation: the `Data.p4k` archive and the formats inside it.
They were split out of EditorSC (a local asset browser) so that other projects can use them.

P4kTools ships no game data. Everything is read at run time from a copy of the game that you own: the archive, and
for the few things that need it (the key of the archive's encrypted entries, the Wwise Vorbis codebooks) the game
executable, where they are located by their structure. Nothing extracted from the game is stored in this repository.

## Crates

| Crate | Directory | What it does |
|---|---|---|
| `p4k` | `crates/p4k` | `Data.p4k` reader: zip64 central directory, stored / deflate / zstd entries, AES-CBC for the encrypted entries (key read from the executable), case-insensitive lookups, folder listings and substring search. |
| `p4k-formats` | `crates/formats` | File-format parsers that take bytes and never touch the archive: `#ivo` geometry (CGF / CGA / skin), CryXmlB, DDS (BC1..BC7, BC6H), PNG writer, MTL materials, socpak object containers, DataCore (`.dcb`) records and their XML form, skeletons, animation databases (`.dba` / `.caf`), mannequin `.adb`, `.cdf`, Wwise banks and streams, SPIR-V / DXBC shader disassembly, surface types, fonts, transit data. |
| `p4k-scene` | `crates/scene` | Builds renderable scenes and catalogues from the archive with the two crates above: single models and object containers, materials and textures, streamed zones, ships, items, weapons, gear, loot, shops, contracts, planets, sounds. |

Dependencies point one way: `scene -> formats / p4k`. `formats` has no archive access.

## Using them

The repositories are meant to sit side by side; depend on the crates by path:

```toml
[dependencies]
p4k = { path = "../P4kTools/crates/p4k" }
p4k-formats = { path = "../P4kTools/crates/formats" }
p4k-scene = { path = "../P4kTools/crates/scene" }
```

```rust
let p4k = p4k::P4k::open(std::path::Path::new("/path/to/StarCitizen/LIVE/Data.p4k"))?;
```

`p4k::default_p4k_path()` returns `$P4K_PATH`, else the path that `P4K_DEFAULT_PATH` held when the crate
was built (set it in your project's `.cargo/config.toml` under `[env]`), else `Data.p4k` in the current directory.
The executable is taken from `$P4K_EXE`, or `Bin64/StarCitizen.exe` next to the archive.

## Building and testing

Needs a Rust toolchain and a C compiler (zstd is built from its bundled C sources).

```bash
cargo build --release --workspace
cargo test --release --workspace
```

Tests that need the game read the archive from `$P4K_PATH` and pass without doing anything when it is unset.
A few decoder cross-checks read reference data from `$P4K_BC6H_REF` / `$P4K_BC7_REF` and skip likewise.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.

The outside sources the code draws on, and their licences, are listed in [THIRD_PARTY.md](THIRD_PARTY.md).

## Disclaimer

Star Citizen is a trademark of Cloud Imperium Rights LLC. This project is an independent, non-commercial, educational
work. It is not affiliated with, endorsed by or sponsored by Cloud Imperium Games. It contains no game files, code,
keys or assets: everything it needs from the game is read at run time from a copy of the game that the user owns, for
interoperability.

The licence above covers this code only, not anything the tools extract from the game, which stays the property of its
owners. You are responsible for complying with the game's EULA and Terms of Service when you run these tools. See
[LEGAL.md](LEGAL.md) for what the project avoids, where the remaining risks are, and the rules for contributors.

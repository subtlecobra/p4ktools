# P4kTools

Rust workspace that reads Star Citizen's `Data.p4k` archive and the file formats inside it. Split out of EditorSC so
other projects can depend on it by path. See `README.md` for the overview and `THIRD_PARTY.md` for attributions.

## Layout

- `crates/p4k`: archive reader (zip64, stored/deflate/zstd, AES-CBC entries, lookups). Contains the `unsafe` code (mmap, crypto).
- `crates/formats` (`p4k-formats`): byte-in, struct-out format parsers. **Never touches the archive.**
- `crates/scene` (`p4k-scene`): builds scenes and catalogues from the archive using the other two.
- `tools/gen_spirv_table.py`: regenerates `crates/formats/src/spirv/table.rs` (generated, committed; don't hand-edit).

Dependency direction is one way: `scene -> formats / p4k`. Don't add `formats -> p4k` or any reverse edge.

## Commands

```bash
cargo build --release --workspace
cargo test --release --workspace          # tests are built with opt-level 2; use --release
cargo test --release -p p4k-formats <name>
cargo clippy --workspace --all-targets
```

Needs a C compiler (zstd builds from bundled C sources).

## Testing against game data

Tests that need the game read `$P4K_PATH` and silently pass when it is unset, so a green run without it proves little
for archive-dependent code. Set it to verify changes to `p4k` or `scene`. Decoder cross-checks use `$P4K_BC6H_REF` and
`$P4K_BC7_REF` and skip likewise. The executable is `$P4K_EXE`, else `Bin64/StarCitizen.exe` next to the archive.

## Rules

- **No game data in the repo.** Never commit extracted files, keys, codebooks, dumps, or test fixtures derived from the
  game. Anything needed from the game is read at run time from the user's own copy (keys and Wwise codebooks are located
  in the executable by structure, not stored).
- Read `LEGAL.md` before touching key handling, decryption, or anything that quotes game data. Never write key bytes, extracted
  game content, or bypass of launcher/online/anti-cheat checks into code, comments, tests or docs.
- Add new dependencies to `[workspace.dependencies]` in the root `Cargo.toml` and reference them with `workspace = true`.
  Record new outside sources or ports in `THIRD_PARTY.md`.
- Parsers must tolerate malformed or truncated input by returning errors (`formats/src/error.rs`), not panicking or
  indexing out of bounds. Input comes from files we don't control and formats change between game patches.
- Keep new `unsafe` out of `formats` and `scene`; justify any in `p4k` with a comment.
- Licence is `MIT OR Apache-2.0`; keep new files compatible.
- Code comments and docs: plain prose, explain why rather than what.

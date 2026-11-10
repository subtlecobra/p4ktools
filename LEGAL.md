# Legal notes

This file explains what this project is, what it deliberately avoids, and where the remaining risk sits. It is the
maintainers' good-faith position, **not legal advice**. If you redistribute this code commercially, or you receive a
complaint from anyone, talk to a lawyer.

## What the project is

P4kTools is an independent, non-commercial, educational set of Rust libraries for reading the file formats of a game
installation you already own. It is not affiliated with, endorsed by or sponsored by Cloud Imperium Games (CIG) or
Roberts Space Industries. "Star Citizen", "Squadron 42", "Roberts Space Industries" and ship, manufacturer and other
in-game names are trademarks or property of Cloud Imperium Rights LLC and its licensors. They appear here only to say
what the code works with (nominative use); no logo or brand asset is used.

## What the repository contains and does not contain

* **No game content.** No models, textures, audio, text, DataCore records, scripts, shader code, or fixtures derived from
  them. Tests that need game data read the user's own copy and skip when it is absent.
* **No keys.** The archive's AES key is not stored in the repository, in a test, in a comment or in an issue. At run
  time the code locates it inside the user's own `StarCitizen.exe` by structure. The byte pattern used to locate it is a
  locator, not the key.
* **No game code.** Nothing is copied from the executable or the engine. The format descriptions in the source were
  worked out by observing the user's own files, or taken from the third-party sources in `THIRD_PARTY.md`, and are
  written in our own words and code.
* **No online interaction.** The code never talks to CIG servers, the launcher, the game process or the anti-cheat. It reads
  local files and never modifies the archive or the executable. The one thing it writes is a small cache of derived
  transit data under `$XDG_CACHE_HOME/p4ktools` (default `~/.cache/p4ktools`), on the user's machine only. It is derived
  from game data, so it falls under point 3 below: never commit or share it.

## Where the risk is

These are the points a rights holder would be most likely to raise. They are listed so contributors can avoid making
them worse.

1. **Terms of use.** CIG's EULA and Terms of Service restrict reverse engineering and extraction of game data. Whoever
   *runs* this against a game installation is bound by those terms, and a breach is a contract matter between that
   user and CIG. The project cannot grant permission. Users must check the current terms themselves.
2. **The encrypted entries.** Roughly 4,300 of the ~1.4 million archive entries (all under `Data/Scripts`) are AES
   encrypted, and reading them needs a key that the executable holds. Fetching a protection key can be characterised as
   circumventing a technical protection measure (DMCA s.1201 in the US, Art. 6 of the EU Copyright Directive 2001/29/EC,
   and national equivalents). Counter-arguments exist, notably interoperability and research exceptions, and the
   scope varies by country, but they are arguments, not guarantees. Keep this functionality narrow: it must stay
   read-only, local, and key-less in the repository. If this point worries you, build or ship without it: the rest of
   the archive does not need the key.
3. **Output derived from game data.** Anything the tools produce (extracted files, glTF/OBJ/PNG exports, catalogues,
   JSON dumps) is CIG's content or derived from it. The licence on this repository covers the *code* only, and gives you
   no rights in that output. Do not publish it, and do not put it in issues, pull requests or releases.
4. **Non-commercial and educational status helps, but is not a defence.** It matters for fair-use and goodwill
   arguments and for how CIG is likely to react, but it does not stop a copyright, trademark or contract claim.

## Rules for contributors

* Never commit, attach or paste game files or excerpts of them, keys, key bytes, memory dumps, extracted strings in
  bulk, or screenshots of unreleased content.
* Do not add code that bypasses account, launcher, anti-cheat, licensing or online-service checks, that patches or
  injects into the game, that unpacks content from a pirated or unlicensed copy, or that helps cheating or
  gameplay-affecting modification.
* Keep comments to what the format *is* (field layouts, algorithms). Do not quote proprietary source, internal
  identifiers beyond what a file itself exposes, or leaked material. If a fact came from a leak or a decompilation
  that the contributor has no right to share, leave it out.
* Credit outside sources in `THIRD_PARTY.md` and keep their licence notices.
* If a rights holder asks for something to be removed, remove it first and discuss afterwards.

## Takedown or contact

If you represent CIG or another rights holder and want something changed or removed, open an issue or contact the
maintainer, GitHub user `subtlecobra`. Reasonable requests will be handled promptly.

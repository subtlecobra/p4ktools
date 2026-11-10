# Third-party sources

No third-party code is vendored. The following outside sources were used as references, or produced generated tables:

* `crates/formats/src/spirv/table.rs` is generated from the Khronos SPIR-V machine-readable grammar (SPIR-V 1.6 rev 4,
  KhronosGroup/SPIRV-Headers, MIT licence, notice below). It holds the opcode and enumerant names and operand kinds.
* `crates/formats/src/cryxml.rs` (CryXmlB reader) is a port of a Python reader that was vendored from the scdatatools project
  (gitlab.com/scmodding/frameworks/scdatatools, MIT licence, notice below). The layout references named there: Lumberyard `XMLBinaryHeaders.h` /
  `XMLBinaryReader.cpp`, and unp4k's `CryXmlSerializer.cs` (dolkensp/unp4k).
* `crates/formats/src/bc6h.rs` and `bc7.rs` follow the public BPTC layout (Khronos Data Format specification); BC6H's mode
  and partition tables were transcribed from the `bcdec` decoder (iOrange/bcdec). Tests cross-check against Pillow's decoder
  using reference files that are not part of this repository.
* `crates/formats/src/wwise.rs` rebuilds Vorbis packets in the form described by ww2ogg (hcs64/ww2ogg); the codebooks are read
  from the user's own game executable (`crates/scene/src/wwise_codebooks.rs`), not bundled.
* Runtime crates (crates.io): `memmap2`, `flate2` (zlib-rs backend), `zstd`, `rayon`, `serde_json`, `regex`, `base64`,
  `indexmap`, `memchr`; see `Cargo.lock` for the full tree.

## Licence notices

### scdatatools

`crates/formats/src/cryxml.rs` ports the CryXmlB reader that scdatatools shipped
(<https://gitlab.com/scmodding/frameworks/scdatatools>; text from its 1.0.4 release on PyPI):

```text
MIT License

Copyright (c) 2021

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### Khronos SPIR-V grammar

`crates/formats/src/spirv/table.rs` is generated from `spirv.core.grammar.json` of KhronosGroup/SPIRV-Headers
(<https://github.com/KhronosGroup/SPIRV-Headers>), whose notice reads:

```text
Copyright: 2014-2024 The Khronos Group Inc.
License: MIT

MODIFICATIONS TO THIS FILE MAY MEAN IT NO LONGER ACCURATELY REFLECTS
KHRONOS STANDARDS. THE UNMODIFIED, NORMATIVE VERSIONS OF KHRONOS
SPECIFICATIONS AND HEADER INFORMATION ARE LOCATED AT
https://www.khronos.org/registry/
```

//! Read-only reader for Star Citizen's Data.p4k (zip64 with store / deflate / zstd and AES-CBC encrypted scripts).

pub mod aes;
pub mod archive;
pub mod browse;
pub mod crypto;

pub use archive::{Entry, Error, P4k};
pub use browse::ArchiveIndex;

/// Default archive location: `$P4K_PATH` at run time; else the path given by `P4K_DEFAULT_PATH` when this crate was built
/// (a project that depends on it can set that in its `.cargo/config.toml` `[env]` table, so the install location stays out of this
/// repository); else `Data.p4k` in the current directory.
pub fn default_p4k_path() -> std::path::PathBuf {
    match std::env::var_os("P4K_PATH") {
        Some(p) if !p.is_empty() => p.into(),
        _ => match option_env!("P4K_DEFAULT_PATH") {
            Some(p) if !p.is_empty() => p.into(),
            _ => "Data.p4k".into(),
        },
    }
}

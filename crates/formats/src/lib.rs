//! File-format parsers (no archive or web imports, callers pass bytes).

pub mod adb;
pub mod bc6h;
pub mod bc7;
pub mod bnk;
pub mod cdf;
pub mod cryxml;
pub mod dba;
pub mod dcb;
pub mod dcbxml;
pub mod dds;
pub mod dxbc;
pub mod error;
pub mod gfx;
pub mod ivo;
pub mod mtl;
pub mod plainxml;
pub mod png;
pub mod shcb;
pub mod skeleton;
pub mod socpak;
pub mod spirv;
pub mod surfacetypes;
pub mod swffont;
pub mod transit;
pub mod wwise;
pub mod xmlq;

pub use error::{Error, Result};

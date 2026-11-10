//! The small interface the scene code needs from the DataCore item records.
//!
//! The one `ItemMaterialSource` trait (`materials.rs` re-exports it); `itemmaterials::ItemMaterials` implements it.  The two methods
//! below are all the scene code calls on `items`.

/// Geometry path -> material paths and entity class -> geometry path, as `itemmaterials` answers them.
pub trait ItemMaterialSource: Sync {
    /// Material paths (no extension, relative to Data/ as written in the records) for a geometry, commonest first.
    fn candidates(&self, geometry_name: &str) -> Vec<String>;
    /// Geometry path (relative to Data/, as written in the record) of an entity class, or None.
    fn class_geometry(&self, class_name: &str) -> Option<String>;
}

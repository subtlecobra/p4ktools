//! Files that live inside a `.socpak` zip (designer brush geometry) made visible next to the archive.
//!
//! The overlay is a plain map of name -> bytes; a geometry name returned by `socpak_scene::collect_instances` is looked up with
//! `Overlay::get` first and in the archive otherwise.  `builder::build_scene` still takes a real `Entry`: building a virtual geometry
//! needs a bytes-based entry point.

use std::collections::HashMap;
use std::sync::Arc;

/// An in-memory file; `name` is `<owner socpak entry name>#<member name, lowercase>`.
#[derive(Debug, Clone)]
pub struct VirtualEntry {
    pub name: String,
    pub data: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
pub struct Overlay {
    entries: HashMap<String, VirtualEntry>,
}

impl Overlay {
    pub fn get(&self, name: &str) -> Option<&VirtualEntry> {
        self.entries.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    pub fn insert(&mut self, name: String, data: Vec<u8>) {
        self.entries.insert(name.clone(), VirtualEntry { name, data: Arc::new(data) });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let mut o = Overlay::default();
        o.insert("a.socpak#x/brush/b.cgf".into(), vec![1, 2, 3]);
        assert_eq!(o.get("a.socpak#x/brush/b.cgf").unwrap().data.len(), 3);
        assert!(!o.contains("zzz"));
        assert_eq!(o.len(), 1);
    }
}

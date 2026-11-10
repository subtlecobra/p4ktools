//! Directory-style views over a `P4k`: folder listings and substring search.

use rayon::prelude::*;

use crate::archive::{lower, Entry, P4k};

/// Sorted view of the entry names.  Folder listings use a binary search on the sorted list, so no per-folder tables are kept in
/// memory (the archive has ~1.4 million entries).  Names sort by code point (UTF-8 byte order equals code point order).
pub struct ArchiveIndex<'a> {
    pub p4k: &'a P4k,
    /// Entry indices sorted by name.
    order: Vec<u32>,
}

impl<'a> ArchiveIndex<'a> {
    pub fn new(p4k: &'a P4k) -> Self {
        let mut order: Vec<u32> = (0..p4k.entries.len() as u32).collect();
        order.par_sort_unstable_by(|&a, &b| p4k.entries[a as usize].name.cmp(&p4k.entries[b as usize].name));
        ArchiveIndex { p4k, order }
    }

    fn name(&self, i: usize) -> &str {
        &self.p4k.entries[self.order[i] as usize].name
    }

    /// First position whose name is >= `key` (like Python's bisect_left).
    fn bisect_left(&self, key: &str) -> usize {
        self.order.partition_point(|&i| &*self.p4k.entries[i as usize].name < key)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Immediate children of `prefix` ('' or 'a/b/'): ([(folder, file_count)], [Entry]).
    pub fn listing(&self, prefix: &str) -> (Vec<(String, usize)>, Vec<&'a Entry>) {
        let mut i = self.bisect_left(prefix);
        let n = self.order.len();
        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        while i < n && self.name(i).starts_with(prefix) {
            let rest = &self.name(i)[prefix.len()..];
            if let Some(slash) = rest.find('/') {
                let d = &rest[..slash];
                // '0' sorts right after '/', ending the subtree
                let hi = self.bisect_left(&format!("{}{}0", prefix, d));
                dirs.push((d.to_string(), hi - i));
                i = hi.max(i + 1);
            } else {
                files.push(&self.p4k.entries[self.order[i] as usize]);
                i += 1;
            }
        }
        (dirs, files)
    }

    /// Every match (up to `cap`), for callers that sort before cutting the list.
    pub fn search_all(&self, terms: &[&str], ext: &str, cap: usize) -> Vec<&'a str> {
        self.search(terms, ext, cap - 1)
    }

    /// Names containing every lowercase term (and ending in .ext); returns at most limit + 1 so callers can tell the list was cut.
    pub fn search(&self, terms: &[&str], ext: &str, limit: usize) -> Vec<&'a str> {
        let dot_ext = if ext.is_empty() { String::new() } else { format!(".{}", ext) };
        let mut out = Vec::new();
        for &i in &self.order {
            let name: &str = &self.p4k.entries[i as usize].name;
            let low = lower(name);
            if !dot_ext.is_empty() && !low.ends_with(&dot_ext) {
                continue;
            }
            if terms.iter().all(|t| low.contains(t)) {
                out.push(name);
                if out.len() > limit {
                    break;
                }
            }
        }
        out
    }
}

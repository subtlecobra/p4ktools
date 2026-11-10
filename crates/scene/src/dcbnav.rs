//! Navigation over Game2.dcb for the DataCore browser: a folder tree of the records' virtual paths, search, struct types and
//! back-references ("who points at this record").
//!
//! Facts (CONFIRMED on the live archive, version 8): the file holds 117,709 records; every record has a name
//! ('EntityClassDefinition.ksar_pistol_ballistic_01'), a virtual source path ('libs/foundry/records/entities/scitem/...xml', all of them
//! under `libs/`), a struct type and a GUID.  62,736 distinct paths: 487 paths hold more than one record, 2 names are used twice (so a
//! name does not always identify a record: `by_name` keeps lists and pages link by index then).  A reference (`("ref", n, guid)`) names
//! its target by GUID only (`n` is an instance index inside a struct, not a record index).
//!
//! Back-references are EMPIRICAL / INFERRED: a record "references" another when any instance reachable from it by strong pointers and class
//! arrays holds a `Reference` (or array of references) to its GUID, or a weak pointer to its main instance.  Pointers other than those
//! (weak pointers to sub-instances) are not followed.
//!
//! `scan_references` is fast enough to run at start-up: the walk over the 117k records is spread over all cores with rayon; there is no
//! cache file.

use std::collections::HashMap;
use std::sync::OnceLock;

use p4k_formats::dcb::{self, Dcb, WalkKind, CONV_SINGLE};
use rayon::prelude::*;
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::dynv::*;

const NULL: u32 = 0xFFFF_FFFF;

fn guid_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap())
}

pub fn is_guid(s_: &str) -> bool {
    guid_re().is_match(s_)
}

pub fn norm_path(p: &str) -> String {
    p.replace('\\', "/").to_lowercase().split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
}

/// Sorted, searchable view of the record table.  Build with `DcbNav::new(dcb)`; all methods are read-only afterwards.
pub struct DcbNav<'a> {
    pub d: &'a Dcb,
    pub n: usize,
    pub names: Vec<String>,
    pub files: Vec<String>,
    pub sidx: Vec<usize>,
    pub snames: Vec<String>,
    pub guids: Vec<String>,
    pub lnames: Vec<String>,
    pub lfiles: Vec<String>,
    pub by_name: HashMap<String, Vec<usize>>,
    pub by_guid: HashMap<String, usize>,
    pub by_root: HashMap<(usize, usize), usize>,
    pub by_lname: HashMap<String, Vec<usize>>,
    pub by_file: HashMap<String, Vec<usize>>,
    pub order: Vec<usize>,
    keys: Vec<String>,
    types: OnceLock<Vec<Value>>,
    alias: OnceLock<(HashMap<usize, String>, HashMap<String, usize>)>,
    ls_cache: std::sync::Mutex<HashMap<String, std::sync::Arc<(Vec<(String, usize)>, Vec<usize>)>>>,
}

impl<'a> DcbNav<'a> {
    pub fn new(d: &'a Dcb) -> Self {
        let n = d.n_rec;
        let recs: Vec<_> = (0..n).into_par_iter().map(|i| d.record(i).expect("record")).collect();
        let names: Vec<String> = recs.iter().map(|r| r.name.clone()).collect();
        let files: Vec<String> = recs.iter().map(|r| r.file.clone()).collect();
        let sidx: Vec<usize> = recs.iter().map(|r| r.struct_idx as usize).collect();
        let snames: Vec<String> = recs.iter().map(|r| r.struct_name.clone()).collect();
        let guids: Vec<String> = recs.iter().map(|r| r.guid.clone()).collect();
        let instance: Vec<usize> = recs.iter().map(|r| r.instance as usize).collect();
        let lnames: Vec<String> = names.iter().map(|x| x.to_lowercase()).collect();
        let lfiles: Vec<String> = files.iter().map(|x| x.to_lowercase()).collect();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_guid = HashMap::new();
        let mut by_root = HashMap::new();
        let mut by_lname: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for i in 0..n {
            by_name.entry(names[i].clone()).or_default().push(i);
            by_guid.insert(guids[i].clone(), i);
            by_root.insert((sidx[i], instance[i]), i);
            by_lname.entry(lnames[i].clone()).or_default().push(i);
            by_file.entry(lfiles[i].clone()).or_default().push(i);
        }
        let mut order: Vec<usize> = (0..n).collect();
        order.par_sort_by(|&a, &b| (&lfiles[a], &lnames[a]).cmp(&(&lfiles[b], &lnames[b])));
        let keys = order.iter().map(|&i| lfiles[i].clone()).collect();
        DcbNav {
            d,
            n,
            names,
            files,
            sidx,
            snames,
            guids,
            lnames,
            lfiles,
            by_name,
            by_guid,
            by_root,
            by_lname,
            by_file,
            order,
            keys,
            types: OnceLock::new(),
            alias: OnceLock::new(),
            ls_cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    // ---- records
    pub fn brief(&self, i: usize) -> Value {
        json!({"i": i, "name": self.names[i], "type": self.snames[i], "guid": self.guids[i], "file": self.files[i]})
    }

    /// Record index for a record name (exact, then case-insensitive), a GUID, a plain index number or a virtual file path with one
    /// record; None when nothing matches.  A name used by two records gives the first.
    pub fn find(&self, key: &str) -> Option<usize> {
        let key = key.trim();
        if let Some(&i) = self.aliases().1.get(key) {
            return Some(i);
        }
        if !key.is_empty() && key.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(v) = key.parse::<usize>() {
                if v < self.n {
                    return Some(v);
                }
            }
        }
        if let Some(&i) = self.by_guid.get(&key.to_lowercase()) {
            return Some(i);
        }
        if let Some(ids) = self.by_name.get(key).or_else(|| self.by_lname.get(&key.to_lowercase())) {
            return ids.first().copied();
        }
        match self.by_file.get(&norm_path(key)) {
            Some(ids) if ids.len() == 1 => Some(ids[0]),
            _ => None,
        }
    }

    /// What the URL of record `i` uses: its name, or its index when the name is shared.
    pub fn link_key(&self, i: usize) -> String {
        if self.by_name[&self.names[i]].len() > 1 {
            i.to_string()
        } else {
            self.names[i].clone()
        }
    }

    /// `({record: alias}, {alias: record})` for the records that have a friendlier name than 'Tag.<guid>': tag records ('Tag' struct,
    /// 18,891 of them) carry a `tagName` ('Pistol'); the alias 'Tag.Pistol' is used only when no other tag has the same name (EMPIRICAL:
    /// tag names repeat under different parents).
    pub fn aliases(&self) -> &(HashMap<usize, String>, HashMap<String, usize>) {
        self.alias.get_or_init(|| {
            let mut groups: Vec<(String, Vec<usize>)> = vec![];
            let mut pos: HashMap<String, usize> = HashMap::new();
            if let Some(si) = self.struct_index("Tag") {
                for i in 0..self.n {
                    if self.sidx[i] == si {
                        let nm = self
                            .d
                            .record_root(i)
                            .ok()
                            .and_then(|(_, inst)| self.d.read_instance(si, inst, 0, 0).ok())
                            .and_then(|v| v.get("tagName").and_then(|x| x.as_str().map(str::to_owned)));
                        if let Some(nm) = nm.filter(|n| !n.is_empty()) {
                            let k = format!("Tag.{}", nm);
                            let p = *pos.entry(k.clone()).or_insert_with(|| {
                                groups.push((k, vec![]));
                                groups.len() - 1
                            });
                            groups[p].1.push(i);
                        }
                    }
                }
            }
            let mut alias = HashMap::new();
            let mut rev = HashMap::new();
            for (k, v) in groups {
                if v.len() == 1 {
                    alias.insert(v[0], k.clone());
                    rev.insert(k, v[0]);
                }
            }
            (alias, rev)
        })
    }

    /// How a reference to record `i` is written in the XML: its alias if it has one, else its name.
    pub fn ref_text(&self, i: usize) -> String {
        self.aliases().0.get(&i).cloned().unwrap_or_else(|| self.names[i].clone())
    }

    pub fn same_file(&self, i: usize) -> Vec<usize> {
        self.by_file.get(&self.lfiles[i]).map_or(vec![], |v| v.iter().copied().filter(|&j| j != i).collect())
    }

    // ---- folder tree
    /// `(folders, records)` directly inside virtual folder `path` ('' = root): folders = `[(display name, record count)]` and records
    /// = `[record index]` of the files in it, both in name order.  Cost is proportional to the number of children (a binary search skips
    /// each subtree).
    pub fn children(&self, path: &str) -> std::sync::Arc<(Vec<(String, usize)>, Vec<usize>)> {
        let path = norm_path(path);
        if let Some(v) = self.ls_cache.lock().unwrap().get(&path) {
            return v.clone();
        }
        let prefix = if path.is_empty() { String::new() } else { format!("{}/", path) };
        let keys = &self.keys;
        let lo = keys.partition_point(|k| k.as_str() < prefix.as_str());
        let top = format!("{}\u{10ffff}", prefix);
        let hi = lo + keys[lo..].partition_point(|k| k.as_str() < top.as_str());
        let plen = prefix.chars().count();
        let mut folders = vec![];
        let mut recs = vec![];
        let mut pos = lo;
        while pos < hi {
            let rest: &str = {
                let k = keys[pos].as_str();
                match k.char_indices().nth(plen) {
                    Some((b, _)) => &k[b..],
                    None => "",
                }
            };
            if let Some((seg, _)) = rest.split_once('/') {
                let stop = format!("{}{}/\u{10ffff}", prefix, seg);
                let end = pos + keys[pos..hi].partition_point(|k| k.as_str() <= stop.as_str());
                let f = &self.files[self.order[pos]];
                let disp: String = f.chars().skip(plen).collect::<String>().split('/').next().unwrap_or("").to_string();
                folders.push((disp, end - pos));
                pos = end;
            } else {
                recs.push(self.order[pos]);
                pos += 1;
            }
        }
        let arc = std::sync::Arc::new((folders, recs));
        let mut c = self.ls_cache.lock().unwrap();
        if c.len() > 256 {
            c.clear();
        }
        c.insert(path, arc.clone());
        arc
    }

    /// JSON-ready folder page: `{path, total, nfolders, nrecords, offset, folders, records}` (folders come first, then records).
    pub fn listing(&self, path: &str, offset: usize, limit: usize) -> Value {
        let ch = self.children(path);
        let mut folders = ch.0.clone();
        folders.sort_by_key(|f| f.0.to_lowercase());
        let recs = &ch.1;
        let total = folders.len() + recs.len();
        let fl: Vec<Value> = folders.iter().skip(offset).take(limit).map(|(n, c)| json!({"name": n, "count": c})).collect();
        let take = limit.saturating_sub(fl.len());
        let start = offset.saturating_sub(folders.len());
        let rl: Vec<Value> = recs
            .iter()
            .skip(start)
            .take(take)
            .map(|&i| {
                let mut b = self.brief(i);
                b["key"] = json!(self.link_key(i));
                b["base"] = json!(self.files[i].rsplit('/').next().unwrap());
                b
            })
            .collect();
        json!({"path": norm_path(path), "total": total, "nfolders": folders.len(), "nrecords": recs.len(), "offset": offset, "folders": fl, "records": rl})
    }

    pub fn exists_folder(&self, path: &str) -> bool {
        let ch = self.children(path);
        !ch.0.is_empty() || !ch.1.is_empty() || norm_path(path).is_empty()
    }

    /// The path with the case the records use (walks the tree: one binary search per segment).
    pub fn display_path(&self, path: &str) -> String {
        let mut out: Vec<String> = vec![];
        for seg in norm_path(path).split('/') {
            if seg.is_empty() {
                continue;
            }
            let ch = self.children(&out.join("/"));
            let hit = ch.0.iter().find(|f| f.0.to_lowercase() == seg).map(|f| f.0.clone());
            out.push(hit.unwrap_or_else(|| seg.to_string()));
        }
        out.join("/")
    }

    // ---- struct types
    /// `[{name, index, records, instances}]` of the struct types that have records, most records first.
    pub fn type_counts(&self) -> &[Value] {
        self.types.get_or_init(|| {
            let mut cnt: Vec<(usize, usize)> = vec![];
            let mut pos: HashMap<usize, usize> = HashMap::new();
            for &si in &self.sidx {
                let p = *pos.entry(si).or_insert_with(|| {
                    cnt.push((si, 0));
                    cnt.len() - 1
                });
                cnt[p].1 += 1;
            }
            let mut v: Vec<(String, usize, usize, usize)> = cnt
                .into_iter()
                .map(|(si, c)| (self.d.struct_name(si).unwrap_or("").to_string(), si, c, self.d.instance_base(si).map_or(0, |b| b.1)))
                .collect();
            v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
            v.into_iter().map(|(n, si, c, inst)| json!({"name": n, "index": si, "records": c, "instances": inst})).collect()
        })
    }

    /// Struct index by name (exact, then case-insensitive), or None.
    pub fn struct_index(&self, name: &str) -> Option<usize> {
        if let Some(&i) = self.d.struct_names().get(name) {
            return Some(i);
        }
        let low = name.to_lowercase();
        self.d.struct_names().iter().filter(|(k, _)| k.to_lowercase() == low).map(|(_, v)| *v).min()
    }

    pub fn types(&self, q: &str, limit: usize) -> Value {
        let q = q.trim().to_lowercase();
        let out: Vec<&Value> = self.type_counts().iter().filter(|t| q.is_empty() || s(&t["name"]).to_lowercase().contains(&q)).collect();
        json!({"total": out.len(), "types": out.into_iter().take(limit).collect::<Vec<_>>()})
    }

    /// Everything the struct page shows: parent chain, properties grouped by declaring struct, children, counts.
    pub fn struct_page(&self, si: usize) -> Option<Value> {
        let d = self.d;
        let sd = d.struct_def(si).ok()?;
        let mut chain = vec![];
        let mut cur = si as i64;
        while cur >= 0 {
            chain.push(cur as usize);
            cur = d.struct_def(cur as usize).ok()?.parent as i64;
        }
        let mut groups = vec![];
        for &st in chain.iter().rev() {
            let props: Vec<Value> = d.own_props(st).ok()?.iter().map(|p| self.describe_prop(&p.name, p.s, p.t, p.c)).collect();
            groups.push(json!({"struct": d.struct_name(st).ok()?, "own": st == si, "props": props}));
        }
        let mut kids: Vec<String> = (0..d.n_struct).filter(|&k| d.struct_def(k).map_or(false, |x| x.parent == si as i32)).map(|k| d.struct_name(k).unwrap().to_string()).collect();
        kids.sort_by_key(|k| k.to_lowercase());
        // stable sort by lower-case text
        let rec_count = self.sidx.iter().filter(|&&x| x == si).count();
        Some(json!({
            "name": sd.name, "index": si, "parent": if sd.parent >= 0 { json!(d.struct_name(sd.parent as usize).ok()?) } else { Value::Null },
            "ownProps": sd.count, "nodeType": sd.node_type, "size": d.struct_size(si).ok()?, "instances": d.instance_base(si).map_or(0, |b| b.1),
            "records": rec_count, "groups": groups, "children": kids, "chain": chain.iter().map(|&x| d.struct_name(x).unwrap().to_string()).collect::<Vec<_>>()
        }))
    }

    /// `{name, type (text), base, array, conv, target, targetKind, values}` of one property definition.
    pub fn describe_prop(&self, name: &str, s_: u16, t: u16, c: u16) -> Value {
        let d = self.d;
        let base = dcb::type_name(t).map_or_else(|| format!("type 0x{:x}", t), str::to_string);
        let array = c != CONV_SINGLE;
        let mut out = json!({"name": name, "base": base, "array": array, "conv": c, "target": null, "targetKind": null, "values": null});
        let mut text = base.clone();
        if matches!(t, dcb::T_CLASS | dcb::T_STRONG | dcb::T_WEAK | dcb::T_REF) && (s_ as usize) < d.n_struct {
            let tn = d.struct_name(s_ as usize).unwrap().to_string();
            out["target"] = json!(tn);
            out["targetKind"] = json!("struct");
            text = match t {
                dcb::T_CLASS => format!("{} (inline)", tn),
                dcb::T_STRONG => format!("pointer to {}", tn),
                dcb::T_WEAK => format!("weak pointer to {}", tn),
                _ => format!("reference to {} record", tn),
            };
        } else if t == dcb::T_ENUM && (s_ as usize) < d.n_enum {
            if let Ok((en, vals)) = d.enum_def(s_ as usize) {
                out["target"] = json!(en);
                out["targetKind"] = json!("enum");
                out["values"] = json!(vals);
                text = format!("enum {}", en);
            }
        }
        out["type"] = json!(format!("{}{}", text, if array { "[]" } else { "" }));
        out
    }

    // ---- search
    /// Records matching every whitespace-separated term of `q` (case-insensitive substrings) in the chosen field ('name', 'file', 'guid' or
    /// 'all' = name + path + GUID), optionally restricted to a struct type (exact name, else a substring of it).  Best matches first: exact
    /// short name, prefix of the short name, prefix of the name, name contains, then the rest by path.
    pub fn search(&self, q: &str, field: &str, type_: &str, offset: usize, limit: usize) -> Value {
        let terms: Vec<String> = q.to_lowercase().split_whitespace().map(str::to_owned).collect();
        let mut ids: Vec<usize> = (0..self.n).collect();
        let tq = type_.trim().to_lowercase();
        if !tq.is_empty() {
            let mut seen: Vec<usize> = self.sidx.clone();
            seen.sort_unstable();
            seen.dedup();
            let exact: Vec<usize> = seen.iter().copied().filter(|&si| self.d.struct_name(si).map_or(false, |n| n.to_lowercase() == tq)).collect();
            let want: Vec<usize> = if exact.is_empty() { seen.iter().copied().filter(|&si| self.d.struct_name(si).map_or(false, |n| n.to_lowercase().contains(&tq))).collect() } else { exact };
            ids.retain(|i| want.contains(&self.sidx[*i]));
        }
        if terms.len() == 1 && guid_re().is_match(&terms[0]) && (field == "all" || field == "guid") {
            let hit = self.by_guid.get(&terms[0]).copied().filter(|i| tq.is_empty() || ids.contains(i));
            return self.page(&hit.into_iter().collect::<Vec<_>>(), offset, limit);
        }
        for t in &terms {
            ids = match field {
                "name" => ids.into_iter().filter(|&i| self.lnames[i].contains(t.as_str())).collect(),
                "file" => ids.into_iter().filter(|&i| self.lfiles[i].contains(t.as_str())).collect(),
                "guid" => ids.into_iter().filter(|&i| self.guids[i].contains(t.as_str())).collect(),
                _ => ids.into_iter().filter(|&i| self.lnames[i].contains(t.as_str()) || self.lfiles[i].contains(t.as_str()) || self.guids[i].contains(t.as_str())).collect(),
            };
        }
        let mut hits = ids;
        if !terms.is_empty() && (field == "all" || field == "name") {
            let q1 = terms.join(" ");
            let score = |i: usize| -> u8 {
                let nm = &self.lnames[i];
                let short = nm.split_once('.').map_or(nm.as_str(), |x| x.1);
                if short == q1 || *nm == q1 {
                    0
                } else if short.starts_with(&q1) {
                    1
                } else if nm.starts_with(&q1) {
                    2
                } else if nm.contains(&q1) {
                    3
                } else {
                    4
                }
            };
            let mut sc: Vec<(u8, usize)> = hits.iter().map(|&i| (score(i), i)).collect();
            sc.sort_by(|a, b| (a.0, &self.lfiles[a.1], &self.lnames[a.1]).cmp(&(b.0, &self.lfiles[b.1], &self.lnames[b.1])));
            hits = sc.into_iter().map(|x| x.1).collect();
            return self.page(&hits, offset, limit);
        }
        hits.sort_by(|&a, &b| (&self.lfiles[a], &self.lnames[a]).cmp(&(&self.lfiles[b], &self.lnames[b])));
        self.page(&hits, offset, limit)
    }

    fn page(&self, hits: &[usize], offset: usize, limit: usize) -> Value {
        let recs: Vec<Value> = hits
            .iter()
            .skip(offset)
            .take(limit)
            .map(|&i| {
                let mut b = self.brief(i);
                b["key"] = json!(self.link_key(i));
                b
            })
            .collect();
        json!({"total": hits.len(), "offset": offset, "limit": limit, "records": recs})
    }
}

// ---------------------------------------------------------------------------------------------------- back-references
/// `{target record: sorted unique list of the records that reference it}`.  One walk from every record's main instance over strong
/// pointers and class arrays (the instances it owns), collecting GUID references and weak pointers to record roots (`Dcb::walk_plan`
/// skips structs that hold neither).  About 4.9 M instances exist, but only those on a path to a link field are visited.  Records are
/// walked in parallel; the result is merged in record order, so every list is sorted and unique without a sort.
pub fn scan_references(nav: &DcbNav) -> HashMap<usize, Vec<u32>> {
    let d = nav.d;
    let m = d.bytes();
    let u32at = |o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
    let mut gmap: HashMap<[u8; 16], usize> = HashMap::with_capacity(d.n_rec);
    for i in 0..d.n_rec {
        gmap.insert(d.record_guid_bytes(i).unwrap().try_into().unwrap(), i);
    }
    // per struct: (first instance offset, size) and the walk plan, resolved once
    let ns = d.n_struct;
    let base: Vec<Option<(usize, usize)>> = (0..ns).map(|si| d.instance_base(si).and_then(|(b, _)| d.struct_size(si).ok().map(|sz| (b, sz)))).collect();
    let plans: Vec<Vec<(WalkKind, usize, usize)>> = (0..ns).map(|si| if base[si].is_some() { d.walk_plan(si).unwrap_or_default() } else { vec![] }).collect();
    let ref_off = d.array_base(dcb::T_REF).unwrap();
    let strong_off = d.array_base(dcb::T_STRONG).unwrap();
    let weak_off = d.array_base(dcb::T_WEAK).unwrap();
    let roots = &nav.by_root;
    let found: Vec<Vec<u32>> = (0..d.n_rec)
        .into_par_iter()
        .map(|i| {
            let Ok((si, inst)) = d.record_root(i) else { return vec![] };
            let Some(Some((b0, sz0))) = base.get(si) else { return vec![] };
            let mut found: Vec<u32> = vec![];
            let mut stack: Vec<(usize, usize)> = vec![(si, b0 + inst * sz0)];
            let push_strong = |stack: &mut Vec<(usize, usize)>, a: u32, b: u32| {
                let a = a as usize;
                if a != NULL as usize && a < ns {
                    if let Some((bs, sz)) = base[a] {
                        if !plans[a].is_empty() {
                            stack.push((a, bs + b as usize * sz));
                        }
                    }
                }
            };
            while let Some((st, off)) = stack.pop() {
                for &(kind, po, s2) in &plans[st] {
                    let o = off + po;
                    match kind {
                        WalkKind::Ref => {
                            if let Some(&t) = gmap.get(&<[u8; 16]>::try_from(&m[o + 4..o + 20]).unwrap()) {
                                found.push(t as u32);
                            }
                        }
                        WalkKind::Strong => push_strong(&mut stack, u32at(o), u32at(o + 4)),
                        WalkKind::Weak => {
                            if let Some(&t) = roots.get(&(u32at(o) as usize, u32at(o + 4) as usize)) {
                                found.push(t as u32);
                            }
                        }
                        _ => {
                            let (cnt, first) = (u32at(o) as usize, u32at(o + 4) as usize);
                            if cnt == 0 {
                                continue;
                            }
                            match kind {
                                WalkKind::Classes => {
                                    if let Some((bs, sz)) = base[s2] {
                                        for k in 0..cnt {
                                            stack.push((s2, bs + (first + k) * sz));
                                        }
                                    }
                                }
                                WalkKind::Refs => {
                                    for k in 0..cnt {
                                        let q = ref_off + (first + k) * 20;
                                        if let Some(&t) = gmap.get(&<[u8; 16]>::try_from(&m[q + 4..q + 20]).unwrap()) {
                                            found.push(t as u32);
                                        }
                                    }
                                }
                                WalkKind::Weaks => {
                                    for k in 0..cnt {
                                        let q = weak_off + (first + k) * 8;
                                        if let Some(&t) = roots.get(&(u32at(q) as usize, u32at(q + 4) as usize)) {
                                            found.push(t as u32);
                                        }
                                    }
                                }
                                _ => {
                                    for k in 0..cnt {
                                        let q = strong_off + (first + k) * 8;
                                        push_strong(&mut stack, u32at(q), u32at(q + 4));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            found.sort_unstable();
            found.dedup();
            found.retain(|&t| t as usize != i);
            found
        })
        .collect();
    let mut by_target: HashMap<usize, Vec<u32>> = HashMap::new();
    for (i, f) in found.into_iter().enumerate() {
        for t in f {
            by_target.entry(t as usize).or_default().push(i as u32);
        }
    }
    by_target
}

/// Reverse reference index, computed on demand (`BackRefs::compute`) and queried with `get`.
pub struct BackRefs {
    pub data: HashMap<usize, Vec<u32>>,
    pub seconds: f64,
}

impl BackRefs {
    pub fn compute(nav: &DcbNav) -> BackRefs {
        let t0 = std::time::Instant::now();
        let data = scan_references(nav);
        BackRefs { data, seconds: t0.elapsed().as_secs_f64() }
    }

    /// The index in the disk cache's binary form (`codec`), keys sorted so equal data gives equal bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = crate::codec::Writer::new();
        let mut keys: Vec<&usize> = self.data.keys().collect();
        keys.sort();
        w.uv(keys.len() as u64);
        for k in keys {
            w.uv(*k as u64);
            w.u32s(&self.data[k]);
        }
        w.buf
    }

    /// Inverse of [`BackRefs::to_bytes`] (`seconds` is the time the load took, set by the caller); None for damaged input.
    pub fn from_bytes(b: &[u8], seconds: f64) -> Option<BackRefs> {
        let mut r = crate::codec::Reader::new(b);
        let n = r.count()?;
        let mut data = HashMap::with_capacity(n);
        for _ in 0..n {
            let k = r.uv()? as usize;
            data.insert(k, r.u32s()?);
        }
        r.done().then_some(BackRefs { data, seconds })
    }

    /// The (sorted) source record indexes that reference record `i`.
    pub fn get(&self, i: usize) -> &[u32] {
        self.data.get(&i).map_or(&[], Vec::as_slice)
    }

    pub fn status(&self) -> Value {
        json!({"phase": "ready", "progress": 1.0, "error": null, "seconds": self.seconds})
    }
}

/// `/dcb/api/refs`: `{phase, progress, error, seconds, total, offset, records}` for record `rec`.
pub fn refs_page(nav: &DcbNav, refs: &BackRefs, rec: &str, offset: usize, limit: usize) -> Option<Value> {
    let i = nav.find(rec)?;
    let ids = refs.get(i);
    let mut out = refs.status();
    out["total"] = json!(ids.len());
    out["offset"] = json!(offset);
    out["records"] = Value::Array(
        ids.iter()
            .skip(offset)
            .take(limit)
            .map(|&j| {
                let mut b = nav.brief(j as usize);
                b["key"] = json!(nav.link_key(j as usize));
                b
            })
            .collect(),
    );
    Some(out)
}

#[allow(dead_code)]
fn _unused(_: Map<String, Value>) {}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn back_refs_survive_the_cache_format() {
        let mut data = HashMap::new();
        data.insert(5usize, vec![1u32, 9, 300_000]);
        data.insert(0usize, vec![]);
        data.insert(1_000_000usize, vec![2]);
        let r = BackRefs { data, seconds: 1.0 };
        let b = r.to_bytes();
        assert_eq!(b, r.to_bytes(), "deterministic");
        let back = BackRefs::from_bytes(&b, 0.5).unwrap();
        assert_eq!((back.data, back.seconds), (r.data.clone(), 0.5));
        assert!(BackRefs::from_bytes(&b[..b.len() - 1], 0.0).is_none());
    }
}

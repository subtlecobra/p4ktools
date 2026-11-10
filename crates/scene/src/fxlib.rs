//! First-person shooting effects of a weapon, flattened from the game's own particle libraries for the weapons lab.
//!
//! Where the data comes from (all CONFIRMED on the live archive, e.g. `none_lmg_ballistic_01`):
//! * the weapon record's `SCItemWeaponComponentParams ... fireEffects` holds `SWeaponParticleEffectParams` entries; `helper="muzzle_flash"`
//!   is the muzzle effect, `helper="shells"` the ejected casings.  Their `firstPerson path="<guid>|<library>.<Effect>.<name>"` names the
//!   effect (the GUID is the `<Particles GUID=...>` of the effect in the library, the library is
//!   `Data/Libs/Particles/<library>.xml`, a CryXmlB file);
//! * a `<Particles>` element has `<Params .../>` and nested `<Childs><Particles .../></Childs>`; the muzzle effect's children are smoke,
//!   sparks, flash sprites, a faked tracer light and a refraction ("distortion") puff.
//!
//! The `Params` attribute values use a small expression language (CONFIRMED by reading the libraries, the meaning of every key is the
//! engine's and is only INFERRED where a comment says so): `base[,Random=r][,ParticleAge=(t=..,v=..;t=..,v=..)][,DistanceLOD=..]`;
//! colours are `(x=r,y=g,z=b)`; vectors `x=..,y=..,z=..`; structs (`LightSource`, `TextureAnimation`) are `Key=value,...`.
//! `ParticleAge` curves have keys `t` (0..1 of the lifetime) and `v`; a key without `v` holds the previous value (INFERRED), a first key
//! without `t` sits at 0.
//!
//! Only what the lab can draw is kept: sprite / flat colour, additive or alpha blend, lifetime, size / alpha / colour over age, count,
//! speed, drag, gravity, light radius / intensity, texture animation grid, shell geometry and bounce.  Disabled emitters
//! (`Enabled="false"`, CONFIRMED to be off in the game), refraction-only emitters and the third person effect are dropped.

use p4k_formats::cryxml::{self, Element};
use p4k::P4k;
use serde_json::{json, Value};

// ------------------------------------------------------------------------------------------------------------------ expression parser

/// One parsed parameter value.  `leaf` is a plain token (`0.5`, `Additive`), `bare` the first unnamed item of a comma list,
/// `named` the `Key=value` items, `seq` the `;`-separated keys of a curve.
#[derive(Debug, Default, Clone)]
pub struct Node {
    pub leaf: Option<String>,
    pub bare: Option<Box<Node>>,
    pub named: Vec<(String, Node)>,
    pub seq: Vec<Node>,
}

/// Index of the `)` closing the `(` at the start of `s`, if any.
fn matching_close(s: &str) -> Option<usize> {
    let mut d = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '(' => d += 1,
            ')' => {
                d -= 1;
                if d == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits at `sep` outside parentheses.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut out, mut d, mut start) = (vec![], 0i32, 0usize);
    for (i, c) in s.char_indices() {
        match c {
            '(' => d += 1,
            ')' => d -= 1,
            c if c == sep && d == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// `Key=rest` when `item` starts with an identifier followed by `=` (outside parentheses).
fn split_named(item: &str) -> Option<(&str, &str)> {
    let i = item.find('=')?;
    let key = item[..i].trim();
    (!key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then(|| (key, &item[i + 1..]))
}

pub fn parse_node(s: &str) -> Node {
    let mut s = s.trim();
    while s.starts_with('(') && matching_close(s) == Some(s.len() - 1) {
        s = s[1..s.len() - 1].trim();
    }
    let mut n = Node::default();
    let segs = split_top(s, ';');
    if segs.len() > 1 {
        n.seq = segs.iter().filter(|x| !x.trim().is_empty()).map(|x| parse_node(x)).collect();
        return n;
    }
    let items = split_top(s, ',');
    if items.len() == 1 && split_named(items[0]).is_none() {
        n.leaf = Some(s.to_string());
        return n;
    }
    for it in items {
        match split_named(it) {
            Some((k, v)) => n.named.push((k.to_string(), parse_node(v))),
            None if n.bare.is_none() && !it.trim().is_empty() => n.bare = Some(Box::new(parse_node(it))),
            None => {}
        }
    }
    n
}

impl Node {
    pub fn get(&self, k: &str) -> Option<&Node> {
        self.named.iter().find(|(n, _)| n == k).map(|(_, v)| v)
    }
    /// The number this node stands for: its leaf, or its unnamed first item.
    pub fn num(&self) -> Option<f64> {
        match &self.leaf {
            Some(l) => l.trim().parse().ok(),
            None => self.bare.as_ref().and_then(|b| b.num()),
        }
    }
    pub fn text(&self) -> Option<&str> {
        match &self.leaf {
            Some(l) => Some(l.as_str()),
            None => self.bare.as_ref().and_then(|b| b.text()),
        }
    }
    fn named_num(&self, k: &str) -> Option<f64> {
        self.get(k).and_then(|n| n.num())
    }
    /// `(x=..,y=..,z=..)` as an array (missing axes 0), when any axis is present here or in the unnamed first item.
    pub fn vec3(&self) -> Option<[f64; 3]> {
        if ["x", "y", "z"].iter().any(|k| self.get(k).is_some()) {
            return Some([self.named_num("x").unwrap_or(0.0), self.named_num("y").unwrap_or(0.0), self.named_num("z").unwrap_or(0.0)]);
        }
        self.bare.as_ref().and_then(|b| b.vec3())
    }
    /// The keys of the curve `name` (usually `ParticleAge`) as `(t, value)`; `dim` is 1 (scalar) or 3 (colour).
    pub fn curve(&self, name: &str, dim: usize) -> Vec<(f64, Vec<f64>)> {
        let Some(c) = self.get(name) else { return vec![] };
        let keys: Vec<&Node> = if c.seq.is_empty() { vec![c] } else { c.seq.iter().collect() };
        let mut out: Vec<(f64, Vec<f64>)> = vec![];
        for (i, k) in keys.iter().enumerate() {
            let t = k.named_num("t").unwrap_or(if i == 0 { 0.0 } else { out[i - 1].0 });
            let v = match k.get("v") {
                Some(v) => {
                    if dim == 3 {
                        v.vec3().map(|a| a.to_vec()).or_else(|| v.num().map(|n| vec![n; 3]))
                    } else {
                        v.num().map(|n| vec![n])
                    }
                }
                None => None,
            };
            let v = v.or_else(|| out.last().map(|p| p.1.clone())).unwrap_or_else(|| vec![1.0; dim]);
            out.push((t, v));
        }
        out
    }
}

/// A scalar parameter: base value, +-random fraction, curve over the particle age.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scalar {
    pub base: Option<f64>,
    pub random: f64,
    pub age: Vec<(f64, f64)>,
}

pub fn scalar(n: &Node) -> Scalar {
    Scalar {
        base: n.num(),
        random: n.named_num("Random").unwrap_or(0.0),
        age: n.curve("ParticleAge", 1).into_iter().map(|(t, v)| (t, v[0])).collect(),
    }
}

// ------------------------------------------------------------------------------------------------------------------ library -> emitters

fn r4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

fn curve_json(c: &[(f64, f64)]) -> Value {
    Value::Array(c.iter().map(|&(t, v)| json!([r4(t), r4(v)])).collect())
}

/// One flattened emitter, or None when it cannot be drawn / is disabled.  `path` is the dotted position in the effect tree.
/// `Params` keys read: Enabled, ParticleSystem, ParticleLifeTime, Count, SpawnProbability, SpawnDelay, Speed, Drag, GravityScale, Size,
/// Alpha, Color, Texture, TextureAnimation, BlendType, LightSource, EmitAngle, VerticalSpread, PositionOffset, VelocityStretch, Geometry,
/// Elasticity, Friction, StopSpeed, InheritVelocity, RandomAngles, RandomRotationRate, MoveRelativeEmitter, EmissiveLighting.
pub fn emitter(name: &str, p: &Element) -> Option<Value> {
    if p.get("Enabled") == Some("false") {
        return None;
    }
    let n = |k: &str| p.get(k).map(parse_node);
    let sc = |k: &str| n(k).map(|x| scalar(&x));
    let tex = p.get("Texture").unwrap_or("").trim().to_string();
    let geometry = p.get("Geometry").unwrap_or("").trim().to_string();
    let light = n("LightSource").and_then(|l| {
        let radius = l.get("Radius").map(scalar)?.base?;
        let int = l.get("Intensity").map(scalar).unwrap_or_default();
        Some(json!({"radius": r4(radius), "intensity": r4(int.base.unwrap_or(1.0)), "intensityAge": curve_json(&int.age), "bulb": l.named_num("LightBulbRadius").map(r4)}))
    });
    // refraction-only puffs (the "distortion" heat haze) have no colour to draw
    if p.get("Refraction").is_some() && geometry.is_empty() && light.is_none() {
        return None;
    }
    let role = if !geometry.is_empty() {
        "shell"
    } else if tex.is_empty() && light.is_some() {
        "light"
    } else {
        "sprite"
    };
    let life = sc("ParticleLifeTime").unwrap_or_default();
    let count = sc("Count").unwrap_or_default();
    let size = sc("Size").unwrap_or_default();
    let alpha = sc("Alpha").unwrap_or_default();
    let speed = sc("Speed").unwrap_or_default();
    let drag = sc("Drag").unwrap_or_default();
    let grav = sc("GravityScale").unwrap_or_default();
    let color = n("Color");
    let base_col = color.as_ref().and_then(|c| c.vec3().or_else(|| c.num().map(|v| [v; 3]))).unwrap_or([1.0; 3]);
    let col_age: Vec<Value> = color.as_ref().map(|c| c.curve("ParticleAge", 3)).unwrap_or_default().iter().map(|(t, v)| json!([r4(*t), r4(v[0]), r4(v[1]), r4(v[2])])).collect();
    let anim = n("TextureAnimation").map(|a| {
        json!({"u": a.named_num("NumFramesU").unwrap_or(1.0), "v": a.named_num("NumFramesV").unwrap_or(1.0), "blend": a.get("FrameBlend").and_then(|x| x.text()) == Some("true"),
               "loop": a.get("Loop").and_then(|x| x.text()) == Some("true"), "first": a.named_num("FirstFrameIntervalStart"), "last": a.named_num("FirstFrameIntervalEnd")})
    });
    let vec = |k: &str| n(k).and_then(|x| x.vec3()).map(|a| json!([r4(a[0]), r4(a[1]), r4(a[2])]));
    Some(json!({
        "name": name, "role": role, "sys": p.get("ParticleSystem").unwrap_or(""),
        "blend": if p.get("BlendType") == Some("Additive") { "add" } else { "alpha" },
        // the engine's default lifetime is not known: 1 s is INFERRED where the record gives none
        "life": [r4(life.base.unwrap_or(1.0)), r4(life.random)], "count": [r4(count.base.unwrap_or(1.0)), r4(count.random)],
        "prob": n("SpawnProbability").and_then(|x| x.num()).unwrap_or(1.0), "delay": n("SpawnDelay").and_then(|x| x.num()).unwrap_or(0.0),
        "speed": [r4(speed.base.unwrap_or(if speed.random > 0.0 { 1.0 } else { 0.0 })), r4(speed.random)], "perDist": sc("EmitPerDistance").and_then(|s| s.base).map(r4), "speedAge": curve_json(&speed.age),
        "drag": [r4(drag.base.unwrap_or(0.0)), r4(drag.random)], "gravity": [r4(grav.base.unwrap_or(0.0)), r4(grav.random)],
        "size": [r4(size.base.unwrap_or(1.0)), r4(size.random)], "sizeAge": curve_json(&size.age),
        "alpha": [r4(alpha.base.unwrap_or(1.0)), r4(alpha.random)], "alphaAge": curve_json(&alpha.age),
        "color": [r4(base_col[0]), r4(base_col[1]), r4(base_col[2])], "colorAge": col_age,
        "tex": tex, "anim": anim, "light": light,
        "angle": sc("EmitAngle").map(|s| r4(s.base.unwrap_or(0.0))), "spread": sc("VerticalSpread").map(|s| r4(s.base.unwrap_or(0.0))),
        "offset": vec("PositionOffset"), "emission": vec("EmissionSize"),
        "stretch": sc("VelocityStretch").and_then(|s| s.base).map(r4), "inherit": sc("InheritVelocity").and_then(|s| s.base).map(r4),
        "geometry": geometry, "elasticity": sc("Elasticity").and_then(|s| s.base).map(r4), "friction": n("Friction").and_then(|x| x.num()).map(r4),
        "stopSpeed": n("StopSpeed").and_then(|x| x.num()).map(r4), "emissive": sc("EmissiveLighting").and_then(|s| s.base).map(r4),
        "angles": vec("RandomAngles"), "spin": vec("RandomRotationRate"),
    }))
}

/// Splits an effect path `guid|Dir/Lib.Effect.name` into (guid, library path under `Libs/Particles`, effect name).  The library stem has no
/// dot, the effect name may (`Muzzle_Flash.muzzle_1st`).
pub fn split_effect_path(path: &str) -> Option<(String, String, String)> {
    let (guid, rest) = path.split_once('|')?;
    let slash = rest.rfind('/').map(|i| i + 1).unwrap_or(0);
    let dot = rest[slash..].find('.')? + slash;
    Some((guid.trim().to_lowercase(), rest[..dot].to_string(), rest[dot + 1..].to_string()))
}

/// Walks the effect element and its `Childs` depth first, dropping what cannot be drawn.
pub fn flatten(effect: &Element) -> Vec<Value> {
    fn go(el: &Element, prefix: &str, out: &mut Vec<Value>) {
        let nm = el.get("Name").unwrap_or("");
        let full = if prefix.is_empty() { nm.to_string() } else { format!("{}.{}", prefix, nm) };
        if let Some(p) = el.find("Params") {
            if let Some(e) = emitter(&full, p) {
                out.push(e);
            }
            if p.get("Enabled") == Some("false") {
                return; // a disabled parent keeps its children running in the game? unknown: INFERRED that they stay off with it
            }
        }
        if let Some(ch) = el.find("Childs") {
            for c in ch.children.iter().filter(|c| c.tag == "Particles") {
                go(c, &full, out);
            }
        }
    }
    let mut out = vec![];
    go(effect, "", &mut out);
    out
}

/// The `<Particles>` of a library matching `guid` (CONFIRMED to be the stable key) or, failing that, `name`.
pub fn find_effect<'a>(lib: &'a Element, guid: &str, name: &str) -> Option<&'a Element> {
    let mut all = vec![];
    lib.iter("Particles", &mut all);
    all.iter().copied().find(|e| e.get("GUID").map_or(false, |g| g.eq_ignore_ascii_case(guid))).or_else(|| all.iter().copied().find(|e| e.get("Name") == Some(name)))
        // EMPIRICAL: some libraries (template_fps_impacts) prefix the effect names with the library name ("Template_FPS_Impacts.Imp_Metal")
        .or_else(|| all.iter().copied().find(|e| e.get("Name").map_or(false, |n| n.len() > name.len() && n.ends_with(name) && n[..n.len() - name.len()].ends_with('.'))))
}

/// `helper` -> first person effect path, in record order, from `SWeaponParticleEffectParams` anywhere in the record JSON.  The first entry
/// per helper wins (a weapon with several fire actions has one list each; the lab shows the first mode).
pub fn effect_paths(rec: &Value) -> Vec<(String, String)> {
    fn go(v: &Value, out: &mut Vec<(String, String)>) {
        match v {
            Value::Object(m) => {
                if m.get("__struct__").and_then(|s| s.as_str()) == Some("SWeaponParticleEffectParams") {
                    let helper = m.get("helper").and_then(|h| h.as_str()).unwrap_or("");
                    let path = m.get("firstPerson").and_then(|f| f.get("path")).and_then(|p| p.as_str()).unwrap_or("");
                    if !path.is_empty() && !out.iter().any(|(h, _)| h == helper) {
                        out.push((helper.to_string(), path.to_string()));
                    }
                }
                for x in m.values() {
                    go(x, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| go(x, out)),
            _ => {}
        }
    }
    let mut out = vec![];
    go(rec, &mut out);
    out
}

/// `helper` -> first person effect path of every `SBeamEffectParams` (the continuous beam effect of a FireBeam action's `beamGroup.beamEffects`, e.g. the
/// Parallax `weapons_volt_rifle_energy_01...1st_person_beam` on `muzzle_flash`).  CONFIRMED in Game2.dcb for the field names `helper` and the effect path;
/// the path is taken as the first `path` string below the struct, so a differently named wrapper field still works (INFERRED).
pub fn beam_effect_paths(rec: &Value) -> Vec<(String, String)> {
    fn first_path(v: &Value) -> Option<String> {
        match v {
            Value::Object(m) => m
                .get("path")
                .and_then(|p| p.as_str())
                .filter(|p| !p.is_empty() && p.contains('|'))
                .map(str::to_string)
                .or_else(|| m.values().find_map(first_path)),
            Value::Array(a) => a.iter().find_map(first_path),
            _ => None,
        }
    }
    fn go(v: &Value, out: &mut Vec<(String, String)>) {
        match v {
            Value::Object(m) => {
                if m.get("__struct__").and_then(|s| s.as_str()) == Some("SBeamEffectParams") {
                    let helper = m.get("helper").and_then(|h| h.as_str()).unwrap_or("");
                    // the first person effect is listed before the third person one
                    let path = m.get("firstPerson").and_then(first_path).or_else(|| first_path(v));
                    if let Some(path) = path {
                        if !out.iter().any(|(h, _)| h == helper) {
                            out.push((helper.to_string(), path));
                        }
                    }
                }
                m.values().for_each(|x| go(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| go(x, out)),
            _ => {}
        }
    }
    let mut out = vec![];
    go(rec, &mut out);
    out
}

fn read_library(p4k: &P4k, lib: &str) -> Option<Element> {
    let rel = format!("Libs/Particles/{}.xml", lib);
    let ent = p4k.get(&format!("Data/{}", rel)).or_else(|| p4k.find_path(&rel, ".xml"))?;
    cryxml::parse(&p4k.read(ent).ok()?).ok()
}

/// The lab's effect description for a weapon record: `{"muzzle": [emitters], "shells": [emitters], "paths": {...}}`.  Emitters of the
/// `muzzle_flash` helper are the muzzle effect (flash, smoke, sparks, light); those of `shells` the casing.  None when the record names no
/// readable library.
pub fn weapon_fx(p4k: &P4k, rec: &Value) -> Option<Value> {
    let mut out = json!({"muzzle": [], "shells": [], "beam": [], "paths": {}});
    let mut any = false;
    if let Some((_, path)) = beam_effect_paths(rec).into_iter().next() {
        if let Some((guid, lib, name)) = split_effect_path(&path) {
            if let Some(eff) = read_library(p4k, &lib).and_then(|root| find_effect(&root, &guid, &name).map(flatten)) {
                out["beam"] = Value::Array(eff);
                out["paths"]["beam"] = json!(path);
                any = true;
            }
        }
    }
    for (helper, path) in effect_paths(rec) {
        let key = match helper.as_str() {
            "muzzle_flash" => "muzzle",
            "shells" => "shells",
            _ => continue,
        };
        let Some((guid, lib, name)) = split_effect_path(&path) else { continue };
        let Some(root) = read_library(p4k, &lib) else { continue };
        let Some(eff) = find_effect(&root, &guid, &name) else { continue };
        out[key] = Value::Array(flatten(eff));
        out["paths"][key] = json!(path);
        any = true;
    }
    any.then_some(out)
}

/// Flattened emitters of the effect `lib` (`Dir/Name`, under `Libs/Particles`) + `name` (or the GUID of an effect path).
pub fn effect_emitters(p4k: &P4k, lib: &str, guid: &str, name: &str) -> Option<Vec<Value>> {
    let root = read_library(p4k, lib)?;
    Some(flatten(find_effect(&root, guid, name)?))
}

/// Ammo effects for the lab: `tracer` = the ammo record's `trailParticles` (CONFIRMED: path `guid|lib.effect`, e.g. the ballistic ammo's
/// `template_fps_imp_ballistic.tracer_round`, an `EmitPerDistance` smoke trail) and `impact`: `default` (bullet hitting metal / armour / anything)
/// and `ground`.  The impact effects are INFERRED from the libraries' names: the ammo's `hitEffect` ("ammo_fps_ballistic_R_M_5mm") is a
/// MaterialEffects key whose surface table is not parsed, so `Imp_Default_01` of `template_fps_imp_ballistic` stands in for a hard surface and
/// `Imp_Concrete_Default` of `template_fps_impacts` for the floor.
pub fn ammo_fx(p4k: &P4k, ammo: Option<&Value>) -> Value {
    let tracer = ammo
        .and_then(|a| a.get("trailParticles"))
        .and_then(|t| t.get("path"))
        .and_then(|p| p.as_str())
        .and_then(split_effect_path)
        .and_then(|(g, l, n)| effect_emitters(p4k, &l, &g, &n));
    let imp = |lib: &str, name: &str| effect_emitters(p4k, lib, "", name).unwrap_or_default();
    json!({
        "tracer": tracer.unwrap_or_default(),
        "impact": {
            "default": imp("FPS_Weapons/Ammo_Templates/template_fps_imp_ballistic", "Imp_Default_01"),
            "ground": imp("FPS_Weapons/Ammo_Templates/template_fps_impacts", "Imp_Concrete_Default"),
        },
    })
}

/// First value stored under `key` anywhere in a record JSON.
pub fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m.get(key).or_else(|| m.values().find_map(|x| find_key(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(xml: &str) -> Element {
        let mut stack: Vec<Element> = vec![];
        // minimal XML reader for the tests: tags and attributes only
        let mut rest = xml;
        let mut root = None;
        while let Some(i) = rest.find('<') {
            let j = rest[i..].find('>').unwrap() + i;
            let tag = &rest[i + 1..j];
            rest = &rest[j + 1..];
            if let Some(name) = tag.strip_prefix('/') {
                let _ = name;
                let done = stack.pop().unwrap();
                match stack.last_mut() {
                    Some(p) => p.children.push(done),
                    None => root = Some(done),
                }
                continue;
            }
            let selfclose = tag.ends_with('/');
            let tag = tag.trim_end_matches('/');
            let (name, attrs) = tag.split_once(' ').unwrap_or((tag, ""));
            let mut at = vec![];
            let mut a = attrs.trim();
            while let Some(eq) = a.find("=\"") {
                let k = a[..eq].trim().to_string();
                let end = a[eq + 2..].find('"').unwrap() + eq + 2;
                at.push((k, a[eq + 2..end].to_string()));
                a = a[end + 1..].trim();
            }
            let el = Element { tag: name.to_string(), attrs: at, text: String::new(), children: vec![] };
            if selfclose {
                match stack.last_mut() {
                    Some(p) => p.children.push(el),
                    None => root = Some(el),
                }
            } else {
                stack.push(el);
            }
        }
        root.unwrap()
    }

    #[test]
    fn scalar_with_random_and_curve() {
        let s = scalar(&parse_node("0.65,Random=0.2,ParticleAge=(v=0.49,flags=32;t=1,v=1,flags=4)"));
        assert_eq!(s.base, Some(0.65));
        assert_eq!(s.random, 0.2);
        assert_eq!(s.age, vec![(0.0, 0.49), (1.0, 1.0)]);
    }

    #[test]
    fn scalar_only_random_has_no_base() {
        let s = scalar(&parse_node("Random=0.3"));
        assert_eq!((s.base, s.random), (None, 0.3));
        assert_eq!(scalar(&parse_node("10,DistanceLOD=(t=0.78,v=1;t=1)")).base, Some(10.0));
    }

    #[test]
    fn colour_curve_holds_missing_value() {
        let c = parse_node("ParticleAge=(t=0.01,v=(x=1,y=0.67,z=0.28);t=1)");
        let k = c.curve("ParticleAge", 3);
        assert_eq!(k.len(), 2);
        assert_eq!(k[0], (0.01, vec![1.0, 0.67, 0.28]));
        assert_eq!(k[1], (1.0, vec![1.0, 0.67, 0.28]));
        let b = parse_node("(x=0.027,y=0.027,z=0.027),ParticleAge=(v=(x=0.9,y=0.9,z=0.9);t=1,v=(x=1,y=1,z=1))");
        assert_eq!(b.vec3(), Some([0.027, 0.027, 0.027]));
        assert_eq!(b.curve("ParticleAge", 3)[1].1, vec![1.0, 1.0, 1.0]);
    }

    #[test]
    fn structs_and_vectors() {
        let l = parse_node("Radius=(7,Random=0.2),LightBulbRadius=0.5,SoftBulb=true,Intensity=(ParticleAge=(v=1;t=0.082,v=1;t=1))");
        assert_eq!(l.get("Radius").map(scalar).unwrap().base, Some(7.0));
        assert_eq!(l.get("LightBulbRadius").and_then(|x| x.num()), Some(0.5));
        assert_eq!(scalar(l.get("Intensity").unwrap()).base, None);
        assert_eq!(scalar(l.get("Intensity").unwrap()).age.len(), 3);
        assert_eq!(parse_node("y=-0.1").vec3(), Some([0.0, -0.1, 0.0]));
        let a = parse_node("NumFramesU=5,NumFramesV=5,FrameBlend=true");
        assert_eq!(a.named_num("NumFramesU"), Some(5.0));
    }

    #[test]
    fn effect_path_split() {
        let (g, l, n) = split_effect_path("293EFAF2-3fe6|FPS_Weapons/Weapons/weapons_none_lmg_ballistic_01.Muzzle_Flash.muzzle_1st").unwrap();
        assert_eq!((g.as_str(), l.as_str(), n.as_str()), ("293efaf2-3fe6", "FPS_Weapons/Weapons/weapons_none_lmg_ballistic_01", "Muzzle_Flash.muzzle_1st"));
        assert!(split_effect_path("no pipe").is_none());
    }

    #[test]
    fn record_paths_first_per_helper() {
        let rec = json!({"a": [{"__struct__": "SWeaponParticleEffectParams", "helper": "muzzle_flash", "firstPerson": {"path": "g|L/x.A.b"}},
            {"__struct__": "SWeaponParticleEffectParams", "helper": "muzzle_flash", "firstPerson": {"path": "g2|L/y.A.b"}},
            {"__struct__": "SWeaponParticleEffectParams", "helper": "shells", "firstPerson": {"path": "g3|L/x.S.s"}}]});
        assert_eq!(effect_paths(&rec), vec![("muzzle_flash".to_string(), "g|L/x.A.b".to_string()), ("shells".to_string(), "g3|L/x.S.s".to_string())]);
    }

    #[test]
    fn flatten_keeps_enabled_drawable_emitters() {
        let x = lib(r#"<ParticleLibrary><Particles Name="Muzzle_Flash.m" GUID="AA"><Params ParticleLifeTime="0.025" BlendType="Additive" Color="ParticleAge=(t=0.01,v=(x=1,y=0.67,z=0.28);t=1)" LightSource="Radius=3,Intensity=0.1"/><Childs>
            <Particles Name="smoke"><Params Count="10" ParticleLifeTime="2,Random=0.3" Texture="a/smoke.tif" TextureAnimation="NumFramesU=5,NumFramesV=5" Size="0.65,Random=0.2" Speed="0.5,Random=0.5" Drag="0.2" GravityScale="-0.02"/></Particles>
            <Particles Name="off"><Params Enabled="false" Texture="a/b.tif"/></Particles>
            <Particles Name="dist"><Params Refraction="true" Texture="a/r.tif"/></Particles>
            </Childs></Particles></ParticleLibrary>"#);
        let e = find_effect(&x, "aa", "zzz").unwrap();
        let f = flatten(e);
        assert_eq!(f.len(), 2, "{:?}", f);
        assert_eq!(f[0]["role"], "light");
        assert_eq!(f[0]["light"]["radius"], 3.0);
        assert_eq!(f[0]["blend"], "add");
        assert_eq!(f[1]["name"], "Muzzle_Flash.m.smoke");
        assert_eq!(f[1]["count"], json!([10.0, 0.0]));
        assert_eq!(f[1]["life"], json!([2.0, 0.3]));
        assert_eq!(f[1]["anim"]["u"], 5.0);
        assert_eq!(f[1]["gravity"][0], -0.02);
    }

    #[test]
    fn find_key_nested() {
        let v = json!({"a": [{"b": {"ammoParamsRecord": ["ref", 1, "g"]}}]});
        assert_eq!(find_key(&v, "ammoParamsRecord"), Some(&json!(["ref", 1, "g"])));
        assert!(find_key(&v, "x").is_none());
    }

    #[test]
    fn per_distance_and_default_speed() {
        let x = lib(r#"<L><Particles Name="t" GUID="g"><Params EmitPerDistance="12,DistanceLOD=(v=1;t=1)" Texture="a.tif" Speed="Random=0.9"/></Particles></L>"#);
        let f = flatten(find_effect(&x, "g", "").unwrap());
        assert_eq!(f[0]["perDist"], 12.0);
        assert_eq!(f[0]["speed"], json!([1.0, 0.9]));
    }

    /// Real archive: the LMG muzzle effect has a light root, smoke and sparks; the shell effect has geometry.  Skips without the archive.
    #[test]
    fn real_archive_lmg() {
        let Ok(path) = std::env::var("P4K_PATH") else { return };
        let Ok(p4k) = P4k::open(std::path::Path::new(&path)) else { return };
        let lib = read_library(&p4k, "FPS_Weapons/Weapons/weapons_none_lmg_ballistic_01").expect("library");
        let m = find_effect(&lib, "293efaf2-3fe6-207a-6a11-822fe0fd3db0", "").expect("muzzle effect");
        let f = flatten(m);
        assert!(f.iter().any(|e| e["role"] == "light"));
        assert!(f.iter().any(|e| e["name"].as_str().unwrap().ends_with("smoke")));
        let s = flatten(find_effect(&lib, "6d293c21-5cbc-ac32-0b6a-fce456d251a4", "").unwrap());
        assert_eq!(s[0]["role"], "shell");
    }
}

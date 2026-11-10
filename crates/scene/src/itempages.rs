//! The payloads of the weapon, gear and outfit pages: the entry points an HTTP server calls.  Everything returns the JSON `Value` the
//! route serialises (typed arrays as base64, so the geometry blobs are byte-identical); the server only has to add headers.
//!
//! * `WeaponPages`: `/weapons/<class>/{info,model,mats,cands}` with `load=` (see `swaps::parse_swaps`), `clean`.
//! * `GearPages`: `/gear/<class>/{info,model,mats,cands}` and the `skins` list.
//! * `OutfitPages`: `/outfit/data.json`, `/outfit/skins/<class>.json`, `/outfit/body.json`.
//!
//! Not here: the HTML shells (static assets), the crafting joins (`craft`, `craftmap`), the thread/pickle index caches (the caller builds
//! the indexes once with `weapons::build_index` / `GearList::build`).

use std::collections::{BTreeMap, HashMap, HashSet};

use p4k_formats::error::py_round;
use serde_json::{json, Map, Value};

use crate::dynv::{arr, g, jf, row, s, section, truthy};
use crate::gear::{self, GearList};
use crate::gearmodel;
use crate::gearstats;
use crate::itemstats::{damage, pretty};
use crate::outfit;
use crate::swaps::{self, Port, SwapIndex};
use crate::weaponmodel::{self as wm, Ctx, Look, PortRow, Scene};
use crate::weaponstats::WeaponStats;
use crate::weapons::{self, clean_text};

/// `#rrggbb` swatches of the three entries of a palette.
fn swatches(ctx: &Ctx, palette: &Value) -> Value {
    let Some(key) = palette.as_str().filter(|k| !k.is_empty()) else { return json!([]) };
    let Some(pal) = ctx.items.palette(key) else { return json!([]) };
    json!(arr(g(&pal, "entries"))
        .iter()
        .map(|e| {
            let t = arr(g(e, "t"));
            format!("#{:02x}{:02x}{:02x}", py_round(t[0].as_f64().unwrap_or(0.0) * 255.0), py_round(t[1].as_f64().unwrap_or(0.0) * 255.0), py_round(t[2].as_f64().unwrap_or(0.0) * 255.0))
        })
        .collect::<Vec<_>>())
}

fn with_swatches(ctx: &Ctx, mut skins: Vec<Value>) -> Vec<Value> {
    for sk in skins.iter_mut() {
        sk["sw"] = swatches(ctx, &sk["palette"]);
    }
    skins
}

/// The pick of `skin_id` among `skins` (the first one when unknown) and the material override it implies.
fn pick_skin<'v>(skins: &'v [Value], skin_id: &str) -> (Option<&'v Value>, Option<String>) {
    let sk = skins.iter().find(|x| s(g(x, "id")) == skin_id).or(skins.first());
    let default = skins.first();
    let material = match (sk, default) {
        (Some(k), Some(d)) if k["material"] != d["material"] => k["material"].as_str().map(str::to_string),
        _ => None,
    };
    (sk, material)
}

fn wport(p: &Port) -> weapons::Port {
    weapons::Port { name: p.name.clone(), flags: p.flags.clone(), types: p.types.clone() }
}

fn type_list(p: &Port) -> Value {
    json!(p.types.iter().map(|(t, subs)| json!([t, subs.iter().collect::<Vec<_>>()])).collect::<Vec<_>>())
}

// ---------------------------------------------------------------------------------------------------------------- weapons
pub struct WeaponPages<'a> {
    pub ctx: Ctx<'a>,
    pub index: &'a SwapIndex<'a>,
    pub ws: &'a WeaponStats<'a>,
    /// the weapon index (scan + link_variants [+ add_damage])
    pub list: Vec<Value>,
}

impl<'a> WeaponPages<'a> {
    pub fn new(ctx: Ctx<'a>, index: &'a SwapIndex<'a>, ws: &'a WeaponStats<'a>, list: Vec<Value>) -> Self {
        WeaponPages { ctx, index, ws, list }
    }

    pub fn entry(&self, cls: &str) -> Option<&Value> {
        self.list.iter().find(|e| s(g(e, "cls")) == cls)
    }

    fn variants(&self, base: &str) -> Vec<String> {
        self.list.iter().filter(|e| s(g(e, "base")) == base).map(|e| s(g(e, "cls")).to_string()).collect()
    }

    /// The valid part of `load` ({lower-case port path: class or ""}): ports must exist and be attachable, a class must be "none" or fit the port
    /// (host tags from the port's default item), and overrides equal to the default are dropped.
    pub fn clean(&self, cls: &str, load: &BTreeMap<String, String>) -> HashMap<String, String> {
        let mut ok: HashMap<String, String> = HashMap::new();
        let mut paths: Vec<&String> = load.keys().collect();
        paths.sort_by_key(|p| (p.matches('/').count(), (*p).clone()));
        for path in paths {
            let want = &load[path];
            let rows = wm::port_rows(&self.ctx, cls, &ok);
            let Some(row) = rows.iter().find(|r| &r.path == path) else { continue };
            if !weapons::attachable(&wport(&row.port_def)) {
                continue;
            }
            if want.to_lowercase() == "none" {
                if !row.default.is_empty() {
                    ok.insert(path.clone(), String::new());
                }
            } else if *want != row.default && self.index.why(&row.port_def, want, &self.index.host_tags(&row.default)).is_none() {
                ok.insert(path.clone(), want.clone());
            }
        }
        ok
    }

    pub fn parse(&self, cls: &str, text: &str) -> HashMap<String, String> {
        self.clean(cls, &swaps::parse_swaps(text))
    }

    fn ident(&self, cls: &str) -> Value {
        if cls.is_empty() {
            json!({"name": "", "manufacturer": "", "size": null, "grade": null, "type": "", "subtype": ""})
        } else {
            self.ctx.stats.identity(cls)
        }
    }

    pub fn skins(&self, cls: &str) -> Vec<Value> {
        if self.entry(cls).is_none() {
            return vec![];
        }
        with_swatches(&self.ctx, self.ctx.weapons.skins(cls, &self.variants(cls)))
    }

    /// Compatible, non-skin items of a port row; variants of another candidate (same model, class name extends its) are left out.
    pub fn candidates(&self, row: &PortRow) -> Vec<swaps::PoolItem> {
        let found = self.index.candidates(&row.port_def, &self.index.host_tags(&row.default));
        let models: HashMap<&str, Option<String>> = found.iter().map(|c| (c.cls.as_str(), self.ctx.weapons.model_path(self.ctx.p4k, &c.cls))).collect();
        let names: Vec<(String, &str)> = found.iter().map(|c| (c.cls.to_lowercase(), c.cls.as_str())).collect();
        let mut out = vec![];
        for c in &found {
            let m = &models[c.cls.as_str()];
            let low = c.cls.to_lowercase();
            let mut variant = false;
            if let Some(m) = m {
                for (other, ocls) in &names {
                    if *other != low && low.starts_with(&format!("{}_", other)) && models[ocls].as_ref().map(|x| x.to_lowercase()).unwrap_or_default() == m.to_lowercase() {
                        variant = true;
                        break;
                    }
                }
            }
            if !variant {
                out.push((*c).clone());
            }
        }
        out
    }

    pub fn scene(&self, cls: &str, load_text: &str) -> Result<(Scene, HashMap<String, String>), String> {
        let load = self.parse(cls, load_text);
        let sc = wm::build_scene(&self.ctx, cls, &load)?;
        Ok((sc, load))
    }

    /// `/weapons/<class>/model?load=`.
    pub fn model(&self, cls: &str, load_text: &str) -> Result<Value, String> {
        Ok(wm::model_json(&self.scene(cls, load_text)?.0))
    }

    /// `/weapons/<class>/mats?load=&skin=`.
    pub fn mats(&self, cls: &str, load_text: &str, skin_id: &str) -> Result<Value, String> {
        let (sc, _) = self.scene(cls, load_text)?;
        let skins = self.skins(cls);
        let (sk, material) = pick_skin(&skins, skin_id);
        let tag = sk.map(|k| s(g(k, "tag")).to_string()).unwrap_or_default();
        let palette = sk.and_then(|k| k["palette"].as_str().map(str::to_string));
        let mut looks_json: HashMap<String, Value> = HashMap::new();
        looks_json.insert(String::new(), json!({"palette": palette, "material": material}));
        for r in &sc.rows {
            if !r.cls.is_empty() {
                let l = self.ctx.weapons.item_look(&r.cls, &tag, looks_json.get(&r.parent));
                looks_json.insert(r.path.clone(), l);
            }
        }
        let looks: HashMap<String, Look> = looks_json
            .iter()
            .map(|(k, v)| (k.clone(), Look { palette: v["palette"].as_str().map(str::to_string), material: v["material"].as_str().map(str::to_string) }))
            .collect();
        let mut res = wm::slot_materials(self.ctx.p4k, self.ctx.items, &sc.slots, palette.as_deref(), material.as_deref(), true, 256, &looks, true);
        res["skin"] = json!(sk.map(|k| s(g(k, "id"))).unwrap_or(""));
        Ok(res)
    }

    /// `/weapons/<class>/cands?load=&port=`; None when the port is unknown or not attachable.
    pub fn cands(&self, cls: &str, load_text: &str, port_path: &str) -> Option<Value> {
        let load = self.parse(cls, load_text);
        let rows = wm::port_rows(&self.ctx, cls, &load);
        let row = rows.iter().find(|r| r.path == port_path.to_lowercase())?;
        if !weapons::attachable(&wport(&row.port_def)) {
            return None;
        }
        let mut out: Vec<Value> = vec![];
        for c in self.candidates(row) {
            let ident = self.ident(&c.cls);
            let effects: Vec<Value> = self.ws.changes(&c.cls).iter().map(|ch| json!({"label": ch["label"], "value": ch["value"], "kind": ch["kind"]})).collect();
            out.push(json!({"cls": c.cls, "name": ident["name"], "manufacturer": ident["manufacturer"], "size": c.size, "grade": c.grade, "subtype": c.subtype,
                "default": c.cls == row.default, "current": c.cls == row.cls, "effects": effects}));
        }
        out.sort_by(|a, b| {
            let ka = (a["size"].as_i64().unwrap_or(0), s(&a["name"]).to_lowercase(), s(&a["cls"]).to_string());
            let kb = (b["size"].as_i64().unwrap_or(0), s(&b["name"]).to_lowercase(), s(&b["cls"]).to_string());
            ka.cmp(&kb)
        });
        Some(json!({"port": row.path, "label": weapons::port_label(&wport(&row.port_def)), "current": row.cls, "default": row.default, "items": out}))
    }

    /// Section "Damage falloff" of the ammunition in use, or None.
    fn falloff(&self, data: &Value, classes: &[String]) -> Option<Value> {
        let (_cont, ammo, _src) = self.ws.ammo_source(data, classes);
        let drop = ammo.as_ref().map(|a| g(g(a, "projectileParams"), "damageDropParams").clone()).unwrap_or(Value::Null);
        let mx = |d: &Value| damage(d).iter().map(|x| x.1).fold(None, |m: Option<f64>, x| Some(m.map_or(x, |y| y.max(x))));
        let (start, per_m, floor) = (mx(g(&drop, "damageDropMinDistance")), mx(g(&drop, "damageDropPerMeter")), mx(g(&drop, "damageDropMinDamage")));
        if start.is_none() && per_m.is_none() {
            return None;
        }
        let v = |x: Option<f64>| x.map_or(Value::Null, jf);
        section("Damage falloff", vec![Some(row("Full damage up to", v(start), "m", "", "")), Some(row("Then loses", v(per_m), "per m", "", "")), Some(row("Never below", v(floor), "per hit", "", ""))])
    }

    /// Collateral-damage inputs of the ammunition in use, or Null: the per-hit damage, the particle
    /// `pierceability`, `pierceabilityParams.damageFalloffLevel1..3` and `maxPenetrationThickness`.  EMPIRICAL (records); the page's
    /// model of them is INFERRED (percent of the current damage lost per entity passed, level 3 repeats), engine semantics unverified.
    fn pierce(&self, data: &Value, classes: &[String]) -> Value {
        let (_cont, ammo, _src) = self.ws.ammo_source(data, classes);
        let Some(ammo) = ammo.filter(|a| truthy(a)) else { return Value::Null };
        let proj = g(&ammo, "projectileParams");
        let pz = g(proj, "pierceabilityParams");
        let dmg: f64 = damage(g(proj, "damage")).iter().map(|x| x.1).sum();
        if !truthy(pz) || dmg <= 0.0 {
            return Value::Null;
        }
        let lv = |k: &str| pz.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        json!({"damage": jf(dmg), "pierceability": g(g(g(&ammo, "physicsControllerParams"), "PhysType"), "pierceability").clone(),
            "falloff": [lv("damageFalloffLevel1"), lv("damageFalloffLevel2"), lv("damageFalloffLevel3")],
            "max_thickness": g(pz, "maxPenetrationThickness").clone()})
    }

    /// Distance damage drop of the ammunition in use as plain numbers for the page's curve, or Null: per damage type the damage, the full-damage
    /// distance, the loss per metre and the floor (`damageDropParams`), plus the range (speed x lifetime).  EMPIRICAL (records); the formula
    /// the page draws (damage - per_m x (distance - start), never below the floor) is INFERRED from the field names (engine not read).
    fn drop_curve(&self, data: &Value, classes: &[String]) -> Value {
        let (_cont, ammo, _src) = self.ws.ammo_source(data, classes);
        let Some(ammo) = ammo.filter(|a| truthy(a)) else { return Value::Null };
        let proj = g(&ammo, "projectileParams");
        let drop = g(proj, "damageDropParams");
        if !truthy(drop) {
            return Value::Null;
        }
        let pick = |d: &Value, t: &str| damage(d).iter().find(|x| x.0 == t).map_or(0.0, |x| x.1);
        let mut types = vec![];
        for (t, dmg) in damage(g(proj, "damage")) {
            let (start, per_m, floor) = (pick(g(drop, "damageDropMinDistance"), t), pick(g(drop, "damageDropPerMeter"), t), pick(g(drop, "damageDropMinDamage"), t));
            types.push(json!({"type": t, "damage": jf(dmg), "start": jf(start), "per_m": jf(per_m), "floor": jf(floor)}));
        }
        let (speed, life) = (crate::dynv::num(g(&ammo, "speed")).unwrap_or(0.0), crate::dynv::num(g(&ammo, "lifetime")).unwrap_or(0.0));
        if types.is_empty() {
            return Value::Null;
        }
        json!({"types": types, "range": jf(speed * life)})
    }

    /// Inputs of the weapons lab (`/lab/`): per fire mode the name, kind, rounds/min, pellets, damage multiplier, ammo per shot and spread, the
    /// magazine size, speed and lifetime of the ammunition in use and per damage type its damage.  EMPIRICAL (records, same reading as the
    /// "Fire mode" sections); Null for an item without fire actions.  The reload time is NOT in the records read here (the lab lets the user set it).
    fn lab(&self, data: &Value, classes: &[String]) -> Value {
        let wc = crate::itemstats::comp(data, "SCItemWeaponComponentParams");
        if !truthy(wc) {
            return Value::Null;
        }
        let (cont, ammo, _src) = self.ws.ammo_source(data, classes);
        let ammo = ammo.unwrap_or(Value::Null);
        let proj = g(&ammo, "projectileParams");
        let num = |v: &Value| crate::dynv::num(v).unwrap_or(0.0);
        let mut modes = vec![];
        let mut seen = HashSet::new();
        let mk = |outer: &Value, fa: &Value| -> Value {
            let kind = crate::dynv::struct_of(fa).replace("SWeaponAction", "").replace("Params", "");
            let name = [g(outer, "name"), g(fa, "name")].iter().find(|v| truthy(v)).and_then(|v| v.as_str()).unwrap_or(&kind).to_string();
            let launch = g(fa, "launchParams");
            let sp = g(launch, "spreadParams");
            json!({"name": name, "kind": kind, "rate": num(g(fa, "fireRate")), "pellets": if truthy(g(launch, "pelletCount")) { num(g(launch, "pelletCount")) } else { 1.0 },
                "mult": if g(launch, "damageMultiplier").is_null() { 1.0 } else { num(g(launch, "damageMultiplier")) }, "cost": if truthy(g(launch, "ammoCost")) { num(g(launch, "ammoCost")) } else { 1.0 },
                "spread": [num(g(sp, "min")), num(g(sp, "max"))],
                "grow": [num(g(sp, "firstAttack")), num(g(sp, "attack")), num(g(sp, "decay"))], "charged": !std::ptr::eq(outer, fa), "charge_time": num(g(outer, "chargeTime"))})
        };
        for (outer, fa) in crate::itemstats::ItemStats::modes(wc) {
            let m = mk(outer, fa);
            if seen.insert(m.to_string()) {
                modes.push(m);
            }
        }
        // A Parallel action (Tripledown: three barrels) or an `Automatically` sequence (Deadrig "Burst": two barrels; Scalpel "Burst": two shots) runs every child on one pull, and a child
        // may differ only in its spread numbers: such a group is ONE mode (the first child's) with `volley` = the children (x repetitions); the other children's modes are dropped.  A group
        // whose children differ in rate, pellets, damage or ammo cost, a `Looping` sequence (Killshot alternates barrels) and an `Individually` one (one entry per pull) keep their
        // modes.  INFERRED: the entries' delay + unit + repetitions are read as the repeat interval, so the entries go off together (the user sees both Deadrig barrels fire at once).
        let sig = |m: &Value| format!("{}|{}|{}|{}|{}|{}", m["name"], m["kind"], m["rate"], m["pellets"], m["mult"], m["cost"]);
        for fa in arr(g(wc, "fireActions")) {
            let ty = crate::dynv::struct_of(fa);
            let kids: Vec<(Value, f64)> = if ty == "SWeaponActionParallelParams" {
                arr(g(fa, "weaponActions")).iter().filter(|c| truthy(c)).map(|c| (mk(c, c), 1.0)).collect()
            } else if ty == "SWeaponActionSequenceParams" && s(g(fa, "mode")) == "Automatically" {
                arr(g(fa, "sequenceEntries"))
                    .iter()
                    .filter(|e| truthy(e) && truthy(g(e, "weaponAction")))
                    .map(|e| (mk(g(e, "weaponAction"), g(e, "weaponAction")), if truthy(g(e, "repetitions")) { num(g(e, "repetitions")).max(1.0) } else { 1.0 }))
                    .collect()
            } else {
                continue;
            };
            let n: f64 = kids.iter().map(|k| k.1).sum();
            if kids.len() < 2 || n <= 1.0 || !kids.iter().all(|k| sig(&k.0) == sig(&kids[0].0)) {
                continue;
            }
            let keys: Vec<String> = kids.iter().map(|k| k.0.to_string()).collect();
            let Some(first) = modes.iter().position(|m| keys.contains(&m.to_string())) else { continue };
            let mut i = 0;
            modes.retain(|m| {
                i += 1;
                i - 1 == first || !keys.contains(&m.to_string())
            });
            modes[first]["volley"] = json!(n);
        }
        if modes.is_empty() {
            return Value::Null;
        }
        let dmg: Vec<Value> = damage(g(proj, "damage")).iter().map(|(t, v)| json!([t, jf(*v)])).collect();
        // what the loadout changes (product of the attachments' multipliers, weaponstats `factor`): the lab applies them to damage, speed, fire rate, spread, zoom and recoil
        let fac = |p: &str| jf(self.ws.factor(classes, p));
        let mods = json!({"damage": fac("damageMultiplier"), "speed": fac("projectileSpeedMultiplier"), "rate": fac("fireRateMultiplier"), "spread_min": fac("spreadModifier.minMultiplier"),
            "spread_max": fac("spreadModifier.maxMultiplier"), "spread_first": fac("spreadModifier.firstAttackMultiplier"),
            "spread_attack": fac("spreadModifier.attackMultiplier"), "spread_decay": fac("spreadModifier.decayMultiplier"), "zoom": fac("aimModifier.zoomScale"), "zoom_time": fac("aimModifier.zoomTimeScale"),
            "pitch": fac("recoilModifier.aimRecoilModifier.curveRecoil.pitchMaxDegreesModifier"), "yaw": fac("recoilModifier.aimRecoilModifier.curveRecoil.yawMaxDegreesModifier"),
            "kick": fac("recoilModifier.curveRecoil.positionModifiers.yMaxValueModifier"), "sound": fac("soundRadiusMultiplier")});
        // the weapon's own aim action (zoomScale, zoomTime) and default ADS camera offset (EMPIRICAL: SCItemWeaponComponentParams.aimAction / defaultAdsCameraOffset)
        let v3 = |v: &Value| json!([num(g(v, "x")), num(g(v, "y")), num(g(v, "z"))]);
        let aim_a = g(wc, "aimAction");
        // while aiming the weapon's spread is scaled by the aim action's own spreadModifier (EMPIRICAL: aimAction.aimModifier.weaponStats.spreadModifier; 0 on the P6-LR, i.e. no spread when aimed)
        let asm = g(g(g(aim_a, "aimModifier"), "weaponStats"), "spreadModifier");
        let mult = |k: &str| if g(asm, k).is_null() { 1.0 } else { num(g(asm, k)) };
        let (min_mult, max_mult, first_mult, attack_mult, decay_mult) = (mult("minMultiplier"), mult("maxMultiplier"), mult("firstAttackMultiplier"), mult("attackMultiplier"), mult("decayMultiplier"));
        let aim = json!({"zoom": if truthy(g(aim_a, "zoomScale")) { num(g(aim_a, "zoomScale")) } else { 1.0 }, "time": num(g(aim_a, "zoomTime")), "ads": v3(g(wc, "defaultAdsCameraOffset")),
        "spread": [min_mult, max_mult, first_mult, attack_mult, decay_mult]});
        // attachments the lab animates: optics (scopeAttachmentParams + adsCameraOffset), laser pointers (auxiliary beam action, range) and flashlights, with their audio triggers
        let st = self.ctx.stats;
        let trig = |v: &Value, k: &str| g(g(v, k), "audioTrigger").clone();
        let mut att = vec![];
        for c in classes {
            let Some(d) = st.record(c, 6) else { continue };
            let m = crate::itemstats::comp(&d, "SWeaponModifierComponentParams");
            let scope = g(m, "scopeAttachmentParams");
            // an optic has scopeAttachmentParams (transition sounds) or, without them (EMPIRICAL: the NV-TAC reflex / red dot, Behring reflex, 1x sights), only an adsCameraOffset
            let ads_off = ["x", "y", "z"].iter().any(|k| num(g(g(m, "adsCameraOffset"), k)) != 0.0);
            if truthy(scope) || ads_off {
                att.push(json!({"cls": c, "kind": "optic", "ads": v3(g(m, "adsCameraOffset")), "zoom": jf(self.ws.factor(&[c.clone()], "aimModifier.zoomScale")), "zoom2": jf(self.ws.factor(&[c.clone()], "aimModifier.secondZoomScale")), "in": trig(scope, "transitionInSound"), "out": trig(scope, "transitionOutSound")}));
            }
            let ax = crate::itemstats::comp(&d, "SAuxiliaryWeaponActionComponentParams");
            if truthy(ax) {
                let act = g(ax, "auxiliaryWeaponAction");
                let ty = crate::dynv::struct_of(act);
                let kind = if ty.contains("Flashlight") { "light" } else if ty.contains("Beam") { "laser" } else { continue };
                att.push(json!({"cls": c, "kind": kind, "range": num(g(act, "range")), "on": trig(ax, "startAudioTriggerOneshot"), "loop": trig(ax, "startAudioTriggerLoop"), "stop": trig(ax, "stopAudioTrigger"), "off": trig(ax, "endOfUseAudioTrigger")}));
            }
        }
        json!({"modes": modes, "magazine": num(g(&cont, "maxAmmoCount")), "speed": num(g(&ammo, "speed")), "lifetime": num(g(&ammo, "lifetime")), "damage": dmg, "mods": mods, "aim": aim, "att": att})
    }

    /// `/weapons/<class>/info?load=`.
    pub fn info(&self, cls: &str, load_text: &str) -> Value {
        let st = self.ctx.stats;
        let entry = self.entry(cls);
        let load = self.parse(cls, load_text);
        let rows = wm::port_rows(&self.ctx, cls, &load);
        let base_rows = wm::port_rows(&self.ctx, cls, &HashMap::new());
        let data = st.record(cls, 14);
        let classes = |rs: &[PortRow]| rs.iter().filter(|r| !r.cls.is_empty()).map(|r| r.cls.clone()).collect::<Vec<_>>();
        let (now_cls, dflt_cls) = (classes(&rows), classes(&base_rows));
        let att: Vec<&PortRow> = rows.iter().filter(|r| !r.cls.is_empty()).collect();
        let mut ports = vec![];
        for r in &rows {
            let wp = wport(&r.port_def);
            let can = weapons::attachable(&wp);
            let ident = self.ident(&r.cls);
            let name = if truthy_s(&ident["name"]) { ident["name"].clone() } else { json!("(empty)") };
            ports.push(json!({"path": r.path, "port": r.port, "label": weapons::port_label(&wp), "depth": r.depth, "parent": r.parent, "cls": r.cls,
                "name": name, "default": r.default, "default_name": self.ident(&r.default)["name"], "attachable": can,
                "size": [r.port_def.min, r.port_def.max], "types": type_list(&r.port_def),
                "candidates": if can { self.candidates(r).len() } else { 0 }, "placed": r.frame || r.cls.is_empty()}));
        }
        let mut card = self.ident(cls).as_object().cloned().unwrap_or_default();
        let full = st.item(cls).unwrap_or(json!({}));
        card.insert("description".into(), g(&full, "description").clone());
        card.insert("cls".into(), json!(cls));
        let mut sections = self.ws.sections(cls, &now_cls);
        let eff = self.ws.attachment_section(&json!(att.iter().map(|r| json!({"label": weapons::port_label(&wport(&r.port_def)), "cls": r.cls, "name": self.ident(&r.cls)["name"]})).collect::<Vec<_>>()));
        if let Some(e) = &eff {
            sections.push(e.clone());
        }
        let fall = data.as_ref().and_then(|d| self.falloff(d, &now_cls));
        if let Some(f) = fall {
            let at = sections.len() - usize::from(eff.is_some());
            sections.insert(at, f);
        }
        let mut sorted: Vec<(&String, &String)> = load.iter().collect();
        sorted.sort();
        let fmt_load = sorted.iter().map(|(p, c)| format!("{}:{}", p, if c.is_empty() { "none" } else { c })).collect::<Vec<_>>().join(";");
        let metrics = |cl: &[String]| Value::Object(self.ws.metrics(cls, cl, data.as_ref()));
        json!({"cls": cls, "card": card, "group": entry.map_or(Value::Null, |e| g(e, "group").clone()), "kind": entry.map_or(json!(""), |e| g(e, "kind").clone()),
            "dclass": entry.map_or(json!(""), |e| g(e, "dclass").clone()), "ports": ports, "load": fmt_load, "metrics": metrics(&now_cls), "base": metrics(&dflt_cls),
            "recoil": self.ws.recoil_detail(cls, &now_cls, data.as_ref()), "models": self.ws.damage_models(cls, &now_cls, data.as_ref()),
            "drop": data.as_ref().map_or(Value::Null, |d| self.drop_curve(d, &now_cls)),
            "pierce": data.as_ref().map_or(Value::Null, |d| self.pierce(d, &now_cls)),
            "lab": data.as_ref().map_or(Value::Null, |d| self.lab(d, &now_cls)),
            "sections": sections, "skins": self.skins(cls)})
    }
}

fn truthy_s(v: &Value) -> bool {
    truthy(v)
}

// ---------------------------------------------------------------------------------------------------------------- gear
const NOISE: [&str; 5] = ["entityspawner", "template", "_test", "test_", "nodraw"];

/// Readable name of a port: "wep_stocked_3" -> "Wep stocked 3".
pub fn port_label(name: &str) -> String {
    // re.sub(r"[_\s]+", " ", re.sub(r"(?<=[a-z])(?=[A-Z])", " ", name)).strip().capitalize()
    let mut spaced = String::new();
    let mut prev: Option<char> = None;
    for c in name.chars() {
        if c.is_ascii_uppercase() && prev.map_or(false, |p| p.is_ascii_lowercase()) {
            spaced.push(' ');
        }
        spaced.push(c);
        prev = Some(c);
    }
    let mut out = String::new();
    let mut gap = false;
    for c in spaced.chars() {
        if c == '_' || c.is_whitespace() {
            gap = true;
        } else {
            if gap {
                out.push(' ');
            }
            gap = false;
            out.push(c);
        }
    }
    let t = out.trim().to_string();
    let mut it = t.chars();
    match it.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &it.as_str().to_lowercase(),
        None => String::new(),
    }
}

pub struct GearPages<'a> {
    pub ctx: Ctx<'a>,
    pub index: &'a SwapIndex<'a>,
    pub list: &'a GearList,
}

impl<'a> GearPages<'a> {
    pub fn new(ctx: Ctx<'a>, index: &'a SwapIndex<'a>, list: &'a GearList) -> Self {
        GearPages { ctx, index, list }
    }

    pub fn skins(&self, cls: &str) -> Vec<Value> {
        let Some(_) = self.list.get(cls) else { return vec![] };
        let c = &self.ctx;
        with_swatches(c, gear::skins(c.items, c.stats, c.weapons, c.p4k, cls, &self.list.variants(cls)))
    }

    fn candidates(&self, port: &Port) -> Vec<swaps::PoolItem> {
        let found = self.index.candidates(port, &self.index.host_tags(""));
        let names: HashMap<String, &str> = found.iter().map(|c| (c.cls.to_lowercase(), c.cls.as_str())).collect();
        let mut out = vec![];
        for c in &found {
            let low = c.cls.to_lowercase();
            if NOISE.iter().any(|n| low.contains(n)) {
                continue;
            }
            let ident = self.ctx.stats.identity(&c.cls);
            if ident["name"].as_str() == Some(&pretty(&c.cls)) {
                continue; // no localised name: an internal record
            }
            let m = self.ctx.weapons.model_path(self.ctx.p4k, &c.cls).unwrap_or_default();
            let variant = !m.is_empty()
                && names.iter().any(|(o, ocls)| *o != low && low.starts_with(&format!("{}_", o)) && self.ctx.weapons.model_path(self.ctx.p4k, ocls).unwrap_or_default().to_lowercase() == m.to_lowercase());
            if !variant {
                out.push((*c).clone());
            }
        }
        out
    }

    /// `/gear/<class>/info`.
    pub fn info(&self, cls: &str) -> Value {
        let st = self.ctx.stats;
        let entry = self.list.get(cls);
        let data = st.record(cls, 14);
        let ident = st.identity(cls);
        let full = st.item(cls).unwrap_or(json!({}));
        let mut ports = vec![];
        for (name, port) in swaps::item_ports(st, cls) {
            if port.types.is_empty() {
                continue;
            }
            ports.push(json!({"port": name, "label": port_label(&name), "types": type_list(&port), "size": [port.min, port.max],
                "flags": port.flags.iter().collect::<Vec<_>>(), "candidates": self.candidates(&port).len()}));
        }
        let order: Vec<&str> = gear::SLOTS.iter().map(|x| x.0).collect();
        let mut fam: Vec<&Value> = entry.filter(|e| truthy(g(e, "set"))).map(|e| self.list.family(s(g(e, "set")))).unwrap_or_default();
        fam.sort_by_key(|m| order.iter().position(|x| *x == s(g(m, "slot"))).unwrap_or(99));
        let fam: Vec<Value> = fam.iter().map(|m| json!({"cls": m["cls"], "name": m["name"], "slot": m["slot"], "label": gear::slot_label(s(g(m, "slot"))).unwrap_or(s(g(m, "slot")))})).collect();
        let e = |k: &str, d: Value| entry.map_or(d, |e| g(e, k).clone());
        json!({"cls": cls, "card": {"name": ident["name"], "manufacturer": clean_text(s(&ident["manufacturer"])), "description": g(&full, "description"),
                "size": ident["size"], "grade": ident["grade"], "cls": cls},
            "kind": e("kind", json!("")), "slot": e("slot", json!("")), "slot_label": entry.map_or("", |x| gear::slot_label(s(g(x, "slot"))).unwrap_or("")),
            "subtype": e("subtype", json!("")), "type": e("type", json!("")), "set": e("set", Value::Null), "set_name": e("set_name", json!("")),
            "set_items": fam, "ports": ports, "skins": self.skins(cls), "sections": gearstats::sections(st, cls, data.as_ref(), None),
            "facts": e("facts", json!({})), "model_path": e("model", json!("")), "dcb": format!("/dcb/record/EntityClassDefinition.{}", cls)})
    }

    /// `/gear/<class>/cands?port=`; None when the item has no such port.
    pub fn cands(&self, cls: &str, port_name: &str) -> Option<Value> {
        let st = self.ctx.stats;
        let ports = swaps::item_ports(st, cls);
        let (_, port) = ports.iter().find(|(k, _)| *k == port_name.to_lowercase())?;
        if port.types.is_empty() {
            return None;
        }
        let mut out: Vec<Value> = vec![];
        for c in self.candidates(port) {
            let ident = st.identity(&c.cls);
            let url: Value = if let Some(e) = self.list.get(&c.cls) {
                if e["base"].is_null() { json!(format!("/gear/{}", c.cls)) } else { json!(format!("/gear/{}#skin={}", s(&e["base"]), c.cls.to_lowercase())) }
            } else {
                Value::Null
            };
            out.push(json!({"cls": c.cls, "name": ident["name"], "manufacturer": ident["manufacturer"], "size": c.size, "subtype": c.subtype, "url": url}));
        }
        out.sort_by(|a, b| (s(&a["name"]).to_lowercase(), s(&a["cls"]).to_string()).cmp(&(s(&b["name"]).to_lowercase(), s(&b["cls"]).to_string())));
        Some(json!({"port": port_name, "label": port_label(port_name), "types": type_list(port), "items": out}))
    }

    pub fn scene(&self, cls: &str, sex: &str, worn: bool) -> Result<Scene, String> {
        let entry = self.list.get(cls).ok_or("no such item")?;
        let c = &self.ctx;
        gearmodel::build_scene(c.p4k, c.items, c.stats, c.weapons, entry, sex, worn, None)
    }

    /// `/gear/<class>/model?sex=&worn=`.
    pub fn model(&self, cls: &str, sex: &str, worn: bool) -> Result<Value, String> {
        Ok(gearmodel::model_json(&self.scene(cls, sex, worn)?, worn))
    }

    /// `/gear/<class>/mats?skin=&sex=&worn=`.
    pub fn mats(&self, cls: &str, skin_id: &str, sex: &str, worn: bool) -> Result<Value, String> {
        let sc = self.scene(cls, sex, worn)?;
        let skins = self.skins(cls);
        let (sk, material) = pick_skin(&skins, skin_id);
        let palette = sk.and_then(|k| k["palette"].as_str().map(str::to_string));
        // a skin other than the default also recolours the default-loadout parts (arms, legs)
        let att_palette = if skins.first().map_or(false, |d| sk.map_or(false, |k| k["id"] != d["id"])) { palette.as_deref() } else { None };
        let mut res = gearmodel::slot_materials(self.ctx.p4k, self.ctx.items, &sc.slots, palette.as_deref(), att_palette, material.as_deref(), true, true);
        res["skin"] = json!(sk.map(|k| s(g(k, "id"))).unwrap_or(""));
        Ok(res)
    }
}

// ---------------------------------------------------------------------------------------------------------------- outfit
pub struct OutfitPages<'a> {
    pub gear: &'a GearPages<'a>,
    /// `outfit::build_rules`
    pub rules: Value,
}

impl<'a> OutfitPages<'a> {
    pub fn new(gear: &'a GearPages<'a>) -> Self {
        let rules = outfit::build_rules(gear.ctx.items, gear.ctx.stats, &gear.list.listed());
        OutfitPages { gear, rules }
    }

    /// `/outfit/data.json` (crafting flags are null: `craftable`).
    pub fn data(&self, craftable: Option<&HashSet<String>>) -> Value {
        let cat = outfit::catalog_rows(&self.gear.list.listed(), &self.rules, craftable);
        let slots: Vec<Value> = gear::SLOTS.iter().filter(|(sl, _, _)| cat.values().any(|r| s(g(r, "slot")) == *sl)).map(|(a, b, c)| json!([a, b, c])).collect();
        let mut hosts = Map::new();
        for (k, v) in self.rules["hosts"].as_object().into_iter().flatten() {
            hosts.insert(k.clone(), json!({"cls": v["cls"], "ports": v["ports"]}));
        }
        json!({"slots": slots, "catalog": cat, "hosts": hosts, "craft": craftable.is_some(), "regions": outfit::REGIONS, "carry": outfit::CARRY_FLAGS})
    }

    /// `/outfit/skins/<class>.json`.
    pub fn skins(&self, cls: &str) -> Value {
        json!(self.gear.skins(cls).iter().map(|x| json!({"id": x["id"], "name": x["name"], "cls": x["cls"], "sw": x["sw"]})).collect::<Vec<_>>())
    }

    /// `/outfit/body.json?sex=`: {model, mats}.
    pub fn body(&self, sex: &str) -> Value {
        let c = &self.gear.ctx;
        let parts = gearmodel::body_parts(c.p4k, sex);
        let slots = vec![wm::Slot { src: "human".into(), cls: None, mtl: None, sub: -1 }];
        let mats = gearmodel::slot_materials(c.p4k, c.items, &slots, None, None, None, true, true);
        let sps: Vec<wm::ScenePart> = parts
            .iter()
            .map(|p| wm::ScenePart { name: p.name.clone(), verts: p.verts.clone(), tris: p.tris.clone(), uv: p.uv.clone(), slot: 0, nrm: p.nrm.clone(), tan: p.tan.clone(), vc: p.vc.clone() })
            .collect();
        let (enc, bbox) = wm::encode_parts(&sps);
        let model = json!({"parts": enc, "bbox": bbox, "tris": sps.iter().map(|p| p.tris.len()).sum::<usize>(), "slots": [{"src": "human"}], "notes": [], "reason": "", "worn": true});
        json!({"model": model, "mats": mats})
    }
}

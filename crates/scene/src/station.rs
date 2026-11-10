//! Stations: what the DataCore says about the seats and turrets you sit at, and the turret joints that rotate.
//! Pure functions over decoded DataCore structs given as JSON (`dcb::Value::to_json`: a struct is an object with
//! `__struct__`, a strong / weak pointer beyond the decode depth is `["strong", struct, index]`), no archive access.
//!
//! Evidence marks: EMPIRICAL = read from Game2.dcb of the live archive,
//! INFERRED = a reading, not confirmed against the engine.  Notably: yaw = the joint's local Z, pitch = local X, roll = local Y (INFERRED);
//! 462 turret classes, 32 axes carry a movementTag (untagged ones are used); the command groups of `vehicle_commands` are INFERRED from
//! the names; the sub kind from the class name is INFERRED.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

pub const DEFAULT_SPEED: f64 = 60.0;

pub fn jstr<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// `float(x or 0)` of a JSON field.
pub fn jnum(v: &Value, k: &str) -> f64 {
    match v.get(k) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::Bool(b)) => *b as u8 as f64,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0), // the DCB stores some numbers as text
        _ => 0.0,
    }
}

/// Python truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(true, |f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn rx(p: &str) -> Regex {
    Regex::new(&format!("(?i){}", p)).unwrap()
}

struct SubRe {
    atc: Regex,
    remote: Regex,
    engineer: Regex,
    named: Vec<(&'static str, Regex)>,
    second: Vec<(&'static str, Regex)>,
    pod: Regex,
    bridge: Regex,
    passenger: Regex,
    prison: Regex,
}

fn sub_re() -> &'static SubRe {
    static R: OnceLock<SubRe> = OnceLock::new();
    R.get_or_init(|| SubRe {
        atc: rx(r"(^|_)atc(_|$)|air_?traffic"),
        remote: rx(r"remote_?turret|turret_?console|tractor_?console|torpedo_?console|support_seat|turret_?seat|seat_?turret|seat_remote|console_turret"),
        engineer: rx("engineer"),
        named: vec![("captain", rx("captain")), ("copilot", rx("co_?pilot")), ("pilot", rx("pilot|driver")), ("gunner", rx("gunner"))],
        second: vec![
            ("tactical", rx("tactical")),
            ("science", rx("science")),
            ("comms", rx(r"(^|_)comms?(_|$)")),
            ("security", rx("security")),
            ("brig", rx(r"(^|_)brig(_|$)")),
        ],
        pod: rx(r"esc_?pod|escapepod"),
        bridge: rx("bridge"),
        passenger: rx(r"passenger|(^|_)drop(_|$)"),
        prison: rx("prison"),
    })
}

/// Seat classification (one of SUBKINDS).
pub fn subkind(cls: &str, kind: &str, default_mode: &str, modes: &[String], has_turret: bool, bed: bool, source: &str) -> &'static str {
    let r = sub_re();
    if bed {
        return "bed";
    }
    if kind == "turret" {
        return "turret";
    }
    if default_mode == "AirTrafficController" || r.atc.is_match(cls) {
        return "atc";
    }
    if r.engineer.is_match(cls) {
        return "engineer";
    }
    if (default_mode == "Turret" && !has_turret) || r.remote.is_match(cls) {
        return "remote_turret";
    }
    for (k, re) in &r.named {
        if re.is_match(cls) {
            return k;
        }
    }
    for (k, re) in &r.second {
        if re.is_match(cls) {
            return k;
        }
    }
    if r.pod.is_match(cls) {
        return "escape_pod";
    }
    if r.bridge.is_match(cls) {
        return "bridge";
    }
    if ["Salvage", "Mining", "Scanning", "Missile", "Refuel"].contains(&default_mode) {
        return "operator";
    }
    if r.passenger.is_match(cls) || r.prison.is_match(cls) {
        return "passenger";
    }
    if modes.iter().any(|m| m == "Flight") && source == "seat" {
        return "pilot";
    }
    if source == "seat" {
        "seat"
    } else {
        "chair"
    }
}

fn ref_set(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Array(a)) => a.len() == 3 && a[1].as_f64() != Some(4294967295.0),
        _ => false,
    }
}

/// A turret joint of a `turret_spec`: {n, ax, speed, lo, hi, rel, tab}.
#[derive(Debug, Clone, PartialEq)]
pub struct Joint {
    pub n: String,
    pub ax: &'static str,
    pub speed: f64,
    pub lo: Option<f64>,
    pub hi: Option<f64>,
    pub rel: Option<String>,
    pub tab: Option<Vec<[f64; 3]>>,
    tagged: bool,
}

impl Joint {
    pub fn to_json(&self) -> Value {
        json!({"n": self.n, "ax": self.ax, "speed": self.speed, "lo": self.lo, "hi": self.hi, "rel": self.rel,
               "tab": self.tab.as_ref().map(|t| t.iter().map(|r| json!([r[0], r[1], r[2]])).collect::<Vec<_>>())})
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurretSpec {
    pub style: String,
    pub joints: Vec<Joint>,
    pub gyro: bool,
    pub remote: bool,
    pub fire_angle: Option<Value>,
    pub recenter: bool,
}

/// Rotating joints of a decoded SCItemTurretParams, or None without joints.
pub fn turret_spec(tp: &Value) -> Option<TurretSpec> {
    if !tp.is_object() {
        return None;
    }
    let mut ents: Vec<Joint> = vec![];
    for m in tp.get("movementList").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        if !m.is_object() || jstr(m, "jointName").is_empty() {
            continue;
        }
        let tagged = ref_set(m.get("movementTag"));
        for (ax, key) in [("yaw", "yawAxis"), ("pitch", "pitchAxis"), ("roll", "rollAxis")] {
            let a = match m.get(key) {
                Some(a) if a.is_object() => a,
                _ => continue,
            };
            let lim = a.get("angleLimits").filter(|x| truthy(Some(x))).cloned().unwrap_or(json!({}));
            let sp = jnum(a, "speed");
            let mut e = Joint { n: jstr(m, "jointName").to_string(), ax, speed: if sp != 0.0 { sp } else { DEFAULT_SPEED }, lo: None, hi: None, rel: None, tab: None, tagged };
            if jstr(&lim, "__struct__") == "SCItemTurretCustomAngleLimitParams" {
                let mut tab: Vec<[f64; 3]> = lim
                    .get("AngleLimits")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .filter(|x| x.is_object())
                    .map(|x| [pymod(jnum(x, "TurretRotation"), 360.0), jnum(x, "LowestAngle"), jnum(x, "HighestAngle")])
                    .collect();
                tab.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                if !tab.is_empty() {
                    let rel = jstr(&lim, "RelativeJointName");
                    e.rel = if rel.is_empty() { None } else { Some(rel.to_string()) };
                    e.lo = Some(tab.iter().map(|t| t[1]).fold(f64::INFINITY, f64::min));
                    e.hi = Some(tab.iter().map(|t| t[2]).fold(f64::NEG_INFINITY, f64::max));
                    e.tab = Some(tab);
                }
            } else if lim.get("LowestAngle").is_some() {
                e.lo = Some(jnum(&lim, "LowestAngle"));
                e.hi = Some(jnum(&lim, "HighestAngle"));
            }
            ents.push(e);
        }
    }
    if ents.is_empty() {
        return None;
    }
    let mut joints = vec![];
    for ax in ["yaw", "pitch", "roll"] {
        let mine: Vec<&Joint> = ents.iter().filter(|e| e.ax == ax).collect();
        let untagged: Vec<&Joint> = mine.iter().copied().filter(|e| !e.tagged).collect();
        let use_ = if untagged.is_empty() { mine } else { untagged };
        joints.extend(use_.into_iter().cloned());
    }
    let gyro = tp.get("gyroStabilize").and_then(|g| g.get("defaultGyroStabilize"));
    let safety = tp.get("gunFireSafety").cloned().unwrap_or(json!({}));
    Some(TurretSpec {
        style: jstr(tp, "rotationStyle").to_string(),
        joints,
        gyro: truthy(gyro),
        remote: truthy(tp.get("remoteTurret")),
        fire_angle: safety.get("fireConsentAngleDeg").cloned().filter(|v| !v.is_null()),
        recenter: truthy(tp.get("recenterIfUnused")),
    })
}

/// Python's float `%` (result has the sign of the divisor).
pub fn pymod(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && (r < 0.0) != (b < 0.0) {
        r + b
    } else {
        r
    }
}

/// A joint of a spec that exists in a model.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedJoint {
    pub j: Joint,
    pub ni: usize,
    pub lead: Option<String>,
}

impl ResolvedJoint {
    /// The feed form (`n, ax, speed, lo, hi, rel, tab, lead`), without the node index.
    pub fn to_json(&self) -> Value {
        let mut v = self.j.to_json();
        v.as_object_mut().unwrap().insert("lead".into(), json!(self.lead));
        v
    }
}

/// Which joints of `spec` exist in a model; `node_names` = {lower case node name: index}, `parents` = {node index: parent index}.
pub fn resolve_joints(spec: &TurretSpec, node_names: &HashMap<String, usize>, parents: &HashMap<usize, usize>) -> Vec<ResolvedJoint> {
    let chain = |mut i: usize| -> Vec<usize> {
        let mut out = vec![i];
        while let Some(&p) = parents.get(&i) {
            out.push(p);
            i = p;
        }
        out
    };
    let mut out: Vec<ResolvedJoint> = vec![];
    let mut lead: HashMap<&str, (usize, String)> = HashMap::new();
    for j in &spec.joints {
        let ni = match node_names.get(&j.n.to_lowercase()) {
            Some(&ni) => ni,
            None => continue,
        };
        let mut e = ResolvedJoint { j: j.clone(), ni, lead: None };
        if let Some((lni, ln)) = lead.get(j.ax) {
            if chain(ni).contains(lni) || chain(*lni).contains(&ni) {
                continue;
            }
            e.lead = Some(ln.clone());
        } else {
            lead.insert(j.ax, (ni, j.n.clone()));
        }
        out.push(e);
    }
    out
}

/// (lowest, highest) degrees of a joint given the current angles (linear between the rows of a custom table, wrapping at 360).
pub fn limit_at(j: &Joint, angles: &HashMap<String, f64>) -> (Option<f64>, Option<f64>) {
    let (tab, rel) = match (&j.tab, &j.rel) {
        (Some(t), Some(r)) if !t.is_empty() => (t, r),
        _ => return (j.lo, j.hi),
    };
    let mut r = pymod(*angles.get(rel).unwrap_or(&0.0), 360.0);
    let mut rows = tab.clone();
    rows.push([tab[0][0] + 360.0, tab[0][1], tab[0][2]]);
    if r < rows[0][0] {
        r += 360.0;
    }
    for w in rows.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a[0] <= r && r <= b[0] {
            let u = if b[0] == a[0] { 0.0 } else { (r - a[0]) / (b[0] - a[0]) };
            return (Some(a[1] + (b[1] - a[1]) * u), Some(a[2] + (b[2] - a[2]) * u));
        }
    }
    let l = rows.last().unwrap();
    (Some(l[1]), Some(l[2]))
}

// ---------------------------------------------------------------------------- vehicle commands
fn odd(n: &str) -> Option<(&'static str, i32)> {
    Some(match n {
        "OpenAllDoors" => ("AllDoors", 1),
        "CloseAllDoors" => ("AllDoors", -1),
        "LockAllDoors" => ("DoorLocks", 1),
        "UnlockAllDoors" => ("DoorLocks", -1),
        "OpenAccessPoints" => ("AccessPoints", 1),
        "CloseAccessPoints" => ("AccessPoints", -1),
        "SetLagPips" => ("Pips", 1),
        "SetLeadPips" => ("Pips", -1),
        "SetGimbalLock" => ("Gimbal", 1),
        "SetFreeGimbal" => ("Gimbal", -1),
        "SetStaggeredFire" => ("FireMode", 1),
        "SetSynchronousFire" => ("FireMode", -1),
        "Coupled_SetCoupled" => ("Coupled", 1),
        "Coupled_SetDecoupled" => ("Coupled", -1),
        "EngineOn" => ("Engines", 1),
        "EngineOff" => ("Engines", -1),
        "PowerOn" => ("Power", 1),
        "PowerOff" => ("Power", -1),
        "SelfDestruct_Engage" => ("SelfDestruct", 1),
        "SelfDestruct_Disengage" => ("SelfDestruct", -1),
        _ => return None,
    })
}

struct CmdRe {
    rules: Vec<(Regex, i32)>,
    group: Vec<(Regex, &'static str)>,
    skip: Regex,
    camel1: Regex,
    camel2: Regex,
}

fn cmd_re() -> &'static CmdRe {
    static R: OnceLock<CmdRe> = OnceLock::new();
    R.get_or_init(|| CmdRe {
        rules: vec![
            (Regex::new(r"^Enable(.+)$").unwrap(), 1),
            (Regex::new(r"^Disable(.+)$").unwrap(), -1),
            (Regex::new(r"^(.+?)_(?:On|SetActive|Deploy|Engage)$").unwrap(), 1),
            (Regex::new(r"^(.+?)_(?:Off|SetInactive|Retract|Disengage)$").unwrap(), -1),
        ],
        group: [
            (r"^(Weapons|Missiles|Countermeasures|Pips|Gimbal|FireMode|FireWeaponGroup|SetAimAssist|AimAssist|SetStaggered)", "weapons"),
            (r"^(Flight_?Ready|Power|Engines?|Coupled|Decoupled|GSafe|ESP|Cruise|VTOL|ProximityAssist|QuantumSpool|QuantumTravel|LandingGear)", "flight"),
            (r"^(Shields|Sensors|Headlights|Lights?|SelfDestruct)", "systems"),
            (r"(Doors?|DoorLocks|AccessPoints)", "doors"),
            (r"^(Hail|Accept|Decline|EndCall|Comms)", "comms"),
            (r"^Turret_", "turret"),
        ]
        .iter()
        .map(|(p, g)| (Regex::new(p).unwrap(), *g))
        .collect(),
        skip: Regex::new(r"Invalid|^Dummy|^AI_Use$|^CommsInteraction$|^Comms_DeclineCall$|^RetractTurret$").unwrap(),
        camel1: Regex::new(r"([a-z])([A-Z])").unwrap(),
        camel2: Regex::new(r"([a-z])(\d)").unwrap(),
    })
}

fn pretty(name: &str) -> String {
    match name {
        "GSafe" => return "G-safe".into(),
        "ESP" => return "ESP".into(),
        "VTOL" => return "VTOL".into(),
        "LandingGear" => return "Landing gear".into(),
        "ProximityAssist" => return "Proximity assist".into(),
        "QuantumSpool" => return "Quantum drive spool".into(),
        "QuantumTravel" => return "Quantum travel".into(),
        "Flight_Ready" => return "Flight ready".into(),
        "Coupled" => return "Coupled flight".into(),
        "AccessPoints" => return "Exterior access points".into(),
        "SelfDestruct" => return "Self destruct".into(),
        "AllDoors" => return "All doors".into(),
        "DoorLocks" => return "Door locks".into(),
        "Pips" => return "Lag / lead pips".into(),
        "Gimbal" => return "Gimbal lock".into(),
        "FireMode" => return "Staggered fire".into(),
        "Engines" => return "Engines".into(),
        "Power" => return "Ship power".into(),
        "ArmMissile" => return "Arm missile".into(),
        "CycleMissiles" => return "Cycle missiles".into(),
        "FireMissiles" => return "Fire missiles".into(),
        _ => {}
    }
    let r = cmd_re();
    let mut name = name;
    if let Some((_, rest)) = name.split_once('_') {
        name = rest;
    }
    let s = r.camel1.replace_all(&name.replace('_', " "), "$1 $2").trim().to_string();
    let s = r.camel2.replace_all(&s, "$1 $2").to_string();
    let mut ch = s.chars();
    match ch.next() {
        Some(c) => c.to_uppercase().collect::<String>() + &ch.as_str().to_lowercase(),
        None => name.to_string(),
    }
}

fn group_of(name: &str) -> &'static str {
    cmd_re().group.iter().find(|(rx, _)| rx.is_match(name)).map_or("other", |(_, g)| g)
}

fn norm(n: &str) -> (String, i32) {
    if let Some((b, s)) = odd(n) {
        return (b.into(), s);
    }
    for (rx, sign) in &cmd_re().rules {
        if let Some(m) = rx.captures(n) {
            return (m[1].to_string(), *sign);
        }
    }
    (n.to_string(), 0)
}

/// Controls from command names (a seat's / controller's SharedInteractions: objects with `Name`, or plain strings): on/off pairs and
/// single actions, in first-seen order.
pub fn vehicle_commands(shared: &[Value]) -> Vec<Value> {
    let mut names: Vec<String> = vec![];
    for s in shared {
        let n = match s {
            Value::Object(o) => o.get("Name").and_then(Value::as_str).map(str::to_owned),
            Value::String(x) => Some(x.clone()),
            _ => None,
        };
        if let Some(n) = n {
            if !n.is_empty() && !names.contains(&n) && !cmd_re().skip.is_match(&n) {
                names.push(n);
            }
        }
    }
    let mut order: Vec<String> = vec![];
    let mut ctl: HashMap<String, [Option<String>; 3]> = HashMap::new();
    for n in &names {
        let (base, sign) = norm(n);
        if !ctl.contains_key(&base) {
            order.push(base.clone());
        }
        let c = ctl.entry(base).or_default();
        let slot = if sign > 0 { 0 } else if sign < 0 { 1 } else { 2 };
        c[slot] = Some(n.clone());
    }
    let mut out = vec![];
    for base in order {
        let c = &ctl[&base];
        let g = group_of(&base);
        if let (Some(on), Some(off)) = (&c[0], &c[1]) {
            out.push(json!({"id": base, "label": pretty(&base), "grp": g, "on": on, "off": off}));
        } else {
            for (k, name) in c.iter().enumerate() {
                if let Some(n) = name {
                    let is_do = k == 2;
                    let id = if !is_do { n.clone() } else { base.clone() };
                    let label = pretty(if !is_do || base == *n { n } else { &base });
                    out.push(json!({"id": id, "label": label, "grp": g, "do": n}));
                }
            }
        }
    }
    out
}

fn mode_groups(m: &str) -> &'static [&'static str] {
    match m {
        "Flight" => &["power", "flight", "systems", "doors", "comms"],
        "Combat" => &["weapons", "systems", "power"],
        "Missile" => &["weapons"],
        "Scanning" => &["systems", "comms"],
        "QuantumNavigation" => &["flight"],
        "Turret" => &["turret"],
        "Salvage" | "Mining" | "Refuel" => &["systems"],
        "AirTrafficController" => &["comms"],
        _ => &[],
    }
}

fn role_groups(sub: &str) -> Option<&'static [&'static str]> {
    Some(match sub {
        "engineer" => &["power", "systems", "doors"],
        "captain" => &["power", "flight", "weapons", "systems", "doors", "comms"],
        "tactical" => &["weapons", "systems"],
        "science" => &["systems", "comms"],
        "comms" => &["comms"],
        "security" => &["doors", "systems"],
        "atc" => &["comms", "doors"],
        _ => return None,
    })
}

/// Operator modes `{def, av}`.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorModes {
    pub def: String,
    pub av: Vec<String>,
}

impl OperatorModes {
    pub fn to_json(&self) -> Value {
        json!({"def": self.def, "av": self.av})
    }
}

/// Command groups to show: the role's, else those of the operator modes; power / doors / comms without modes.
pub fn groups_for(modes: Option<&OperatorModes>, sub: &str) -> Vec<&'static str> {
    if let Some(g) = role_groups(sub) {
        return g.to_vec();
    }
    let m = match modes {
        Some(m) => m,
        None => return vec!["power", "doors", "comms"],
    };
    let mut out: Vec<&'static str> = vec![];
    let mut all: Vec<&str> = m.av.iter().map(String::as_str).collect();
    all.push(&m.def);
    for x in all {
        for g in mode_groups(x) {
            if !out.contains(g) {
                out.push(g);
            }
        }
    }
    out
}

/// [{id, helper, grp}] from a decoded SCItemSeatDashboardParams.
pub fn dashboard_buttons(dash: &Value) -> Vec<Value> {
    let mut out = vec![];
    if let Value::Object(o) = dash {
        for (g, v) in o {
            if let (Some(grp), Value::Object(inner)) = (g.strip_suffix("Interactions"), v) {
                for (k, x) in inner {
                    if x.is_object() && truthy(x.get("HelperName")) {
                        out.push(json!({"id": k, "helper": x["HelperName"], "grp": grp}));
                    }
                }
            }
        }
    }
    out
}

/// `{def, av}` from a decoded OperatorModeAvailabilityParams, or None; 'None' / 'Inactive' left out.
pub fn operator_modes(rec: &Value) -> Option<OperatorModes> {
    if !rec.is_object() {
        return None;
    }
    let av: Vec<String> = rec
        .get("availableModes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(Value::as_str)
        .filter(|m| *m != "None" && *m != "Inactive")
        .map(str::to_owned)
        .collect();
    let d = match rec.get("defaultMode") {
        Some(Value::String(s)) if s != "None" && s != "Inactive" => s.clone(),
        _ => String::new(),
    };
    if !av.is_empty() || !d.is_empty() {
        Some(OperatorModes { def: d, av })
    } else {
        None
    }
}

#[allow(dead_code)]
fn _unused(_: Map<String, Value>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_pair_and_single() {
        let names: Vec<Value> = ["Weapons_On", "Weapons_Off", "OpenAllDoors", "Dummy_x", "Missiles_FireMissiles"].iter().map(|s| json!({"Name": s})).collect();
        let c = vehicle_commands(&names);
        assert_eq!(c[0]["id"], "Weapons");
        assert_eq!(c[0]["on"], "Weapons_On");
        assert_eq!(c[1]["do"], "OpenAllDoors");
        assert_eq!(c[1]["label"], "Open all doors");
        assert_eq!(c.len(), 3);
        assert_eq!(c[2]["label"], "Fire missiles");
    }

    #[test]
    fn subkinds() {
        assert_eq!(subkind("AEGS_Idris_Seat_Pilot", "seat", "", &[], false, false, "seat"), "pilot");
        assert_eq!(subkind("X_Seat_CoPilot", "seat", "", &[], false, false, "seat"), "copilot");
        assert_eq!(subkind("Chair", "seat", "", &[], false, false, "usable"), "chair");
    }
}

//! Multi-function displays (MFDs) and other render-to-texture screens: how the data marks them.
//!
//! In the game each screen is a Scaleform (Flash) UI rendered into a texture; this viewer does NOT decode that.  It only needs to
//! know *which* materials are screens (to give them a canvas the viewer draws on) and what the DataCore says each seat's
//! console has.  Evidence marks (EMPIRICAL = read from the live archive, INFERRED = a reading).
//!
//! * Materials (EMPIRICAL, the 9,201 .mtl under spaceships / ui / interior folders; 2,110 contain a render-target sub-material):
//!   `Shader="DisplayScreen"` with `TexSlot9 = "$RenderToTexture"` is a screen; `UIPlane` (`RTT_Hud`) is a head-up display plane
//!   with the same slot; `MeshDecal` with the render target in TexSlot1 (`RTT_Text_To_Decal`) is text projected onto a decal;
//!   `GlassPBR` with it is a display behind glass.
//! * The DataCore side (`SCItemSeatDashboardParams`, `SMFDModeConfig`) arrives here as decoded `serde_json::Value`s; `dashboard_geometries`
//!   walks Game2.dcb with the DataCore reader.

use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

const RTT: &str = "$rendertotexture";

/// Kind of render-target material: "screen" (DisplayScreen with the render target: gets an MFD canvas), "hud" (UIPlane), "decal"
/// (MeshDecal text decal), "glass" (GlassPBR display), "static" (DisplayScreen without a render target: glow / fake screens), else
/// None.  `textures` is the {slot: file} list of the sub-material.
pub fn rtt_kind(shader: &str, textures: &[(String, String)]) -> Option<&'static str> {
    let sh = shader.to_lowercase();
    let has = textures.iter().any(|(_, v)| v.to_lowercase() == RTT);
    if sh == "displayscreen" {
        return Some(if has { "screen" } else { "static" });
    }
    if sh == "uiplane" {
        return Some("hud");
    }
    if !has {
        return None;
    }
    match sh.as_str() {
        "meshdecal" => Some("decal"),
        "glasspbr" => Some("glass"),
        "uimesh" => Some("screen"),
        _ => None,
    }
}

/// Nominal width / height of a screen from its normal-map slot name (`ui_screen_16x9_ddna`, `ui_screen_9x16_ddna`,
/// `ui_screen_16x9_ships_ddna`), or None.  EMPIRICAL: those names occur on ~60 DisplayScreen sub-materials (9x16 on 27, 16x9 on 49);
/// the viewer prefers the aspect measured from the UV layout and uses this one only when that fails.
pub fn screen_aspect(textures: &[(String, String)]) -> Option<f64> {
    for (_, v) in textures {
        let name = v.replace('\\', "/");
        let name = name.rsplit('/').next().unwrap_or("").to_lowercase();
        // regex ui_screen_(\d+)x(\d+), case-insensitive, leftmost match
        let mut from = 0;
        while let Some(p) = name[from..].find("ui_screen_") {
            let rest = &name[from + p + 10..];
            let d1: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let after = &rest[d1.len()..];
            if !d1.is_empty() && after.starts_with('x') {
                let d2: String = after[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
                if !d2.is_empty() {
                    let (a, b) = (d1.parse::<f64>().unwrap_or(f64::INFINITY), d2.parse::<f64>().unwrap_or(f64::INFINITY));
                    if b != 0.0 {
                        return Some(a / b);
                    }
                    break; // this string matched but its denominator is 0: move on to the next value
                }
            }
            from += p + 1;
        }
    }
    None
}

fn obj<'a>(v: &'a Value, k: &str) -> Option<&'a Map<String, Value>> {
    v.get(k).and_then(Value::as_object)
}

fn geometry_names(list: Option<&Value>, drop_fake: bool) -> Vec<String> {
    let mut names: Vec<String> = vec![];
    for x in list.and_then(Value::as_array).into_iter().flatten() {
        let n = x.get("geometryName").and_then(Value::as_str).unwrap_or("");
        if !n.is_empty() && !(drop_fake && n.to_lowercase() == "fake") && !names.iter().any(|e| e == n) {
            names.push(n.to_string());
        }
    }
    names
}

/// {'screens': [node names], 'annunciators': [node names], 'hud': bool, 'views': [view names]} from a decoded
/// SCItemSeatDashboardParams, or None when the console has no MFD at all.  `views` is the decoded SMFDModeConfig record (optional);
/// its defaultConfiguration gives the page of each screen.  'Fake' (a placeholder name the records use for no MFD) is dropped.
pub fn dashboard_mfd(dash: &Value, views: Option<&Value>) -> Option<Value> {
    if !dash.is_object() {
        return None;
    }
    let m = dash.get("MFDParams").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
    let names = geometry_names(m.get("MFDs"), true);
    let ph = dash.get("PhysicalScreenParams").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
    let ann = geometry_names(ph.get("Screens"), false);
    let ui = dash.get("uiDescription").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
    let hud = ui
        .get("Screens")
        .and_then(Value::as_array)
        .map_or(false, |a| a.iter().any(|s| s.get("Style").and_then(|st| st.get("Type")).and_then(Value::as_str) == Some("HeadUpDisplay")));
    let mut vs: Vec<String> = vec![];
    if let Some(d) = views.and_then(|v| obj(v, "defaultConfiguration")) {
        for v in d.values() {
            if let Some(s) = v.as_str() {
                if s.starts_with("eView_") && s != "eView_Off" && !vs.iter().any(|e| e == &s[6..]) {
                    vs.push(s[6..].to_string());
                }
            }
        }
    }
    if names.is_empty() && ann.is_empty() && !hud {
        return None;
    }
    Some(json!({"screens": names, "annunciators": ann, "hud": hud, "views": vs}))
}

/// Geometry key (lower case, no leading 'data/') of a model path.
pub fn geo_key(path: &str) -> String {
    let p = path.replace('\\', "/").trim_start_matches('/').to_lowercase();
    p.strip_prefix("data/").map(str::to_string).unwrap_or(p)
}

// ---- manufacturer look.  EVIDENCE: per-manufacturer MFD textures and UI style kits exist; the DisplayScreen sub-materials carry NO
// tint; the viewer's palettes are VISUAL, approximate.
pub const MFR_THEME: [(&str, &str); 8] =
    [("ANVL", "anvil"), ("DRAK", "drake"), ("CRUS", "crusader"), ("AEGS", "aegis"), ("ARGO", "argo"), ("MISC", "misc"), ("ORIG", "origin"), ("RSI", "rsi")];
pub const THEMES: [&str; 9] = ["anvil", "drake", "crusader", "aegis", "argo", "misc", "origin", "rsi", "generic"];
const KNOWN_MFR: [&str; 21] = [
    "AEGS", "ANVL", "ARGO", "BANU", "CNOU", "CRUS", "DRAK", "ESPR", "GAMA", "GLSN", "GRIN", "KRIG", "MISC", "MRAI", "ORIG", "RSI", "TMBL", "VNCL", "XIAN", "XNAA",
    "MIRA",
];

fn re_path_mfr() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/(?:ships|turrets|seats|vehicles)/([a-z]{3,4})/").unwrap())
}

/// Four-letter manufacturer code of an entity class (`AEGS_Idris_SCItem_Dashboard_Pilot` -> AEGS), else the one in the model path
/// (`objects/spaceships/seats/anvl/...`), else "".
pub fn manufacturer(cls: &str, geometry: &str) -> String {
    let head = cls.split('_').next().unwrap_or("").to_uppercase();
    let in_theme = MFR_THEME.iter().any(|(k, _)| *k == head);
    if in_theme || (head.chars().count() == 4 && head.chars().all(char::is_alphabetic) && !cls.is_empty() && cls.contains('_') && KNOWN_MFR.contains(&head.as_str())) {
        return head;
    }
    let g = geometry.replace('\\', "/").to_lowercase();
    if let Some(c) = re_path_mfr().captures(&g) {
        let m = c[1].to_uppercase();
        if KNOWN_MFR.contains(&m.as_str()) {
            return m;
        }
    }
    String::new()
}

/// Theme id for a manufacturer code or an entity / ship class name (its prefix is used); unknown makers get "generic".
pub fn theme_for(prefix: &str) -> &'static str {
    let p = prefix.split('_').next().unwrap_or("").to_uppercase();
    MFR_THEME.iter().find(|(k, _)| *k == p).map_or("generic", |(_, t)| t)
}

/// View enum of the records -> the page that shows it.
pub fn view_page(view: &str) -> Option<&'static str> {
    Some(match view {
        "SelfStatus" => "Status",
        "TargetStatus" => "Target",
        "ResourceNetwork" => "Pips",
        "Scanning" => "Scanning",
        "Diagnostics" => "Components",
        "Weapons" => "Weapons",
        "Shields" => "Shields",
        "Configuration" => "Config",
        _ => return None,
    })
}

const SECONDARY: [&str; 5] = ["secondaryMFDScreen1View", "secondaryMFDScreen2View", "secondaryMFDScreen3View", "secondaryMFDScreen4View", "secondaryMFDScreen5View"];

/// {screen node name (lowercase): page} from a decoded SCItemSeatDashboardParams and its SMFDModeConfig.  EMPIRICAL / INFERRED:
/// `SMFDModeConfig.defaultConfiguration` names the view of `primaryMFDScreenView` and `secondaryMFDScreen1..5View`; the primary one
/// belongs to `MFDParams.primaryMFD` and the secondary ones to the other `MFDs[]` entries in list order (the mapping of those slots
/// to list entries is INFERRED, the record does not name the pairing).  'Off' views are skipped.
pub fn screen_defaults(dash: &Value, views: Option<&Value>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    let d = views.and_then(|v| obj(v, "defaultConfiguration"));
    let m = dash.get("MFDParams").filter(|v| v.is_object());
    let (Some(d), Some(m)) = (d, m) else { return out };
    let pname = m.get("primaryMFD").and_then(|p| p.get("geometryName")).and_then(Value::as_str).unwrap_or("");
    let names: Vec<&str> = m
        .get("MFDs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| x.get("geometryName").and_then(Value::as_str))
        .filter(|n| !n.is_empty() && n.to_lowercase() != "fake")
        .collect();
    let sec: Vec<&str> = names.into_iter().filter(|n| *n != pname).collect();
    let page = |key: &str| d.get(key).map(|v| v.as_str().map_or(v.to_string(), str::to_string)).and_then(|s| view_page(&s.replace("eView_", "")));
    let mut put = |k: String, v: &str| match out.iter_mut().find(|(ek, _)| *ek == k) {
        Some(e) => e.1 = v.to_string(),
        None => out.push((k, v.to_string())),
    };
    if let (false, Some(pv)) = (pname.is_empty(), page("primaryMFDScreenView")) {
        put(pname.to_lowercase(), pv);
    }
    for (n, key) in sec.iter().zip(SECONDARY) {
        if let Some(v) = page(key) {
            put(n.to_lowercase(), v);
        }
    }
    out
}

fn re_ann() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)annunciator|(?:^|_)A[LR]$").unwrap())
}
fn re_ind() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)(?:^|_)[LR]Ind(?:_|$)|Indicators?(?:_|$)|_[LR]2$").unwrap())
}

/// (kind, page) for the name of a PhysicalScreenParams canvas record ('BuildingBlocks_Canvas.<name>'): ("annunciator", AnnL | AnnR |
/// AnnC), ("indicators", "Summary") or ("instrument", "") for the flight-instrument canvases (left static); ("", "") for None.
pub fn canvas_kind(canvas: &str) -> (&'static str, &'static str) {
    let n = canvas.split_once('.').map_or(canvas, |x| x.1);
    if n.is_empty() {
        return ("", "");
    }
    if re_ann().is_match(n) {
        let low = n.to_lowercase();
        if low.contains("combined") {
            return ("annunciator", "AnnC");
        }
        static R: OnceLock<(Regex, Regex)> = OnceLock::new();
        let (right, left) = R.get_or_init(|| (Regex::new(r"(?i)right|_R$|_AR$").unwrap(), Regex::new(r"(?i)left|_L$|_AL$").unwrap()));
        if right.is_match(n) {
            return ("annunciator", "AnnR");
        }
        if left.is_match(n) {
            return ("annunciator", "AnnL");
        }
        return ("annunciator", "AnnC");
    }
    if re_ind().is_match(n) {
        return ("indicators", "Summary");
    }
    ("instrument", "")
}

/// {node name (lowercase): {kind, page}} of a decoded SCItemSeatDashboardParams: `MFDs[]` entries are "mfd" (page = the record
/// default or ""), PhysicalScreenParams entries take their kind from the canvas record name (`canvas_name` maps a canvas ref to its
/// record name).
pub fn screen_kinds(dash: &Value, canvas_name: &dyn Fn(Option<&Value>) -> String, defaults: &[(String, String)]) -> Map<String, Value> {
    let mut out = Map::new();
    let m = obj(dash, "MFDParams");
    for x in m.and_then(|m| m.get("MFDs")).and_then(Value::as_array).into_iter().flatten() {
        let g = x.get("geometryName").and_then(Value::as_str).unwrap_or("");
        if !g.is_empty() && g.to_lowercase() != "fake" {
            let page = defaults.iter().find(|(k, _)| *k == g.to_lowercase()).map_or("", |(_, p)| p.as_str());
            out.insert(g.to_lowercase(), json!({"kind": "mfd", "page": page}));
        }
    }
    let ps = obj(dash, "PhysicalScreenParams");
    for sc in ps.and_then(|p| p.get("Screens")).and_then(Value::as_array).into_iter().flatten() {
        let g = sc.get("geometryName").and_then(Value::as_str).unwrap_or("");
        if !g.is_empty() && !out.contains_key(&g.to_lowercase()) {
            let (kind, page) = canvas_kind(&canvas_name(sc.get("canvas")));
            if !kind.is_empty() {
                out.insert(g.to_lowercase(), json!({"kind": kind, "page": page}));
            }
        }
    }
    out
}

/// Kind entry {kind, page} for a render-target node of a dashboard model: the record's, else 'radar' for `*radar*` names
/// (INFERRED), else None.
pub fn node_kind(screens: &Map<String, Value>, node: &str) -> Option<Value> {
    if let Some(e) = screens.get(&node.to_lowercase()) {
        return Some(e.clone());
    }
    if node.to_lowercase().contains("radar") {
        return Some(json!({"kind": "radar", "page": "Radar"}));
    }
    None
}

// ---- which screens get an MFD: only those of a seat's dashboard (pages per role INFERRED: the data gives every dashboard the same
// eight views and the role only through the seat's operator modes, so the subsets follow what each role can do in the game).
pub const ALL_PAGES: [&str; 11] = ["Status", "Pips", "Components", "Comms", "Target", "Shields", "Weapons", "Scanning", "Traffic", "Doors", "Config"];
pub const DEFAULT_PAGES: [&str; 4] = ["Status", "Comms", "Target", "Config"];

/// Page names for a seat role (a sub kind of `station::subkind`); roles without an entry get `DEFAULT_PAGES`.
pub fn role_pages(role: &str) -> Vec<&'static str> {
    match role {
        "pilot" => vec!["Status", "Pips", "Components", "Comms", "Target", "Shields", "Weapons", "Scanning", "Doors", "Config"],
        "copilot" => vec!["Status", "Weapons", "Target", "Scanning", "Comms", "Shields", "Config"],
        "captain" => vec!["Status", "Comms", "Target", "Components", "Shields", "Doors", "Config"],
        "bridge" => vec!["Status", "Comms", "Target", "Doors", "Config"],
        "engineer" => vec!["Status", "Pips", "Components", "Shields", "Doors", "Config"],
        "gunner" | "turret" | "remote_turret" => vec!["Weapons", "Target", "Status"],
        "tactical" => vec!["Weapons", "Target", "Scanning", "Shields", "Status"],
        "science" => vec!["Scanning", "Target", "Status"],
        "comms" => vec!["Comms", "Target", "Scanning", "Status"],
        "security" => vec!["Doors", "Comms", "Status"],
        "brig" => vec!["Doors", "Status"],
        "atc" => vec!["Traffic", "Comms", "Status"],
        "operator" => vec!["Status", "Scanning", "Components"],
        _ => DEFAULT_PAGES.to_vec(),
    }
}

// ---- the DataCore walk: the dashboard of every seat / turret class, by model.
fn subkind_re() -> &'static Vec<(&'static str, Regex)> {
    static R: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    R.get_or_init(|| {
        [
            ("atc", r"(^|_)atc(_|$)|air_?traffic"),
            ("remote", r"remote_?turret|turret_?console|tractor_?console|torpedo_?console|support_seat|turret_?seat|seat_?turret|seat_remote|console_turret"),
            ("engineer", r"engineer"),
            ("captain", r"captain"),
            ("copilot", r"co_?pilot"),
            ("pilot", r"pilot|driver"),
            ("gunner", r"gunner"),
            ("tactical", r"tactical"),
            ("science", r"science"),
            ("comms", r"(^|_)comms?(_|$)"),
            ("security", r"security"),
            ("brig", r"(^|_)brig(_|$)"),
            ("bridge", r"bridge"),
            ("pod", r"esc_?pod|escapepod"),
            ("passenger", r"passenger|(^|_)drop(_|$)"),
            ("prison", r"prison"),
        ]
        .into_iter()
        .map(|(k, v)| (k, Regex::new(&format!("(?i){}", v)).unwrap()))
        .collect()
    })
}

/// `station::subkind` with its default arguments (no operator mode, no turret component, not a bed, a seat): the class-name rules
/// (INFERRED).
pub fn seat_subkind(cls: &str, kind: &str) -> &'static str {
    if kind == "turret" {
        return "turret";
    }
    let re = subkind_re();
    let has = |k: &str| re.iter().find(|(n, _)| *n == k).unwrap().1.is_match(cls);
    if has("atc") {
        return "atc";
    }
    if has("engineer") {
        return "engineer";
    }
    if has("remote") {
        return "remote_turret";
    }
    for k in ["captain", "copilot", "pilot", "gunner"] {
        if has(k) {
            return k;
        }
    }
    for k in ["tactical", "science", "comms", "security", "brig"] {
        if has(k) {
            return k;
        }
    }
    if has("pod") {
        return "escape_pod";
    }
    if has("bridge") {
        return "bridge";
    }
    if has("passenger") || has("prison") {
        return "passenger";
    }
    "seat"
}

fn is_ref_json(v: &Value) -> bool {
    matches!(v, Value::Array(a) if a.len() == 3 && a[1].as_u64() != Some(0xFFFF_FFFF))
}

/// `{geometry key: {role, mfr, theme, defaults, screens}}` for the models of every entity class with `SCItemSeatDashboardParams`, from
/// an `ItemMaterials` (Game2.dcb).  The role is `seat_subkind` of the class name ('turret' when the name says so; desk / captain / bridge
/// consoles without a role word get captain / bridge), `mfr` / `theme` come from the class prefix and `defaults` is `screen_defaults`.  A
/// model shared by several classes keeps the first (alphabetical) class for role / maker and the union of their screens.
pub fn dashboard_geometries(items: &crate::itemmaterials::ItemMaterials) -> Map<String, Value> {
    use p4k_formats::dcb::Value as DV;
    let mut out: Map<String, Value> = Map::new();
    let Some(d) = items.datacore() else { return out };
    let mut names: Vec<(&String, &Vec<usize>)> = d.record_index().iter().filter(|(k, _)| k.starts_with("EntityClassDefinition.")).collect();
    names.sort_by(|a, b| a.0.cmp(b.0));
    let mut vcache: std::collections::HashMap<String, Option<Value>> = std::collections::HashMap::new();
    let desk = Regex::new(r"(?i)desk|captain|bridge").unwrap();
    let canvas_name = |r: Option<&Value>| -> String {
        let Some(r) = r.filter(|r| is_ref_json(r)) else { return String::new() };
        r[2].as_str().and_then(|gd| items.guid_record(gd)).and_then(|i| d.record_name(i).ok()).map(|n| n.into_owned()).unwrap_or_default()
    };
    for (name, ids) in names {
        let cls = name.split_once('.').map_or(name.as_str(), |x| x.1);
        let Ok(rec) = d.record(ids[0]) else { continue };
        let Ok(top) = d.record_data(&rec, 0) else { continue };
        let comp = top.get("Components").and_then(DV::as_list).and_then(|l| {
            l.iter().find(|c| {
                let si = match c {
                    DV::Strong(a, _) | DV::Weak(a, _) | DV::ClassRef(a, _) => *a,
                    DV::Ref(a, _) => *a,
                    _ => return false,
                };
                si != 0xFFFF_FFFF && d.struct_name(si as usize).map_or(false, |n| n == "SCItemSeatDashboardParams")
            })
        });
        let Some(comp) = comp else { continue };
        let Some(g) = items.class_geometry(cls) else { continue };
        let key = geo_key(&g);
        let merge = out.contains_key(&key);
        let kind = if cls.to_lowercase().contains("turret") { "turret" } else { "seat" };
        let mut role = seat_subkind(cls, kind);
        if (role == "seat" || role == "chair") && desk.is_match(cls) {
            role = if cls.to_lowercase().contains("captain") { "captain" } else { "bridge" };
        }
        let (mut defaults, mut screens): (Vec<(String, String)>, Map<String, Value>) = (vec![], Map::new());
        if let Ok(dash) = d.deref(comp.clone(), 0, 6) {
            let dash = dash.to_json();
            let r = dash.get("MFDParams").and_then(|m| m.get("modeConfiguration")).cloned().unwrap_or(Value::Null);
            if is_ref_json(&r) {
                let guid = r[2].as_str().unwrap_or("").to_string();
                let views = vcache
                    .entry(guid.clone())
                    .or_insert_with(|| items.guid_record(&guid).and_then(|gi| d.record(gi).ok()).and_then(|rc| d.record_data(&rc, 3).ok()).map(|v| v.to_json()))
                    .clone();
                defaults = screen_defaults(&dash, views.as_ref());
            }
            screens = screen_kinds(&dash, &canvas_name, &defaults);
        }
        let dmap = |v: &[(String, String)]| -> Map<String, Value> { v.iter().map(|(k, p)| (k.clone(), json!(p))).collect() };
        if merge {
            let cur = out.get_mut(&key).unwrap();
            for (k, v) in screens {
                cur["screens"].as_object_mut().unwrap().entry(k).or_insert(v);
            }
            for (k, v) in dmap(&defaults) {
                cur["defaults"].as_object_mut().unwrap().entry(k).or_insert(v);
            }
            continue;
        }
        let mfr = manufacturer(cls, &g);
        out.insert(key, json!({"role": role, "mfr": mfr, "theme": theme_for(&mfr), "defaults": dmap(&defaults), "screens": screens}));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn render_target_kinds() {
        assert_eq!(rtt_kind("DisplayScreen", &tx(&[("TexSlot9", "$RenderToTexture")])), Some("screen"));
        assert_eq!(rtt_kind("DisplayScreen", &tx(&[])), Some("static"));
        assert_eq!(rtt_kind("UIPlane", &tx(&[])), Some("hud"));
        assert_eq!(rtt_kind("MeshDecal", &tx(&[("TexSlot1", "$rendertotexture")])), Some("decal"));
        assert_eq!(rtt_kind("MeshDecal", &tx(&[])), None);
        assert_eq!(rtt_kind("UIMesh", &tx(&[("TexSlot1", "$RenderToTexture")])), Some("screen"));
    }

    #[test]
    fn aspect_from_the_normal_map_name() {
        let a = screen_aspect(&tx(&[("TexSlot1", "a/b.tif"), ("TexSlot2", "Data\\UI\\UI_Screen_16x9_ships_ddna.tif")])).unwrap();
        assert!((a - 16.0 / 9.0).abs() < 1e-12);
        assert_eq!(screen_aspect(&tx(&[("TexSlot2", "ui_screen_4x0_ddna.tif")])), None);
        assert!((screen_aspect(&tx(&[("a", "ui_screen_4x0_ddna"), ("b", "ui_screen_9x16_ddna")])).unwrap() - 0.5625).abs() < 1e-12);
    }

    #[test]
    fn canvas_and_roles() {
        assert_eq!(canvas_kind("BuildingBlocks_Canvas.H_Eng_Annunciator_Master_Left"), ("annunciator", "AnnL"));
        assert_eq!(canvas_kind("BuildingBlocks_Canvas.H_Eng_Annunciator_Combined"), ("annunciator", "AnnC"));
        assert_eq!(canvas_kind("BuildingBlocks_Canvas.HC_HUD_Ship_RInd_Master"), ("indicators", "Summary"));
        assert_eq!(canvas_kind("BuildingBlocks_Canvas.H_HUD_Compass"), ("instrument", ""));
        assert_eq!(canvas_kind(""), ("", ""));
        assert_eq!(manufacturer("AEGS_Idris_SCItem_Dashboard_Pilot", ""), "AEGS");
        assert_eq!(manufacturer("Foo_Bar", "objects/spaceships/seats/anvl/x.cgf"), "ANVL");
        assert_eq!(theme_for("DRAK_Cutlass"), "drake");
        assert_eq!(theme_for("XXXX"), "generic");
        assert_eq!(role_pages("pilot").len(), 10);
        assert_eq!(role_pages("nothing"), DEFAULT_PAGES.to_vec());
        assert_eq!(geo_key("\\Data/Objects/A.cgf"), "objects/a.cgf");
    }

    #[test]
    fn dashboard_screens() {
        let dash = json!({"MFDParams": {"primaryMFD": {"geometryName": "mfd_main"}, "MFDs": [{"geometryName": "mfd_main"}, {"geometryName": "Fake"}, {"geometryName": "mfd_l"}]},
                          "PhysicalScreenParams": {"Screens": [{"geometryName": "ann", "canvas": "x"}]}});
        let views = json!({"defaultConfiguration": {"primaryMFDScreenView": "eView_Weapons", "secondaryMFDScreen1View": "eView_Off", "x": "eView_Shields"}});
        let m = dashboard_mfd(&dash, Some(&views)).unwrap();
        assert_eq!(m["screens"], json!(["mfd_main", "mfd_l"]));
        assert_eq!(m["views"], json!(["Weapons", "Shields"]));
        let d = screen_defaults(&dash, Some(&views));
        assert_eq!(d, vec![("mfd_main".to_string(), "Weapons".to_string())]);
        let k = screen_kinds(&dash, &|_| "BuildingBlocks_Canvas.Annunciator_Right".into(), &d);
        assert_eq!(k["mfd_main"]["page"], "Weapons");
        assert_eq!(k["ann"]["page"], "AnnR");
        assert_eq!(node_kind(&k, "Radar_Scope").unwrap()["kind"], "radar");
    }
}

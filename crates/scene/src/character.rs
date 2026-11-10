//! Skinned human characters for the viewer: skeleton + body mesh with bone weights + animation clips of the Human .dba databases.
//!
//! What the archive holds (Data.p4k 4.x, EMPIRICAL; layouts in formats/skeleton and formats/dba):
//!   Objects/Characters/Human/<male_v7|female_v2>/export/<bhm_skeleton_v7|bhf_skeleton_v2>.chr   the 220-bone skeleton
//!   .../body/<m_body|f_body>.skin / .skinm / .mtl  the whole body incl. underwear and head as ONE mesh (12,356 vertices male), 84 bones, 3 sub-materials
//!   Animations/Characters/Human/<male_v7|female_v2>/**.dba  the clips are keyed by CRC32 of the 220 skeleton bone names.  Locomotion lives in
//!   weapons/no_weapon/locomotion/{stand,crouch}.dba, ladders in ladders.dba, use / sit / lie poses in use.dba, getting up in getup.dba.
//!   Clips are absolute parent-relative bone poses (as for doors); the 'World' bone carries the root motion (+Y metres), 'Hips' the pelvis height.
//! Approximations (everything else is data): the idle pose is the end of a 'start to idle' clip because the neutral idle clips are additive-only; no
//! armor, clothes, hair, face or hands items; no blend trees, IK, foot locking or root-motion synchronisation; skin shading is a diffuse map under a
//! simple hemisphere light (the HumanSkin_V2 shader is not evaluated).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use p4k_formats::dba::{self, Clip, Dba};
use p4k_formats::error::py_round;
use p4k_formats::ivo::{self, FrameReader, IvoFile};
use p4k_formats::mtl;
use p4k_formats::skeleton::{self, Bone};
use p4k_formats::xmlq::{latin1, split_nul};
use p4k_formats::{Error, Result};
use p4k::P4k;
use serde_json::{json, Map, Value};

use crate::textures::{load_texture, ng_png, texture_base};

pub const ROOT: &str = "Data/Objects/Characters/Human/";
pub const ANIM: &str = "Data/Animations/Characters/Human/";
pub const HEAD_BONE: &str = "Head";
pub const FPS: f64 = dba::FPS;

pub struct Part {
    pub skin: &'static str,
    pub head: bool,
}

pub struct Kind {
    pub name: &'static str,
    pub folder: &'static str,
    pub skeleton: &'static str,
    pub parts: &'static [Part],
}

pub const KINDS: [Kind; 2] = [
    Kind {
        name: "male",
        folder: "male_v7",
        skeleton: "export/bhm_skeleton_v7.chr",
        parts: &[
            Part { skin: "Data/Objects/Characters/Human/male_v7/body/m_body", head: false },
            Part { skin: "Data/Objects/Characters/Human/heads/male/npc/male01/male01_t2_head_lod1", head: true },
            Part { skin: "Data/Objects/Characters/Human/heads/male/npc/male01/male01_t2_eyes_lod1", head: true },
        ],
    },
    Kind {
        name: "female",
        folder: "female_v2",
        skeleton: "export/bhf_skeleton_v2.chr",
        parts: &[
            Part { skin: "Data/Objects/Characters/Human/female_v2/body/f_body", head: false },
            Part { skin: "Data/Objects/Characters/Human/heads/female/npc/female01/female01_t2_head_lod1", head: true },
            Part { skin: "Data/Objects/Characters/Human/heads/female/npc/female01/female01_t2_eyes_lod1", head: true },
        ],
    },
];

/// animation databases listed in the clip picker (relative to Animations/Characters/Human/<folder>/), in this order
pub const CLIP_DBAS: [&str; 10] = [
    "weapons/no_weapon/locomotion/stand.dba",
    "weapons/no_weapon/locomotion/crouch.dba",
    "ladders.dba",
    "use.dba",
    "weapons/no_weapon/getup.dba",
    "weapons/no_weapon/jump.dba",
    "vault_mantle.dba",
    "zerog/no_weapon/locomotion.dba",
    "spaceships/cockpit_types.dba",
    "emotes.dba",
];

/// weapon-handling animation databases (EMPIRICAL: Animations/Characters/Human/<folder>/weapons/*): held-weapon stances and locomotion, firing, hit
/// reactions, reloads.  Loaded on first use apart from CLIP_DBAS (some are several MB) and listed only by `clip_list_weapons`, so the clip picker is unchanged.
pub const WEAPON_DBAS: [&str; 20] = [
    "weapons/stocked/locomotion/stand.dba",
    "weapons/stocked/locomotion/crouch.dba",
    "weapons/stocked/locomotion/prone.dba",
    "weapons/stocked/fire.dba",
    "weapons/stocked/hit_reactions.dba",
    "weapons/stocked/fidgets.dba",
    "weapons/pistol/locomotion/stand.dba",
    "weapons/pistol/locomotion/crouch.dba",
    "weapons/pistol/locomotion/prone.dba",
    "weapons/pistol/fire.dba",
    "weapons/pistol/hit_reactions.dba",
    "weapons/reloads.dba",
    "weapons/no_weapon/hit_reactions.dba",
    "deaths.dba",
    "weapons/stocked/jump.dba",
    "weapons/shouldered/locomotion/stand.dba",
    "weapons/shouldered/locomotion/crouch.dba",
    "weapons/shouldered/locomotion/prone.dba",
    "weapons/no_weapon/locomotion/prone.dba",
    // knife: light slashes (stand/crouch, left/right, additive), heavy stabs / overs / unders (full body), blocks, dodges (EMPIRICAL names in the file)
    "weapons/knife.dba",
];

/// stand-alone stance-transition clips (EMPIRICAL names under Animations/Characters/Human/<folder>/), listed as one more database after WEAPON_DBAS
pub const WEAPON_CAFS: [&str; 81] = [
    // the VOLT energy weapons' overheat clips (additive; Animations/Mannequin/ADB/Players/playerItemFixOverheatAnims.adb, fragment fixoverheat, names per weapon CONFIRMED there)
    "weapons/overheat/pistol_alerted_stand_epst_volt_pulse_overheat_01_add.caf",
    "weapons/overheat/pistol_alerted_stand_epst_volt_pulse_overheat_closing_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_erfl_volt_parallax_overheat_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_erfl_volt_parallax_overheat_closing_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_esmg_volt_quartz_overheat_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_esmg_volt_quartz_overheat_closing_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_elmg_volt_fresnel_overheat_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_elmg_volt_fresnel_overheat_closing_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_esgn_volt_prism_overheat_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_esgn_volt_prism_overheat_closing_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_esnp_volt_zenith_overheat_01_add.caf",
    "weapons/overheat/stocked_alerted_stand_esnp_volt_zenith_overheat_closing_01_add.caf",
    "stance_transitions_v2/stocked_alerted_stand_toprone_ots_01.caf",
    "stance_transitions_v2/stocked_alerted_crouch_toprone_ots_01.caf",
    "stance_transitions_v2/stocked_alerted_prone_tocrouch_ots_01.caf",
    "stance_transitions_v2/stocked_alerted_prone_tostand_ots_01.caf",
    "stance_transitions_v2/shouldered_alerted_stand_tocrouch_ots_01.caf",
    "stance_transitions_v2/shouldered_alerted_crouch_tostand_ots_01.caf",
    "stance_transitions/pistol_alerted_crouch_toprone_ots.caf",
    "stance_transitions/pistol_alerted_prone_tocrouch_ots.caf",
    "revived/nw_neutral_prone_revived.caf",
    "revived/nw_neutral_prone_back_revived.caf",
    // the player's grenade clips (EMPIRICAL names under Animations/Characters/Human/<folder>/throw/): equip = taking the grenade, pullpin = pulling the pin, throw = the
    // swing; `_add` ones are additive upper-body layers, the others full body.  A viewer can play them on the player body.
    "throw/throw_alerted_stand_1hr_equip_01_add.caf",
    "throw/throw_alerted_stand_1hr_pullpin_01_add.caf",
    "throw/throw_alerted_stand_1hr_throw_overarm_01_add.caf",
    "throw/throw_alerted_stand_1hr_throw_underarm_01_add.caf",
    "throw/throw_alerted_stand_idle_1hr_upperbody_01.caf",
    "throw/throw_alerted_crouch_1hr_equip_01_add.caf",
    "throw/throw_alerted_crouch_1hr_throw_overarm_01.caf",
    "throw/throw_alerted_crouch_1hr_throw_underarm_01.caf",
    "throw/throw_alerted_prone_1hr_equip_01_add.caf",
    "throw/throw_alerted_prone_1hr_pullpin_01_add.caf",
    "throw/throw_alerted_prone_1hr_throw_overarm_01.caf",
    "throw/throw_1hr_grip_pose_grenade.caf",
    // knife select clips and the standing takedowns of the expert level (`_a` = attacker, `_v` = victim, played together; EMPIRICAL names, one clip each)
    "weapons/select_deselect_v2/knife/knife_alerted_stand_1hr_select_01_add.caf",
    "weapons/select_deselect_v2/knife/knife_alerted_stand_1hr_deselect_01_add.caf",
    "takedown/knife/exp/knife_stand_takedown_exp_stand_back_a_01.caf",
    "takedown/knife/exp/knife_stand_takedown_exp_stand_back_v_01.caf",
    "takedown/knife/exp/knife_stand_takedown_exp_stand_front_a_01.caf",
    "takedown/knife/exp/knife_stand_takedown_exp_stand_front_v_01.caf",
    "takedown/knife/exp/knife_stand_takedown_exp_stand_left_a_01.caf",
    "takedown/knife/exp/knife_stand_takedown_exp_stand_left_v_01.caf",
    "takedown/knife/exp/knife_stand_takedown_exp_stand_right_a_01.caf",
    "takedown/knife/exp/knife_stand_takedown_exp_stand_right_v_01.caf",
    // unarmed (fists): additive upper-body jabs / heavier swings (weapons/fists/) and the standing takedowns of the expert level (takedown/fists/exp/, `_a` / `_v` as for the knife)
    "weapons/fists/fists_alerted_stand_lh_jab_01_add.caf",
    "weapons/fists/fists_alerted_stand_lh_jab_03_add.caf",
    "weapons/fists/fists_alerted_stand_rh_jab_01_add.caf",
    "weapons/fists/fists_alerted_stand_rh_jab_02_add.caf",
    "weapons/fists/fists_alerted_stand_lh_hook_ub_01_add.caf",
    "weapons/fists/fists_alerted_stand_rh_haymaker_ub_01_add.caf",
    "weapons/fists/fists_alerted_stand_rh_uppercut_ub_01_add.caf",
    "weapons/fists/fists_alerted_crouch_lh_jab_01_add.caf",
    "weapons/fists/fists_alerted_crouch_rh_jab_01_add.caf",
    "takedown/fists/exp/fists_stand_takedown_exp_stand_back_a_01.caf",
    "takedown/fists/exp/fists_stand_takedown_exp_stand_back_v_01.caf",
    "takedown/fists/exp/fists_stand_takedown_exp_stand_front_a_01.caf",
    "takedown/fists/exp/fists_stand_takedown_exp_stand_front_v_01.caf",
    "takedown/fists/exp/fists_stand_takedown_exp_stand_left_a_01.caf",
    "takedown/fists/exp/fists_stand_takedown_exp_stand_left_v_01.caf",
    "takedown/fists/exp/fists_stand_takedown_exp_stand_right_a_01.caf",
    "takedown/fists/exp/fists_stand_takedown_exp_stand_right_v_01.caf",
    // medpen (weapons/medpen/): self-injection layers, injecting a standing player, and the paired revive of a downed one (`a` kneeling, `v` on the floor; EMPIRICAL names)
    "weapons/medpen/medpen_alerted_stand_idle_1hr_selfinject_01_add.caf",
    "weapons/medpen/medpen_alerted_crouch_idle_1hr_selfinject_01_add.caf",
    "weapons/medpen/medpen_alerted_prone_idle_1hr_selfinject_01_add.caf",
    "weapons/medpen/medpen_alerted_stand_idle_1hr_playerinject_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_a_facedown_head_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_v_facedown_head_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_a_facedown_feet_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_v_facedown_feet_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_a_facedown_left_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_v_facedown_left_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_a_facedown_right_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_v_facedown_right_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_a_faceup_head_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_v_faceup_head_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_a_faceup_feet_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_v_faceup_feet_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_a_faceup_left_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_v_faceup_left_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_a_faceup_right_01.caf",
    "weapons/medpen/revive_player/medpen_crouch_revive_v_faceup_right_01.caf",
];

/// clips the player body uses (EMPIRICAL names), matched by file stem; first one found wins (in role order)
pub const DEFAULT_POSES: [(&str, &[&str]); 14] = [
    ("idle", &["nw_neutral_stand_runslow_forward_start2idle_rf", "nw_neutral_stand_idle_to_nw_alerted_stand_idle"]),
    ("walk", &["nw_neutral_stand_walkslow_forward", "nw_neutral_stand_walkfast_forward"]),
    ("run", &["nw_neutral_stand_runslow_forward", "nw_neutral_stand_runfast_forward"]),
    ("sprint", &["nw_neutral_stand_sprint_forward"]),
    ("walkback", &["nw_neutral_stand_walkslow_back", "nw_neutral_stand_walkfast_back"]),
    ("strafel", &["nw_neutral_stand_walkslow_left"]),
    ("strafer", &["nw_neutral_stand_walkslow_right"]),
    ("crouchidle", &["nw_neutral_crouch_idle"]),
    ("crouchwalk", &["nw_neutral_crouch_walkfast_forward"]),
    ("sit", &["use_jumpseat_idle", "cockpit_hotas_c_l_hornet_idle", "use_civ_ntrl_sit_mhall_relaxed_a_idle_01"]),
    ("lie", &["use_lie_00_ntrl_a_idle_01", "use_lie080_ntrl_idle_01"]),
    ("ladder", &["ladders_walk_idle_l", "ladders_walk_idle_lfoot"]),
    ("climb", &["ladders_walk_ascend_singlecycle", "ladders_run_ascend_singlecycle"]),
    ("jump", &["nw_neutral_jump_idle", "nw_neutral_jump_loop"]),
];

/// `round(x, nd)` with Python's semantics: the correctly rounded decimal of the binary value (ties to even), as a float again.
pub fn round_nd(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", nd, x).parse().unwrap_or(x)
}

/// EMPIRICAL: additive / overlay clips are named '*_add', '*_additive', '*_overlay' (all with no meaningful absolute pose); their duration is
/// often 0.
pub fn is_additive(clip: &Clip) -> bool {
    let last = clip.name.rsplit('/').next().unwrap_or("");
    let n: String = {
        let c: Vec<char> = last.chars().collect();
        c[..c.len().saturating_sub(4)].iter().collect()
    };
    n.ends_with("_add") || n.ends_with("_additive") || n.ends_with("_overlay") || n.contains("_add_") || (clip.frames <= 1 && n.contains("_add"))
}

pub fn stem(name: &str) -> &str {
    let last = name.rsplit('/').next().unwrap_or("");
    last.rsplit_once('.').map_or(last, |(a, _)| a)
}

/// Lower-case file-name suffix of a texture path ('ddna' of '.../x_ddna.tif'); "" for none.
fn suffix(path: &str) -> String {
    let p = path.replace('\\', "/");
    let last = p.rsplit('/').next().unwrap_or("");
    let s = last.rsplit_once('.').map_or(last, |(a, _)| a);
    s.rsplit('_').next().unwrap_or("").to_lowercase()
}

#[derive(Debug, Clone)]
pub struct MatInfo {
    pub name: String,
    pub color: Vec<f64>,
    pub tex: Option<String>,
    pub nrm: Option<String>,
    pub tone: Option<String>,
}

/// All parts (body, head, eyes) as one vertex set (see `Character::load_mesh`).
pub struct CharMesh {
    pub pos: Vec<[f64; 3]>,
    pub uv: Vec<[f64; 2]>,
    pub nrm: Vec<[f64; 3]>,
    pub b4: Vec<u8>,
    pub w4: Vec<u8>,
    pub headw: Vec<u8>,
    pub idx: Vec<u32>,
    pub ranges: Vec<[i64; 3]>,
    pub mats: Vec<MatInfo>,
    pub mesh_to_full: Vec<usize>,
}

/// `_read`: the exact name, else a case-insensitive match.
fn read_name(p4k: &P4k, name: &str) -> Result<Vec<u8>> {
    let ent = p4k.get(name).or_else(|| {
        let ext = name.rfind('.').map(|i| &name[i..]).unwrap_or("");
        p4k.find_path(name, ext)
    });
    let ent = ent.ok_or_else(|| Error::msg(format!("KeyError: {}", name)))?;
    p4k.read(ent).map_err(|e| Error::msg(e.to_string()))
}

type Clips = (Vec<String>, HashMap<String, (usize, Arc<Clip>)>);

/// One human character kind ('male' / 'female'): skeleton, skinned body mesh, clip databases (all read lazily, thread safe).
pub struct Character<'a> {
    p4k: &'a P4k,
    pub kind: &'static Kind,
    base: String,
    bones: OnceLock<std::result::Result<Vec<Bone>, Error>>,
    mesh: OnceLock<std::result::Result<CharMesh, Error>>,
    clips: OnceLock<Clips>,
    wclips: OnceLock<Clips>,
    blobs: Mutex<HashMap<String, Option<Arc<Vec<u8>>>>>,
}

impl<'a> Character<'a> {
    pub fn new(p4k: &'a P4k, kind: &str) -> Option<Self> {
        let k = KINDS.iter().find(|k| k.name == kind)?;
        Some(Character { p4k, kind: k, base: format!("{}{}/", ROOT, k.folder), bones: OnceLock::new(), mesh: OnceLock::new(), clips: OnceLock::new(), wclips: OnceLock::new(), blobs: Mutex::new(HashMap::new()) })
    }

    fn read(&self, name: &str) -> Result<Vec<u8>> {
        read_name(self.p4k, name)
    }

    pub fn bones(&self) -> Result<&Vec<Bone>> {
        self.bones.get_or_init(|| skeleton::parse_bones(&self.read(&format!("{}{}", self.base, self.kind.skeleton))?)).as_ref().map_err(|e| e.clone())
    }

    pub fn crc_index(&self) -> Result<HashMap<u32, usize>> {
        Ok(self.bones()?.iter().enumerate().map(|(i, b)| (b.crc, i)).collect())
    }

    fn head_set(&self) -> Result<std::collections::HashSet<usize>> {
        let bones = self.bones()?;
        let mut out = std::collections::HashSet::new();
        for i in 0..bones.len() {
            let mut j = i as i64;
            while j >= 0 {
                if bones[j as usize].name == HEAD_BONE {
                    out.insert(i);
                    break;
                }
                j = bones[j as usize].parent as i64;
            }
        }
        Ok(out)
    }

    fn materials(&self, mtl_path: &str) -> Result<Vec<MatInfo>> {
        let subs = mtl::parse_mtl(&self.read(mtl_path)?)?;
        let mut out = vec![];
        for s in subs {
            let tex = s.texture("TexSlot1").map(str::to_string);
            let min = s.diffuse.iter().cloned().fold(f64::INFINITY, f64::min);
            let flat: Vec<f64> = if min > 0.95 { vec![0.30, 0.32, 0.38] } else { s.diffuse.to_vec() };
            let mut nrm = s.texture("TexSlot2").map(str::to_string);
            if s.shader != "HumanSkin_V2" || !matches!(suffix(nrm.as_deref().unwrap_or("")).as_str(), "ddna" | "ddn") {
                nrm = None;
            }
            let color: Vec<f64> = if tex.as_deref().map_or(true, str::is_empty) { flat.iter().map(|c| round_nd(*c, 3)).collect() } else { vec![1.0, 1.0, 1.0] };
            out.push(MatInfo {
                name: s.name.clone(),
                color,
                tex,
                nrm,
                tone: s.params.iter().find(|(k, _)| k == "FinalSkinTone").map(|(_, v)| v.clone()),
            });
        }
        Ok(out)
    }

    /// All parts (body, head, eyes) as one vertex set: positions, normals, uvs, 4 bone influences, head weight; sub-materials of every part are
    /// appended to one list and the draw ranges refer to it.
    fn load_mesh(&self) -> Result<CharMesh> {
        let bones = self.bones()?;
        let full = self.crc_index()?;
        let head = self.head_set()?;
        let mut m = CharMesh { pos: vec![], uv: vec![], nrm: vec![], b4: vec![], w4: vec![], headw: vec![], idx: vec![], ranges: vec![], mats: vec![], mesh_to_full: vec![] };
        for spec in self.kind.parts {
            let stem_ = spec.skin;
            let skin = self.read(&format!("{}.skin", stem_))?;
            let part = skeleton::parse_bones(&skin)?;
            let skinm = self.read(&format!("{}.skinm", stem_))?;
            let meshes = ivo::parse_cgfm(&skinm)?;
            let mesh = meshes.first().ok_or_else(|| Error::msg("IndexError: no mesh"))?;
            let pos = mesh.positions()?;
            let uvs = mesh.uvs()?;
            let (mut idx, mut wts) = skeleton::mesh_skin(mesh)?;
            let rigid = idx.is_empty(); // EMPIRICAL (eyes): a part without a skin stream is fixed to the head bone
            let f = IvoFile::new(&skin)?;
            let mc = f.get(ivo::CH_CGF_MATERIALS).ok_or_else(|| Error::msg("AttributeError: no material chunk"))?;
            let mname = latin1(split_nul(ivo::slice(&skin, mc.offset, mc.offset + mc.size), usize::MAX - 1)[0]);
            let folder = stem_.rsplit_once('/').map_or("", |(a, _)| a);
            let mats = self.materials(&format!("{}/{}.mtl", folder, mname))?;
            // a part's bone indices refer to its own sub-skeleton: append its mapping, shift this part's indices
            // EMPIRICAL: the head's sub-skeleton has face bones the body skeleton lacks; they follow their nearest known ancestor (rigidly)
            let known = |mut i: i64| -> usize {
                while i >= 0 && !full.contains_key(&part[i as usize].crc) {
                    i = part[i as usize].parent as i64;
                }
                if i >= 0 {
                    full[&part[i as usize].crc]
                } else {
                    0
                }
            };
            let mut local: Vec<usize> = vec![];
            for i in 0..part.len() {
                let f_ = known(i as i64);
                if !m.mesh_to_full.contains(&f_) {
                    m.mesh_to_full.push(f_);
                }
                local.push(m.mesh_to_full.iter().position(|x| *x == f_).unwrap());
            }
            let base_v = m.pos.len();
            let n = pos.len();
            if rigid {
                let hb = bones.iter().position(|b| b.name == HEAD_BONE).ok_or_else(|| Error::msg("StopIteration"))?;
                if !m.mesh_to_full.contains(&hb) {
                    m.mesh_to_full.push(hb);
                }
                local = vec![m.mesh_to_full.iter().position(|x| *x == hb).unwrap()];
                idx = vec![[0u16; 8]; n];
                wts = vec![[255, 0, 0, 0, 0, 0, 0, 0]; n];
            }
            for v in 0..n {
                let mut pairs: Vec<(u32, u16)> = idx[v].iter().zip(wts[v].iter()).filter(|(_, w)| **w != 0).map(|(i, w)| (*w as u32, *i)).collect();
                pairs.sort_by(|a, b| b.cmp(a)); // reverse=True on (weight, index) tuples
                pairs.truncate(4);
                let tot: u32 = pairs.iter().map(|p| p.0).sum();
                let tot = if tot == 0 { 1 } else { tot };
                let mut ws: Vec<i64> = pairs.iter().map(|(w, _)| py_round(*w as f64 * 255.0 / tot as f64)).collect();
                while ws.len() < 4 {
                    ws.push(0);
                }
                ws[0] += 255 - ws.iter().sum::<i64>(); // exactly 255 in total
                for k in 0..4 {
                    let b = if k < pairs.len() { *local.get(pairs[k].1 as usize).ok_or_else(|| Error::msg("IndexError: bone index"))? } else { 0 };
                    if b > 255 || !(0..=255).contains(&ws[k]) {
                        return Err(Error::msg("ValueError: byte must be in range(0, 256)"));
                    }
                    m.b4.push(b as u8);
                    m.w4.push(ws[k] as u8);
                }
                let hw: f64 = if spec.head {
                    255.0
                } else {
                    let s: u32 = pairs.iter().filter(|(_, i)| head.contains(&m.mesh_to_full[local[*i as usize]])).map(|p| p.0).sum();
                    s as f64 * 255.0 / tot as f64
                };
                m.headw.push(py_round(hw).min(255) as u8);
            }
            let tris = mesh.indices()?;
            let nrm: Vec<[f64; 3]> = match FrameReader::new(mesh) {
                Some(fr) => (0..n).map(|i| Ok(fr.get(i)?.n)).collect::<Result<_>>()?,
                None => {
                    let mut nrm = vec![[0.0f64; 3]; n];
                    let mut t = 0;
                    while t + 2 < tris.len() {
                        let (a, b, c) = (tris[t] as usize, tris[t + 1] as usize, tris[t + 2] as usize);
                        let (pa, pb, pc) = (pos[a], pos[b], pos[c]);
                        let u = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
                        let w = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
                        let fnrm = [u[1] * w[2] - u[2] * w[1], u[2] * w[0] - u[0] * w[2], u[0] * w[1] - u[1] * w[0]];
                        for i in [a, b, c] {
                            for k in 0..3 {
                                nrm[i][k] += fnrm[k];
                            }
                        }
                        t += 3;
                    }
                    for v in nrm.iter_mut() {
                        let ln = (v[0].powi(2) + v[1].powi(2) + v[2].powi(2)).powf(0.5);
                        let ln = if ln == 0.0 { 1.0 } else { ln };
                        *v = [v[0] / ln, v[1] / ln, v[2] / ln];
                    }
                    nrm
                }
            };
            m.pos.extend(pos);
            m.uv.extend(uvs);
            m.nrm.extend(nrm);
            let first_idx = m.idx.len();
            m.idx.extend(tris.iter().map(|&i| i as u32 + base_v as u32));
            let mbase = m.mats.len() as i64;
            for s in &mesh.subsets {
                m.ranges.push([(first_idx + s.first_index as usize) as i64, s.num_indices as i64, mbase + (s.mat_id & 0xFFFF) as i64]);
            }
            m.mats.extend(mats);
        }
        Ok(m)
    }

    pub fn mesh(&self) -> Result<&CharMesh> {
        self.mesh.get_or_init(|| self.load_mesh()).as_ref().map_err(|e| e.clone())
    }

    /// Binary model: u32 header length, JSON header, then position f32x3, normal f32x3, uv f32x2, bone index u8x4, weight u8x4 (sum 255), head
    /// weight u8, 16-bit indices (padded to 4 bytes).  The bone index of a vertex is a MESH bone (0..len(skin)-1); header `skin` maps it to the
    /// full skeleton index.
    pub fn model(&self) -> Result<Vec<u8>> {
        let m = self.mesh()?;
        let n = m.pos.len();
        let bones = self.bones()?;
        let flat = |vs: Vec<[f64; 7]>| -> Vec<f64> { vs.iter().flatten().map(|c| round_nd(*c, 6)).collect() };
        let mut poses = Map::new();
        for (role, _) in DEFAULT_POSES.iter() {
            if let Some(c) = self.pose_clip(role) {
                poses.insert(role.to_string(), json!(c));
            }
        }
        let rest: Vec<[f64; 7]> = bones.iter().map(|b| [b.rq[0], b.rq[1], b.rq[2], b.rq[3], b.rp[0], b.rp[1], b.rp[2]]).collect();
        let bind: Vec<[f64; 7]> = bones.iter().map(|b| [b.aq[0], b.aq[1], b.aq[2], b.aq[3], b.ap[0], b.ap[1], b.ap[2]]).collect();
        let head = json!({
            "kind": self.kind.name, "nv": n, "ni": m.idx.len(), "ranges": m.ranges, "skin": m.mesh_to_full,
            "mats": m.mats.iter().enumerate().map(|(i, x)| json!({
                "name": x.name,
                // a textured material carries the int list [1, 1, 1], an untextured one rounded floats
                "color": if x.tex.as_deref().map_or(false, |t| !t.is_empty()) { json!([1, 1, 1]) } else { json!(x.color) },
                "tex": if x.tex.as_deref().map_or(false, |t| !t.is_empty()) { i as i64 } else { -1 },
                "nrm": if x.nrm.as_deref().map_or(false, |t| !t.is_empty()) { i as i64 } else { -1 },
            })).collect::<Vec<_>>(),
            "bones": {"names": bones.iter().map(|b| b.name.clone()).collect::<Vec<_>>(), "parent": bones.iter().map(|b| b.parent).collect::<Vec<_>>(),
                      "rest": flat(rest), "bind": flat(bind)},
            "poses": poses,
        });
        let mut hj = crate::pycompact::compact(&head);
        hj.extend(std::iter::repeat(b' ').take(hj.len().wrapping_neg() & 3));
        let mut out = (hj.len() as u32).to_le_bytes().to_vec();
        out.extend(hj);
        for v in &m.pos {
            for c in v {
                out.extend((*c as f32).to_le_bytes());
            }
        }
        for v in &m.nrm {
            for c in v {
                out.extend((*c as f32).to_le_bytes());
            }
        }
        for v in &m.uv {
            for c in v {
                out.extend((*c as f32).to_le_bytes());
            }
        }
        out.extend(&m.b4);
        out.extend(&m.w4);
        out.extend(&m.headw);
        out.extend(std::iter::repeat(0u8).take(n.wrapping_neg() & 3));
        for &i in &m.idx {
            if i > 0xFFFF {
                return Err(Error::msg("struct.error: ushort format requires 0 <= number <= 65535"));
            }
            out.extend((i as u16).to_le_bytes());
        }
        if m.idx.len() & 1 == 1 {
            out.extend([0u8, 0]);
        }
        Ok(out)
    }

    /// (content type, bytes) of the diffuse map of sub-material i (a PNG, 512 px), or None.
    pub fn texture(&self, i: usize) -> Option<(&'static str, Vec<u8>)> {
        let m = &self.mesh().ok()?.mats;
        let tex = m.get(i)?.tex.as_deref().filter(|t| !t.is_empty())?;
        let t = load_texture(self.p4k, tex, 512)?;
        (t.fmt == "png").then(|| ("image/png", t.data))
    }

    /// Sub-material i's ddna normal map packed with its gloss (`textures::ng_png`: R, G = normal x, y; B = gloss), or None.
    pub fn normal_texture(&self, i: usize) -> Option<(&'static str, Vec<u8>)> {
        let m = &self.mesh().ok()?.mats;
        let nrm = m.get(i)?.nrm.as_deref().filter(|t| !t.is_empty())?;
        let got = ng_png(self.p4k, &texture_base(nrm), 512)?;
        Some(("image/png", got.2))
    }

    // ------------------------------------------------------------------ clips
    /// {lower-case clip path: (database index into CLIP_DBAS, Clip)} of CLIP_DBAS in insertion order.
    fn clips(&self) -> &Clips {
        self.clips.get_or_init(|| {
            let mut order: Vec<String> = vec![];
            let mut map: HashMap<String, (usize, Arc<Clip>)> = HashMap::new();
            for (di, rel) in CLIP_DBAS.iter().enumerate() {
                let name = format!("{}{}/{}", ANIM, self.kind.folder, rel);
                let Ok(data) = self.read(&name) else { continue };
                let Ok(db) = dba::parse_dba(&data) else { continue };
                let Dba { order: names, clips } = db;
                let mut clips = clips;
                for k in names {
                    let c = clips.remove(&k).unwrap();
                    if map.insert(k.clone(), (di, c)).is_none() {
                        order.push(k);
                    }
                }
            }
            (order, map)
        })
    }

    /// the clips of WEAPON_DBAS (database index into WEAPON_DBAS), parsed on first use
    fn wclips(&self) -> &Clips {
        self.wclips.get_or_init(|| {
            let mut order: Vec<String> = vec![];
            let mut map: HashMap<String, (usize, Arc<Clip>)> = HashMap::new();
            for (di, rel) in WEAPON_DBAS.iter().enumerate() {
                let Ok(data) = self.read(&format!("{}{}/{}", ANIM, self.kind.folder, rel)) else { continue };
                let Ok(db) = dba::parse_dba(&data) else { continue };
                let Dba { order: names, mut clips } = db;
                for k in names {
                    let c = clips.remove(&k).unwrap();
                    if map.insert(k.clone(), (di, c)).is_none() {
                        order.push(k);
                    }
                }
            }
            for rel in WEAPON_CAFS {
                let Ok(data) = self.read(&format!("{}{}/{}", ANIM, self.kind.folder, rel)) else { continue };
                let k = format!("animations/characters/human/{}/{}", self.kind.folder, rel).to_lowercase();
                if let Ok(c) = dba::parse_caf(&data, &k) {
                    if map.insert(k.clone(), (WEAPON_DBAS.len(), Arc::new(c))).is_none() {
                        order.push(k);
                    }
                }
            }
            (order, map)
        })
    }

    pub fn clip(&self, id: &str) -> Option<&Clip> {
        self.clips().1.get(id).or_else(|| self.wclips().1.get(id)).map(|(_, c)| &**c)
    }

    /// like `clip_list` for WEAPON_DBAS
    pub fn clip_list_weapons(&self) -> Value {
        let (order, map) = self.wclips();
        let mut by_db: Vec<Vec<Value>> = vec![vec![]; WEAPON_DBAS.len() + 1];
        for k in order {
            let (di, c) = &map[k];
            by_db[*di].push(json!({"id": k, "name": stem(k), "secs": round_nd(c.seconds, 2), "bones": c.bones.len(), "add": is_additive(c)}));
        }
        let mut out = vec![];
        for (di, rel) in WEAPON_DBAS.iter().chain(std::iter::once(&"(stance transition clips)")).enumerate() {
            let mut clips = std::mem::take(&mut by_db[di]);
            clips.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            out.push(json!({"db": rel, "clips": clips}));
        }
        Value::Array(out)
    }

    pub fn clip_by_stem(&self, s: &str) -> Option<String> {
        let s = s.to_lowercase();
        self.clips().0.iter().find(|k| stem(k) == s).cloned()
    }

    pub fn pose_clip(&self, role: &str) -> Option<String> {
        let list = DEFAULT_POSES.iter().find(|(r, _)| *r == role)?.1;
        list.iter().find_map(|s| self.clip_by_stem(s))
    }

    /// JSON-able catalogue [{db, clips: [{id, name, secs, bones, add}]}] in CLIP_DBAS order.
    pub fn clip_list(&self) -> Value {
        let (order, map) = self.clips();
        let mut by_db: Vec<Vec<Value>> = vec![vec![]; CLIP_DBAS.len()];
        for k in order {
            let (di, c) = &map[k];
            by_db[*di].push(json!({"id": k, "name": stem(k), "secs": round_nd(c.seconds, 2), "bones": c.bones.len(), "add": is_additive(c)}));
        }
        let mut out = vec![];
        for (di, rel) in CLIP_DBAS.iter().enumerate() {
            if by_db[di].is_empty() {
                continue;
            }
            let mut clips = std::mem::take(&mut by_db[di]);
            clips.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            out.push(json!({"db": rel, "clips": clips}));
        }
        Value::Array(out)
    }

    /// Binary clip (cached, at most 64 entries): u32 header length, JSON header {n, bones: [skeleton indices], secs, fps, travel: [x,y,z], speed},
    /// then n x bones x 7 float32 (quaternion xyzw, position) sampled every frame (30 fps, n = frames + 1).  The 'World' bone's position is zeroed
    /// (the character is moved by the caller; `travel` is the root motion over the clip, `speed` its horizontal m/s).
    pub fn clip_blob(&self, cid: &str) -> Option<Arc<Vec<u8>>> {
        if let Some(b) = self.blobs.lock().unwrap().get(cid) {
            return b.clone();
        }
        let got = self.make_clip_blob(cid).map(Arc::new);
        let mut c = self.blobs.lock().unwrap();
        if c.len() >= 64 {
            c.clear();
        }
        c.insert(cid.to_string(), got.clone());
        got
    }

    fn make_clip_blob(&self, cid: &str) -> Option<Vec<u8>> {
        let clip = self.clip(cid)?;
        let bones = self.bones().ok()?;
        let full = self.crc_index().ok()?;
        let tm = clip.tracks();
        let mut tracks: Vec<(usize, &dba::Track)> = tm.iter().filter_map(|(crc, t)| full.get(&crc).map(|&i| (i, t))).collect();
        tracks.sort_by_key(|x| x.0);
        let n = clip.frames.max(1) as usize + 1;
        let world = bones.iter().position(|b| b.name == "World").unwrap_or(0);
        let mut travel = [0.0f64; 3];
        let mut root: Vec<f64> = vec![];
        let mut data: Vec<f32> = Vec::with_capacity(n * tracks.len() * 7);
        let mut first = [0.0f64; 3];
        for f in 0..n {
            for (bi, t) in &tracks {
                let (q, p) = t.sample(f as f64);
                let b = &bones[*bi];
                let q = q.unwrap_or(b.rq);
                let mut p = p.unwrap_or(b.rp);
                if *bi == world {
                    root.extend(p.iter().map(|v| round_nd(*v, 3)));
                    if f == 0 {
                        first = p;
                    }
                    if f == n - 1 {
                        travel = [p[0] - first[0], p[1] - first[1], p[2] - first[2]];
                    }
                    p = [0.0, 0.0, 0.0];
                }
                data.extend(q.iter().map(|v| *v as f32));
                data.extend(p.iter().map(|v| *v as f32));
            }
        }
        let secs = clip.seconds;
        let mut head = Map::new();
        head.insert("n".into(), json!(n));
        head.insert("fps".into(), json!(FPS));
        head.insert("bones".into(), json!(tracks.iter().map(|x| x.0).collect::<Vec<_>>()));
        head.insert("secs".into(), json!(round_nd(secs, 4)));
        head.insert("travel".into(), json!(travel.iter().map(|x| round_nd(*x, 4)).collect::<Vec<_>>()));
        head.insert("speed".into(), json!(if secs > 0.0 { round_nd((travel[0].powi(2) + travel[1].powi(2)).powf(0.5) / secs, 3) } else { 0.0 }));
        head.insert("add".into(), json!(is_additive(clip)));
        head.insert("name".into(), json!(stem(cid)));
        if root.len() == 3 * n {
            head.insert("root".into(), json!(root));
        }
        let mut hj = crate::pycompact::compact(&Value::Object(head));
        hj.extend(std::iter::repeat(b' ').take(hj.len().wrapping_neg() & 3));
        let mut out = (hj.len() as u32).to_le_bytes().to_vec();
        out.extend(hj);
        for v in data {
            out.extend(v.to_le_bytes());
        }
        Some(out)
    }
}

/// The characters of one archive, created on first use.
pub struct CharacterBank<'a> {
    p4k: &'a P4k,
    chars: Mutex<HashMap<String, Arc<Character<'a>>>>,
}

impl<'a> CharacterBank<'a> {
    pub fn new(p4k: &'a P4k) -> Self {
        CharacterBank { p4k, chars: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, kind: &str) -> Option<Arc<Character<'a>>> {
        let mut c = self.chars.lock().unwrap();
        if let Some(x) = c.get(kind) {
            return Some(x.clone());
        }
        let ch = Arc::new(Character::new(self.p4k, kind)?);
        c.insert(kind.to_string(), ch.clone());
        Some(ch)
    }
}

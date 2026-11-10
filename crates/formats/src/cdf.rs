//! Character definition files (`.cdf`): the model of a first-person weapon (and of anything built from a skeleton plus attached parts).
//! No archive access: callers pass bytes.
//!
//! EMPIRICAL (`brfl_fps_behr_p4ar.cdf` and the 500-odd weapon .cdf files of `Objects/fps_weapons`): a CryXmlB document
//! `<CharacterDefinition><Model File=".chr" Material="..."/><AttachmentList><Attachment .../>...`.
//!
//! * `Model`: the skeleton (`.chr`) and the default material (no extension).
//! * `Attachment Type="CA_SKIN"`: a skinned part (`Binding` = a `.skin`; its mesh is the `.skinm` next to it) with its own `Material`; its
//!   vertices are stored in the model space of the skeleton's bind pose.
//! * `Type="CA_BONE"`: a rigid piece (`Binding` = a `.cgf`) hung on `BoneName` with `RelPosition` (x,y,z) and `RelRotation` (w,x,y,z).
//! * `AName` is the attachment's name; `Flags` / `VisFlags` are kept as strings.

use serde_json::{json, Value};

use crate::error::*;
use crate::mtl::py_float;
use crate::xmlq::{self, attr};

/// n comma separated floats, else the default.
pub fn floats(s: Option<&str>, n: usize) -> Option<Vec<f64>> {
    let v: Option<Vec<f64>> = s?.split(',').map(py_float).collect();
    v.filter(|v| v.len() == n)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub atype: String,
    pub name: String,
    pub binding: String,
    pub material: String,
    pub bone: String,
    pub pos: [f64; 3],
    /// w, x, y, z
    pub rot: [f64; 4],
    pub flags: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cdf {
    pub model: String,
    pub material: String,
    pub attachments: Vec<Attachment>,
}

/// Model, default material and attachments of a `.cdf` (CryXmlB or plain XML bytes).  Paths are as written (relative to Data/).
pub fn parse_cdf(data: &[u8]) -> Result<Cdf> {
    let root = xmlq::parse_any(data)?;
    let model = root.find("Model");
    let mut out = Cdf {
        model: model.map(|m| attr(m, "File").to_string()).unwrap_or_default(),
        material: model.map(|m| attr(m, "Material").to_string()).unwrap_or_default(),
        attachments: vec![],
    };
    for a in xmlq::iter(&root, "Attachment") {
        let pos = floats(a.get("RelPosition"), 3).map(|v| [v[0], v[1], v[2]]).unwrap_or([0.0; 3]);
        let rot = floats(a.get("RelRotation"), 4).map(|v| [v[0], v[1], v[2], v[3]]).unwrap_or([1.0, 0.0, 0.0, 0.0]);
        out.attachments.push(Attachment {
            atype: attr(a, "Type").to_string(),
            name: attr(a, "AName").to_string(),
            binding: attr(a, "Binding").replace('\\', "/"),
            material: attr(a, "Material").replace('\\', "/"),
            bone: attr(a, "BoneName").to_string(),
            pos,
            rot,
            flags: attr(a, "Flags").to_string(),
        });
    }
    Ok(out)
}

impl Cdf {
    /// The JSON object form (`type`, `name`, ... keys).
    pub fn to_json(&self) -> Value {
        json!({"model": self.model, "material": self.material, "attachments": self.attachments.iter().map(|a| json!({
            "type": a.atype, "name": a.name, "binding": a.binding, "material": a.material, "bone": a.bone, "pos": a.pos, "rot": a.rot, "flags": a.flags
        })).collect::<Vec<_>>()})
    }
}

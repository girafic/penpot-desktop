//! Penpot data model.
//!
//! Files, projects, teams, and the file-data substructure (pages, components,
//! colors, typographies, media). The shape data inside a page is kept as
//! `serde_json::Value` because Penpot adds new fields with every release —
//! round-trip fidelity matters more than static typing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::transit;

/// Root frame UUID — present in every page's `objects` map.
/// Hard-coded by Penpot; we surface it as a named constant so callers
/// don't sprinkle `Uuid::nil()` literals in the change-applier.
#[allow(dead_code)]
pub const ROOT_FRAME_ID: Uuid = Uuid::nil();

/// File-data version we author. Older files are migrated forward at import
/// time (TODO Phase 6); newer files we refuse with a clear error.
pub const FILE_DATA_VERSION: u32 = 64;

// ───────────────────────── Snapshot ─────────────────────────

/// Versioned capture of `file.data` at a specific `revn`. Surfaced
/// through Penpot's "Version History" sidebar; manual ones are created
/// via `take-file-snapshot` (or the File-menu "Pin Version" item),
/// auto ones come from the Phase-2 every-N-revns logic.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Snapshot {
    pub id: Uuid,
    pub file_id: Uuid,
    pub revn: i64,
    /// `Some("auto")` for the auto-snapshot path; user-supplied string
    /// for "Pin Version"; `None` is permitted but discouraged — most
    /// UIs treat label-less rows as untitled drafts.
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Snapshot {
    pub fn is_auto(&self) -> bool {
        matches!(self.label.as_deref(), Some("auto"))
    }
}

// ───────────────────────── Font variant ─────────────────────────

/// One entry in a team's font collection. Penpot variants ship in
/// multiple formats (TTF, OTF, WOFF, WOFF2) so the frontend can pick
/// whichever the user's browser supports best. Each `*_file_id` points
/// at a media row keyed by its storage-object UUID.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FontVariant {
    pub id: Uuid,
    pub team_id: Uuid,
    pub font_id: Uuid,
    pub font_family: String,
    pub font_weight: i64,
    /// `"normal"` or `"italic"`.
    pub font_style: String,
    pub woff1_file_id: Option<Uuid>,
    pub woff2_file_id: Option<Uuid>,
    pub ttf_file_id: Option<Uuid>,
    pub otf_file_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

// ───────────────────────── Team / Project / File ─────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    pub is_default: bool,
    #[serde(default)]
    pub features: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

impl Team {
    pub fn local(id: Uuid) -> Self {
        let now = Utc::now();
        Self {
            id,
            name: "Local Team".into(),
            is_default: true,
            features: default_features(),
            created_at: now,
            modified_at: now,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Project {
    pub id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    pub is_default: bool,
    #[serde(default)]
    pub is_pinned: bool,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
}

impl Project {
    pub fn local(id: Uuid, team_id: Uuid) -> Self {
        let now = Utc::now();
        Self {
            id,
            team_id,
            name: "Drafts".into(),
            is_default: true,
            is_pinned: false,
            created_at: now,
            modified_at: now,
        }
    }
}

/// Penpot file. `data` holds the entire file-data tree (pages, components,
/// colors, typographies, media, tokens) as JSON for forward-compat.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct File {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    /// Monotonic revision counter — bumps on every successful `update-file`.
    pub revn: i64,
    /// Snapshot version counter — bumps on manual/auto snapshots only.
    pub vern: i64,
    /// File-data schema version (independent of `revn`/`vern`).
    pub version: u32,
    #[serde(default)]
    pub is_shared: bool,
    #[serde(default)]
    pub features: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    /// Full file-data tree.
    pub data: Value,
}

impl File {
    /// Build a brand-new empty file with one untitled page.
    pub fn empty(id: Uuid, project_id: Uuid, name: &str) -> Self {
        let now = Utc::now();
        let page_id = Uuid::new_v4();
        let data = empty_file_data(page_id);
        Self {
            id,
            project_id,
            name: name.into(),
            revn: 0,
            vern: 0,
            version: FILE_DATA_VERSION,
            is_shared: false,
            features: default_features(),
            created_at: now,
            modified_at: now,
            data,
        }
    }
}

fn default_features() -> Vec<String> {
    vec![
        "components/v2".into(),
        "fdata/objects-map".into(),
        "fdata/pointer-map".into(),
        "fdata/shape-data-type".into(),
        "fdata/path-data".into(),
        "design-tokens/v1".into(),
        "layout/grid".into(),
        "styles/v2".into(),
    ]
}

fn empty_file_data(page_id: Uuid) -> Value {
    json!({
        "id": null,
        "version": FILE_DATA_VERSION,
        "options": {},
        "pages": [page_id.to_string()],
        "pagesIndex": {
            page_id.to_string(): {
                "id": page_id.to_string(),
                "name": "Page 1",
                "objects": {
                    Uuid::nil().to_string(): {
                        "id": Uuid::nil().to_string(),
                        "type": "frame",
                        "name": "Root Frame",
                        "frameId": Uuid::nil().to_string(),
                        "parentId": Uuid::nil().to_string(),
                        "x": 0.0, "y": 0.0,
                        "width": 0.1, "height": 0.1,
                        "rotation": 0.0,
                        "shapes": [],
                    }
                },
                "options": {
                    "background": "#FFFFFF"
                }
            }
        },
        "components": {},
        "colors": {},
        "typographies": {},
        "media": {}
    })
}

// ───────────────────────── Profile ─────────────────────────

/// Local single-user profile served to the frontend. Penpot routes anonymous
/// profiles to the login page, so we always return this filled-in stub.
pub const LOCAL_PROFILE_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0001);
pub const LOCAL_TEAM_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0001_0000_0000_0000);
pub const LOCAL_PROJECT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0002_0000_0000_0000);

pub fn local_profile_json(language: &str) -> Value {
    let now = Utc::now().to_rfc3339();
    json!({
        "id": LOCAL_PROFILE_ID,
        "email": "local@penpot-desktop.local",
        "fullname": "Local User",
        "isActive": true,
        "isBlocked": false,
        "isDemo": false,
        "isMuted": false,
        "authBackend": "penpot",
        "defaultTeamId": LOCAL_TEAM_ID,
        "defaultProjectId": LOCAL_PROJECT_ID,
        "lang": language,
        "theme": "default",
        "props": {
            "onboarding-viewed": true,
            "release-notes-viewed": true,
            "v2-info-shown": true,
        },
        "createdAt": now,
        "modifiedAt": now,
    })
}

// ───────────────────────── transit ↔ json bridges ─────────────────────────

/// Decode a transit JSON string into a JSON value with kebab-case keys
/// converted to camelCase. Penpot writes transit with kebab-case keywords
/// (`:project-id`); the JS frontend's reader turns them into camelCase
/// (`projectId`). We mirror that here.
#[allow(dead_code)]
pub fn transit_to_camel_json(transit_json: &str) -> Result<Value, transit::ReadError> {
    let mut r = transit::Reader::new();
    let v = r.read(transit_json)?;
    Ok(camelize(v.to_json()))
}

/// Encode a JSON value (camelCase keys) as transit+JSON (kebab-case keywords).
/// Used when writing `.penpot` archives.
pub fn json_to_transit(value: &Value) -> String {
    let kebab = kebabize(value);
    let transit_value = json_to_transit_value(&kebab);
    let mut w = transit::Writer::new();
    w.write(&transit_value)
}

/// Walk a JSON tree and rewrite object keys from kebab-case → camelCase.
pub fn camelize(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (kebab_to_camel(&k), camelize(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(camelize).collect()),
        other => other,
    }
}

/// Walk a JSON tree and rewrite object keys from camelCase → kebab-case.
pub fn kebabize(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (camel_to_kebab(k), kebabize(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(kebabize).collect()),
        other => other.clone(),
    }
}

fn kebab_to_camel(s: &str) -> String {
    if !s.contains('-') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut upper_next = false;
    for c in s.chars() {
        if c == '-' {
            upper_next = true;
        } else if upper_next {
            out.push(c.to_ascii_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn camel_to_kebab(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Best-effort JSON-value → transit-Value conversion that promotes likely
/// keyword-shaped strings (UUID, kebab-case IDs) to the proper transit type.
fn json_to_transit_value(value: &Value) -> transit::Value {
    use std::rc::Rc;
    match value {
        Value::Null => transit::Value::Nil,
        Value::Bool(b) => transit::Value::Bool(*b),
        Value::Number(n) => n
            .as_i64()
            .map(transit::Value::Int)
            .or_else(|| n.as_f64().map(transit::Value::Float))
            .unwrap_or(transit::Value::Nil),
        Value::String(s) => {
            if let Ok(u) = uuid::Uuid::parse_str(s) {
                transit::Value::Uuid(u)
            } else {
                transit::Value::Str(Rc::from(s.as_str()))
            }
        }
        Value::Array(items) => {
            transit::Value::Vec(items.iter().map(json_to_transit_value).collect())
        }
        Value::Object(map) => {
            let entries: Vec<(transit::Value, transit::Value)> = map
                .iter()
                .map(|(k, v)| (transit::Value::Keyword(Rc::from(k.as_str())), json_to_transit_value(v)))
                .collect();
            transit::Value::Map(entries)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_camel_round_trip() {
        assert_eq!(kebab_to_camel("project-id"), "projectId");
        assert_eq!(kebab_to_camel("plain"), "plain");
        assert_eq!(camel_to_kebab("projectId"), "project-id");
        assert_eq!(camel_to_kebab("plain"), "plain");
    }

    #[test]
    fn empty_file_has_root_frame() {
        let f = File::empty(Uuid::new_v4(), Uuid::new_v4(), "t");
        let pages_index = f.data.get("pagesIndex").unwrap().as_object().unwrap();
        let (_, page) = pages_index.iter().next().unwrap();
        let objects = page.get("objects").unwrap().as_object().unwrap();
        assert!(objects.contains_key(&Uuid::nil().to_string()));
    }
}

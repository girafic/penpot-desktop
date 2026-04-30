//! Penpot change applier — the in-process equivalent of `process-change`.
//!
//! Each change is a small map describing a single mutation to a file's
//! `data` tree. Changes flow through `update-file` from the editor; the
//! applier runs them sequentially, mutating in place and returning a
//! reverse change-vector for undo.
//!
//! Coverage for Phase 1 — the change types the editor emits during normal
//! edit sessions:
//! - Page-level: `add-page`, `mod-page`, `del-page`, `mov-page`
//! - Object: `add-obj`, `mod-obj`, `del-obj`, `mov-objects`, `reg-objects`
//! - Asset: add/mod/del for `color`, `typography`, `media`, `component`
//! - Token: add/mod/del for `token`, `token-set`, `token-theme`,
//!   `set-tokens-lib`
//!
//! Operations inside `mod-obj`: `set`, `assign`, `unassign`, `set-touched`.
//!
//! Unknown change types are tolerated — they are recorded as no-ops with a
//! trace, so a slightly newer frontend doesn't brick the local backend.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

/// Apply a sequence of changes in order, mutating `file_data` in place.
/// Returns a reversed undo vector.
pub fn apply_changes(file_data: &mut Value, changes: &[Value]) -> Result<Vec<Value>> {
    let mut undo: Vec<Value> = Vec::with_capacity(changes.len());
    for change in changes {
        let inv = apply_change(file_data, change)?;
        undo.push(inv);
    }
    undo.reverse();
    Ok(undo)
}

fn apply_change(file_data: &mut Value, change: &Value) -> Result<Value> {
    let change_type = change
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("change without :type"))?;

    match change_type {
        // Pages
        "add-page" => add_page(file_data, change),
        "mod-page" => mod_page(file_data, change),
        "del-page" => del_page(file_data, change),
        "mov-page" => mov_page(file_data, change),

        // Objects
        "add-obj" => add_obj(file_data, change),
        "mod-obj" => mod_obj(file_data, change),
        "del-obj" => del_obj(file_data, change),
        "mov-objects" => mov_objects(file_data, change),
        "reg-objects" => Ok(json!({"type": "reg-objects",
            "pageId": change.get("pageId").cloned().unwrap_or(Value::Null),
            "shapes": change.get("shapes").cloned().unwrap_or(json!([]))})),

        // Library assets
        t @ ("add-color" | "mod-color" | "del-color"
            | "add-typography" | "mod-typography" | "del-typography"
            | "add-media" | "mod-media" | "del-media"
            | "add-component" | "mod-component" | "del-component") => {
            apply_lib_change(file_data, t, change)
        }

        // Tokens (design-tokens/v1)
        t @ ("add-token" | "mod-token" | "del-token"
            | "add-token-set" | "mod-token-set" | "del-token-set"
            | "move-token-set"
            | "add-token-theme" | "mod-token-theme" | "del-token-theme"
            | "set-tokens-lib") => apply_token_change(file_data, t, change),

        // Tolerate-and-trace anything else.
        other => {
            eprintln!("[backend] unknown change type: {other}");
            Ok(json!({"type": "noop"}))
        }
    }
}

// ───────────────────────── Pages ─────────────────────────

fn add_page(file: &mut Value, ch: &Value) -> Result<Value> {
    let page_id = require_id(ch, "id")?.to_string();
    let page = ch
        .get("page")
        .cloned()
        .or_else(|| ch.get("data").cloned())
        .unwrap_or_else(|| {
            json!({
                "id": page_id,
                "name": "New Page",
                "objects": {
                    uuid::Uuid::nil().to_string(): root_frame_obj()
                },
                "options": {}
            })
        });
    let pages_index = ensure_object(file, "pagesIndex")?;
    pages_index.insert(page_id.clone(), page);
    let pages = ensure_array(file, "pages")?;
    pages.push(json!(page_id));
    Ok(json!({"type": "del-page", "id": page_id}))
}

fn mod_page(file: &mut Value, ch: &Value) -> Result<Value> {
    let page_id = require_id(ch, "id")?.to_string();
    let pages_index = ensure_object(file, "pagesIndex")?;
    let page = pages_index
        .get_mut(&page_id)
        .ok_or_else(|| anyhow!("page {page_id} not found"))?;
    let mut prev: Map<String, Value> = Map::new();
    if let Some(name) = ch.get("name").and_then(Value::as_str) {
        prev.insert(
            "name".into(),
            page.get("name").cloned().unwrap_or(Value::Null),
        );
        page["name"] = json!(name);
    }
    Ok(json!({"type": "mod-page", "id": page_id, "name": prev.get("name").cloned().unwrap_or(Value::Null)}))
}

fn del_page(file: &mut Value, ch: &Value) -> Result<Value> {
    let page_id = require_id(ch, "id")?.to_string();
    let pages_index = ensure_object(file, "pagesIndex")?;
    let removed = pages_index
        .remove(&page_id)
        .ok_or_else(|| anyhow!("page {page_id} not found"))?;
    let pages = ensure_array(file, "pages")?;
    pages.retain(|v| v.as_str() != Some(&page_id));
    Ok(json!({"type": "add-page", "id": page_id, "page": removed}))
}

fn mov_page(file: &mut Value, ch: &Value) -> Result<Value> {
    let page_id = require_id(ch, "id")?.to_string();
    let index = ch
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("mov-page without :index"))? as usize;
    let pages = ensure_array(file, "pages")?;
    let prev_index = pages
        .iter()
        .position(|v| v.as_str() == Some(&page_id))
        .ok_or_else(|| anyhow!("page {page_id} not in pages"))?;
    let entry = pages.remove(prev_index);
    let target = index.min(pages.len());
    pages.insert(target, entry);
    Ok(json!({"type": "mov-page", "id": page_id, "index": prev_index}))
}

fn root_frame_obj() -> Value {
    json!({
        "id": uuid::Uuid::nil().to_string(),
        "type": "frame",
        "name": "Root Frame",
        "frameId": uuid::Uuid::nil().to_string(),
        "parentId": uuid::Uuid::nil().to_string(),
        "x": 0.0, "y": 0.0,
        "width": 0.1, "height": 0.1,
        "rotation": 0.0,
        "shapes": [],
    })
}

// ───────────────────────── Objects ─────────────────────────

fn add_obj(file: &mut Value, ch: &Value) -> Result<Value> {
    let page_id = require_id(ch, "pageId")?.to_string();
    let id = require_id(ch, "id")?.to_string();
    let parent_id = ch
        .get("parentId")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let _frame_id = ch
        .get("frameId")
        .and_then(Value::as_str)
        .unwrap_or(&parent_id)
        .to_string();
    let index = ch.get("index").and_then(Value::as_u64).map(|i| i as usize);
    let obj = ch
        .get("obj")
        .cloned()
        .ok_or_else(|| anyhow!("add-obj without :obj"))?;

    let objects = page_objects_mut(file, &page_id)?;
    objects.insert(id.clone(), obj);

    if let Some(parent) = objects.get_mut(&parent_id) {
        let shapes = parent
            .get_mut("shapes")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow!("parent {parent_id} has no :shapes"))?;
        match index {
            Some(i) => {
                let target = i.min(shapes.len());
                shapes.insert(target, json!(id));
            }
            None => shapes.push(json!(id)),
        }
    }

    Ok(json!({"type": "del-obj", "pageId": page_id, "id": id}))
}

fn mod_obj(file: &mut Value, ch: &Value) -> Result<Value> {
    let page_id = require_id(ch, "pageId")?.to_string();
    let id = require_id(ch, "id")?.to_string();
    let ops = ch
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("mod-obj without :operations"))?;

    let obj = page_objects_mut(file, &page_id)?
        .get_mut(&id)
        .ok_or_else(|| anyhow!("obj {id} not found in page {page_id}"))?;

    let mut undo_ops: Vec<Value> = Vec::with_capacity(ops.len());
    for op in ops {
        let undo_op = apply_obj_op(obj, op)?;
        undo_ops.push(undo_op);
    }
    undo_ops.reverse();

    Ok(json!({
        "type": "mod-obj",
        "pageId": page_id,
        "id": id,
        "operations": undo_ops
    }))
}

fn apply_obj_op(obj: &mut Value, op: &Value) -> Result<Value> {
    let op_type = op
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("op without :type"))?;
    let map = obj
        .as_object_mut()
        .ok_or_else(|| anyhow!("object is not a map"))?;

    match op_type {
        "set" => {
            let attr = op
                .get("attr")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("set without :attr"))?
                .to_string();
            let val = op.get("val").cloned().unwrap_or(Value::Null);
            let prev = map.get(&attr).cloned().unwrap_or(Value::Null);
            if val.is_null() {
                map.remove(&attr);
            } else {
                map.insert(attr.clone(), val);
            }
            Ok(json!({"type": "set", "attr": attr, "val": prev}))
        }
        "assign" => {
            let value = op
                .get("value")
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("assign without :value"))?;
            let mut prev = Map::new();
            for (k, v) in value {
                prev.insert(k.clone(), map.get(k).cloned().unwrap_or(Value::Null));
                map.insert(k.clone(), v.clone());
            }
            Ok(json!({"type": "assign", "value": Value::Object(prev)}))
        }
        "unassign" => {
            let keys = op
                .get("keys")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("unassign without :keys"))?;
            let mut prev = Map::new();
            for k in keys {
                if let Some(name) = k.as_str() {
                    if let Some(v) = map.remove(name) {
                        prev.insert(name.to_string(), v);
                    }
                }
            }
            Ok(json!({"type": "assign", "value": Value::Object(prev)}))
        }
        "set-touched" => {
            let prev = map.get("touched").cloned().unwrap_or(json!([]));
            map.insert(
                "touched".into(),
                op.get("touched").cloned().unwrap_or(json!([])),
            );
            Ok(json!({"type": "set-touched", "touched": prev}))
        }
        other => bail!("unknown mod-obj op: {other}"),
    }
}

fn del_obj(file: &mut Value, ch: &Value) -> Result<Value> {
    let page_id = require_id(ch, "pageId")?.to_string();
    let id = require_id(ch, "id")?.to_string();
    let objects = page_objects_mut(file, &page_id)?;
    let removed = objects
        .remove(&id)
        .ok_or_else(|| anyhow!("obj {id} not found"))?;
    let parent_id = removed
        .get("parentId")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_string();
    if let Some(parent) = objects.get_mut(&parent_id) {
        if let Some(shapes) = parent.get_mut("shapes").and_then(Value::as_array_mut) {
            shapes.retain(|v| v.as_str() != Some(&id));
        }
    }
    Ok(json!({
        "type": "add-obj",
        "pageId": page_id,
        "id": id,
        "parentId": parent_id,
        "obj": removed
    }))
}

fn mov_objects(file: &mut Value, ch: &Value) -> Result<Value> {
    let page_id = require_id(ch, "pageId")?.to_string();
    let parent_id = require_id(ch, "parentId")?.to_string();
    let shapes = ch
        .get("shapes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("mov-objects without :shapes"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect::<Vec<_>>();
    let index = ch.get("index").and_then(Value::as_u64).map(|i| i as usize);

    let objects = page_objects_mut(file, &page_id)?;

    // Remember previous parents for undo.
    let mut prev_parents: Vec<(String, String)> = Vec::with_capacity(shapes.len());
    for sid in &shapes {
        if let Some(obj) = objects.get(sid) {
            let prev = obj
                .get("parentId")
                .and_then(Value::as_str)
                .unwrap_or(&parent_id)
                .to_string();
            prev_parents.push((sid.clone(), prev));
        }
    }

    // Detach from old parents
    for (sid, prev_parent) in &prev_parents {
        if let Some(parent) = objects.get_mut(prev_parent) {
            if let Some(arr) = parent.get_mut("shapes").and_then(Value::as_array_mut) {
                arr.retain(|v| v.as_str() != Some(sid));
            }
        }
    }

    // Reparent + reattach
    for sid in &shapes {
        if let Some(obj) = objects.get_mut(sid) {
            if let Some(map) = obj.as_object_mut() {
                map.insert("parentId".into(), json!(parent_id));
                map.insert("frameId".into(), json!(parent_id));
            }
        }
    }
    if let Some(parent) = objects.get_mut(&parent_id) {
        let arr = parent
            .get_mut("shapes")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow!("parent has no :shapes"))?;
        match index {
            Some(i) => {
                let target = i.min(arr.len());
                for (offset, sid) in shapes.iter().enumerate() {
                    arr.insert(target + offset, json!(sid));
                }
            }
            None => {
                for sid in &shapes {
                    arr.push(json!(sid));
                }
            }
        }
    }

    Ok(json!({
        "type": "mov-objects-undo",
        "pageId": page_id,
        "parents": prev_parents
    }))
}

// ───────────────────────── Library assets ─────────────────────────

fn apply_lib_change(file: &mut Value, change_type: &str, ch: &Value) -> Result<Value> {
    // Layout: type prefix `add-`/`mod-`/`del-`, suffix is the asset bucket.
    let (verb, asset) = change_type
        .split_once('-')
        .ok_or_else(|| anyhow!("malformed change type {change_type}"))?;
    let bucket_key = match asset {
        "color" => "colors",
        "typography" => "typographies",
        "media" => "media",
        "component" => "components",
        other => bail!("unknown asset bucket {other}"),
    };
    let id = require_id(ch, "id")?.to_string();
    let bucket = ensure_object(file, bucket_key)?;
    match verb {
        "add" => {
            let payload = ch
                .get(asset)
                .cloned()
                .ok_or_else(|| anyhow!("{change_type} missing :{asset}"))?;
            bucket.insert(id.clone(), payload);
            Ok(json!({"type": format!("del-{asset}"), "id": id}))
        }
        "mod" => {
            let entry = bucket
                .get_mut(&id)
                .ok_or_else(|| anyhow!("{asset} {id} not found"))?;
            let prev = entry.clone();
            if let Some(payload) = ch.get(asset).cloned() {
                *entry = payload;
            }
            Ok(json!({"type": format!("mod-{asset}"), "id": id, asset: prev}))
        }
        "del" => {
            let prev = bucket
                .remove(&id)
                .ok_or_else(|| anyhow!("{asset} {id} not found"))?;
            Ok(json!({"type": format!("add-{asset}"), "id": id, asset: prev}))
        }
        other => bail!("unknown verb {other}"),
    }
}

// ───────────────────────── Tokens ─────────────────────────

fn apply_token_change(file: &mut Value, change_type: &str, ch: &Value) -> Result<Value> {
    // Tokens live under file.data.tokensLib (camelCase form of `tokens-lib`).
    // For Phase 1 we round-trip the entire tokens-lib subtree on each write
    // — every change type carries enough payload to reconstruct.
    let lib = ensure_object(file, "tokensLib")?;
    let prev = Value::Object(lib.clone());
    if change_type == "set-tokens-lib" {
        if let Some(payload) = ch.get("tokensLib").cloned() {
            *lib = payload
                .as_object()
                .cloned()
                .unwrap_or_else(Map::new);
        }
        return Ok(json!({"type": "set-tokens-lib", "tokensLib": prev}));
    }
    // Generic mutate: drop the change body into a per-type log.
    let log = lib
        .entry("changes".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = log.as_array_mut() {
        arr.push(ch.clone());
    }
    Ok(json!({"type": "set-tokens-lib", "tokensLib": prev}))
}

// ───────────────────────── helpers ─────────────────────────

fn require_id<'a>(ch: &'a Value, key: &str) -> Result<&'a str> {
    ch.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing :{key}"))
}

fn ensure_object<'a>(file: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>> {
    let map = file
        .as_object_mut()
        .ok_or_else(|| anyhow!("file is not a map"))?;
    let entry = map
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    entry
        .as_object_mut()
        .ok_or_else(|| anyhow!("file.{key} is not an object"))
}

fn ensure_array<'a>(file: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>> {
    let map = file
        .as_object_mut()
        .ok_or_else(|| anyhow!("file is not a map"))?;
    let entry = map
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    entry
        .as_array_mut()
        .ok_or_else(|| anyhow!("file.{key} is not an array"))
}

fn page_objects_mut<'a>(
    file: &'a mut Value,
    page_id: &str,
) -> Result<&'a mut Map<String, Value>> {
    let pages_index = ensure_object(file, "pagesIndex")?;
    let page = pages_index
        .get_mut(page_id)
        .ok_or_else(|| anyhow!("page {page_id} not found"))?;
    let objects = page
        .get_mut("objects")
        .ok_or_else(|| anyhow!("page {page_id} has no :objects"))?;
    objects
        .as_object_mut()
        .ok_or_else(|| anyhow!("page.objects is not an object"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model;
    use uuid::Uuid;

    fn fresh_file() -> Value {
        let f = model::File::empty(Uuid::new_v4(), Uuid::new_v4(), "test");
        f.data
    }

    fn first_page_id(file: &Value) -> String {
        let pages_index = file.get("pagesIndex").unwrap().as_object().unwrap();
        pages_index.keys().next().unwrap().clone()
    }

    #[test]
    fn add_then_del_obj_round_trips() {
        let mut file = fresh_file();
        let page_id = first_page_id(&file);
        let new_id = Uuid::new_v4().to_string();
        let add = json!({
            "type": "add-obj",
            "id": new_id,
            "pageId": page_id,
            "parentId": Uuid::nil().to_string(),
            "frameId": Uuid::nil().to_string(),
            "obj": {
                "id": new_id,
                "type": "rect",
                "name": "Rect 1",
                "x": 0.0, "y": 0.0,
                "width": 100.0, "height": 50.0
            }
        });
        let undo = apply_changes(&mut file, &[add]).unwrap();
        let objects = file
            .pointer(&format!("/pagesIndex/{page_id}/objects"))
            .unwrap()
            .as_object()
            .unwrap();
        assert!(objects.contains_key(&new_id));
        // Replay undo
        apply_changes(&mut file, &undo).unwrap();
        let objects = file
            .pointer(&format!("/pagesIndex/{page_id}/objects"))
            .unwrap()
            .as_object()
            .unwrap();
        assert!(!objects.contains_key(&new_id));
    }

    #[test]
    fn mod_obj_set_round_trips() {
        let mut file = fresh_file();
        let page_id = first_page_id(&file);
        let id = Uuid::new_v4().to_string();
        apply_changes(
            &mut file,
            &[json!({
                "type": "add-obj",
                "id": id,
                "pageId": page_id,
                "parentId": Uuid::nil().to_string(),
                "obj": {"id": id, "type": "rect", "x": 0.0}
            })],
        )
        .unwrap();
        let mut undo = apply_changes(
            &mut file,
            &[json!({
                "type": "mod-obj",
                "pageId": page_id,
                "id": id,
                "operations": [
                    {"type": "set", "attr": "x", "val": 42.0}
                ]
            })],
        )
        .unwrap();
        let x = file
            .pointer(&format!("/pagesIndex/{page_id}/objects/{id}/x"))
            .and_then(Value::as_f64);
        assert_eq!(x, Some(42.0));
        // Replay undo
        apply_changes(&mut file, &undo).unwrap();
        let x = file
            .pointer(&format!("/pagesIndex/{page_id}/objects/{id}/x"))
            .and_then(Value::as_f64);
        assert_eq!(x, Some(0.0));
        undo.clear();
    }

    #[test]
    fn add_color_then_del_color() {
        let mut file = fresh_file();
        let id = Uuid::new_v4().to_string();
        let add = json!({
            "type": "add-color",
            "id": id,
            "color": {"id": id, "name": "Red", "color": "#FF0000"}
        });
        apply_changes(&mut file, &[add]).unwrap();
        let colors = file.pointer(&format!("/colors/{id}")).unwrap();
        assert_eq!(colors.get("name").and_then(Value::as_str), Some("Red"));
        apply_changes(&mut file, &[json!({"type": "del-color", "id": id})]).unwrap();
        assert!(file.pointer(&format!("/colors/{id}")).is_none());
    }

    #[test]
    fn unknown_change_does_not_panic() {
        let mut file = fresh_file();
        let undo = apply_changes(
            &mut file,
            &[json!({"type": "fancy-future-change", "data": "ignored"})],
        )
        .unwrap();
        assert_eq!(undo.len(), 1);
    }
}

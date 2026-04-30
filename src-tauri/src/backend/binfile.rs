//! `.penpot` archive (binfile-v3) reader/writer.
//!
//! Format: ZIP container with a top-level `manifest.json` (transit+JSON) and
//! per-file directories `files/<file-id>/` containing `file.json`,
//! `pages/<page-id>.json`, and `media/<media-id>` (raw bytes).
//!
//! The legacy `binfile-v1` (`PNPT` magic + Fressian) is detected and
//! rejected with a clear error — Fressian has no Rust implementation. The
//! pre-2023 `legacy-zip` format is also detected; supporting it is Phase 6
//! work because it requires running the file-data migration chain.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, Write};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::ZipArchive;

use super::model::{self, File};
use super::transit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    LegacyZip,
    BinfileV1,
    BinfileV3,
}

/// Detect the `.penpot` format from the first few bytes + manifest.
pub fn detect<R: Read + Seek>(reader: &mut R) -> Result<Format> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).context("reading magic bytes")?;
    reader.rewind()?;
    if &magic == b"PNPT" {
        return Ok(Format::BinfileV1);
    }
    if &magic[..2] != b"PK" {
        bail!("not a .penpot file (bad magic)");
    }
    let mut archive = ZipArchive::new(reader)?;
    if let Ok(mut entry) = archive.by_name("manifest.json") {
        let mut content = String::new();
        entry.read_to_string(&mut content).ok();
        if content.contains("penpot/export-files") || content.contains("export-files") {
            return Ok(Format::BinfileV3);
        }
    }
    Ok(Format::LegacyZip)
}

/// Parsed `.penpot` archive contents (binfile-v3).
#[derive(Debug, Clone)]
pub struct PenpotImport {
    pub files: Vec<ImportedFile>,
}

#[derive(Debug, Clone)]
pub struct ImportedFile {
    pub id: Uuid,
    pub name: String,
    pub features: Vec<String>,
    pub version: u32,
    pub data: Value,
    pub media: HashMap<String, Vec<u8>>,
}

/// Import a `.penpot` archive. Returns one `ImportedFile` per top-level file
/// recorded in the manifest.
pub fn import_binfile_v3<R: Read + Seek>(reader: R) -> Result<PenpotImport> {
    let mut archive = ZipArchive::new(reader).context("opening ZIP archive")?;
    let mut transit_reader = transit::Reader::new();

    let manifest_str = read_string(&mut archive, "manifest.json")
        .context("reading manifest.json")?;
    let manifest_value = transit_reader
        .read(&manifest_str)
        .context("decoding manifest.json (transit)")?;
    let manifest_json = model::camelize(manifest_value.to_json());

    let manifest_files = manifest_json
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if manifest_files.is_empty() {
        bail!("manifest has no :files entries");
    }

    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_string()))
        .collect();

    let mut imported = Vec::with_capacity(manifest_files.len());

    for entry in manifest_files {
        let id_str = entry
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("manifest file entry missing :id"))?
            .to_string();
        let id = Uuid::parse_str(&id_str)
            .with_context(|| format!("parsing file id {id_str}"))?;
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Untitled")
            .to_string();
        let features = entry
            .get("features")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let version = entry
            .get("version")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(model::FILE_DATA_VERSION);

        let prefix = format!("files/{id_str}/");

        // file.json carries options/components/colors/typographies/media.
        let file_meta_str = read_string(&mut archive, &format!("{prefix}file.json"))
            .with_context(|| format!("reading {prefix}file.json"))?;
        let file_meta_value = transit_reader.read(&file_meta_str)?;
        let file_meta = model::camelize(file_meta_value.to_json());

        let mut data = file_meta.clone();
        // Normalize: ensure keys exist so the change applier never has to deal
        // with missing buckets.
        if let Some(map) = data.as_object_mut() {
            map.entry("components".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            map.entry("colors".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            map.entry("typographies".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            map.entry("media".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            map.entry("pagesIndex".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            map.entry("pages".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            map.insert("version".into(), json!(version));
        }

        // Read all pages in this file.
        let mut page_ids: Vec<String> = Vec::new();
        for n in &names {
            if let Some(rest) = n.strip_prefix(&format!("{prefix}pages/")) {
                if let Some(stem) = rest.strip_suffix(".json") {
                    if Uuid::parse_str(stem).is_ok() {
                        let raw = read_string(&mut archive, n)
                            .with_context(|| format!("reading {n}"))?;
                        let pv = transit_reader.read(&raw)?;
                        let mut page_json = model::camelize(pv.to_json());
                        // Make sure the page has its own id field.
                        if let Some(map) = page_json.as_object_mut() {
                            map.entry("id".to_string())
                                .or_insert_with(|| Value::String(stem.to_string()));
                            map.entry("objects".to_string())
                                .or_insert_with(|| Value::Object(Map::new()));
                            map.entry("options".to_string())
                                .or_insert_with(|| Value::Object(Map::new()));
                        }
                        if let Some(map) = data.as_object_mut() {
                            let pages_index = map
                                .entry("pagesIndex".to_string())
                                .or_insert_with(|| Value::Object(Map::new()))
                                .as_object_mut()
                                .unwrap();
                            pages_index.insert(stem.to_string(), page_json);
                        }
                        page_ids.push(stem.to_string());
                    }
                }
            }
        }
        // If the manifest didn't pre-populate `pages` ordering, infer it now.
        if let Some(pages) = data
            .get_mut("pages")
            .and_then(Value::as_array_mut)
        {
            if pages.is_empty() {
                for pid in &page_ids {
                    pages.push(Value::String(pid.clone()));
                }
            }
        }

        // Read media bytes — keyed by storage-object id (uuid), value = bytes.
        let mut media: HashMap<String, Vec<u8>> = HashMap::new();
        for n in &names {
            if let Some(rest) = n.strip_prefix(&format!("{prefix}media/")) {
                let stem = rest.split('.').next().unwrap_or(rest).to_string();
                let mut entry = archive.by_name(n)?;
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf)?;
                media.insert(stem, buf);
            }
        }

        imported.push(ImportedFile {
            id,
            name,
            features,
            version,
            data,
            media,
        });
    }

    Ok(PenpotImport { files: imported })
}

/// Convert an imported file into a [`File`] under the given project.
pub fn imported_to_file(imp: ImportedFile, project_id: Uuid) -> File {
    let now = Utc::now();
    File {
        id: imp.id,
        project_id,
        name: imp.name,
        revn: 0,
        vern: 0,
        version: imp.version,
        is_shared: false,
        features: imp.features,
        created_at: now,
        modified_at: now,
        data: imp.data,
    }
}

/// Serialize a [`File`] into a binfile-v3 ZIP. `media_provider` resolves
/// media-storage IDs (UUIDs referenced from `data.media[*].id`) to raw
/// bytes; pass `|_| None` if you don't have media to export.
pub fn export_binfile_v3<W: Write + Seek>(
    writer: W,
    file: &File,
    media_provider: impl Fn(&str) -> Option<Vec<u8>>,
) -> Result<()> {
    let mut zip = zip::ZipWriter::new(writer);
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    // ── manifest.json
    let manifest = build_manifest(file);
    write_transit(&mut zip, "manifest.json", &manifest, opts)?;

    let prefix = format!("files/{}/", file.id);

    // ── files/<id>/file.json — meta + non-page assets
    let meta = file_meta(file);
    write_transit(&mut zip, &format!("{prefix}file.json"), &meta, opts)?;

    // ── files/<id>/pages/<page-id>.json
    if let Some(pages_index) = file.data.get("pagesIndex").and_then(Value::as_object) {
        for (page_id, page) in pages_index {
            write_transit(
                &mut zip,
                &format!("{prefix}pages/{page_id}.json"),
                page,
                opts,
            )?;
        }
    }

    // ── files/<id>/media/<storage-id>
    if let Some(media_index) = file.data.get("media").and_then(Value::as_object) {
        for (_asset_id, m) in media_index {
            let storage_id = m
                .get("id")
                .and_then(Value::as_str)
                .map(String::from);
            let mtype = m
                .get("mtype")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            let ext = mime_to_ext(mtype);
            if let Some(sid) = storage_id {
                if let Some(bytes) = media_provider(&sid) {
                    let entry_name = if ext.is_empty() {
                        format!("{prefix}media/{sid}")
                    } else {
                        format!("{prefix}media/{sid}.{ext}")
                    };
                    zip.start_file(&entry_name, opts)?;
                    zip.write_all(&bytes)?;
                }
            }
        }
    }

    zip.finish()?;
    Ok(())
}

/// In-memory variant of [`export_binfile_v3`] — useful for Tauri commands
/// that hand bytes to the frontend.
pub fn export_to_bytes(
    file: &File,
    media_provider: impl Fn(&str) -> Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    export_binfile_v3(&mut buf, file, media_provider)?;
    Ok(buf.into_inner())
}

fn write_transit<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    name: &str,
    value: &Value,
    opts: SimpleFileOptions,
) -> Result<()> {
    let transit = model::json_to_transit(value);
    zip.start_file(name, opts)?;
    zip.write_all(transit.as_bytes())?;
    Ok(())
}

fn build_manifest(file: &File) -> Value {
    json!({
        "type": "penpot/export-files",
        "version": 1,
        "format": "binfile-v3",
        "generatedBy": "penpot-desktop",
        "generatedAt": Utc::now().to_rfc3339(),
        "files": [
            {
                "id": file.id.to_string(),
                "name": file.name,
                "features": file.features,
                "version": file.version,
            }
        ],
        "relations": []
    })
}

/// Build the file.json payload — everything in `data` except the per-page
/// `pagesIndex` (those are emitted as separate ZIP entries).
fn file_meta(file: &File) -> Value {
    let mut meta = match &file.data {
        Value::Object(map) => Value::Object(map.clone()),
        _ => Value::Object(Map::new()),
    };
    if let Some(map) = meta.as_object_mut() {
        map.remove("pagesIndex");
        map.insert("id".into(), json!(file.id));
        map.insert("name".into(), json!(file.name));
        map.insert("revn".into(), json!(file.revn));
        map.insert("vern".into(), json!(file.vern));
        map.insert("version".into(), json!(file.version));
        map.insert("features".into(), json!(file.features));
    }
    meta
}

fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/avif" => "avif",
        "font/ttf" | "application/x-font-ttf" => "ttf",
        "font/otf" => "otf",
        "font/woff" => "woff",
        "font/woff2" => "woff2",
        _ => "",
    }
}

fn read_string<R: Read + Seek>(archive: &mut ZipArchive<R>, name: &str) -> Result<String> {
    let mut entry = archive
        .by_name(name)
        .with_context(|| format!("entry {name} missing"))?;
    let mut s = String::new();
    entry.read_to_string(&mut s)?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::File;

    #[test]
    fn round_trip_empty_file() {
        let f = File::empty(Uuid::new_v4(), Uuid::new_v4(), "Test File");
        let bytes = export_to_bytes(&f, |_| None).unwrap();
        // Detect should return BinfileV3.
        let mut cursor = Cursor::new(&bytes);
        let fmt = detect(&mut cursor).unwrap();
        assert_eq!(fmt, Format::BinfileV3);

        let cursor = Cursor::new(bytes);
        let imp = import_binfile_v3(cursor).unwrap();
        assert_eq!(imp.files.len(), 1);
        assert_eq!(imp.files[0].id, f.id);
        assert_eq!(imp.files[0].name, "Test File");
    }

    #[test]
    fn round_trip_preserves_pages() {
        let mut f = File::empty(Uuid::new_v4(), Uuid::new_v4(), "Multi-Page");
        // Add a second page directly to the data tree.
        let extra_page_id = Uuid::new_v4().to_string();
        if let Some(pages_index) = f
            .data
            .get_mut("pagesIndex")
            .and_then(|v| v.as_object_mut())
        {
            pages_index.insert(
                extra_page_id.clone(),
                serde_json::json!({
                    "id": extra_page_id,
                    "name": "Page 2",
                    "objects": {
                        Uuid::nil().to_string(): {
                            "id": Uuid::nil().to_string(),
                            "type": "frame",
                            "shapes": []
                        }
                    },
                    "options": {}
                }),
            );
        }
        if let Some(pages) = f
            .data
            .get_mut("pages")
            .and_then(|v| v.as_array_mut())
        {
            pages.push(serde_json::json!(extra_page_id));
        }
        let bytes = export_to_bytes(&f, |_| None).unwrap();
        let imp = import_binfile_v3(Cursor::new(bytes)).unwrap();
        let imported = &imp.files[0];
        let pages_index = imported
            .data
            .get("pagesIndex")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(pages_index.len(), 2);
        assert!(pages_index.contains_key(&extra_page_id));
    }
}

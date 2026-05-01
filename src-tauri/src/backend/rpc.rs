//! Penpot RPC dispatcher for offline mode.
//!
//! Implements the subset of `/api/rpc/command/<name>` and
//! `/api/rpc/query/<name>` calls needed to boot the Penpot dashboard and
//! the workspace editor against an in-memory file store.
//!
//! Wire format: plain JSON in both directions. We rely on the
//! `enable-transit-readable-response` Penpot flag (set in
//! [`super::flags::OFFLINE_FLAGS`]) so the frontend will accept JSON.
//!
//! Unknown commands return `null` rather than 404 — the Penpot frontend
//! tolerates empty/missing data but throws on network errors.

use anyhow::Result;
use serde_json::{json, Value};
use uuid::Uuid;

use super::model::{
    self, local_profile_json, FILE_DATA_VERSION, LOCAL_PROFILE_ID, LOCAL_PROJECT_ID, LOCAL_TEAM_ID,
};
use super::store::{Store, UpdateOutcome, UpdateRequest};

/// Kind of RPC the request maps to.
#[derive(Debug, Clone, Copy)]
pub enum RpcKind {
    Command,
    Query,
}

/// Outcome of dispatching an RPC.
pub enum RpcResponse {
    Json(Value),
    /// Map a 4xx error back to the frontend.
    Error {
        status: u16,
        message: String,
    },
}

impl RpcResponse {
    pub fn ok(value: Value) -> Self {
        RpcResponse::Json(value)
    }

    pub fn null() -> Self {
        RpcResponse::Json(Value::Null)
    }
}

pub struct Backend {
    pub store: Store,
    pub language: String,
}

impl Backend {
    pub fn new(store: Store, language: String) -> Self {
        Self { store, language }
    }

    /// Dispatch an RPC by name. `body` is the JSON request body (already
    /// parsed); `kind` differentiates command vs. query (mostly cosmetic).
    pub fn dispatch(&self, kind: RpcKind, name: &str, body: &Value) -> RpcResponse {
        match self.dispatch_inner(kind, name, body) {
            Ok(resp) => resp,
            Err(e) => RpcResponse::Error {
                status: 500,
                message: e.to_string(),
            },
        }
    }

    fn dispatch_inner(&self, _kind: RpcKind, name: &str, body: &Value) -> Result<RpcResponse> {
        let resp = match name {
            // ── Auth / profile
            "get-profile" => RpcResponse::ok(local_profile_json(&self.language)),
            "login-with-password" | "login-with-passcode" | "login-with-token" => {
                RpcResponse::ok(local_profile_json(&self.language))
            }
            "logout" => RpcResponse::null(),
            "request-profile-recovery" | "recover-profile" => RpcResponse::null(),
            "update-profile" | "update-profile-photo" | "delete-profile" => {
                RpcResponse::ok(local_profile_json(&self.language))
            }

            // ── Teams
            "get-teams" => RpcResponse::ok(self.list_teams()),
            "get-team" => RpcResponse::ok(self.get_team(body)),
            "get-team-members" => RpcResponse::ok(json!([self.member_payload()])),
            "get-team-users" => RpcResponse::ok(json!([self.member_payload()])),
            "get-team-stats" => RpcResponse::ok(json!({"projects": 1, "files": 0})),
            "get-team-invitations" => RpcResponse::ok(json!([])),
            "get-team-shared-files" => RpcResponse::ok(json!([])),
            "get-team-recent-files" => RpcResponse::ok(json!([])),

            // ── Projects
            "get-projects" => RpcResponse::ok(self.list_projects(body)),
            "get-all-projects" => RpcResponse::ok(self.list_all_projects()),
            "get-project" => RpcResponse::ok(self.get_project(body)),

            // ── Files
            "get-project-files" => RpcResponse::ok(self.list_project_files(body)),
            "get-file" => self.get_file(body),
            "get-file-summary" => self.get_file_summary(body),
            "get-file-fragment" => RpcResponse::null(),
            "get-file-libraries" => RpcResponse::ok(json!([])),
            "get-file-object-thumbnails" => RpcResponse::ok(json!({})),
            "get-file-thumbnail" => RpcResponse::null(),
            "get-page" => self.get_page(body),
            "create-file" => self.create_file(body)?,
            "rename-file" => self.rename_file(body)?,
            "delete-file" => self.delete_file(body)?,
            "set-file-shared" => self.set_file_shared(body)?,
            "update-file" => self.update_file(body)?,
            "persist-temp-file" => self.update_file(body)?,

            // ── Snapshots / version history
            "get-file-snapshots" => self.get_file_snapshots(body),
            "take-file-snapshot" => self.take_file_snapshot(body)?,
            "restore-file-snapshot" => self.restore_file_snapshot(body)?,
            "update-file-snapshot" => self.update_file_snapshot(body)?,
            "remove-file-snapshot" => self.remove_file_snapshot(body)?,

            // ── Comments / fonts / plugins / templates
            "get-comment-threads" | "get-unread-comment-threads" => RpcResponse::ok(json!([])),
            "get-comments" => RpcResponse::ok(json!([])),
            "get-font-variants" => RpcResponse::ok(json!([])),
            "get-team-plugins" => RpcResponse::ok(json!([])),
            "get-builtin-templates" => RpcResponse::ok(json!([])),

            // ── Webhooks / audit / access tokens / fonts (no-ops)
            "get-webhooks"
            | "get-access-tokens"
            | "push-audit-events"
            | "get-presence-on-file" => RpcResponse::ok(json!([])),

            // ── Catch-all: tolerate anything else with `null`
            other => {
                eprintln!("[backend/rpc] unhandled command: {other}");
                RpcResponse::null()
            }
        };
        Ok(resp)
    }

    // ──────────────── Teams / Projects / Members ────────────────

    fn list_teams(&self) -> Value {
        Value::Array(
            self.store
                .list_teams()
                .into_iter()
                .map(|t| self.team_payload(&t))
                .collect(),
        )
    }

    fn get_team(&self, body: &Value) -> Value {
        let id = body
            .get("id")
            .and_then(parse_uuid_field)
            .unwrap_or(LOCAL_TEAM_ID);
        match self.store.get_team(id) {
            Some(t) => self.team_payload(&t),
            None => Value::Null,
        }
    }

    fn team_payload(&self, t: &model::Team) -> Value {
        json!({
            "id": t.id,
            "name": t.name,
            "isDefault": t.is_default,
            "permissions": {"type": "owner", "isOwner": true, "isAdmin": true, "canEdit": true},
            "features": t.features,
            "createdAt": t.created_at,
            "modifiedAt": t.modified_at,
        })
    }

    fn member_payload(&self) -> Value {
        json!({
            "id": LOCAL_PROFILE_ID,
            "email": "local@penpot-desktop.local",
            "fullname": "Local User",
            "isActive": true,
            "isMuted": false,
            "isOwner": true,
            "isAdmin": true,
            "canEdit": true,
        })
    }

    fn list_projects(&self, body: &Value) -> Value {
        let team_id = body
            .get("teamId")
            .and_then(parse_uuid_field)
            .unwrap_or(LOCAL_TEAM_ID);
        Value::Array(
            self.store
                .list_projects(team_id)
                .into_iter()
                .map(|p| self.project_payload(&p))
                .collect(),
        )
    }

    fn list_all_projects(&self) -> Value {
        Value::Array(
            self.store
                .list_all_projects()
                .into_iter()
                .map(|p| self.project_payload(&p))
                .collect(),
        )
    }

    fn get_project(&self, body: &Value) -> Value {
        let id = body
            .get("id")
            .and_then(parse_uuid_field)
            .unwrap_or(LOCAL_PROJECT_ID);
        match self
            .store
            .list_all_projects()
            .into_iter()
            .find(|p| p.id == id)
        {
            Some(p) => self.project_payload(&p),
            None => Value::Null,
        }
    }

    fn project_payload(&self, p: &model::Project) -> Value {
        json!({
            "id": p.id,
            "teamId": p.team_id,
            "name": p.name,
            "isDefault": p.is_default,
            "isPinned": p.is_pinned,
            "createdAt": p.created_at,
            "modifiedAt": p.modified_at,
        })
    }

    // ──────────────── Files ────────────────

    fn list_project_files(&self, body: &Value) -> Value {
        let project_id = body
            .get("projectId")
            .and_then(parse_uuid_field)
            .unwrap_or(LOCAL_PROJECT_ID);
        Value::Array(
            self.store
                .list_project_files(project_id)
                .into_iter()
                .map(|f| self.file_summary(&f))
                .collect(),
        )
    }

    fn file_summary(&self, f: &model::File) -> Value {
        json!({
            "id": f.id,
            "projectId": f.project_id,
            "name": f.name,
            "revn": f.revn,
            "vern": f.vern,
            "isShared": f.is_shared,
            "features": f.features,
            "createdAt": f.created_at,
            "modifiedAt": f.modified_at,
        })
    }

    fn get_file(&self, body: &Value) -> RpcResponse {
        let id = match body.get("id").and_then(parse_uuid_field) {
            Some(u) => u,
            None => {
                return RpcResponse::Error {
                    status: 400,
                    message: "missing :id".into(),
                };
            }
        };
        match self.store.get_file(id) {
            Some(f) => RpcResponse::ok(self.full_file_payload(&f)),
            None => RpcResponse::Error {
                status: 404,
                message: format!("file {id} not found"),
            },
        }
    }

    fn get_file_summary(&self, body: &Value) -> RpcResponse {
        let id = match body.get("id").and_then(parse_uuid_field) {
            Some(u) => u,
            None => return RpcResponse::null(),
        };
        match self.store.get_file(id) {
            Some(f) => RpcResponse::ok(self.file_summary(&f)),
            None => RpcResponse::null(),
        }
    }

    fn get_page(&self, body: &Value) -> RpcResponse {
        let file_id = match body.get("fileId").and_then(parse_uuid_field) {
            Some(u) => u,
            None => return RpcResponse::null(),
        };
        let page_id = match body.get("pageId").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return RpcResponse::null(),
        };
        match self.store.get_file(file_id) {
            Some(f) => {
                let pages_index = f
                    .data
                    .get("pagesIndex")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let page = pages_index
                    .get(&page_id)
                    .cloned()
                    .unwrap_or(Value::Null);
                RpcResponse::ok(page)
            }
            None => RpcResponse::null(),
        }
    }

    fn full_file_payload(&self, f: &model::File) -> Value {
        json!({
            "id": f.id,
            "projectId": f.project_id,
            "name": f.name,
            "revn": f.revn,
            "vern": f.vern,
            "version": f.version,
            "isShared": f.is_shared,
            "features": f.features,
            "createdAt": f.created_at,
            "modifiedAt": f.modified_at,
            "data": f.data,
        })
    }

    fn create_file(&self, body: &Value) -> Result<RpcResponse> {
        let id = body
            .get("id")
            .and_then(parse_uuid_field)
            .unwrap_or_else(Uuid::new_v4);
        let project_id = body
            .get("projectId")
            .and_then(parse_uuid_field)
            .unwrap_or(LOCAL_PROJECT_ID);
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Untitled")
            .to_string();
        let mut file = model::File::empty(id, project_id, &name);
        file.version = FILE_DATA_VERSION;
        self.store.put_file(file.clone());
        Ok(RpcResponse::ok(self.full_file_payload(&file)))
    }

    fn rename_file(&self, body: &Value) -> Result<RpcResponse> {
        let id = body
            .get("id")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :id"))?;
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing :name"))?
            .to_string();
        self.store.with_file_mut(id, |f| {
            f.name = name.clone();
        });
        Ok(RpcResponse::ok(json!({"id": id, "name": name})))
    }

    fn delete_file(&self, body: &Value) -> Result<RpcResponse> {
        let id = body
            .get("id")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :id"))?;
        self.store.delete_file(id);
        Ok(RpcResponse::ok(json!({"id": id})))
    }

    fn set_file_shared(&self, body: &Value) -> Result<RpcResponse> {
        let id = body
            .get("id")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :id"))?;
        let shared = body
            .get("isShared")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.store.with_file_mut(id, |f| {
            f.is_shared = shared;
        });
        Ok(RpcResponse::ok(json!({"id": id, "isShared": shared})))
    }

    /// `update-file` — apply a vector of changes, bump revn, record the
    /// change log entry, return the new revn back to the editor.
    /// Delegates to [`Store::apply_update`] so the SQLite backend can wrap
    /// the whole pipeline in one transaction.
    fn update_file(&self, body: &Value) -> Result<RpcResponse> {
        let id = body
            .get("id")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :id"))?;
        let client_revn = body.get("revn").and_then(Value::as_i64).unwrap_or(0);
        let session_id = body
            .get("sessionId")
            .and_then(parse_uuid_field)
            .unwrap_or_else(Uuid::new_v4);
        let changes = body
            .get("changes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let outcome = self.store.apply_update(UpdateRequest {
            file_id: id,
            client_revn,
            session_id,
            changes,
        });

        match outcome {
            UpdateOutcome::Applied { new_revn, vern } => Ok(RpcResponse::ok(json!({
                "revn": new_revn,
                "vern": vern,
                "lagged": false,
                "changes": []
            }))),
            UpdateOutcome::Lagged { server_revn, missed } => Ok(RpcResponse::ok(json!({
                "revn": server_revn,
                "vern": 0,
                "lagged": true,
                "changes": missed,
            }))),
            UpdateOutcome::NotFound => Ok(RpcResponse::Error {
                status: 404,
                message: format!("file {id} not found"),
            }),
            UpdateOutcome::Error(message) => Ok(RpcResponse::Error {
                status: 400,
                message,
            }),
        }
    }

    // ──────────────── Snapshots / version history ────────────────

    fn get_file_snapshots(&self, body: &Value) -> RpcResponse {
        let file_id = match body.get("fileId").and_then(parse_uuid_field) {
            Some(u) => u,
            None => return RpcResponse::ok(json!([])),
        };
        let snapshots = self.store.list_snapshots(file_id);
        let payload: Vec<Value> = snapshots.iter().map(snapshot_payload).collect();
        RpcResponse::ok(Value::Array(payload))
    }

    fn take_file_snapshot(&self, body: &Value) -> Result<RpcResponse> {
        let file_id = body
            .get("fileId")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :file-id"))?;
        let label = body
            .get("label")
            .and_then(Value::as_str)
            .map(String::from);
        match self.store.take_snapshot(file_id, label) {
            Ok(s) => Ok(RpcResponse::ok(snapshot_payload(&s))),
            Err(e) => Ok(RpcResponse::Error {
                status: 400,
                message: e.to_string(),
            }),
        }
    }

    fn restore_file_snapshot(&self, body: &Value) -> Result<RpcResponse> {
        let file_id = body
            .get("fileId")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :file-id"))?;
        let snapshot_id = body
            .get("id")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :id"))?;
        match self.store.restore_snapshot(file_id, snapshot_id) {
            Ok(new_revn) => Ok(RpcResponse::ok(json!({
                "fileId": file_id,
                "id": snapshot_id,
                "revn": new_revn,
            }))),
            Err(e) => Ok(RpcResponse::Error {
                status: 400,
                message: e.to_string(),
            }),
        }
    }

    fn update_file_snapshot(&self, body: &Value) -> Result<RpcResponse> {
        let snapshot_id = body
            .get("id")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :id"))?;
        let label = body
            .get("label")
            .and_then(Value::as_str)
            .map(String::from);
        match self.store.rename_snapshot(snapshot_id, label) {
            Ok(()) => Ok(RpcResponse::ok(json!({"id": snapshot_id}))),
            Err(e) => Ok(RpcResponse::Error {
                status: 404,
                message: e.to_string(),
            }),
        }
    }

    fn remove_file_snapshot(&self, body: &Value) -> Result<RpcResponse> {
        let snapshot_id = body
            .get("id")
            .and_then(parse_uuid_field)
            .ok_or_else(|| anyhow::anyhow!("missing :id"))?;
        self.store.delete_snapshot(snapshot_id).ok();
        Ok(RpcResponse::ok(json!({"id": snapshot_id})))
    }
}

fn snapshot_payload(s: &model::Snapshot) -> Value {
    json!({
        "id": s.id,
        "fileId": s.file_id,
        "revn": s.revn,
        "label": s.label,
        "createdAt": s.created_at,
        "isAuto": s.is_auto(),
    })
}

/// Parse a UUID from a JSON value that may be either a plain string or a
/// `{"~uuid": "..."}` legacy form.
fn parse_uuid_field(v: &Value) -> Option<Uuid> {
    v.as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .or_else(|| {
            v.get("~uuid")
                .and_then(Value::as_str)
                .and_then(|s| Uuid::parse_str(s).ok())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::store::Store;

    fn fresh_backend() -> Backend {
        Backend::new(Store::seeded(), "en".into())
    }

    #[test]
    fn get_profile_returns_local() {
        let b = fresh_backend();
        let resp = b.dispatch(RpcKind::Command, "get-profile", &Value::Null);
        let v = match resp {
            RpcResponse::Json(v) => v,
            _ => panic!("expected JSON"),
        };
        assert_eq!(
            v.get("email").and_then(Value::as_str),
            Some("local@penpot-desktop.local")
        );
    }

    #[test]
    fn get_teams_lists_local_team() {
        let b = fresh_backend();
        let resp = b.dispatch(RpcKind::Command, "get-teams", &Value::Null);
        let v = match resp {
            RpcResponse::Json(v) => v,
            _ => panic!("expected JSON"),
        };
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0].get("isDefault").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn create_then_get_file() {
        let b = fresh_backend();
        let id = Uuid::new_v4();
        let _ = b.dispatch(
            RpcKind::Command,
            "create-file",
            &json!({"id": id.to_string(), "name": "Test"}),
        );
        let resp = b.dispatch(
            RpcKind::Command,
            "get-file",
            &json!({"id": id.to_string()}),
        );
        let v = match resp {
            RpcResponse::Json(v) => v,
            other => panic!("expected JSON, got {:?}", debug_resp(&other)),
        };
        assert_eq!(v.get("name").and_then(Value::as_str), Some("Test"));
    }

    #[test]
    fn update_file_bumps_revn_and_applies_changes() {
        let b = fresh_backend();
        let id = Uuid::new_v4();
        let _ = b.dispatch(
            RpcKind::Command,
            "create-file",
            &json!({"id": id.to_string(), "name": "F"}),
        );
        let file_v = match b.dispatch(
            RpcKind::Command,
            "get-file",
            &json!({"id": id.to_string()}),
        ) {
            RpcResponse::Json(v) => v,
            _ => panic!("get-file failed"),
        };
        let revn = file_v.get("revn").and_then(Value::as_i64).unwrap();
        let pages_index = file_v
            .get("data")
            .and_then(|d| d.get("pagesIndex"))
            .and_then(Value::as_object)
            .unwrap();
        let page_id = pages_index.keys().next().unwrap().clone();
        let new_obj_id = Uuid::new_v4().to_string();

        let resp = b.dispatch(
            RpcKind::Command,
            "update-file",
            &json!({
                "id": id.to_string(),
                "revn": revn,
                "sessionId": Uuid::new_v4().to_string(),
                "changes": [{
                    "type": "add-obj",
                    "id": new_obj_id,
                    "pageId": page_id,
                    "parentId": Uuid::nil().to_string(),
                    "frameId": Uuid::nil().to_string(),
                    "obj": {"id": new_obj_id, "type": "rect"}
                }]
            }),
        );
        let v = match resp {
            RpcResponse::Json(v) => v,
            _ => panic!("update-file failed"),
        };
        assert_eq!(v.get("revn").and_then(Value::as_i64), Some(revn + 1));
        assert_eq!(v.get("lagged").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn update_file_revn_mismatch_returns_lagged() {
        let b = fresh_backend();
        let id = Uuid::new_v4();
        let _ = b.dispatch(
            RpcKind::Command,
            "create-file",
            &json!({"id": id.to_string(), "name": "F"}),
        );
        // Send a stale revn — should come back as lagged.
        let resp = b.dispatch(
            RpcKind::Command,
            "update-file",
            &json!({
                "id": id.to_string(),
                "revn": 999,
                "sessionId": Uuid::new_v4().to_string(),
                "changes": []
            }),
        );
        let v = match resp {
            RpcResponse::Json(v) => v,
            _ => panic!("update-file failed"),
        };
        assert_eq!(v.get("lagged").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn unknown_command_returns_null_not_error() {
        let b = fresh_backend();
        let resp = b.dispatch(RpcKind::Command, "totally-fake-command", &json!({}));
        match resp {
            RpcResponse::Json(Value::Null) => {}
            other => panic!("expected null JSON, got {:?}", debug_resp(&other)),
        }
    }

    #[test]
    fn snapshot_take_list_restore_via_rpc() {
        let b = fresh_backend();
        let id = Uuid::new_v4();
        let _ = b.dispatch(
            RpcKind::Command,
            "create-file",
            &json!({"id": id.to_string(), "name": "F"}),
        );
        let take = b.dispatch(
            RpcKind::Command,
            "take-file-snapshot",
            &json!({"fileId": id.to_string(), "label": "v1"}),
        );
        let snap = match take {
            RpcResponse::Json(v) => v,
            _ => panic!("take-file-snapshot failed"),
        };
        let snap_id = snap.get("id").and_then(Value::as_str).unwrap().to_string();
        assert_eq!(snap.get("label").and_then(Value::as_str), Some("v1"));

        let listed = b.dispatch(
            RpcKind::Command,
            "get-file-snapshots",
            &json!({"fileId": id.to_string()}),
        );
        let arr = match listed {
            RpcResponse::Json(Value::Array(a)) => a,
            _ => panic!("get-file-snapshots failed"),
        };
        assert_eq!(arr.len(), 1);

        let restored = b.dispatch(
            RpcKind::Command,
            "restore-file-snapshot",
            &json!({"fileId": id.to_string(), "id": snap_id}),
        );
        match restored {
            RpcResponse::Json(v) => {
                // Restoring a freshly-taken snapshot only bumps revn — no
                // data changes — but the response should still echo the
                // new revn so the editor reloads.
                assert_eq!(v.get("revn").and_then(Value::as_i64), Some(1));
            }
            _ => panic!("restore-file-snapshot failed"),
        }
    }

    fn debug_resp(r: &RpcResponse) -> String {
        match r {
            RpcResponse::Json(_) => "json".into(),
            RpcResponse::Error { status, message } => format!("err {status}: {message}"),
        }
    }
}

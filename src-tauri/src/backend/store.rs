//! In-memory file/project/team store for offline mode.
//!
//! Phase 1 keeps everything RAM-resident. Phase 2 will swap this for SQLite
//! without changing the public API. The store is wrapped in `Arc<RwLock<>>`
//! so the proxy can serve concurrent reads while the change applier holds
//! a write lock during `update-file`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::Utc;
use uuid::Uuid;

use super::model::{File, Project, Team, LOCAL_PROJECT_ID, LOCAL_TEAM_ID};

#[derive(Default)]
struct Inner {
    teams: HashMap<Uuid, Team>,
    projects: HashMap<Uuid, Project>,
    files: HashMap<Uuid, File>,
    /// History of applied change vectors, latest first. Capped to keep
    /// memory bounded; for richer history use Phase 2 snapshots.
    change_log: HashMap<Uuid, Vec<ChangeRecord>>,
}

#[derive(Clone, Debug)]
pub struct ChangeRecord {
    pub revn: i64,
    pub session_id: Uuid,
    pub changes: serde_json::Value,
    pub undo: serde_json::Value,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<RwLock<Inner>>,
}

impl Store {
    /// Build a new store seeded with one default team + one Drafts project.
    /// The IDs are stable across launches so the saved frontend state always
    /// resolves to the same logical containers.
    pub fn seeded() -> Self {
        let store = Self::default();
        store.seed_defaults();
        store
    }

    fn seed_defaults(&self) {
        let mut inner = self.inner.write().unwrap();
        if !inner.teams.contains_key(&LOCAL_TEAM_ID) {
            inner.teams.insert(LOCAL_TEAM_ID, Team::local(LOCAL_TEAM_ID));
        }
        if !inner.projects.contains_key(&LOCAL_PROJECT_ID) {
            inner
                .projects
                .insert(LOCAL_PROJECT_ID, Project::local(LOCAL_PROJECT_ID, LOCAL_TEAM_ID));
        }
    }

    pub fn list_teams(&self) -> Vec<Team> {
        self.inner.read().unwrap().teams.values().cloned().collect()
    }

    pub fn get_team(&self, id: Uuid) -> Option<Team> {
        self.inner.read().unwrap().teams.get(&id).cloned()
    }

    pub fn list_projects(&self, team_id: Uuid) -> Vec<Project> {
        self.inner
            .read()
            .unwrap()
            .projects
            .values()
            .filter(|p| p.team_id == team_id)
            .cloned()
            .collect()
    }

    pub fn list_all_projects(&self) -> Vec<Project> {
        self.inner.read().unwrap().projects.values().cloned().collect()
    }

    pub fn list_project_files(&self, project_id: Uuid) -> Vec<File> {
        self.inner
            .read()
            .unwrap()
            .files
            .values()
            .filter(|f| f.project_id == project_id)
            .cloned()
            .collect()
    }

    pub fn get_file(&self, id: Uuid) -> Option<File> {
        self.inner.read().unwrap().files.get(&id).cloned()
    }

    pub fn put_file(&self, file: File) {
        let mut inner = self.inner.write().unwrap();
        inner.files.insert(file.id, file);
    }

    pub fn delete_file(&self, id: Uuid) -> Option<File> {
        let mut inner = self.inner.write().unwrap();
        inner.change_log.remove(&id);
        inner.files.remove(&id)
    }

    /// Mutate a file under a write lock. Returns whatever the closure returned;
    /// errors propagate after the lock is released.
    pub fn with_file_mut<F, R>(&self, id: Uuid, f: F) -> Option<R>
    where
        F: FnOnce(&mut File) -> R,
    {
        let mut inner = self.inner.write().unwrap();
        inner.files.get_mut(&id).map(|file| {
            let r = f(file);
            file.modified_at = Utc::now();
            r
        })
    }

    pub fn record_changes(
        &self,
        file_id: Uuid,
        record: ChangeRecord,
    ) {
        const HISTORY_CAP: usize = 200;
        let mut inner = self.inner.write().unwrap();
        let log = inner.change_log.entry(file_id).or_default();
        log.push(record);
        if log.len() > HISTORY_CAP {
            let drop = log.len() - HISTORY_CAP;
            log.drain(0..drop);
        }
    }

    pub fn change_log(&self, file_id: Uuid) -> Vec<ChangeRecord> {
        self.inner
            .read()
            .unwrap()
            .change_log
            .get(&file_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Reset the entire store — used by tests and the "switch mode" command.
    #[allow(dead_code)]
    pub fn reset(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.teams.clear();
        inner.projects.clear();
        inner.files.clear();
        inner.change_log.clear();
        drop(inner);
        self.seed_defaults();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_has_default_team_and_project() {
        let s = Store::seeded();
        let teams = s.list_teams();
        assert_eq!(teams.len(), 1);
        let projects = s.list_projects(teams[0].id);
        assert_eq!(projects.len(), 1);
    }

    #[test]
    fn put_and_fetch_file() {
        let s = Store::seeded();
        let f = File::empty(Uuid::new_v4(), LOCAL_PROJECT_ID, "test");
        let id = f.id;
        s.put_file(f);
        assert!(s.get_file(id).is_some());
    }
}

//! Slot storage.
//!
//! A [`SlotStore`] is one set of 9 slots (addressed 1..=9 to match the
//! hotkeys). A [`FolderStore`] holds many named folders, each with its own
//! independent `SlotStore`, plus a pointer to the active one. All hotkeys and
//! clear/undo actions operate on the **active** folder only.
//!
//! Folders are keyed by a numeric `id`, never by name, so renaming can't orphan
//! the active pointer or a pending undo.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::clipboard::{ClipSnapshot, SlotPreview};

/// Name given to the folder every install starts with.
pub const DEFAULT_FOLDER: &str = "Main";
/// Hard cap on a folder name. The UI ellipsizes well before this.
const MAX_NAME: usize = 24;

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredItem {
    pub snapshot: ClipSnapshot,
    pub preview: SlotPreview,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct SlotStore {
    items: [Option<StoredItem>; 9],
}

/// One slot as sent to the frontend.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SlotDto {
    pub index: usize,
    pub filled: bool,
    pub preview: Option<SlotPreview>,
}

impl SlotStore {
    pub fn set(&mut self, slot: usize, snapshot: ClipSnapshot, preview: SlotPreview) {
        if (1..=9).contains(&slot) {
            self.items[slot - 1] = Some(StoredItem { snapshot, preview });
        }
    }

    pub fn clear(&mut self, slot: usize) {
        if (1..=9).contains(&slot) {
            self.items[slot - 1] = None;
        }
    }

    pub fn clear_all(&mut self) {
        self.items = Default::default();
    }

    pub fn get_snapshot(&self, slot: usize) -> Option<ClipSnapshot> {
        if (1..=9).contains(&slot) {
            self.items[slot - 1].as_ref().map(|it| it.snapshot.clone())
        } else {
            None
        }
    }

    pub fn filled_count(&self) -> usize {
        self.items.iter().filter(|i| i.is_some()).count()
    }

    pub fn dtos(&self) -> Vec<SlotDto> {
        (1..=9)
            .map(|n| match &self.items[n - 1] {
                Some(it) => SlotDto {
                    index: n,
                    filled: true,
                    preview: Some(it.preview.clone()),
                },
                None => SlotDto {
                    index: n,
                    filled: false,
                    preview: None,
                },
            })
            .collect()
    }
}

// ---- Folders -----------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: u64,
    pub name: String,
    pub slots: SlotStore,
}

/// One folder as sent to the frontend / tray.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FolderDto {
    pub id: u64,
    pub name: String,
    /// How many of the folder's 9 slots are filled.
    pub filled: usize,
    pub active: bool,
    /// The home folder, which can't be renamed or deleted.
    pub permanent: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FolderStore {
    folders: Vec<Folder>,
    active: u64,
    next_id: u64,
}

impl Default for FolderStore {
    fn default() -> Self {
        Self {
            folders: vec![Folder {
                id: 1,
                name: DEFAULT_FOLDER.to_string(),
                slots: SlotStore::default(),
            }],
            active: 1,
            next_id: 2,
        }
    }
}

/// Trim, collapse internal whitespace, and cap length. Empty names fall back to
/// a generic label so a folder can never be nameless.
fn clean_name(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed: String = collapsed.chars().take(MAX_NAME).collect();
    if trimmed.is_empty() {
        "Folder".to_string()
    } else {
        trimmed
    }
}

impl FolderStore {
    // ---- Active-folder delegation ----
    //
    // These mirror `SlotStore`'s API so every existing call site just routes
    // through the active folder.

    fn active_index(&self) -> usize {
        self.folders
            .iter()
            .position(|f| f.id == self.active)
            .unwrap_or(0)
    }

    fn active_slots_mut(&mut self) -> &mut SlotStore {
        let i = self.active_index();
        &mut self.folders[i].slots
    }

    fn active_slots(&self) -> &SlotStore {
        &self.folders[self.active_index()].slots
    }

    pub fn set(&mut self, slot: usize, snapshot: ClipSnapshot, preview: SlotPreview) {
        self.active_slots_mut().set(slot, snapshot, preview);
    }

    pub fn clear(&mut self, slot: usize) {
        self.active_slots_mut().clear(slot);
    }

    pub fn clear_all(&mut self) {
        self.active_slots_mut().clear_all();
    }

    pub fn get_snapshot(&self, slot: usize) -> Option<ClipSnapshot> {
        self.active_slots().get_snapshot(slot)
    }

    pub fn dtos(&self) -> Vec<SlotDto> {
        self.active_slots().dtos()
    }

    // ---- Folder management ----

    pub fn active_id(&self) -> u64 {
        self.folders[self.active_index()].id
    }

    pub fn active_name(&self) -> String {
        self.folders[self.active_index()].name.clone()
    }

    /// A clone of the active folder's slots, for stashing before a clear.
    pub fn active_slots_clone(&self) -> SlotStore {
        self.active_slots().clone()
    }

    /// Overwrite one folder's slots wholesale. Used by undo. Returns false if
    /// the folder no longer exists (e.g. it was deleted after the clear).
    pub fn replace_slots(&mut self, id: u64, slots: SlotStore) -> bool {
        match self.folders.iter_mut().find(|f| f.id == id) {
            Some(f) => {
                f.slots = slots;
                true
            }
            None => false,
        }
    }

    /// Create a folder and make it active — you make one in order to use it.
    pub fn create(&mut self, name: &str) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.folders.push(Folder {
            id,
            name: clean_name(name),
            slots: SlotStore::default(),
        });
        self.active = id;
        id
    }

    /// The home folder. It is always the first in the list: `create` appends,
    /// and `delete` refuses to remove it, so index 0 never moves.
    pub fn permanent_id(&self) -> u64 {
        self.folders[0].id
    }

    pub fn rename(&mut self, id: u64, name: &str) -> bool {
        if id == self.permanent_id() {
            return false; // the home folder keeps its name
        }
        match self.folders.iter_mut().find(|f| f.id == id) {
            Some(f) => {
                f.name = clean_name(name);
                true
            }
            None => false,
        }
    }

    /// Delete a folder. Refuses to remove the home folder or the last one —
    /// there must always be somewhere for the hotkeys to land. If the active
    /// folder goes, the neighbour takes over.
    pub fn delete(&mut self, id: u64) -> bool {
        if id == self.permanent_id() || self.folders.len() <= 1 {
            return false;
        }
        let Some(pos) = self.folders.iter().position(|f| f.id == id) else {
            return false;
        };
        self.folders.remove(pos);
        if self.active == id {
            let next = pos.min(self.folders.len() - 1);
            self.active = self.folders[next].id;
        }
        true
    }

    pub fn select(&mut self, id: u64) -> bool {
        if self.folders.iter().any(|f| f.id == id) {
            self.active = id;
            true
        } else {
            false
        }
    }

    pub fn folder_dtos(&self) -> Vec<FolderDto> {
        let active = self.active_id();
        let permanent = self.permanent_id();
        self.folders
            .iter()
            .map(|f| FolderDto {
                id: f.id,
                name: f.name.clone(),
                filled: f.slots.filled_count(),
                active: f.id == active,
                permanent: f.id == permanent,
            })
            .collect()
    }

    // ---- Persistence ----

    /// Load the folder store, migrating a pre-folders `slots.bin` if that's all
    /// we find.
    ///
    /// The new format lives in its own file rather than versioning the old one:
    /// bincode isn't self-describing, so an old blob can half-decode as the new
    /// type and silently produce garbage. A distinct filename makes the choice
    /// unambiguous and leaves the old file in place as a fallback.
    pub fn load(path: &Path, legacy: &Path) -> Self {
        if let Some(store) = std::fs::read(path)
            .ok()
            .and_then(|bytes| bincode::deserialize::<FolderStore>(&bytes).ok())
        {
            return store.repaired();
        }
        // Migration: the user's existing 9 slots become the "Main" folder.
        if let Some(old) = std::fs::read(legacy)
            .ok()
            .and_then(|bytes| bincode::deserialize::<SlotStore>(&bytes).ok())
        {
            let mut store = FolderStore::default();
            store.folders[0].slots = old;
            return store;
        }
        FolderStore::default()
    }

    /// Guard against a corrupt or hand-edited file: there must be at least one
    /// folder, `active` must point at a real one, and `next_id` must not
    /// collide with an existing id.
    fn repaired(mut self) -> Self {
        if self.folders.is_empty() {
            return FolderStore::default();
        }
        if !self.folders.iter().any(|f| f.id == self.active) {
            self.active = self.folders[0].id;
        }
        let max_id = self.folders.iter().map(|f| f.id).max().unwrap_or(0);
        if self.next_id <= max_id {
            self.next_id = max_id + 1;
        }
        self
    }

    /// Persist to disk so folders and slots survive restarts. Best-effort.
    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = bincode::serialize(self) {
            let _ = std::fs::write(path, bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::ClipSnapshot;

    /// A minimal one-text snapshot.
    ///
    /// The two platforms model a clipboard differently — Windows as a flat map
    /// of numeric format ids, macOS as a list of items each holding string UTIs
    /// — so only this constructor is platform-specific. Every test below cares
    /// solely that a snapshot survives a store/load round trip, not what is in
    /// it, so the rest of the module stays shared.
    #[cfg(not(target_os = "macos"))]
    fn snap(text: &str) -> ClipSnapshot {
        use crate::clipboard::ClipFormat;
        ClipSnapshot {
            formats: vec![ClipFormat {
                id: 1,
                data: text.as_bytes().to_vec(),
            }],
        }
    }

    #[cfg(target_os = "macos")]
    fn snap(text: &str) -> ClipSnapshot {
        use crate::clipboard::{ClipItem, ClipType};
        ClipSnapshot {
            items: vec![ClipItem {
                types: vec![ClipType {
                    uti: "public.utf8-plain-text".into(),
                    data: text.as_bytes().to_vec(),
                }],
            }],
        }
    }

    /// The bytes [`snap`] stored, read back without the test needing to know
    /// which platform's snapshot shape they landed in.
    #[cfg(not(target_os = "macos"))]
    fn payload(snap: &ClipSnapshot) -> Vec<u8> {
        snap.formats[0].data.clone()
    }

    #[cfg(target_os = "macos")]
    fn payload(snap: &ClipSnapshot) -> Vec<u8> {
        snap.items[0].types[0].data.clone()
    }

    fn preview(text: &str) -> SlotPreview {
        SlotPreview {
            kind: "text".into(),
            text: Some(text.into()),
            files: vec![],
            bytes: text.len(),
            width: None,
            height: None,
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cq-paster-test-{name}"));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Write a file in the pre-folders format, as v0.4.x would have left it.
    fn save_legacy(store: &SlotStore, path: &Path) {
        std::fs::write(path, bincode::serialize(store).unwrap()).unwrap();
    }

    fn name_of(store: &FolderStore, id: u64) -> String {
        store
            .folder_dtos()
            .into_iter()
            .find(|f| f.id == id)
            .unwrap()
            .name
    }

    /// A pre-folders `slots.bin` must come back as the "Main" folder with every
    /// slot intact — losing a user's saved slots on upgrade is not acceptable.
    #[test]
    fn migrates_legacy_slots_into_main() {
        let legacy = tmp("legacy.bin");
        let modern = tmp("modern-missing.bin");

        let mut old = SlotStore::default();
        old.set(1, snap("first"), preview("first"));
        old.set(9, snap("ninth"), preview("ninth"));
        save_legacy(&old, &legacy);

        let store = FolderStore::load(&modern, &legacy);
        assert_eq!(store.folder_dtos().len(), 1);
        assert_eq!(store.active_name(), DEFAULT_FOLDER);
        assert_eq!(store.folder_dtos()[0].filled, 2);
        assert_eq!(payload(&store.get_snapshot(1).unwrap()), b"first");
        assert_eq!(payload(&store.get_snapshot(9).unwrap()), b"ninth");
        assert!(store.get_snapshot(5).is_none());
    }

    /// The new file wins; the legacy file is ignored once folders exist.
    #[test]
    fn prefers_modern_file_over_legacy() {
        let legacy = tmp("legacy2.bin");
        let modern = tmp("modern2.bin");

        let mut old = SlotStore::default();
        old.set(1, snap("stale"), preview("stale"));
        save_legacy(&old, &legacy);

        let mut store = FolderStore::default();
        store.set(1, snap("current"), preview("current"));
        store.save(&modern);

        let loaded = FolderStore::load(&modern, &legacy);
        assert_eq!(payload(&loaded.get_snapshot(1).unwrap()), b"current");
    }

    /// Folders must not share slots.
    #[test]
    fn folders_are_independent() {
        let mut store = FolderStore::default();
        let main = store.active_id();
        store.set(1, snap("in main"), preview("in main"));

        let work = store.create("Work"); // create also switches
        assert_eq!(store.active_id(), work);
        assert!(store.get_snapshot(1).is_none(), "new folder starts empty");

        store.set(1, snap("in work"), preview("in work"));
        store.clear_all();
        assert!(store.get_snapshot(1).is_none());

        store.select(main);
        assert_eq!(
            payload(&store.get_snapshot(1).unwrap()),
            b"in main",
            "clearing Work must not touch Main"
        );
    }

    /// "Main" is home: it must survive every operation, so the hotkeys always
    /// have somewhere to land and the name in the pill never changes.
    #[test]
    fn home_folder_cannot_be_renamed_or_deleted() {
        let mut store = FolderStore::default();
        let main = store.permanent_id();
        store.create("Work");

        assert!(!store.rename(main, "Something else"));
        assert!(!store.delete(main));
        assert_eq!(name_of(&store, main), DEFAULT_FOLDER);
        assert_eq!(store.folder_dtos().len(), 2);

        // Everything else stays fully editable.
        let work = store.active_id();
        assert!(store.rename(work, "Renamed"));
        assert!(store.delete(work));
        assert_eq!(store.folder_dtos().len(), 1);
        assert_eq!(store.active_id(), main);
    }

    /// The home folder is index 0, so creating folders must never displace it.
    #[test]
    fn home_folder_stays_first_after_churn() {
        let mut store = FolderStore::default();
        let main = store.permanent_id();
        let a = store.create("A");
        store.create("B");
        store.delete(a);
        store.create("C");
        assert_eq!(store.permanent_id(), main);
        assert!(store.folder_dtos()[0].permanent);
        assert_eq!(
            store.folder_dtos().iter().filter(|f| f.permanent).count(),
            1
        );
    }

    #[test]
    fn cannot_delete_the_last_folder() {
        let mut store = FolderStore::default();
        assert!(!store.delete(store.active_id()));
        assert_eq!(store.folder_dtos().len(), 1);
    }

    /// Deleting the active folder hands over to a neighbour rather than leaving
    /// the active pointer dangling.
    #[test]
    fn deleting_active_folder_selects_neighbour() {
        let mut store = FolderStore::default();
        let main = store.active_id();
        let work = store.create("Work");
        assert_eq!(store.active_id(), work);

        assert!(store.delete(work));
        assert_eq!(store.active_id(), main);
        assert_eq!(store.folder_dtos().len(), 1);
    }

    #[test]
    fn undo_targets_the_folder_it_came_from() {
        let mut store = FolderStore::default();
        let main = store.active_id();
        store.set(1, snap("saved"), preview("saved"));

        let stashed = store.active_slots_clone();
        store.clear_all();

        // User wanders off to another folder before hitting undo.
        store.create("Elsewhere");
        assert!(store.replace_slots(main, stashed));

        store.select(main);
        assert_eq!(payload(&store.get_snapshot(1).unwrap()), b"saved");
    }

    #[test]
    fn repairs_a_dangling_active_pointer() {
        let path = tmp("corrupt.bin");
        let mut store = FolderStore::default();
        store.create("Work");
        store.active = 999; // never existed
        store.save(&path);

        let loaded = FolderStore::load(&path, Path::new("nonexistent"));
        assert!(loaded.folder_dtos().iter().any(|f| f.active));
        assert_eq!(loaded.active_name(), DEFAULT_FOLDER);
    }

    #[test]
    fn names_are_trimmed_and_capped() {
        let mut store = FolderStore::default();
        let id = store.create("   spaced   out   ");
        assert_eq!(name_of(&store, id), "spaced out");

        let long = store.create(&"x".repeat(80));
        assert_eq!(name_of(&store, long).chars().count(), MAX_NAME);

        let blank = store.create("   ");
        assert_eq!(name_of(&store, blank), "Folder");
    }

    /// Ids must never be reused, or a stale undo could land in the wrong folder.
    #[test]
    fn ids_are_not_reused_after_delete() {
        let mut store = FolderStore::default();
        let a = store.create("A");
        store.delete(a);
        let b = store.create("B");
        assert_ne!(a, b);
    }
}

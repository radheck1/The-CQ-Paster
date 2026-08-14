//! The 9-slot store. Each slot holds a full clipboard snapshot plus a cheap
//! preview for display. Slots are addressed 1..=9 to match the hotkeys.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::clipboard::{ClipSnapshot, SlotPreview};

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

    /// Load a persisted store, or a fresh empty one if the file is missing or
    /// unreadable (e.g. a format change between versions).
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| bincode::deserialize(&bytes).ok())
            .unwrap_or_default()
    }

    /// Persist the store to disk so slots survive restarts. Best-effort.
    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = bincode::serialize(self) {
            let _ = std::fs::write(path, bytes);
        }
    }
}

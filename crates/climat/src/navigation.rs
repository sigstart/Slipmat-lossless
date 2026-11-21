// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use serde::{Deserialize, Serialize};
use slipmat_core::entry::Entry;
use slipmat_core::ipc::{CatalogFilter, PageKind, View};
use slipmat_core::sort::SortBy;

#[derive(Clone, Serialize, Deserialize)]
pub struct Page {
    pub kind: PageKind,
    pub id: String,
    pub title: String,
    pub subtitle: String,
}

#[derive(Serialize, Deserialize)]
pub struct Navigation {
    pub view: View,
    pub catalog: bool,
    pub page: Option<Page>,
    pub filters: [String; 6],
    pub sorts: [(SortBy, bool); 4],
    pub kinds: CatalogFilter,
    pub selection: Selection,
}

impl Navigation {
    pub fn load() -> Option<Self> {
        let path = slipmat_core::paths::state_dir()?.join("climat-navigation.json");
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(dir) = slipmat_core::paths::state_dir() {
            slipmat_core::artwork::write_atomically(
                &dir.join("climat-navigation.json"),
                &serde_json::to_vec(self)?,
            )?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct Selection {
    pub index: usize,
    pub id: Option<RowId>,
}

#[derive(PartialEq, Serialize, Deserialize)]
pub enum RowId {
    Song(String),
    Page(PageKind, String),
}

impl RowId {
    pub fn of(entry: &Entry) -> Option<Self> {
        match entry {
            Entry::Song(track) => Some(Self::Song(track.id.0.clone())),
            other => other.page_target().map(|(kind, id)| Self::Page(kind, id)),
        }
    }
}

impl Selection {
    pub fn locate(&self, rows: &[Entry]) -> usize {
        if let Some(id) = &self.id {
            if rows.get(self.index).and_then(RowId::of).as_ref() == Some(id) {
                return self.index;
            }
            if let Some(index) = rows
                .iter()
                .position(|row| RowId::of(row).as_ref() == Some(id))
            {
                return index;
            }
        }
        self.index.min(rows.len().saturating_sub(1))
    }
}

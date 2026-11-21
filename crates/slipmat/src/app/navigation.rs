// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

use relm4::ComponentSender;
use relm4::adw::prelude::NavigationPageExt;
use relm4::gtk::{self, glib, prelude::*};
use serde::{Deserialize, Serialize};

use super::pages::Arrival;
use super::{AppModel, PageKind, SidebarRow, View};

type Widgets = <AppModel as relm4::Component>::Widgets;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct Navigation {
    pub library_query: String,
    pub catalog_query: String,
    pub catalog_filter: String,
    pub searching: bool,
    pub pages: Vec<SavedPage>,
    pub scroll: f64,
}

#[derive(Serialize, Deserialize)]
pub(super) struct SavedPage {
    kind: PageKind,
    sidebar: bool,
    scroll: f64,
}

impl Navigation {
    pub fn load() -> Self {
        slipmat_core::paths::state_dir()
            .and_then(|dir| std::fs::read(dir.join("gtk-navigation.json")).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(dir) = slipmat_core::paths::state_dir() {
            slipmat_core::artwork::write_atomically(
                &dir.join("gtk-navigation.json"),
                &serde_json::to_vec(self)?,
            )?;
        }
        Ok(())
    }
}

impl AppModel {
    fn results_scroller<'a>(&self, widgets: &'a Widgets) -> &'a gtk::ScrolledWindow {
        match self.view {
            View::Albums => &widgets.album_scroller,
            View::Artists => &widgets.artist_scroller,
            View::Playlists => &widgets.playlist_scroller,
            View::Songs | View::Search => &widgets.library_scroller,
        }
    }

    pub(super) fn save_navigation(&self, widgets: &Widgets) {
        if matches!(self.stage, super::Stage::SignedOut) || self.resume_pages.is_some() {
            return;
        }
        let saved = Navigation {
            library_query: self.library_query.clone(),
            catalog_query: self.catalog_query.clone(),
            catalog_filter: self.catalog_filter.id().into(),
            searching: self.searching,
            pages: self
                .pages
                .iter()
                .map(|page| SavedPage {
                    kind: page.kind.clone(),
                    sidebar: !page.widget().can_pop(),
                    scroll: page.scroll_position(),
                })
                .collect(),
            scroll: self
                .resume_scroll
                .unwrap_or_else(|| self.results_scroller(widgets).vadjustment().value()),
        };
        if let Err(error) = saved.save() {
            tracing::warn!(%error, "could not save browsing position");
        }
    }

    pub(super) fn resume_navigation(&mut self, sender: &ComponentSender<Self>) {
        let Some(pages) = self.resume_pages.take() else {
            return;
        };
        if self.view == View::Search && !self.catalog_query.trim().is_empty() {
            self.run_catalog_search(sender, self.search_gen, 0);
        }
        for page in pages {
            let arrival = if page.sidebar {
                if !self
                    .settings
                    .pinned_playlists
                    .iter()
                    .any(|id| id == page.kind.id())
                {
                    break;
                }
                self.selected_row = Some(SidebarRow::Pinned(page.kind.id().into()));
                self.pins_dirty = true;
                Arrival::FromTheSidebar
            } else {
                Arrival::PushedOnto
            };
            self.open_page(page.kind, sender, arrival);
            if let Some(opened) = self.pages.last_mut() {
                opened.resume_scroll = Some(page.scroll);
            }
        }
    }

    pub(super) fn resume_results_scroll(&mut self, widgets: &Widgets) {
        if self.resume_scroll.is_none()
            || self.resume_pages.is_some()
            || self.searching_catalog
            || !self.results_scroller(widgets).is_mapped()
        {
            return;
        }
        let empty = match self.view {
            View::Albums => self.album_grid.is_empty(),
            View::Artists => self.artist_grid.is_empty(),
            View::Playlists => self.playlist_grid.is_empty(),
            View::Songs | View::Search => self.library.is_empty(),
        };
        if empty {
            return;
        }
        if let Some(value) = self.resume_scroll.take() {
            restore_scroll(self.results_scroller(widgets), value);
        }
    }
}

/// Wait for the scroller to allocate its contents before applying the offset.
pub(crate) fn restore_scroll(scroller: &gtk::ScrolledWindow, value: f64) {
    if value <= 0.0 {
        return;
    }
    let waiting_for_layout = std::cell::Cell::new(true);
    scroller.add_tick_callback(move |scroller, _| {
        if !scroller.is_mapped() {
            return glib::ControlFlow::Continue;
        }
        if waiting_for_layout.replace(false) {
            return glib::ControlFlow::Continue;
        }
        scroller.vadjustment().set_value(value);
        glib::ControlFlow::Break
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsing_state_round_trips_a_pin_and_its_drill_down() {
        let saved = Navigation {
            library_query: "live".into(),
            catalog_query: "deluxe".into(),
            catalog_filter: "albums".into(),
            searching: true,
            pages: vec![
                SavedPage {
                    kind: PageKind::LibraryPlaylist("playlist".into()),
                    sidebar: true,
                    scroll: 728.0,
                },
                SavedPage {
                    kind: PageKind::Album("album".into()),
                    sidebar: false,
                    scroll: 420.0,
                },
            ],
            scroll: 128.0,
        };
        let bytes = serde_json::to_vec(&saved).unwrap();
        let reopened: Navigation = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(reopened.library_query, "live");
        assert_eq!(reopened.catalog_query, "deluxe");
        assert_eq!(reopened.catalog_filter, "albums");
        assert!(reopened.searching);
        assert_eq!(reopened.scroll, 128.0);
        assert_eq!(
            reopened.pages[0].kind,
            PageKind::LibraryPlaylist("playlist".into())
        );
        assert!(reopened.pages[0].sidebar);
        assert_eq!(reopened.pages[0].scroll, 728.0);
        assert_eq!(reopened.pages[1].kind, PageKind::Album("album".into()));
        assert!(!reopened.pages[1].sidebar);
        assert_eq!(reopened.pages[1].scroll, 420.0);
    }
}

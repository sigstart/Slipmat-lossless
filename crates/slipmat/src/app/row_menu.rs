// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The row context menu: what it offers, and what each item sends.
//!
//! Built imperatively rather than in `view!` because it is per-row and
//! transient — a `ListView` recycles the widget underneath it while it is open,
//! so the popover is parented, shown, and unparented per click rather than
//! living in the tree.
//!
//! Each item appears only when it would do something. Offering "Add to Library"
//! for a track already saved, or "Favourite" for one already starred, is a menu
//! that lies about the state of things.

use relm4::gtk;
use relm4::gtk::prelude::*;

use super::{AppModel, AppMsg, LibraryAction};
use crate::components::track_row::RowMenuRequest;

impl AppModel {
    /// Show a row's context menu where it was clicked.
    ///
    /// Built fresh each time and parented to the row: a single long-lived
    /// popover would have to be re-parented on every click anyway, and a
    /// `ListView` recycles the widget under it while it is open.
    pub(super) fn show_row_menu(&self, req: RowMenuRequest) {
        let menu = gtk::gio::Menu::new();

        let queue = gtk::gio::Menu::new();
        queue.append(Some("Play _Next"), Some("row.play-next"));
        queue.append(Some("Add to _Queue"), Some("row.play-later"));
        menu.append_section(None, &queue);

        // A second section, because these leave the app and change the user's
        // account — a different kind of act from reordering a queue.
        //
        // Each item appears only when it would do something. Offering "Add to
        // Library" for a track read *out of* the library, or "Favourite" for
        // one already starred, is a menu that lies about the state of things.
        let account = gtk::gio::Menu::new();
        if !req.in_library {
            account.append(Some("Add to _Library"), Some("row.add-to-library"));
        }
        if !req.favorite {
            account.append(Some("_Favourite"), Some("row.favorite"));
        }
        if account.n_items() > 0 {
            menu.append_section(None, &account);
        }

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(req.at.0, req.at.1, 1, 1)));
        popover.set_parent(&req.over);

        // The actions live on the popover, so they go away with it rather than
        // accumulating on the window one right-click at a time.
        let actions = gtk::gio::SimpleActionGroup::new();
        for (name, next) in [("play-next", true), ("play-later", false)] {
            let action = gtk::gio::SimpleAction::new(name, None);
            let id = req.catalog_id.clone();
            let sender = self.menu_sender.clone();
            action.connect_activate(move |_, _| {
                sender.input(AppMsg::Enqueue {
                    catalog_id: id.clone(),
                    next,
                });
            });
            actions.add_action(&action);
        }

        for (name, what) in [
            ("add-to-library", LibraryAction::AddToLibrary),
            ("favorite", LibraryAction::Favorite),
        ] {
            let action = gtk::gio::SimpleAction::new(name, None);
            let id = req.catalog_id.clone();
            let sender = self.menu_sender.clone();
            action.connect_activate(move |_, _| {
                sender.input(AppMsg::LibraryWrite {
                    catalog_id: id.clone(),
                    action: what,
                });
            });
            actions.add_action(&action);
        }
        popover.insert_action_group("row", Some(&actions));

        // Unparent on close, or it leaks and keeps the row widget alive after
        // the list has recycled it out from under us — but **not during** the
        // close.
        //
        // GTK closes a PopoverMenu *before* activating the item you clicked. So
        // unparenting here tore down the action group a moment before the
        // action fired, and every menu item silently did nothing: no command
        // left the app, and the daemon logged nothing because nothing was sent.
        // Deferring to an idle lets the activation land first.
        popover.connect_closed(|p| {
            let p = p.clone();
            gtk::glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }
}

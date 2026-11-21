// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

//! The native sign-in dialog and its connection to the daemon.

use std::time::Duration;

use relm4::adw::prelude::*;
use relm4::{Component, ComponentController, ComponentParts, ComponentSender, adw, gtk};
use slipmat_core::native_auth::{Client, Reply};
use tokio::sync::mpsc;

use super::{AppModel, AppMsg, CommandMsg};

const TITLE: &str = "Native Sign-In";
const ACCOUNT_LABEL: &str = "Apple Account";
const PASSWORD_LABEL: &str = "Password";
const CODE_LABEL: &str = "Verification Code";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn register_action(
    window: &adw::ApplicationWindow,
    sender: &ComponentSender<AppModel>,
) -> gtk::gio::SimpleAction {
    let action = gtk::gio::SimpleAction::new("open", None);
    action.set_enabled(false);
    let sender = sender.clone();
    action.connect_activate(move |_, _| sender.input(AppMsg::OpenNativeAuth));
    let group = gtk::gio::SimpleActionGroup::new();
    group.add_action(&action);
    window.insert_action_group("native-auth", Some(&group));
    action
}

pub(super) fn append_menu_item(menu: &gtk::gio::Menu) {
    let item = gtk::gio::MenuItem::new(Some(TITLE), Some("native-auth.open"));
    item.set_attribute_value("hidden-when", Some(&"action-disabled".to_variant()));
    menu.append_item(&item);
}

pub(super) fn onboarding_button(
    action: &gtk::gio::SimpleAction,
    sender: &ComponentSender<AppModel>,
) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(TITLE)
        .halign(gtk::Align::Center)
        .css_classes(["pill"])
        .build();
    action
        .bind_property("enabled", &button, "visible")
        .sync_create()
        .build();
    let sender = sender.clone();
    button.connect_clicked(move |_| sender.input(AppMsg::OpenNativeAuth));
    button
}

pub(super) fn watch_availability(sender: &ComponentSender<AppModel>) {
    sender.command(|out, shutdown| {
        shutdown
            .register(async move {
                let mut available = false;
                loop {
                    let connected = connect().await.is_some();
                    if connected != available {
                        available = connected;
                        if out
                            .send(CommandMsg::NativeAuthAvailable(available))
                            .is_err()
                        {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            })
            .drop_on_shutdown()
    });
}

async fn connect() -> Option<Client> {
    tokio::time::timeout(CONNECT_TIMEOUT, Client::connect())
        .await
        .ok()?
        .ok()
}

impl AppModel {
    pub(super) fn open_native_auth(
        &mut self,
        sender: &ComponentSender<Self>,
        window: &adw::ApplicationWindow,
    ) {
        if !self.native_auth_action.is_enabled() || self.native_auth_dialog.is_some() {
            return;
        }
        let dialog = AuthDialog::builder()
            .launch(())
            .forward(sender.input_sender(), |signed_in| {
                AppMsg::NativeAuthClosed { signed_in }
            });
        // Present over the web sign-in gate when it already owns the window.
        let parent = self.onboarding.as_ref().map_or_else(
            || window.upcast_ref(),
            |gate| gate.upcast_ref::<gtk::Widget>(),
        );
        dialog.widget().present(Some(parent));
        self.native_auth_dialog = Some(dialog);
    }
}

#[derive(Debug)]
pub(super) enum Input {
    Changed,
    Submit,
    Dismiss,
    Closed,
}

enum Request {
    Credentials { account: String, password: String },
    Verification(String),
    SignOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Screen {
    Working,
    Credentials,
    Verification,
    VerificationUnsupported,
    SignedIn,
    SignedOut,
    Busy,
    Rejected,
    Expired,
    Failed,
    Unavailable,
    UnexpectedReply,
}

impl Screen {
    fn message(self) -> &'static str {
        match self {
            Self::Working => "Contacting the native sign-in service…",
            Self::Credentials => "Enter your Apple Account details to start a native session.",
            Self::Verification => "Enter the verification code from Apple.",
            Self::VerificationUnsupported => "Apple requested an unsupported verification method.",
            Self::SignedIn => "Your native Apple Music session is active.",
            Self::SignedOut => "Your native Apple Music session has ended.",
            Self::Busy => "Native sign-in is busy. Close this dialog and try again shortly.",
            Self::Rejected => {
                "The sign-in service rejected the request. Close this dialog to try again."
            }
            Self::Expired => "The sign-in request expired. Close this dialog to start again.",
            Self::Failed => "The native sign-in request failed. Close this dialog to try again.",
            Self::Unavailable => "The native sign-in service is unavailable.",
            Self::UnexpectedReply => "The native sign-in service sent an unexpected reply.",
        }
    }

    fn action(self) -> Option<&'static str> {
        match self {
            Self::Credentials => Some("Continue"),
            Self::Verification => Some("Verify"),
            Self::SignedIn => Some("Sign Out"),
            _ => None,
        }
    }

    fn dismiss_label(self) -> &'static str {
        match self {
            Self::Working | Self::Credentials | Self::Verification => "Cancel",
            _ => "Close",
        }
    }
}

impl From<Reply> for Screen {
    fn from(reply: Reply) -> Self {
        match reply {
            Reply::CredentialsRequired => Self::Credentials,
            Reply::VerificationRequired => Self::Verification,
            Reply::VerificationUnsupported => Self::VerificationUnsupported,
            Reply::SignedIn => Self::SignedIn,
            Reply::SignedOut => Self::SignedOut,
            Reply::Busy => Self::Busy,
            Reply::Rejected => Self::Rejected,
            Reply::Expired => Self::Expired,
            Reply::Failed => Self::Failed,
            Reply::Playing
            | Reply::Paused
            | Reply::Position { .. }
            | Reply::Stopped
            | Reply::Played { .. }
            | Reply::LicenceDenied
            | Reply::TrialFailed { .. } => Self::UnexpectedReply,
        }
    }
}

pub(super) struct AuthDialog {
    screen: Screen,
    requests: mpsc::UnboundedSender<Request>,
}

#[relm4::component(pub(super))]
impl Component for AuthDialog {
    type Init = ();
    type Input = Input;
    type Output = bool;
    type CommandOutput = Screen;

    view! {
        adw::Dialog {
            set_title: TITLE,
            set_content_width: 440,
            connect_closed[sender] => move |_| {
                let _ = sender.input_sender().send(Input::Closed);
            },
            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},
                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 16,
                    set_margin_top: 12,
                    set_margin_bottom: 24,
                    set_margin_start: 24,
                    set_margin_end: 24,
                    gtk::Label {
                        set_wrap: true,
                        set_xalign: 0.0,
                        #[watch]
                        set_label: model.screen.message(),
                    },
                    adw::Spinner {
                        set_halign: gtk::Align::Center,
                        #[watch]
                        set_visible: model.screen == Screen::Working,
                    },
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        #[watch]
                        set_visible: model.screen == Screen::Credentials,
                        #[name = "account"]
                        gtk::Entry {
                            set_placeholder_text: Some(ACCOUNT_LABEL),
                            set_input_purpose: gtk::InputPurpose::Email,
                            set_enable_undo: false,
                            connect_changed => Input::Changed,
                            connect_activate => Input::Submit,
                        },
                        #[name = "password"]
                        gtk::PasswordEntry {
                            set_placeholder_text: Some(PASSWORD_LABEL),
                            set_show_peek_icon: false,
                            set_enable_undo: false,
                            connect_changed => Input::Changed,
                            connect_activate => Input::Submit,
                        },
                    },
                    #[name = "code"]
                    gtk::PasswordEntry {
                        set_placeholder_text: Some(CODE_LABEL),
                        set_show_peek_icon: false,
                        set_enable_undo: false,
                        #[watch]
                        set_visible: model.screen == Screen::Verification,
                        connect_changed => Input::Changed,
                        connect_activate => Input::Submit,
                    },
                    gtk::Box {
                        set_halign: gtk::Align::End,
                        set_spacing: 8,
                        gtk::Button {
                            #[watch]
                            set_label: model.screen.dismiss_label(),
                            connect_clicked => Input::Dismiss,
                        },
                        #[name = "submit"]
                        gtk::Button {
                            #[watch]
                            set_visible: model.screen.action().is_some(),
                            #[watch]
                            set_label: model.screen.action().unwrap_or_default(),
                            #[watch]
                            set_css_classes: if model.screen == Screen::SignedIn {
                                &["destructive-action"]
                            } else {
                                &["suggested-action"]
                            },
                            connect_clicked => Input::Submit,
                        },
                    },
                },
            },
        }
    }

    fn init(_: (), root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let (requests, rx) = mpsc::unbounded_channel();
        let model = Self {
            screen: Screen::Working,
            requests,
        };
        let widgets = view_output!();
        use gtk::accessible::Property::Label;
        widgets.account.update_property(&[Label(ACCOUNT_LABEL)]);
        widgets.password.update_property(&[Label(PASSWORD_LABEL)]);
        widgets.code.update_property(&[Label(CODE_LABEL)]);
        sender.command(move |out, shutdown| {
            shutdown.register(authenticate(rx, out)).drop_on_shutdown()
        });
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        input: Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match input {
            Input::Dismiss => {
                root.close();
            }
            Input::Closed => {
                widgets.clear_entries();
                let _ = sender.output(self.screen == Screen::SignedIn);
                return;
            }
            Input::Submit if self.can_submit(widgets) => {
                let request = match self.screen {
                    Screen::Credentials => Request::Credentials {
                        account: widgets.account.text().trim().to_owned(),
                        password: widgets.password.text().into(),
                    },
                    Screen::Verification => Request::Verification(widgets.code.text().into()),
                    Screen::SignedIn => Request::SignOut,
                    _ => return,
                };
                widgets.clear_entries();
                self.screen = if self.requests.send(request).is_ok() {
                    Screen::Working
                } else {
                    Screen::Failed
                };
            }
            _ => {}
        }
        self.update_view(widgets, sender);
    }

    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        screen: Screen,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        self.screen = screen;
        self.update_view(widgets, sender);
        match screen {
            Screen::Credentials => widgets.account.grab_focus(),
            Screen::Verification => widgets.code.grab_focus(),
            _ => false,
        };
    }

    fn post_view(&self, widgets: &mut Self::Widgets) {
        widgets.submit.set_sensitive(self.can_submit(widgets));
    }
}

impl AuthDialog {
    fn can_submit(&self, widgets: &AuthDialogWidgets) -> bool {
        match self.screen {
            Screen::Credentials => {
                !widgets.account.text().trim().is_empty() && !widgets.password.text().is_empty()
            }
            Screen::Verification => !widgets.code.text().is_empty(),
            Screen::SignedIn => true,
            _ => false,
        }
    }
}

impl AuthDialogWidgets {
    fn clear_entries(&self) {
        self.account.set_text("");
        self.password.set_text("");
        self.code.set_text("");
    }
}

async fn authenticate(mut requests: mpsc::UnboundedReceiver<Request>, out: relm4::Sender<Screen>) {
    let Some(mut client) = connect().await else {
        let _ = out.send(Screen::Unavailable);
        return;
    };
    let mut reply = match client.status().await {
        Ok(Reply::SignedOut) => client.begin().await,
        reply => reply,
    };
    loop {
        let screen = reply.map_or(Screen::Failed, Screen::from);
        if out.send(screen).is_err() || screen.action().is_none() {
            return;
        }
        reply = match requests.recv().await {
            Some(Request::Credentials { account, password }) => {
                client.credentials(account, password).await
            }
            Some(Request::Verification(code)) => client.verification_code(code).await,
            Some(Request::SignOut) => client.sign_out().await,
            None => return,
        };
    }
}

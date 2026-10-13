//! Theme switcher button for the bar.
//!
//! A round icon button that opens a popover listing the built-in themes, each
//! with a small palette preview and a checkmark on the active one. Picking one
//! emits [`ThemeButtonOutput::Apply`]; the bar forwards that straight to the
//! shell, which reloads the global stylesheet and tells every bar which theme
//! is now active. A final "Browse themes…" row opens the fly-out gallery
//! ([`crate::theme::ThemePickerModel`]).

use gtk::prelude::*;
use relm4::prelude::*;

use crate::theme;

/// One theme entry in the popover, kept so its checkmark can follow the
/// active theme.
struct Row {
    id: String,
    check: gtk::Image,
}

pub struct ThemeButtonModel {
    current: String,
    rows: Vec<Row>,
}

#[derive(Debug)]
pub enum ThemeButtonInput {
    /// The active theme changed elsewhere in the shell.
    SetTheme(String),
}

#[derive(Debug)]
pub enum ThemeButtonOutput {
    /// Apply this theme.
    Apply(String),
    /// Open the fly-out theme gallery.
    OpenGallery,
}

#[relm4::component(pub)]
impl SimpleComponent for ThemeButtonModel {
    type Init = String;
    type Input = ThemeButtonInput;
    type Output = ThemeButtonOutput;

    view! {
        #[root]
        gtk::MenuButton {
            set_css_classes: &["mango-icon-button", "mango-theme-button"],
            set_tooltip_text: Some("Theme"),
            set_always_show_arrow: false,
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        root.set_child(Some(&gtk::Image::from_icon_name("color-select-symbolic")));

        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-theme-popover");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.add_css_class("mango-theme-menu");

        let mut rows = Vec::with_capacity(theme::THEMES.len());
        for theme in theme::THEMES {
            let preview = theme::preview_widget(theme, 44, 16);
            let label = gtk::Label::new(Some(theme.name));
            label.set_xalign(0.0);
            label.set_halign(gtk::Align::Start);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);

            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.add_css_class("mango-theme-check");
            check.set_visible(false);

            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.append(&preview);
            line.append(&label);
            line.append(&check);

            let button = gtk::Button::new();
            button.add_css_class("mango-theme-item");
            button.set_child(Some(&line));

            let id = theme.id.to_string();
            button.connect_clicked({
                let sender = sender.clone();
                let popover = popover.clone();
                let id = id.clone();
                move |_| {
                    let _ = sender.output(ThemeButtonOutput::Apply(id.clone()));
                    popover.popdown();
                }
            });

            content.append(&button);
            rows.push(Row { id, check });
        }

        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.add_css_class("mango-theme-separator");
        content.append(&separator);

        let gallery = gtk::Button::new();
        gallery.add_css_class("mango-theme-item");
        let gallery_line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        gallery_line.append(&gtk::Image::from_icon_name("view-app-grid-symbolic"));
        let gallery_label = gtk::Label::new(Some("Browse themes…"));
        gallery_label.set_xalign(0.0);
        gallery_label.set_hexpand(true);
        gallery_line.append(&gallery_label);
        gallery.set_child(Some(&gallery_line));
        gallery.connect_clicked({
            let sender = sender.clone();
            let popover = popover.clone();
            move |_| {
                let _ = sender.output(ThemeButtonOutput::OpenGallery);
                popover.popdown();
            }
        });
        content.append(&gallery);

        popover.set_child(Some(&content));
        root.set_popover(Some(&popover));

        let mut model = ThemeButtonModel {
            current: init,
            rows,
        };
        let widgets = view_output!();

        let current = model.current.clone();
        model.set_current(&current);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            ThemeButtonInput::SetTheme(id) => self.set_current(&id),
        }
    }
}

impl ThemeButtonModel {
    fn set_current(&mut self, id: &str) {
        self.current = id.to_string();
        for row in &self.rows {
            row.check.set_visible(row.id == id);
        }
    }
}

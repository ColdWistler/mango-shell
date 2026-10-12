//! One layer-shell bar per monitor.
//!
//! The bar itself is a thin shell: it owns the widget components
//! (`TagsModel`, `ClockModel`) and forwards messages between them and the
//! compositor.

use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use relm4::prelude::*;

use crate::mango::{self, TagView};
use crate::widgets::clock::ClockModel;
use crate::widgets::tags::{TagsInput, TagsModel, TagsOutput};

/// Everything a bar needs to exist.
pub struct BarInit {
    /// Monitor connector name, e.g. `DP-1`.
    pub name: String,
    /// The GDK monitor this bar is drawn on.
    pub monitor: gdk::Monitor,
}

#[derive(Debug)]
pub enum BarInput {
    /// Replace the tag state for this monitor.
    SetTags(Vec<TagView>),
    /// A tag button was clicked.
    SwitchTag(TagsOutput),
}

pub struct BarModel {
    name: String,
    tags: Controller<TagsModel>,
    clock: Controller<ClockModel>,
}

#[relm4::component(pub)]
impl SimpleComponent for BarModel {
    type Init = BarInit;
    type Input = BarInput;
    type Output = ();

    view! {
        #[root]
        gtk::Window {
            set_css_classes: &["mango-bar"],

            gtk::CenterBox {
                // Left: the tag (workspace) buttons.
                #[wrap(Some)]
                set_start_widget = &gtk::Box {
                    set_spacing: 6,
                    set_halign: gtk::Align::Start,
                    append = &model.tags.widget().clone() {},
                },

                // Center: which monitor this bar belongs to.
                #[wrap(Some)]
                set_center_widget = &gtk::Label {
                    #[watch]
                    set_label: &model.name,
                    set_css_classes: &["mango-title"],
                },

                // Right: the clock.
                #[wrap(Some)]
                set_end_widget = &gtk::Box {
                    set_spacing: 6,
                    set_halign: gtk::Align::End,
                    append = &model.clock.widget().clone() {},
                },
            }
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Turn the window into a top-of-screen bar on `init.monitor` before it
        // is realized.
        root.init_layer_shell();
        root.set_monitor(Some(&init.monitor));
        root.set_namespace(Some("mango-bar"));
        root.set_layer(Layer::Top);
        // Reserve exactly the bar's height so tiled windows never overlap it.
        root.auto_exclusive_zone_enable();
        root.set_anchor(Edge::Top, true);
        root.set_anchor(Edge::Left, true);
        root.set_anchor(Edge::Right, true);
        root.set_decorated(false);

        let tags = TagsModel::builder()
            .launch(())
            .forward(sender.input_sender(), BarInput::SwitchTag);
        let clock = ClockModel::builder().launch(()).detach();

        let model = BarModel {
            name: init.name,
            tags,
            clock,
        };

        let widgets = view_output!();

        // Child windows are not presented automatically.
        root.set_visible(true);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            BarInput::SetTags(tags) => self.tags.emit(TagsInput::Set(tags)),
            BarInput::SwitchTag(TagsOutput::Switch(index)) => mango::switch_tag(index),
        }
    }
}

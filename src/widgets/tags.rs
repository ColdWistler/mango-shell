//! Tag (workspace) buttons, fed by Mango's IPC.
//!
//! Widgets in Relm4 normally update through `#[watch]`, but a list of buttons
//! changes shape, so we keep a handle to the container box and rebuild its
//! children when the state changes. That is the same trade-off OkShell makes
//! for its workspace widget.

use gtk::prelude::*;
use relm4::prelude::*;

use crate::mango::TagView;

pub struct TagsModel {
    /// The root box, cloned so `update` can rebuild the buttons.
    container: Option<gtk::Box>,
    tags: Vec<TagView>,
}

#[derive(Debug)]
pub enum TagsInput {
    /// Replace the whole tag list.
    Set(Vec<TagView>),
}

#[derive(Debug)]
pub enum TagsOutput {
    /// A tag was clicked.
    Switch(u32),
}

#[relm4::component(pub)]
impl SimpleComponent for TagsModel {
    type Init = ();
    type Input = TagsInput;
    type Output = TagsOutput;

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 4,
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = TagsModel {
            container: Some(root.clone()),
            tags: Vec::new(),
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            TagsInput::Set(tags) => {
                self.tags = tags;
                self.render(&sender);
            }
        }
    }
}

impl TagsModel {
    fn render(&self, sender: &ComponentSender<Self>) {
        let Some(container) = &self.container else {
            return;
        };

        while let Some(child) = container.first_child() {
            container.remove(&child);
        }

        for tag in &self.tags {
            let label = if tag.count > 0 {
                format!("{}·{}", tag.index, tag.count)
            } else {
                tag.index.to_string()
            };

            let button = gtk::Button::with_label(&label);
            if tag.active {
                button.add_css_class("active");
            }
            if tag.urgent {
                button.add_css_class("urgent");
            }

            let index = tag.index;
            let sender = sender.clone();
            button.connect_clicked(move |_| {
                let _ = sender.output(TagsOutput::Switch(index));
            });

            container.append(&button);
        }
    }
}

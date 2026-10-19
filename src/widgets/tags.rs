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
    /// The live buttons, paired with the tag index they represent, so a
    /// state change can restyle them in place instead of rebuilding the row
    /// (Mango pushes an update on every window focus/open/close).
    buttons: Vec<(u32, gtk::Button)>,
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
            buttons: Vec::new(),
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
    fn render(&mut self, sender: &ComponentSender<Self>) {
        let Some(container) = &self.container else {
            return;
        };

        // The common case is the same tags with different state (a window
        // opened, focus moved): restyle the existing buttons rather than
        // tearing the row down and building it again.
        let same_shape = self.buttons.len() == self.tags.len()
            && self
                .buttons
                .iter()
                .zip(&self.tags)
                .all(|((index, _), tag)| *index == tag.index);
        if same_shape {
            for ((_, button), tag) in self.buttons.iter().zip(&self.tags) {
                style(button, tag);
            }
            return;
        }

        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        self.buttons.clear();

        for tag in &self.tags {
            // Like end4's workspaces, but the tag number is written with
            // Japanese kanji numerals (1 → 一, 10 → 十, 11 → 十一, …).
            let button = gtk::Button::with_label(&tag_label(tag.index));
            style(&button, tag);

            let index = tag.index;
            let sender = sender.clone();
            button.connect_clicked(move |_| {
                let _ = sender.output(TagsOutput::Switch(index));
            });

            container.append(&button);
            self.buttons.push((index, button));
        }
    }
}

/// Apply the tag's state to its button: occupancy is a subtle pill behind the
/// glyph and the active tag is a filled pill.
fn style(button: &gtk::Button, tag: &TagView) {
    let mut classes = vec!["mango-tag"];
    if tag.count > 0 {
        classes.push("occupied");
    }
    if tag.urgent {
        classes.push("urgent");
    }
    if tag.active {
        classes.push("active");
    }
    button.set_css_classes(&classes);
    button.set_tooltip_text(Some(&format!(
        "Tag {} ({} window(s))",
        tag.index, tag.count
    )));
}

/// The label shown for a 1-based tag index, written with Japanese kanji
/// numerals: 1 → 一, 9 → 九, 10 → 十, 11 → 十一, 21 → 二十一, 100 → 百, and so
/// on. Falls back to arabic digits past the kanji's useful range (10 000+).
///
/// `pub(crate)` so the dashboard's workspaces card draws the same glyphs the
/// bar does.
pub(crate) fn tag_label(index: u32) -> String {
    const DIGITS: [&str; 9] = ["一", "二", "三", "四", "五", "六", "七", "八", "九"];
    if index == 0 {
        return "0".to_string();
    }
    if index >= 10_000 {
        return index.to_string();
    }

    let mut remaining = index;
    let mut out = String::new();
    for (value, kanji) in [(1000, "千"), (100, "百"), (10, "十")] {
        let count = remaining / value;
        remaining %= value;
        match count {
            0 => {}
            1 => out.push_str(kanji),
            _ => {
                out.push_str(DIGITS[(count - 1) as usize]);
                out.push_str(kanji);
            }
        }
    }
    if remaining > 0 {
        out.push_str(DIGITS[(remaining - 1) as usize]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::tag_label;

    #[test]
    fn numbers_read_as_kanji() {
        assert_eq!(tag_label(1), "一");
        assert_eq!(tag_label(9), "九");
        assert_eq!(tag_label(10), "十");
        assert_eq!(tag_label(11), "十一");
        assert_eq!(tag_label(21), "二十一");
        assert_eq!(tag_label(100), "百");
        assert_eq!(tag_label(101), "百一");
        assert_eq!(tag_label(1000), "千");
        assert_eq!(tag_label(10_000), "10000");
    }
}

//! System tray widget (a `StatusNotifierItem` host plus `DBusMenu`).
//!
//! Hosting the tray means running the `org.kde.StatusNotifierWatcher` service
//! on the session bus, so we lean on the [`system_tray`] crate, which is
//! Tokio-based. That runtime lives on its own thread and talks to the GTK
//! component through two channels:
//!
//! * `ComponentSender::input` for tray events (add/update/remove/show menu),
//! * a Tokio mpsc sender for click/menu actions going the other way.
//!
//! Icons are drawn with the icon theme when the item provides an icon name and
//! with a decoded `IconPixmap` otherwise.

use std::collections::HashMap;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use relm4::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use system_tray::client::{ActivateRequest, Client, Event, UpdateEvent};
use system_tray::item::{IconPixmap, Status, StatusNotifierItem};
use system_tray::menu::{MenuItem, MenuType, ToggleState, ToggleType, TrayMenu};

/// Tray icons are drawn at this size, matching the other bar icons.
const ICON_SIZE: i32 = 16;

pub struct TrayModel {
    items: Vec<TrayItemView>,
    /// Live buttons, keyed by item address, so a menu can be anchored.
    buttons: HashMap<String, gtk::Button>,
    container: Option<gtk::Box>,
    command: Option<UnboundedSender<TrayCommand>>,
    /// The menu currently on screen, kept alive while it is open.
    popover: Option<gtk::Popover>,
}

/// Messages sent from the tray thread into the UI.
#[derive(Debug)]
pub enum TrayInput {
    /// A new item registered.
    Add(String, Box<StatusNotifierItem>),
    /// An existing item changed.
    Update(String, UpdateEvent),
    /// An item went away.
    Remove(String),
    /// The owning item's menu is ready to be shown.
    ShowMenu {
        address: String,
        menu_path: String,
        menu: TrayMenu,
    },
}

/// Actions sent from the UI back to the tray thread.
enum TrayCommand {
    Activate {
        address: String,
    },
    Secondary {
        address: String,
    },
    MenuItem {
        address: String,
        menu_path: String,
        id: i32,
    },
    FetchMenu {
        address: String,
    },
}

#[relm4::component(pub)]
impl SimpleComponent for TrayModel {
    type Init = ();
    type Input = TrayInput;
    type Output = ();

    view! {
        #[root]
        gtk::Box {
            set_css_classes: &["mango-group", "mango-tray"],
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 2,
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut model = TrayModel {
            items: Vec::new(),
            buttons: HashMap::new(),
            container: Some(root.clone()),
            command: None,
            popover: None,
        };

        let widgets = view_output!();

        // The pill only appears once an item registers.
        root.set_visible(false);
        model.command = Some(spawn_tray(sender));

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            TrayInput::Add(address, item) => {
                // Some items ship their own icon directory; make it searchable.
                if let Some(path) = &item.icon_theme_path
                    && let Some(display) = gdk::Display::default()
                {
                    gtk::IconTheme::for_display(&display).add_search_path(path.as_str());
                }

                let view = TrayItemView::new(address.clone(), &item);
                match self.items.iter_mut().find(|entry| entry.address == address) {
                    Some(existing) => *existing = view,
                    None => self.items.push(view),
                }
                self.render();
            }

            TrayInput::Update(address, event) => {
                let changed = self
                    .items
                    .iter_mut()
                    .find(|entry| entry.address == address)
                    .is_some_and(|entry| entry.apply_event(&event));
                if changed {
                    self.render();
                }
            }

            TrayInput::Remove(address) => {
                self.items.retain(|entry| entry.address != address);
                self.render();
            }

            TrayInput::ShowMenu {
                address,
                menu_path,
                menu,
            } => self.show_menu(&address, &menu_path, &menu),
        }
    }
}

impl TrayModel {
    /// Rebuild the row of tray icons.
    fn render(&mut self) {
        let Some(container) = self.container.clone() else {
            return;
        };

        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        self.buttons.clear();

        let command = self.command.clone();
        for item in &self.items {
            let button = build_tray_button(item, command.clone());
            container.append(&button);
            self.buttons.insert(item.address.clone(), button);
        }

        container.set_visible(!self.items.is_empty());
    }

    /// Pop up an item's DBusMenu, anchored under its tray icon.
    fn show_menu(&mut self, address: &str, menu_path: &str, menu: &TrayMenu) {
        let Some(button) = self.buttons.get(address).cloned() else {
            return;
        };
        let Some(command) = self.command.clone() else {
            return;
        };

        if let Some(previous) = self.popover.take() {
            previous.popdown();
            if previous.parent().is_some() {
                previous.unparent();
            }
        }

        let context = MenuContext {
            address: address.to_string(),
            menu_path: menu_path.to_string(),
            command,
        };
        let content = build_menu(&menu.submenus, &context);
        content.set_margin_top(4);
        content.set_margin_bottom(4);

        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("mango-menu-popover");
        popover.set_child(Some(&content));
        popover.set_parent(&button);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(0, button.height(), 1, 1)));
        popover.popup();

        self.popover = Some(popover);
    }
}

/// The UI-facing subset of a tray item.
struct TrayItemView {
    address: String,
    id: String,
    title: Option<String>,
    tooltip: Option<String>,
    icon_name: Option<String>,
    texture: Option<gdk::Texture>,
    item_is_menu: bool,
    status: Status,
}

impl TrayItemView {
    fn new(address: String, item: &StatusNotifierItem) -> Self {
        Self {
            address,
            id: item.id.clone(),
            title: item.title.clone(),
            tooltip: tooltip_of(item),
            icon_name: item.icon_name.clone(),
            texture: item.icon_pixmap.as_deref().and_then(best_texture),
            item_is_menu: item.item_is_menu,
            status: item.status,
        }
    }

    /// Apply an update event. Returns whether the button needs redrawing.
    fn apply_event(&mut self, event: &UpdateEvent) -> bool {
        match event {
            UpdateEvent::Icon {
                icon_name,
                icon_pixmap,
            } => {
                self.icon_name.clone_from(icon_name);
                self.texture = icon_pixmap.as_deref().and_then(best_texture);
                true
            }
            UpdateEvent::Status(status) => {
                self.status = *status;
                true
            }
            UpdateEvent::Title(title) => {
                self.title.clone_from(title);
                true
            }
            UpdateEvent::Tooltip(tooltip) => {
                self.tooltip = tooltip
                    .as_ref()
                    .map(|tooltip| tooltip.description.clone())
                    .filter(|text| !text.is_empty());
                true
            }
            _ => false,
        }
    }

    fn tooltip_text(&self) -> String {
        if let Some(tooltip) = &self.tooltip {
            return tooltip.clone();
        }
        if let Some(title) = &self.title {
            return title.clone();
        }
        self.id.clone()
    }

    fn icon_widget(&self) -> gtk::Image {
        // Prefer the themed icon name, but fall back to the pixmap when the
        // name cannot be resolved (common for app-provided icons).
        if let Some(name) = self.icon_name.as_deref().filter(|name| icon_exists(name)) {
            let image = gtk::Image::from_icon_name(name);
            image.set_pixel_size(ICON_SIZE);
            return image;
        }
        if let Some(texture) = &self.texture {
            let image = gtk::Image::from_paintable(Some(texture));
            image.set_pixel_size(ICON_SIZE);
            return image;
        }

        let image = gtk::Image::from_icon_name("image-missing-symbolic");
        image.set_pixel_size(ICON_SIZE);
        image
    }
}

fn build_tray_button(
    item: &TrayItemView,
    command: Option<UnboundedSender<TrayCommand>>,
) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("mango-tray-item");
    button.set_tooltip_text(Some(&item.tooltip_text()));
    if item.status == Status::NeedsAttention {
        button.add_css_class("attention");
    }
    button.set_child(Some(&item.icon_widget()));

    let address = item.address.clone();
    let item_is_menu = item.item_is_menu;

    // Primary click: activate, or open the menu for menu-only items.
    button.connect_clicked({
        let command = command.clone();
        let address = address.clone();
        move |_| {
            let Some(command) = &command else {
                return;
            };
            let message = if item_is_menu {
                TrayCommand::FetchMenu {
                    address: address.clone(),
                }
            } else {
                TrayCommand::Activate {
                    address: address.clone(),
                }
            };
            let _ = command.send(message);
        }
    });

    // Secondary (right) click: always the context menu.
    {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        gesture.connect_pressed({
            let command = command.clone();
            let address = address.clone();
            move |_, _, _, _| {
                if let Some(command) = &command {
                    let _ = command.send(TrayCommand::FetchMenu {
                        address: address.clone(),
                    });
                }
            }
        });
        button.add_controller(gesture);
    }

    // Middle click: secondary activate.
    {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(2);
        gesture.connect_pressed({
            let command = command.clone();
            let address = address.clone();
            move |_, _, _, _| {
                if let Some(command) = &command {
                    let _ = command.send(TrayCommand::Secondary {
                        address: address.clone(),
                    });
                }
            }
        });
        button.add_controller(gesture);
    }

    button
}

/// Shared state needed to build (and wire up) a DBusMenu tree.
#[derive(Clone)]
struct MenuContext {
    address: String,
    menu_path: String,
    command: UnboundedSender<TrayCommand>,
}

fn build_menu(items: &[MenuItem], context: &MenuContext) -> gtk::Box {
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu.add_css_class("mango-menu");

    for item in items.iter().filter(|item| item.visible) {
        if item.menu_type == MenuType::Separator {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.add_css_class("mango-menu-separator");
            menu.append(&separator);
        } else {
            menu.append(&build_menu_item(item, context));
        }
    }

    menu
}

fn build_menu_item(item: &MenuItem, context: &MenuContext) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("mango-menu-item");
    button.set_sensitive(item.enabled);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    // A fixed-width indicator column keeps labels aligned across the menu.
    let indicator = match item.toggle_type {
        ToggleType::Checkmark if item.toggle_state == ToggleState::On => {
            Some("object-select-symbolic")
        }
        ToggleType::Radio if item.toggle_state == ToggleState::On => Some("radio-checked-symbolic"),
        ToggleType::Radio => Some("radio-symbolic"),
        ToggleType::Checkmark | ToggleType::CannotBeToggled => None,
    };
    let column = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    column.set_size_request(16, -1);
    if let Some(name) = indicator {
        let image = gtk::Image::from_icon_name(name);
        image.set_pixel_size(14);
        column.append(&image);
    }
    row.append(&column);

    if let Some(name) = item.icon_name.as_deref().filter(|name| icon_exists(name)) {
        let image = gtk::Image::from_icon_name(name);
        image.set_pixel_size(14);
        row.append(&image);
    }

    let label = gtk::Label::new(item.label.as_deref());
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&label);

    let has_submenu = !item.submenu.is_empty();
    if has_submenu {
        let arrow = gtk::Image::from_icon_name("pan-end-symbolic");
        arrow.set_pixel_size(12);
        row.append(&arrow);
    }
    button.set_child(Some(&row));

    if has_submenu {
        let submenu = item.submenu.clone();
        let context = context.clone();
        button.connect_clicked(move |button| {
            let content = build_menu(&submenu, &context);
            content.set_margin_top(4);
            content.set_margin_bottom(4);

            let popover = gtk::Popover::new();
            popover.set_has_arrow(false);
            popover.add_css_class("mango-menu-popover");
            popover.set_child(Some(&content));
            popover.set_parent(button);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(button.width(), 0, 1, 1)));
            popover.connect_closed(|popover| {
                if popover.parent().is_some() {
                    popover.unparent();
                }
            });
            popover.popup();
        });
    } else {
        let context = context.clone();
        let id = item.id;
        button.connect_clicked(move |button| {
            let _ = context.command.send(TrayCommand::MenuItem {
                address: context.address.clone(),
                menu_path: context.menu_path.clone(),
                id,
            });
            close_popovers(button.upcast_ref());
        });
    }

    button
}

/// Pop down every popover between `widget` and the root, innermost first.
fn close_popovers(widget: &gtk::Widget) {
    let mut node = Some(widget.clone());
    while let Some(current) = node {
        if let Ok(popover) = current.clone().downcast::<gtk::Popover>() {
            popover.popdown();
        }
        node = current.parent();
    }
}

/// Spawn the tray thread and return the channel used to drive it.
fn spawn_tray(sender: ComponentSender<TrayModel>) -> UnboundedSender<TrayCommand> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TrayCommand>();

    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        else {
            return;
        };

        runtime.block_on(async move {
            let Ok(client) = Client::new().await else {
                return;
            };

            // Seed the UI with whatever is already registered.
            let map = client.items();
            if let Ok(items) = map.lock() {
                for (address, (item, _menu)) in items.iter() {
                    sender.input(TrayInput::Add(address.clone(), Box::new(item.clone())));
                }
            }

            let mut events = client.subscribe();
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(Event::Add(address, item)) => sender.input(TrayInput::Add(address, item)),
                        Ok(Event::Update(address, update)) => sender.input(TrayInput::Update(address, update)),
                        Ok(Event::Remove(address)) => sender.input(TrayInput::Remove(address)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    command = rx.recv() => match command {
                        Some(command) => handle_command(&client, command, &sender).await,
                        // The widget was torn down; release the watcher name.
                        None => break,
                    },
                }
            }
        });
    });

    tx
}

async fn handle_command(
    client: &Client,
    command: TrayCommand,
    sender: &ComponentSender<TrayModel>,
) {
    match command {
        TrayCommand::Activate { address } => {
            let _ = client
                .activate(ActivateRequest::Default {
                    address,
                    x: 0,
                    y: 0,
                })
                .await;
        }
        TrayCommand::Secondary { address } => {
            let _ = client
                .activate(ActivateRequest::Secondary {
                    address,
                    x: 0,
                    y: 0,
                })
                .await;
        }
        TrayCommand::MenuItem {
            address,
            menu_path,
            id,
        } => {
            let _ = client
                .activate(ActivateRequest::MenuItem {
                    address,
                    menu_path,
                    submenu_id: id,
                })
                .await;
        }
        TrayCommand::FetchMenu { address } => {
            let (menu_path, cached) = {
                let map = client.items();
                let Ok(items) = map.lock() else {
                    return;
                };
                match items.get(&address) {
                    Some((item, menu)) => (item.menu.clone(), menu.clone()),
                    None => return,
                }
            };

            // Menu-only items without a DBusMenu fall back to activation.
            let Some(menu_path) = menu_path else {
                let _ = client
                    .activate(ActivateRequest::Default {
                        address,
                        x: 0,
                        y: 0,
                    })
                    .await;
                return;
            };

            // Give the app a chance to refresh the layout, then re-read it.
            let _ = client
                .about_to_show_menuitem(address.clone(), menu_path.clone(), 0)
                .await;

            let menu = {
                let map = client.items();
                let Ok(items) = map.lock() else {
                    return;
                };
                items
                    .get(&address)
                    .and_then(|(_, menu)| menu.clone())
                    .or(cached)
            };

            if let Some(menu) = menu {
                sender.input(TrayInput::ShowMenu {
                    address,
                    menu_path,
                    menu,
                });
            }
        }
    }
}

fn tooltip_of(item: &StatusNotifierItem) -> Option<String> {
    item.tool_tip
        .as_ref()
        .map(|tooltip| tooltip.description.clone())
        .filter(|text| !text.is_empty())
        .or_else(|| item.title.clone())
}

fn icon_exists(name: &str) -> bool {
    gdk::Display::default()
        .map(|display| gtk::IconTheme::for_display(&display).has_icon(name))
        .unwrap_or(false)
}

/// Decode the largest of an item's `IconPixmap`s into a texture.
fn best_texture(pixmaps: &[IconPixmap]) -> Option<gdk::Texture> {
    pixmaps
        .iter()
        .max_by_key(|pixmap| pixmap.width * pixmap.height)
        .and_then(texture_from_pixmap)
}

fn texture_from_pixmap(pixmap: &IconPixmap) -> Option<gdk::Texture> {
    let width = usize::try_from(pixmap.width).ok()?;
    let height = usize::try_from(pixmap.height).ok()?;
    let expected = width.checked_mul(height)?.checked_mul(4)?;
    if width == 0 || height == 0 || pixmap.pixels.len() < expected {
        return None;
    }

    // The spec transfers ARGB32 in network byte order, i.e. A, R, G, B.
    let mut rgba = Vec::with_capacity(expected);
    for pixel in pixmap.pixels.as_chunks::<4>().0.iter().take(width * height) {
        let alpha = u16::from(pixel[0]);
        rgba.push((u16::from(pixel[1]) * alpha / 255) as u8);
        rgba.push((u16::from(pixel[2]) * alpha / 255) as u8);
        rgba.push((u16::from(pixel[3]) * alpha / 255) as u8);
        rgba.push(alpha as u8);
    }

    let bytes = glib::Bytes::from(&rgba[..]);
    Some(
        gdk::MemoryTexture::new(
            pixmap.width,
            pixmap.height,
            gdk::MemoryFormat::R8g8b8a8Premultiplied,
            &bytes,
            width * 4,
        )
        .upcast(),
    )
}

use crate::{
    drive::{Node, ROOT, Sharing},
    sync::Report,
    worker::{self, Connection, Event, PREVIEW_LIMIT, Preferences, Request, ShareAction},
};
use eframe::egui::{
    self, Align, Button, Color32, Context, Frame, Layout, Margin, RichText, Sense, Stroke,
    TextureHandle, Ui, Vec2,
};
use egui_extras::{Column, TableBuilder};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[derive(Clone)]
struct Folder {
    path: String,
    name: String,
    parent: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Name,
    Size,
    Date,
}

enum Dialog {
    Sync {
        local: String,
        remote: String,
        label: String,
        picker: Option<u64>,
    },
    Save {
        node: Node,
        dir: String,
        picker: Option<u64>,
    },
    Share {
        node: Node,
        email: String,
        edit: bool,
        pending: bool,
        result: Option<Result<Sharing, String>>,
        confirm_link: bool,
    },
    SyncLog,
}

const THEMES: [(&str, u32, u32, u32, u32, u32, u32); 4] = [
    (
        "Tokyo Night",
        0x151620,
        0x1a1b26,
        0x252738,
        0x373b54,
        0xc0caf5,
        0x7aa2f7,
    ),
    (
        "Monokai", 0x1e1f1c, 0x272822, 0x34352e, 0x4a4b3f, 0xf8f8f2, 0xa6e22e,
    ),
    (
        "Catppuccin",
        0x181825,
        0x1e1e2e,
        0x313244,
        0x45475a,
        0xcdd6f4,
        0xcba6f7,
    ),
    (
        "Dracula", 0x20212a, 0x282a36, 0x36394a, 0x44475a, 0xf8f8f2, 0xbd93f9,
    ),
];

fn color(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn set_theme(ctx: &Context, theme: usize) {
    let (_, window, panel, hover, border, text, accent) = THEMES[theme.min(THEMES.len() - 1)];
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = color(panel);
    visuals.window_fill = color(window);
    visuals.extreme_bg_color = color(window);
    visuals.faint_bg_color = color(hover).gamma_multiply(0.42);
    visuals.override_text_color = Some(color(text));
    visuals.widgets.noninteractive.bg_fill = color(window);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, color(border));
    visuals.widgets.inactive.bg_fill = color(hover);
    visuals.widgets.inactive.weak_bg_fill = color(hover);
    visuals.widgets.hovered.bg_fill = color(border);
    visuals.widgets.hovered.weak_bg_fill = color(border);
    visuals.hyperlink_color = color(accent);
    visuals.selection.bg_fill = color(accent).gamma_multiply(0.3);
    visuals.selection.stroke = Stroke::new(1.0_f32, color(accent));
    ctx.set_visuals(visuals);
}

pub struct App {
    conn: Option<Connection>,
    icon: Option<TextureHandle>,
    account: Option<String>,
    connecting: bool,
    prefs: Preferences,
    folders: HashMap<String, Folder>,
    lists: HashMap<String, Vec<Node>>,
    pending: HashSet<String>,
    expanded: HashSet<String>,
    cached: HashSet<String>,
    current: String,
    selected: Option<String>,
    listing_error: HashMap<String, String>,
    sort: Sort,
    descending: bool,
    query: String,
    rows: Vec<usize>,
    tree_rows: Vec<(String, usize)>,
    thumbnail: Option<(String, Result<TextureHandle, String>)>,
    thumbnail_pending: Option<String>,
    dialog: Option<Dialog>,
    notice: Option<(bool, String)>,
    syncing: bool,
    sync_report: Option<Result<Report, String>>,
    next_picker: u64,
}

impl App {
    pub fn new(ctx: &Context, icon: Option<egui::IconData>) -> Self {
        set_theme(ctx, 0);
        let icon = icon.map(|icon| {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            ctx.load_texture("application-icon", image, egui::TextureOptions::LINEAR)
        });
        let (conn, notice) = match worker::connect(ctx.clone()) {
            Ok(conn) => (Some(conn), None),
            Err(e) => (None, Some((true, format!("{e:#}")))),
        };
        let mut app = Self {
            icon,
            connecting: conn.is_some(),
            conn,
            account: None,
            notice,
            prefs: Preferences::default(),
            folders: HashMap::new(),
            lists: HashMap::new(),
            pending: HashSet::new(),
            expanded: HashSet::new(),
            cached: HashSet::new(),
            current: ROOT.into(),
            selected: None,
            listing_error: HashMap::new(),
            sort: Sort::Name,
            descending: false,
            query: String::new(),
            rows: Vec::new(),
            tree_rows: Vec::new(),
            thumbnail: None,
            thumbnail_pending: None,
            dialog: None,
            syncing: false,
            sync_report: None,
            next_picker: 0,
        };
        app.reset();
        app
    }
    fn reset(&mut self) {
        self.folders.clear();
        self.folders.insert(
            ROOT.into(),
            Folder {
                path: ROOT.into(),
                name: "My files".into(),
                parent: None,
            },
        );
        self.lists.clear();
        self.pending.clear();
        self.cached.clear();
        self.expanded = HashSet::from([ROOT.into()]);
        self.current = ROOT.into();
        self.selected = None;
        self.query.clear();
        self.listing_error.clear();
        self.rows.clear();
        self.index_tree();
        self.thumbnail = None;
        self.thumbnail_pending = None;
        self.dialog = None;
        self.sync_report = None;
    }
    fn send(&mut self, request: Request) {
        if self
            .conn
            .as_ref()
            .is_some_and(|conn| conn.send.send(request).is_err())
        {
            self.notice = Some((true, "The Proton Drive worker stopped".into()));
        }
    }
    fn list(&mut self, path: String, refresh: bool) {
        if self.account.is_some()
            && (refresh || !self.lists.contains_key(&path))
            && self.pending.insert(path.clone())
        {
            self.listing_error.remove(&path);
            self.send(Request::List(path));
        }
    }
    fn go(&mut self, path: String) {
        if self.current != path {
            self.current = path.clone();
            self.selected = None;
            self.query.clear();
            self.thumbnail = None;
            self.index();
        }
        self.list(path, true);
    }
    fn index(&mut self) {
        let query = self.query.to_lowercase();
        let Some(nodes) = self.lists.get(&self.current) else {
            self.rows.clear();
            return;
        };
        let mut keys: Vec<_> = nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| {
                let key = n.name().to_lowercase();
                key.contains(&query).then_some((i, key))
            })
            .collect();
        keys.sort_unstable_by(|(ai, name_a), (bi, name_b)| {
            let a = &nodes[*ai];
            let b = &nodes[*bi];
            let by_name = || name_a.cmp(name_b).then_with(|| a.uid.cmp(&b.uid));
            let order = match self.sort {
                Sort::Name => by_name(),
                Sort::Size => a.size().cmp(&b.size()).then_with(by_name),
                Sort::Date => a
                    .modification_time
                    .cmp(&b.modification_time)
                    .then_with(by_name),
            };
            b.folder().cmp(&a.folder()).then(if self.descending {
                order.reverse()
            } else {
                order
            })
        });
        self.rows = keys.into_iter().map(|(i, _)| i).collect();
    }
    fn events(&mut self, ctx: &Context) {
        while let Some(event) = self
            .conn
            .as_ref()
            .and_then(|conn| conn.receive.try_recv().ok())
        {
            match event {
                Event::Preferences(prefs) => {
                    if self.prefs.theme != prefs.theme {
                        set_theme(ctx, prefs.theme);
                    }
                    self.prefs = prefs;
                }
                Event::Account(account) => {
                    self.reset();
                    self.account = account;
                    self.notice = None;
                    self.list(ROOT.into(), true);
                }
                Event::Connecting(state) => self.connecting = state,
                Event::Listing {
                    path,
                    nodes,
                    cached,
                } => {
                    // Keep the in-memory listing while an already visited folder refreshes.
                    if cached && self.lists.contains_key(&path) {
                        continue;
                    }
                    if cached {
                        self.cached.insert(path.clone());
                    } else {
                        self.cached.remove(&path);
                    }
                    for node in nodes.iter().filter(|n| n.folder()) {
                        let node_path = node.path();
                        self.folders.insert(
                            node_path.clone(),
                            Folder {
                                path: node_path,
                                name: node.name().into(),
                                parent: Some(path.clone()),
                            },
                        );
                    }
                    self.lists.insert(path.clone(), nodes);
                    self.index_tree();
                    if path == self.current {
                        self.index();
                    }
                }
                Event::Listed { path, error } => {
                    self.pending.remove(&path);
                    if let Some(error) = error {
                        self.listing_error.insert(path, error);
                    }
                }
                Event::Error(error) => self.notice = Some((true, error)),
                Event::Notice(notice) => self.notice = Some((false, notice)),
                Event::Thumbnail { key, result } => {
                    if self.thumbnail_pending.as_deref() == Some(key.as_str()) {
                        self.thumbnail_pending = None;
                        self.thumbnail = Some((
                            key.clone(),
                            result.map(|img| {
                                ctx.load_texture(key, img, egui::TextureOptions::LINEAR)
                            }),
                        ));
                    }
                }
                Event::Sharing { path, result } => {
                    if let Some(Dialog::Share {
                        node,
                        pending,
                        result: slot,
                        ..
                    }) = &mut self.dialog
                        && node.path() == path
                    {
                        *pending = false;
                        *slot = Some(result);
                    }
                }
                Event::Syncing => self.syncing = true,
                Event::Synced(report) => {
                    self.syncing = false;
                    let changed = report.as_ref().is_ok_and(|r| {
                        r.uploads > 0 || r.downloads > 0 || r.moves > 0 || r.deletions > 0
                    });
                    self.sync_report = Some(report);
                    if changed {
                        self.list(self.current.clone(), true);
                    }
                }
                Event::PickedDirectory(id, path) => match self.dialog.as_mut() {
                    Some(Dialog::Sync { local, picker, .. })
                    | Some(Dialog::Save {
                        dir: local, picker, ..
                    }) if *picker == Some(id) => {
                        *picker = None;
                        if let Some(path) = path {
                            *local = path;
                        }
                    }
                    _ => {}
                },
            }
        }
    }
    fn bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if let Some(icon) = &self.icon {
                ui.image((icon.id(), Vec2::splat(25.0)));
            }
            ui.heading("Fuzzy Proton Drive");
            ui.label(RichText::new("PROTON CLI").small().weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.account.is_some() {
                    if ui
                        .add_enabled(!self.syncing, Button::new("Sign out"))
                        .clicked()
                    {
                        self.send(Request::Logout);
                    }
                } else if ui
                    .add_enabled(
                        self.conn.is_some() && !self.connecting,
                        Button::new("Sign in"),
                    )
                    .clicked()
                {
                    self.connecting = true;
                    self.send(Request::Connect(true));
                }
                egui::ComboBox::from_id_salt("theme")
                    .selected_text(THEMES[self.prefs.theme.min(THEMES.len() - 1)].0)
                    .show_ui(ui, |ui| {
                        let previous = self.prefs.theme;
                        for (i, theme) in THEMES.iter().enumerate() {
                            ui.selectable_value(&mut self.prefs.theme, i, theme.0);
                        }
                        if self.prefs.theme != previous {
                            set_theme(ui.ctx(), self.prefs.theme);
                            self.send(Request::ViewPrefs {
                                theme: self.prefs.theme,
                                details: self.prefs.details,
                            });
                        }
                    });
                if self.connecting {
                    ui.spinner();
                }
            });
        });
    }
    fn index_tree(&mut self) {
        fn flatten(app: &App, rows: &mut Vec<(String, usize)>, path: String, depth: usize) {
            if depth > 64 {
                return;
            }
            if app.expanded.contains(&path) {
                let mut children: Vec<_> = app
                    .lists
                    .get(&path)
                    .into_iter()
                    .flatten()
                    .filter(|n| n.folder())
                    .map(|n| (n.name().to_lowercase(), n.path()))
                    .collect();
                children.sort_unstable();
                rows.push((path, depth));
                for (_, child) in children {
                    flatten(app, rows, child, depth + 1);
                }
            } else {
                rows.push((path, depth));
            }
        }
        let mut rows = std::mem::take(&mut self.tree_rows);
        rows.clear();
        flatten(self, &mut rows, ROOT.into(), 0);
        self.tree_rows = rows;
    }
    fn sidebar(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("DIRECTORIES").small().weak());
        ui.add_space(6.0);
        let mut toggle = None;
        let mut go = None;
        egui::ScrollArea::vertical().show_rows(ui, 22.0, self.tree_rows.len(), |ui, range| {
            for (path, depth) in &self.tree_rows[range] {
                let Some(folder) = self.folders.get(path) else {
                    continue;
                };
                let open = self.expanded.contains(path);
                ui.horizontal(|ui| {
                    ui.set_min_height(22.0);
                    ui.add_space((*depth as f32 * 12.0).min(ui.available_width() * 0.5));
                    if ui.small_button(if open { "▾" } else { "▸" }).clicked() {
                        toggle = Some(path.clone());
                    }
                    if ui
                        .add(Button::selectable(self.current == *path, &folder.name).truncate())
                        .clicked()
                    {
                        go = Some(path.clone());
                    }
                });
            }
        });
        if let Some(path) = toggle {
            if !self.expanded.remove(&path) {
                self.expanded.insert(path.clone());
                self.list(path, false);
            }
            self.index_tree();
        }
        if let Some(path) = go {
            self.expanded.insert(path.clone());
            self.go(path);
            self.index_tree();
        }
    }
    fn status(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let binding = self.prefs.binding.as_ref();
            if self.syncing {
                ui.spinner();
            }
            let state = match binding {
                Some(binding) if self.account.as_ref() != Some(&binding.account) => {
                    "Sync: different account"
                }
                Some(_) if self.syncing => "Sync: working…",
                Some(_) if self.prefs.paused => "Sync: paused",
                Some(_) => "Sync: active",
                None => "Sync: not configured",
            };
            ui.label(state);
            if let Some(binding) = binding {
                ui.label(RichText::new(&binding.remote_label).weak())
                    .on_hover_text(binding.local.display().to_string());
            }
            if let Some(report) = &self.sync_report {
                let label = match report {
                    Ok(report) => format!(
                        "{} changes · {} issues",
                        report.uploads + report.downloads + report.moves + report.deletions,
                        report.conflicts
                    ),
                    Err(_) => "Sync error".into(),
                };
                if ui.small_button(label).clicked() {
                    self.dialog = Some(Dialog::SyncLog);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(
                        self.account.is_some() && !self.syncing,
                        Button::new("Set up sync…"),
                    )
                    .clicked()
                {
                    self.dialog = Some(Dialog::Sync {
                        local: String::new(),
                        remote: self.current.clone(),
                        label: self
                            .folders
                            .get(&self.current)
                            .map(|f| f.name.clone())
                            .unwrap_or_default(),
                        picker: None,
                    });
                }
                let usable = self
                    .prefs
                    .binding
                    .as_ref()
                    .is_some_and(|b| Some(&b.account) == self.account.as_ref());
                if usable {
                    if ui
                        .add_enabled(
                            !self.syncing,
                            Button::new(if self.prefs.paused { "Resume" } else { "Pause" }),
                        )
                        .clicked()
                    {
                        self.send(Request::Pause(!self.prefs.paused));
                    }
                    if ui
                        .add_enabled(!self.syncing && !self.prefs.paused, Button::new("Sync now"))
                        .clicked()
                    {
                        self.send(Request::Sync);
                    }
                }
            });
        });
    }
    fn breadcrumbs(&mut self, ui: &mut Ui) {
        let mut crumbs = Vec::new();
        let mut path = Some(self.current.as_str());
        while let Some(current) = path {
            let Some(folder) = self.folders.get(current) else {
                break;
            };
            crumbs.push((folder.path.clone(), folder.name.clone()));
            path = folder.parent.as_deref();
            if crumbs.len() > 65 {
                break;
            }
        }
        ui.horizontal_wrapped(|ui| {
            for (index, (path, name)) in crumbs.into_iter().rev().enumerate() {
                if index > 0 {
                    ui.label(RichText::new("/").weak());
                }
                if ui.link(name).clicked() {
                    self.go(path);
                }
            }
        });
    }
    fn selected_node(&self) -> Option<Node> {
        let uid = self.selected.as_deref()?;
        self.lists
            .get(&self.current)?
            .iter()
            .find(|n| n.uid == uid)
            .cloned()
    }
    fn inspector(&mut self, ui: &mut Ui, node: Node) {
        ui.label(RichText::new("ITEM DETAILS").small().weak());
        ui.add_space(8.0);
        ui.heading(node.name());
        ui.label(RichText::new(node.media_type.as_deref().unwrap_or(&node.kind)).weak());
        ui.label(size(node.size()));
        ui.label(format!(
            "Modified: {}",
            date(node.modification_time.as_deref())
        ));
        if node.is_shared {
            ui.label("Shared");
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Share…").clicked() {
                self.open_share(node.clone());
            }
            if !node.folder() && ui.button("Save to…").clicked() {
                self.dialog = Some(Dialog::Save {
                    node: node.clone(),
                    dir: String::new(),
                    picker: None,
                });
            }
        });
        if !node.folder() && node.visual() {
            ui.add_space(8.0);
            let key = worker::thumbnail_key(&node);
            if let Some((_, preview)) = self.thumbnail.as_ref().filter(|(k, _)| k == &key) {
                match preview {
                    Ok(tex) => {
                        ui.add(egui::Image::new(tex).max_width(ui.available_width()));
                    }
                    Err(err) => {
                        ui.label(err);
                    }
                }
            } else if self.thumbnail_pending.as_ref() == Some(&key) {
                ui.spinner();
            } else {
                let allowed = node.size().is_some_and(|s| s <= PREVIEW_LIMIT);
                if ui
                    .add_enabled(
                        allowed && self.thumbnail_pending.is_none(),
                        Button::new("Load thumbnail"),
                    )
                    .on_disabled_hover_text("Preview requires a known file size of 32 MiB or less")
                    .clicked()
                {
                    self.thumbnail_pending = Some(key);
                    self.send(Request::Thumbnail(Box::new(node)));
                }
                ui.label(
                    RichText::new("Downloads the original to generate a still preview.")
                        .small()
                        .weak(),
                );
            }
        }
    }
    fn open_share(&mut self, node: Node) {
        self.send(Request::Share(node.path(), ShareAction::Check));
        self.dialog = Some(Dialog::Share {
            node,
            email: String::new(),
            edit: false,
            pending: true,
            result: None,
            confirm_link: false,
        });
    }
    fn toolbar(&mut self, ui: &mut Ui) {
        self.breadcrumbs(ui);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                self.list(self.current.clone(), true);
            }
            if self.pending.contains(&self.current) {
                ui.spinner();
            }
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Filter this folder")
                        .desired_width(168.0),
                )
                .changed()
            {
                self.index();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let before = self.prefs.details;
                ui.selectable_value(&mut self.prefs.details, true, "Details");
                ui.selectable_value(&mut self.prefs.details, false, "List");
                if before != self.prefs.details {
                    self.send(Request::ViewPrefs {
                        theme: self.prefs.theme,
                        details: self.prefs.details,
                    });
                }
            });
        });
        if let Some(error) = self.listing_error.get(&self.current) {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("Could not refresh: {error}"),
            );
        } else if self.cached.contains(&self.current) {
            ui.label(RichText::new("Showing cached results").weak());
        }
        ui.add_space(8.0);
    }
    fn table(&mut self, ui: &mut Ui) {
        let Some(nodes) = self.lists.get(&self.current) else {
            if self.pending.contains(&self.current) {
                ui.label("Loading folder…");
            }
            return;
        };
        let mut sort_change = None;
        let mut select = None;
        let mut open = None;
        let mut share = None;
        let mut save = None;
        let details = self.prefs.details;
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(118.0));
        if details {
            table = table
                .column(Column::exact(86.0))
                .column(Column::exact(122.0));
        }
        table
            .header(26.0, |mut header| {
                let headers: &[(&str, Sort)] = if details {
                    &[
                        ("Name", Sort::Name),
                        ("Size", Sort::Size),
                        ("Modified", Sort::Date),
                    ]
                } else {
                    &[("Name", Sort::Name)]
                };
                for &(label, sort) in headers {
                    header.col(|ui| {
                        let marker = if sort == self.sort {
                            if self.descending { " ↓" } else { " ↑" }
                        } else {
                            ""
                        };
                        if ui
                            .add(Button::new(format!("{label}{marker}")).frame(false))
                            .clicked()
                        {
                            sort_change = Some(sort);
                        }
                        if !details {
                            egui::ComboBox::from_id_salt("list-sort")
                                .width(72.0)
                                .selected_text("Sort")
                                .show_ui(ui, |ui| {
                                    for (label, sort) in [
                                        ("Name", Sort::Name),
                                        ("Size", Sort::Size),
                                        ("Date", Sort::Date),
                                    ] {
                                        if ui.selectable_label(self.sort == sort, label).clicked() {
                                            sort_change = Some(sort);
                                        }
                                    }
                                });
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(30.0, self.rows.len(), |mut row| {
                    let node = &nodes[self.rows[row.index()]];
                    row.set_selected(self.selected.as_deref() == Some(node.uid.as_str()));
                    row.col(|ui| {
                        ui.label(
                            RichText::new(if node.folder() { "▰" } else { "▪" })
                                .color(ui.visuals().hyperlink_color),
                        );
                        ui.add(egui::Label::new(node.name()).truncate())
                            .on_hover_text(node.name());
                        if node.is_shared {
                            ui.label(RichText::new("shared").small().weak());
                        }
                    });
                    if details {
                        row.col(|ui| {
                            ui.label(size(node.size()));
                        });
                        row.col(|ui| {
                            ui.label(date(node.modification_time.as_deref()));
                        });
                    }
                    let response = row.response();
                    if response.clicked() {
                        select = Some(node.uid.clone());
                    }
                    if response.double_clicked() {
                        if node.folder() {
                            open = Some(node.path());
                        } else {
                            select = Some(node.uid.clone());
                        }
                    }
                    response.context_menu(|ui| {
                        if node.folder() && ui.button("Open folder").clicked() {
                            open = Some(node.path());
                            ui.close();
                        }
                        if ui.button("Share…").clicked() {
                            share = Some(node.clone());
                            ui.close();
                        }
                        if !node.folder() && ui.button("Save to…").clicked() {
                            save = Some(node.clone());
                            ui.close();
                        }
                    });
                });
            });
        if let Some(sort) = sort_change {
            self.descending = sort == self.sort && !self.descending;
            self.sort = sort;
            self.index();
        }
        if let Some(uid) = select {
            self.selected = Some(uid);
        }
        if let Some(path) = open {
            self.expanded.insert(path.clone());
            self.go(path);
            self.index_tree();
        }
        if let Some(node) = share {
            self.open_share(node);
        }
        if let Some(node) = save {
            self.dialog = Some(Dialog::Save {
                node,
                dir: String::new(),
                picker: None,
            });
        }
        if self.rows.is_empty() {
            ui.label(if self.query.is_empty() {
                "This folder is empty."
            } else {
                "No matching files."
            });
        }
    }
    fn modal(&mut self, ctx: &Context) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let title = match dialog {
            Dialog::Sync { .. } => "Set up two-way sync",
            Dialog::Share { .. } => "Sharing",
            Dialog::Save { .. } => "Save a file",
            Dialog::SyncLog => "Last sync pass",
        };
        let mut open = true;
        let mut done = false;
        egui::Window::new(title).open(&mut open).collapsible(false).default_width(380.0).show(ctx, |ui| {
            match &mut dialog {
                Dialog::Sync { local, remote, label, picker } => {
                    ui.label(format!("Remote folder: {label}"));
                    ui.label("File edits, moves and renames sync in both directions. Deletions send the unchanged copy to Trash. Conflicting changes are kept for review.");
                    ui.label("Existing local folder (absolute path)");
                    self.directory_input(ui, local, picker);
                    if ui.add_enabled(local.trim().starts_with('/'), Button::new("Start syncing")).clicked() {
                        self.send(Request::Configure { remote: remote.clone(), label: label.clone(),
                            local: PathBuf::from(local.trim()) });
                        done = true;
                    }
                }
                Dialog::Save { node, dir, picker } => {
                    ui.label(node.name());
                    ui.label("Existing destination folder (absolute path)");
                    self.directory_input(ui, dir, picker);
                    if ui.add_enabled(dir.trim().starts_with('/'), Button::new("Save file")).clicked() {
                        self.notice = Some((false, "Download queued…".into()));
                        self.send(Request::Save(Box::new(node.clone()), PathBuf::from(dir.trim())));
                        done = true;
                    }
                }
                Dialog::Share { node, email, edit, pending, result, confirm_link } => {
                    ui.heading(node.name());
                    if *pending { ui.spinner(); }
                    if let Some(Ok(sharing)) = result {
                        ui.label(format!("{} members · {} pending invitations", sharing.members.len(),
                            sharing.proton_invitations.len() + sharing.non_proton_invitations.len()));
                        if let Some(link) = &sharing.url_access {
                            ui.label(format!("Public link ({})", link.role));
                            ui.horizontal(|ui| {
                                if ui.button("Copy link").clicked() { ctx.copy_text(link.url.clone()); }
                                if ui.add_enabled(!*pending, Button::new("Revoke link")).clicked() {
                                    *pending = true;
                                    self.send(Request::Share(node.path(), ShareAction::Unlink));
                                }
                            });
                        } else if *confirm_link {
                            ui.label("Anyone with the link will be able to view this item.");
                            if ui.add_enabled(!*pending, Button::new("Confirm public link")).clicked() {
                                *confirm_link = false; *pending = true;
                                self.send(Request::Share(node.path(), ShareAction::Link));
                            }
                        } else if ui.add_enabled(!*pending, Button::new("Create public link…")).clicked() {
                            *confirm_link = true;
                        }
                    } else if let Some(Err(err)) = result {
                        ui.colored_label(ui.visuals().warn_fg_color, err.as_str());
                        if ui.add_enabled(!*pending, Button::new("Reload sharing status")).clicked() {
                            *pending = true;
                            self.send(Request::Share(node.path(), ShareAction::Check));
                        }
                    }
                    ui.separator();
                    ui.label("Invite by email");
                    ui.text_edit_singleline(email);
                    ui.checkbox(edit, "Allow editing");
                    if ui.add_enabled(!*pending && email.contains('@'), Button::new("Send invitation")).clicked() {
                        *pending = true;
                        self.send(Request::Share(node.path(), ShareAction::Invite { email: email.clone(), edit: *edit }));
                    }
                }
                Dialog::SyncLog => {
                    match &self.sync_report {
                        Some(Ok(report)) => {
                            ui.label(format!(
                                "{} uploaded · {} downloaded · {} moved or renamed · {} trashed",
                                report.uploads, report.downloads, report.moves, report.deletions
                            ));
                            if report.details.is_empty() {
                                ui.label("No issues in the last pass.");
                            } else {
                                ui.label(format!("{} issues (showing up to 50)", report.conflicts));
                                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                                    for item in &report.details { ui.label(item); }
                                });
                            }
                        }
                        Some(Err(err)) => { ui.label(err); }
                        None => {}
                    }
                }
            }
        });
        if open && !done {
            self.dialog = Some(dialog);
        }
    }

    fn directory_input(&mut self, ui: &mut Ui, path: &mut String, picker: &mut Option<u64>) {
        ui.horizontal(|ui| {
            ui.text_edit_singleline(path);
            if ui
                .add_enabled(picker.is_none(), Button::new("Browse…"))
                .on_hover_text("Use the KDE file picker if kdialog is installed")
                .clicked()
            {
                self.next_picker = self.next_picker.wrapping_add(1);
                *picker = Some(self.next_picker);
                self.send(Request::PickDirectory(self.next_picker));
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.events(ctx);
        let panel = Frame::side_top_panel(&ctx.style()).inner_margin(Margin::symmetric(14, 10));
        egui::TopBottomPanel::top("app-bar")
            .frame(panel)
            .show(ctx, |ui| self.bar(ui));
        if self.account.is_some() {
            egui::TopBottomPanel::bottom("app-status")
                .frame(panel)
                .show(ctx, |ui| self.status(ui));
        }
        if let Some((error, message)) = self.notice.clone() {
            egui::TopBottomPanel::bottom("app-notice")
                .frame(panel)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui.small_button("Dismiss").clicked() {
                            self.notice = None;
                        }
                        if error {
                            ui.colored_label(ui.visuals().warn_fg_color, message);
                        } else {
                            ui.label(message);
                        }
                    });
                });
        }
        if self.account.is_some() {
            egui::SidePanel::left("folder-tree")
                .default_width(200.0)
                .width_range(145.0..=320.0)
                .frame(panel)
                .show(ctx, |ui| self.sidebar(ui));
            if let Some(node) = self.selected_node() {
                egui::SidePanel::right("file-inspector")
                    .default_width(220.0)
                    .width_range(155.0..=330.0)
                    .frame(panel)
                    .show(ctx, |ui| self.inspector(ui, node));
            }
        }
        egui::CentralPanel::default().frame(
            Frame::central_panel(&ctx.style()).inner_margin(Margin::same(16)),
        ).show(ctx, |ui| {
            if self.account.is_some() {
                self.toolbar(ui);
                self.table(ui);
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(44.0);
                    ui.heading("Your Proton Drive, on your desktop");
                    ui.add_space(10.0);
                    ui.label("Sign in with the official Proton Drive CLI using your default browser.");
                    ui.add_space(16.0);
                    if self.connecting {
                        ui.spinner();
                        ui.label("Checking the CLI or waiting for your browser sign-in…");
                    } else {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(self.conn.is_some(), Button::new("Sign in in browser")
                                .min_size(Vec2::new(130.0, 30.0))).clicked() {
                                self.connecting = true;
                                self.send(Request::Connect(true));
                            }
                            if ui.add_enabled(self.conn.is_some(), Button::new("Use existing CLI session")).clicked() {
                                self.connecting = true;
                                self.send(Request::Connect(false));
                            }
                        });
                    }
                });
            }
        });
        self.modal(ctx);
    }
}

fn size(size: Option<u64>) -> String {
    match size {
        Some(n) if n >= 1 << 30 => format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64),
        Some(n) if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64),
        Some(n) if n >= 1 << 10 => format!("{:.1} KiB", n as f64 / (1u64 << 10) as f64),
        Some(n) => format!("{n} B"),
        None => "—".into(),
    }
}

fn date(date: Option<&str>) -> &str {
    date.and_then(|s| s.get(..10)).unwrap_or("—")
}

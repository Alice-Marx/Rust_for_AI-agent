//! Durable service-owned work, distinct from local terminal tabs and API chat.
use super::*;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use wonderland::workflow::{
    ProjectRecord, WorkflowCreate, WorkflowEvent, WorkflowRecord, WorkflowStatus,
};
#[path = "teams.rs"]
mod teams;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Work,
    Chat,
    Teams,
    Apps,
    Projects,
    Api,
}

#[derive(Serialize, Deserialize)]
struct SavedView {
    source: String,
    page: Page,
    selected: Option<String>,
    board: bool,
    #[serde(default)]
    team_selected: Option<String>,
}

enum Reply {
    AppProbe {
        source: String,
        app_id: String,
        result: Result<Value, String>,
    },
    Snapshot {
        source: String,
        records: Result<Vec<WorkflowRecord>, String>,
        apps: Option<Result<Vec<Value>, String>>,
        projects: Option<Result<Vec<ProjectRecord>, String>>,
        events: Option<(String, Result<Vec<WorkflowEvent>, String>)>,
    },
    Mutation {
        source: String,
        result: Result<Option<WorkflowRecord>, String>,
    },
    Created {
        source: String,
        record: WorkflowRecord,
    },
    Project {
        source: String,
        result: Result<ProjectRecord, String>,
    },
    Directory(String),
}

pub struct Studio {
    pub page: Page,
    records: Vec<WorkflowRecord>,
    apps: Vec<Value>,
    diagnostics: HashMap<String, Value>,
    probing: Option<String>,
    projects: Vec<ProjectRecord>,
    events: HashMap<String, Vec<WorkflowEvent>>,
    selected: Option<String>,
    pub launch_cli: Option<String>,
    pub open_project: Option<String>,
    pub choose_project: bool,
    tx: mpsc::Sender<Reply>,
    rx: mpsc::Receiver<Reply>,
    source: String,
    polling: bool,
    busy: bool,
    next_poll: Instant,
    next_apps: Instant,
    pub notice: String,
    connected: bool,
    board: bool,
    composing: bool,
    title: String,
    prompt: String,
    cwd: String,
    workspace: String,
    app_id: String,
    model: String,
    read_only: bool,
    minutes: u64,
    acceptance: String,
    evidence: String,
    answers: HashMap<String, Answer>,
    search: String,
    teams: teams::Teams,
    #[cfg(feature = "ui-snapshots")]
    fixture: bool,
}

impl Studio {
    pub fn new(cwd: &str) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            page: Page::Work,
            records: vec![],
            apps: local_apps(),
            diagnostics: HashMap::new(),
            probing: None,
            projects: vec![],
            events: HashMap::new(),
            selected: None,
            launch_cli: None,
            open_project: None,
            choose_project: false,
            tx,
            rx,
            source: String::new(),
            polling: false,
            busy: false,
            next_poll: Instant::now(),
            next_apps: Instant::now(),
            notice: String::new(),
            connected: false,
            board: false,
            composing: true,
            title: String::new(),
            prompt: String::new(),
            cwd: cwd.into(),
            workspace: cwd.into(),
            app_id: "codex".into(),
            model: String::new(),
            read_only: false,
            minutes: 30,
            acceptance: String::new(),
            evidence: String::new(),
            answers: HashMap::new(),
            search: String::new(),
            teams: teams::Teams::new(cwd),
            #[cfg(feature = "ui-snapshots")]
            fixture: false,
        }
    }
    pub fn restore(&mut self, storage: &dyn eframe::Storage, server: &str) {
        if let Some(saved) = eframe::get_value::<SavedView>(storage, "studio_view") {
            if saved.source == server {
                self.source = saved.source;
                self.page = saved.page;
                self.selected = saved.selected;
                self.board = saved.board;
                self.teams.selected = saved.team_selected;
                self.composing = self.selected.is_none() && !self.board;
            }
        }
    }
    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(
            storage,
            "studio_view",
            &SavedView {
                source: self.source.clone(),
                page: self.page,
                selected: self.selected.clone(),
                board: self.board,
                team_selected: self.teams.selected.clone(),
            },
        );
    }
    pub fn set_page(&mut self, page: Page, cwd: &str) {
        if self.page != page && matches!(page, Page::Work | Page::Chat) {
            self.composing = true;
            self.selected = None;
            self.cwd = cwd.into();
        }
        self.page = page;
    }
    pub fn project_context(&mut self, cwd: &str) {
        self.teams.project_context(cwd);
        if self.workspace != cwd {
            if self.cwd == self.workspace || self.cwd.is_empty() {
                self.cwd = cwd.into();
            }
            self.workspace = cwd.into();
        }
    }
    fn merge_record(&mut self, record: WorkflowRecord) {
        if let Some(old) = self.records.iter_mut().find(|r| r.id == record.id) {
            if record.updated_at >= old.updated_at {
                *old = record;
            }
        } else {
            self.records.push(record);
        }
        self.records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    }
    pub fn poll(&mut self, ctx: &egui::Context, server: &str) {
        #[cfg(feature = "ui-snapshots")]
        if self.fixture {
            return;
        }
        self.teams.poll(ctx, server, self.page == Page::Teams);
        if self.source != server {
            self.source = server.into();
            self.records.clear();
            self.events.clear();
            self.apps = local_apps();
            self.diagnostics.clear();
            self.probing = None;
            self.projects.clear();
            self.selected = None;
            self.polling = false;
            self.busy = false;
            self.connected = false;
            self.next_poll = Instant::now();
            self.next_apps = Instant::now();
        }
        while let Ok(reply) = self.rx.try_recv() {
            match reply {
                Reply::AppProbe {
                    source,
                    app_id,
                    result,
                } if source == self.source => {
                    self.probing = None;
                    match result {
                        Ok(value) => {
                            self.diagnostics.insert(app_id, value);
                        }
                        Err(error) => self.notice = error,
                    }
                }
                Reply::Snapshot {
                    source,
                    records,
                    apps,
                    projects,
                    events,
                } if source == self.source => {
                    self.polling = false;
                    match records {
                        Ok(records) => {
                            self.connected = true;
                            for record in records {
                                self.merge_record(record);
                            }
                        }
                        Err(e) => {
                            self.connected = false;
                            self.notice = e;
                        }
                    }
                    if let Some(apps) = apps {
                        match apps {
                            Ok(a) => self.apps = a,
                            Err(e) => self.notice = e,
                        }
                    }
                    if let Some(projects) = projects {
                        match projects {
                            Ok(p) => self.projects = p,
                            Err(e) => self.notice = e,
                        }
                    }
                    if let Some((id, Ok(events))) = events {
                        let cached = self.events.entry(id).or_default();
                        for event in events {
                            if !cached.iter().any(|e| e.seq == event.seq) {
                                cached.push(event);
                            }
                        }
                        cached.sort_by_key(|e| e.seq);
                        if cached.len() > 2000 {
                            let pending = pending_requests(cached);
                            cached.drain(..cached.len() - 2000);
                            for event in pending {
                                if cached.first().is_some_and(|first| event.seq < first.seq) {
                                    cached.push(event);
                                }
                            }
                            cached.sort_by_key(|e| e.seq);
                        }
                    }
                    self.next_poll = Instant::now() + Duration::from_millis(1000);
                }
                Reply::Mutation { source, result } if source == self.source => {
                    self.busy = false;
                    match result {
                        Ok(Some(r)) => {
                            self.selected = Some(r.id.clone());
                            self.composing = false;
                            self.merge_record(r);
                            self.notice = "已保存到任务服务".into();
                        }
                        Ok(None) => self.notice = "操作已提交".into(),
                        Err(e) => self.notice = e,
                    }
                    self.next_poll = Instant::now();
                }
                Reply::Created { source, record } if source == self.source => {
                    self.selected = Some(record.id.clone());
                    self.composing = false;
                    self.board = false;
                    self.merge_record(record);
                }
                Reply::Project { source, result } if source == self.source => {
                    self.busy = false;
                    match result {
                        Ok(project) => {
                            self.open_project = Some(project.cwd.clone());
                            self.projects.retain(|p| p.id != project.id);
                            self.projects.insert(0, project);
                            self.notice.clear();
                        }
                        Err(e) => self.notice = e,
                    }
                    self.next_apps = Instant::now();
                }
                Reply::Directory(path) => self.cwd = path,
                _ => {}
            }
        }
        if !self.polling && Instant::now() >= self.next_poll {
            self.polling = true;
            let apps = Instant::now() >= self.next_apps;
            if apps {
                self.next_apps = Instant::now() + Duration::from_secs(20);
            }
            let selected = self.selected.clone().map(|id| {
                let after = self
                    .events
                    .get(&id)
                    .and_then(|e| e.last())
                    .map(|e| e.seq)
                    .unwrap_or(0);
                (id, after)
            });
            let (tx, source, ctx) = (self.tx.clone(), server.to_owned(), ctx.clone());
            thread::spawn(move || {
                let result = Runtime::new().map_err(|e| e.to_string()).map(|runtime| {
                    runtime.block_on(async {
                        let records = get_json::<Vec<WorkflowRecord>>(&source, "/api/v1/workflows");
                        let catalogue = async {
                            if apps {
                                Some(get_json::<Vec<Value>>(&source, "/api/v1/apps").await)
                            } else {
                                None
                            }
                        };
                        let projects = async {
                            if apps {
                                Some(
                                    get_json::<Vec<ProjectRecord>>(&source, "/api/v1/projects")
                                        .await,
                                )
                            } else {
                                None
                            }
                        };
                        let events =
                            async {
                                if let Some((id, after)) = selected {
                                    let result = get_json::<Vec<WorkflowEvent>>(
                                &source,
                                &format!("/api/v1/workflows/{id}/events?after={after}&limit=1000"),
                            )
                            .await;
                                    Some((id, result))
                                } else {
                                    None
                                }
                            };
                        tokio::join!(records, catalogue, projects, events)
                    })
                });
                let (records, apps, projects, events) =
                    result.unwrap_or_else(|e| (Err(e), None, None, None));
                let _ = tx.send(Reply::Snapshot {
                    source,
                    records,
                    apps,
                    projects,
                    events,
                });
                ctx.request_repaint();
            });
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }
    fn mutate(&mut self, ctx: &egui::Context, path: String, body: Value, start_after: bool) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.notice.clear();
        let (tx, source, ctx) = (self.tx.clone(), self.source.clone(), ctx.clone());
        thread::spawn(move || {
            let result = Runtime::new()
                .map_err(|e| e.to_string())
                .and_then(|runtime| {
                    runtime.block_on(async {
                        let value = post_json(&source, &path, body).await?;
                        let record = serde_json::from_value::<WorkflowRecord>(value).ok();
                        if start_after {
                            let record = record.ok_or_else(|| {
                                "创建成功但服务返回的任务格式无效，请刷新列表".to_string()
                            })?;
                            let _ = tx.send(Reply::Created {
                                source: source.clone(),
                                record: record.clone(),
                            });
                            ctx.request_repaint();
                            post_json(
                                &source,
                                &format!("/api/v1/workflows/{}/start", record.id),
                                json!({}),
                            )
                            .await
                            .map(|value| serde_json::from_value(value).ok())
                            .map_err(|e| format!("任务草稿 {} 已保留，启动失败：{e}", record.id))
                        } else {
                            Ok(record)
                        }
                    })
                });
            let _ = tx.send(Reply::Mutation { source, result });
            ctx.request_repaint();
        });
    }
    pub fn render(&mut self, ui: &mut Ui, cwd: &str) {
        ui.spacing_mut().item_spacing = Vec2::new(10., 10.);
        if !self.connected {
            ui.horizontal_wrapped(|ui| {
                icons::glyph(ui, icons::Icon::Link, 16., MUTED);
                ui.label(RichText::new("任务服务未连接").color(MUTED));
            });
        }
        if !self.notice.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&self.notice).small().color(MUTED));
                if ui.small_button("关闭提示").clicked() {
                    self.notice.clear();
                }
            });
        }
        match self.page {
            Page::Teams => {
                self.teams.render(ui, &self.apps);
                if let Some(project) = self.teams.open_project.take() {
                    self.open_project = Some(project);
                }
                if let Some(id) = self.teams.open_workflow.take() {
                    self.page = Page::Work;
                    self.select_record(&id);
                    let (tx, source, ctx) =
                        (self.tx.clone(), self.source.clone(), ui.ctx().clone());
                    thread::spawn(move || {
                        let result = Runtime::new().map_err(|e| e.to_string()).and_then(|rt| {
                            rt.block_on(get_json::<WorkflowRecord>(
                                &source,
                                &format!("/api/v1/workflows/{id}"),
                            ))
                            .map(Some)
                        });
                        let _ = tx.send(Reply::Mutation { source, result });
                        ctx.request_repaint();
                    });
                }
            }
            Page::Apps => self.render_apps(ui),
            Page::Projects => self.render_projects(ui, cwd),
            Page::Work | Page::Chat => self.render_work(ui, cwd),
            Page::Api => {}
        }
    }
    fn render_work(&mut self, ui: &mut Ui, cwd: &str) {
        let chat = self.page == Page::Chat;
        ui.horizontal(|ui| {
            ui.heading(if chat { "Chat" } else { "Work" });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .button(if chat {
                        "＋ 新讨论"
                    } else {
                        "＋ 新任务"
                    })
                    .clicked()
                {
                    self.composing = true;
                    self.selected = None;
                    self.cwd = cwd.into();
                    self.title.clear();
                    self.prompt.clear();
                    self.acceptance.clear();
                }
                if ui.selectable_label(self.board, "看板").clicked() {
                    self.board = true;
                    self.composing = false;
                }
                if ui
                    .selectable_label(
                        !self.board && !self.composing && self.selected.is_none(),
                        "列表",
                    )
                    .clicked()
                {
                    self.board = false;
                    self.composing = false;
                    self.selected = None;
                }
            });
        });
        ui.add_space(8.);
        if self.board && !self.composing {
            self.render_board(ui);
            return;
        }
        if self.composing {
            self.render_composer(ui, chat);
            return;
        }
        let selected = self
            .selected
            .as_ref()
            .and_then(|id| self.records.iter().find(|r| &r.id == id))
            .cloned();
        if let Some(record) = selected {
            self.render_detail(ui, &record);
        } else {
            self.render_list(ui);
        }
    }
    pub fn sidebar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("任务空间").strong().size(13.));
            if self.polling {
                ui.add(egui::Spinner::new().size(13.));
            }
        });
        ui.add(
            TextEdit::singleline(&mut self.search)
                .hint_text("搜索任务")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(8.);
        let mode = if self.page == Page::Chat {
            "chat"
        } else {
            "work"
        };
        let matching: Vec<_> = self
            .records
            .iter()
            .filter(|r| {
                r.mode == mode
                    && (self.search.is_empty()
                        || r.title.to_lowercase().contains(&self.search.to_lowercase()))
            })
            .cloned()
            .collect();
        if matching.is_empty() {
            ui.label(RichText::new("暂无任务").small().color(MUTED));
        }
        ScrollArea::vertical()
            .id_salt("workflow-list")
            .show(ui, |ui| {
                for record in matching {
                    let selected = self.selected.as_deref() == Some(&record.id) && !self.composing;
                    let frame = Frame::new()
                        .fill(if selected {
                            Color32::from_rgb(35, 52, 49)
                        } else {
                            PANEL
                        })
                        .corner_radius(6)
                        .inner_margin(10.);
                    let response = frame
                        .show(ui, |ui| {
                            ui.set_min_width((ui.available_width() - 20.).max(80.));
                            ui.label(
                                RichText::new(truncate(&record.title, 28))
                                    .strong()
                                    .size(13.),
                            );
                            ui.horizontal(|ui| {
                                status_chip(ui, record.status);
                                ui.label(
                                    RichText::new(app_label(&record.app_id))
                                        .size(10.)
                                        .color(MUTED),
                                );
                            });
                        })
                        .response;
                    if response.interact(egui::Sense::click()).clicked() {
                        self.select_record(&record.id);
                    }
                    ui.add_space(3.);
                }
            });
    }
    fn select_record(&mut self, id: &str) {
        self.selected = Some(id.into());
        self.composing = false;
        self.board = false;
        self.evidence.clear();
        self.next_poll = Instant::now();
    }
    fn render_composer(&mut self, ui: &mut Ui, chat: bool) {
        ScrollArea::vertical().id_salt("workflow-compose").show(ui,|ui|{
            ui.add_space(10.);
            ui.vertical(|ui|{
                ui.set_max_width(ui.available_width().min(840.));
                ui.horizontal(|ui|{icons::glyph(ui,if chat {icons::Icon::Chat}else{icons::Icon::Code},19.,ACCENT);ui.label(RichText::new(if chat {"新讨论"}else{"新任务"}).size(16.).strong());});
                ui.add_space(8.);
                ui.add(TextEdit::singleline(&mut self.title).hint_text("任务名称（可选）").desired_width(f32::INFINITY));
                ui.add(TextEdit::multiline(&mut self.prompt).hint_text(if chat {"讨论的问题与背景…"}else{"任务目标与范围…"}).desired_rows(6).desired_width(f32::INFINITY));
                ui.add_space(8.);
                ui.horizontal_wrapped(|ui|{
                    ui.label(RichText::new("执行应用").small().color(MUTED));
                    let previous_app=self.app_id.clone();
                    egui::ComboBox::from_id_salt("workflow-app").selected_text(app_label(&self.app_id)).width(180.).show_ui(ui,|ui|{for app in &self.apps {let id=app_id(app);if managed(app){ui.selectable_value(&mut self.app_id,id.to_owned(),app_name(app));}}});
                    if previous_app!=self.app_id {self.model.clear();}
                    ui.label(RichText::new("模型").small().color(MUTED));
                    ui.add(TextEdit::singleline(&mut self.model).hint_text("模型 ID").desired_width(200.)).on_hover_text("使用该官方应用账号中可用的模型 ID，任务启动后保持固定");
                });
                ui.add_space(5.);
                ui.horizontal(|ui|{
                    ui.label(RichText::new("项目").small().color(MUTED));
                    ui.add(TextEdit::singleline(&mut self.cwd).desired_width((ui.available_width()-52.).max(80.)));
                    if icon_button(ui,icons::Icon::Folder,"选择任务项目").clicked(){self.pick_directory(ui.ctx());}
                });
                ui.horizontal_wrapped(|ui|{
                    if chat {let mut read_only=true;ui.add_enabled(false,egui::Checkbox::new(&mut read_only,"只读工作区"));}else{ui.checkbox(&mut self.read_only,"只读工作区");}
                    ui.label(RichText::new("最长运行").small().color(MUTED));ui.add(egui::DragValue::new(&mut self.minutes).range(1..=1440).suffix(" 分钟"));
                });
                if !chat {egui::CollapsingHeader::new("验收标准").default_open(true).show(ui,|ui|{ui.add(TextEdit::multiline(&mut self.acceptance).hint_text("每行一项，例如：\n现有测试全部通过\n分页边界行为有回归覆盖").desired_rows(3).desired_width(f32::INFINITY));});}
                ui.separator();
                ui.horizontal_wrapped(|ui|{
                    let valid=self.connected&&!self.busy&&!self.prompt.trim().is_empty()&&!self.model.trim().is_empty()&&!self.cwd.trim().is_empty()&&self.apps.iter().any(|a|app_id(a)==self.app_id&&managed(a));
                    if ui.add_enabled(valid,egui::Button::new(RichText::new(if chat {"开始讨论  →"}else{"创建并开始  →"}).strong().color(BG)).fill(ACCENT)).clicked(){self.create(ui.ctx(),true,chat);}
                    if ui.add_enabled(valid,egui::Button::new("保存草稿")).clicked(){self.create(ui.ctx(),false,chat);}
                    if self.busy {ui.spinner();}
                });
            });
        });
    }
    fn pick_directory(&self, ctx: &egui::Context) {
        let (tx, ctx, cwd) = (self.tx.clone(), ctx.clone(), self.cwd.clone());
        thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("选择任务项目")
                .set_directory(cwd)
                .pick_folder()
            {
                let _ = tx.send(Reply::Directory(path.to_string_lossy().into_owned()));
                ctx.request_repaint();
            }
        });
    }
    fn create(&mut self, ctx: &egui::Context, start: bool, chat: bool) {
        let create = WorkflowCreate {
            title: self.title.trim().into(),
            prompt: self.prompt.trim().into(),
            cwd: self.cwd.trim().into(),
            mode: if chat { "chat".into() } else { "work".into() },
            app_id: self.app_id.clone(),
            model: self.model.trim().into(),
            read_only: chat || self.read_only,
            max_duration_secs: self.minutes.clamp(1, 1440) * 60,
            acceptance: if chat {
                vec![]
            } else {
                self.acceptance
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            },
        };
        self.mutate(ctx, "/api/v1/workflows".into(), json!(create), start);
    }
    fn render_board(&mut self, ui: &mut Ui) {
        let mode = if self.page == Page::Chat {
            "chat"
        } else {
            "work"
        };
        let records: Vec<_> = self
            .records
            .iter()
            .filter(|r| r.mode == mode)
            .cloned()
            .collect();
        if records.is_empty() {
            ui.add_space(24.);
            ui.label(RichText::new("暂无任务").color(MUTED));
            return;
        }
        let lanes = [("待开始", 0), ("进行中", 1), ("待处理", 2), ("已结束", 3)];
        let count = if ui.available_width() < 740. { 2 } else { 4 };
        ScrollArea::vertical()
            .id_salt("workflow-board")
            .show(ui, |ui| {
                for row in lanes.chunks(count) {
                    ui.columns(count, |columns| {
                        for (column, (name, lane)) in columns.iter_mut().zip(row) {
                            let items: Vec<_> = records
                                .iter()
                                .filter(|r| lane_for(r.status) == *lane)
                                .collect();
                            column.horizontal(|ui| {
                                ui.strong(*name);
                                ui.label(
                                    RichText::new(items.len().to_string()).small().color(MUTED),
                                );
                            });
                            column.add_space(6.);
                            for record in items {
                                let response = Frame::new()
                                    .fill(PANEL)
                                    .stroke(Stroke::new(1.0_f32, BORDER))
                                    .corner_radius(8)
                                    .inner_margin(13.)
                                    .show(column, |ui| {
                                        ui.set_min_height(100.);
                                        ui.label(RichText::new(&record.title).strong());
                                        ui.add_space(9.);
                                        status_chip(ui, record.status);
                                        ui.label(
                                            RichText::new(app_label(&record.app_id))
                                                .small()
                                                .color(MUTED),
                                        );
                                        ui.label(RichText::new(&record.model).small().color(MUTED));
                                    })
                                    .response;
                                if response.interact(egui::Sense::click()).clicked() {
                                    self.select_record(&record.id);
                                }
                                column.add_space(8.);
                            }
                        }
                    });
                    ui.add_space(12.);
                }
            });
    }
    fn render_list(&mut self, ui: &mut Ui) {
        let mode = if self.page == Page::Chat {
            "chat"
        } else {
            "work"
        };
        let records: Vec<_> = self
            .records
            .iter()
            .filter(|r| r.mode == mode)
            .cloned()
            .collect();
        ScrollArea::vertical()
            .id_salt("workflow-table")
            .show(ui, |ui| {
                if records.is_empty() {
                    ui.label(RichText::new("暂无任务").color(MUTED));
                }
                for record in records {
                    ui.horizontal_wrapped(|ui| {
                        status_chip(ui, record.status);
                        if ui.link(&record.title).clicked() {
                            self.select_record(&record.id);
                        }
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                app_label(&record.app_id),
                                record.model
                            ))
                            .small()
                            .color(MUTED),
                        );
                    });
                    ui.label(RichText::new(&record.cwd).small().color(MUTED));
                    ui.separator();
                }
            });
    }
    fn render_detail(&mut self, ui: &mut Ui, record: &WorkflowRecord) {
        ui.horizontal_wrapped(|ui| {
            ui.heading(&record.title);
            status_chip(ui, record.status);
        });
        ui.horizontal_wrapped(|ui| {
            pill(ui, &app_label(&record.app_id), ACCENT);
            pill(ui, &record.model, MUTED);
            pill(
                ui,
                if record.read_only {
                    "只读"
                } else {
                    "可编辑"
                },
                MUTED,
            );
            ui.label(RichText::new(&record.cwd).small().color(MUTED));
        });
        ui.horizontal_wrapped(|ui| {
            if record.status == WorkflowStatus::Draft
                && ui
                    .add_enabled(!self.busy, egui::Button::new("开始任务"))
                    .clicked()
            {
                self.mutate(
                    ui.ctx(),
                    format!("/api/v1/workflows/{}/start", record.id),
                    json!({}),
                    false,
                );
            }
            if matches!(
                record.status,
                WorkflowStatus::Blocked
                    | WorkflowStatus::Failed
                    | WorkflowStatus::Interrupted
                    | WorkflowStatus::Cancelled
                    | WorkflowStatus::Succeeded
            ) && ui.button("复制为新任务").clicked()
            {
                self.title = record.title.clone();
                self.prompt = record.prompt.clone();
                self.cwd = record.cwd.clone();
                self.app_id = record.app_id.clone();
                self.model = record.model.clone();
                self.read_only = record.read_only;
                self.minutes = record.max_duration_secs.div_ceil(60);
                self.acceptance = record.acceptance.join("\n");
                self.selected = None;
                self.composing = true;
            }
            if ui
                .add_enabled(
                    !self.busy
                        && !matches!(
                            record.status,
                            WorkflowStatus::Succeeded | WorkflowStatus::Cancelled
                        ),
                    egui::Button::new("取消任务"),
                )
                .clicked()
            {
                self.mutate(
                    ui.ctx(),
                    format!("/api/v1/workflows/{}/cancel", record.id),
                    json!({}),
                    false,
                );
            }
            if ui.button("打开项目").clicked() {
                self.open_project = Some(record.cwd.clone());
            }
            if ui.button("复制结果").clicked() {
                ui.ctx().copy_text(record.output.clone());
            }
            if self.busy {
                ui.spinner();
            }
        });
        ui.add_space(7.);
        ScrollArea::vertical()
            .id_salt(("workflow-detail", &record.id))
            .show(ui, |ui| {
                egui::CollapsingHeader::new("任务说明与验收标准")
                    .default_open(
                        record.output.is_empty() && record.status != WorkflowStatus::WaitingInput,
                    )
                    .show(ui, |ui| {
                        super::desktop_ui::markdown(ui, &record.prompt);
                        for item in &record.acceptance {
                            ui.label(format!("• {item}"));
                        }
                    });
                if let Some(error) = &record.error {
                    Frame::new()
                        .fill(Color32::from_rgb(58, 33, 35))
                        .corner_radius(8)
                        .inner_margin(12.)
                        .show(ui, |ui| {
                            ui.label(RichText::new(error).color(Color32::from_rgb(247, 159, 158)));
                        });
                }
                let events = self.events.get(&record.id).cloned().unwrap_or_default();
                if matches!(
                    record.status,
                    WorkflowStatus::Running | WorkflowStatus::WaitingInput
                ) {
                    for pending in pending_requests(&events) {
                        self.render_request(ui, record, &pending);
                    }
                }
                if !record.output.is_empty() {
                    ui.add_space(12.);
                    ui.separator();
                    super::desktop_ui::markdown(ui, &record.output);
                }
                if record.status == WorkflowStatus::Running && record.output.is_empty() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("应用正在执行，事件会持续记录…");
                    });
                }
                if record.status == WorkflowStatus::Verifying {
                    ui.add_space(14.);
                    Frame::new()
                        .fill(Color32::from_rgb(32, 48, 43))
                        .stroke(Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.4)))
                        .corner_radius(8)
                        .inner_margin(16.)
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("执行结束，等待你的验收")
                                    .strong()
                                    .color(ACCENT),
                            );
                            ui.add(
                                TextEdit::multiline(&mut self.evidence)
                                    .hint_text("验收依据：检查的文件、运行的测试、结果…")
                                    .desired_rows(3)
                                    .desired_width(f32::INFINITY),
                            );
                            if ui
                                .add_enabled(
                                    !self.busy && !self.evidence.trim().is_empty(),
                                    egui::Button::new("确认验收完成"),
                                )
                                .clicked()
                            {
                                self.mutate(
                                    ui.ctx(),
                                    format!("/api/v1/workflows/{}/accept", record.id),
                                    json!({"evidence":self.evidence.trim()}),
                                    false,
                                );
                            }
                        });
                }
                ui.add_space(14.);
                egui::CollapsingHeader::new(format!("执行记录 · {}", events.len()))
                    .default_open(true)
                    .show(ui, |ui| {
                        for event in events.iter().rev().take(120) {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(format!("#{:04}", event.seq))
                                        .monospace()
                                        .small()
                                        .color(MUTED),
                                );
                                ui.label(RichText::new(event_label(&event.kind)).small().strong());
                                ui.label(
                                    RichText::new(truncate(&event_summary(&event.data), 180))
                                        .small()
                                        .color(MUTED),
                                );
                            });
                        }
                        if events.is_empty() {
                            ui.label(RichText::new("尚无执行事件").small().color(MUTED));
                        }
                    });
            });
    }
    fn render_request(&mut self, ui: &mut Ui, record: &WorkflowRecord, event: &WorkflowEvent) {
        let data = &event.data;
        let request_id = request_id(data);
        if request_id.is_empty() {
            return;
        }
        let permission = is_permission(&event.kind, data);
        Frame::new()
            .fill(Color32::from_rgb(49, 42, 27))
            .corner_radius(8)
            .inner_margin(16.)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(if permission {
                        "应用请求权限"
                    } else {
                        "应用需要你的回答"
                    })
                    .strong()
                    .color(Color32::from_rgb(236, 204, 136)),
                );
                if permission {
                    ui.label(RichText::new(event_summary(data)).size(12.));
                    if let Some(command) = data
                        .pointer("/data/command")
                        .or_else(|| data.pointer("/data/action"))
                        .and_then(Value::as_str)
                    {
                        ui.add(egui::Label::new(RichText::new(command).monospace().small()).wrap());
                    }
                    ui.horizontal(|ui| {
                        for (label, approve) in [("允许本次", true), ("拒绝", false)] {
                            if ui
                                .add_enabled(!self.busy, egui::Button::new(label))
                                .clicked()
                            {
                                self.mutate(
                                    ui.ctx(),
                                    format!("/api/v1/workflows/{}/approve", record.id),
                                    json!({"request_id":request_id,"approve":approve}),
                                    false,
                                );
                            }
                        }
                    });
                } else {
                    let questions = request_questions(data, &record.app_id);
                    let mut payload = serde_json::Map::new();
                    let mut complete = !questions.is_empty();
                    for (index, question) in questions.iter().enumerate() {
                        let key = format!("{}:{request_id}:{index}", record.id);
                        let answer = self.answers.entry(key.clone()).or_default();
                        ui.push_id(key, |ui| {
                            ui.add_space(6.);
                            if !question.header.is_empty() {
                                ui.label(RichText::new(&question.header).small().color(MUTED));
                            }
                            ui.label(RichText::new(&question.question).strong());
                            if !question.body.is_empty() {
                                super::desktop_ui::markdown(ui, &question.body);
                            }
                            for option in &question.options {
                                if question.multi_select {
                                    let mut selected = answer.selected.contains(&option.0);
                                    if ui.checkbox(&mut selected, &option.0).changed() {
                                        if selected {
                                            answer.selected.push(option.0.clone());
                                        } else {
                                            answer.selected.retain(|v| v != &option.0);
                                        }
                                    }
                                } else if ui
                                    .radio(
                                        !answer.other && answer.selected.first() == Some(&option.0),
                                        &option.0,
                                    )
                                    .clicked()
                                {
                                    answer.selected = vec![option.0.clone()];
                                    answer.other = false;
                                }
                                if !option.1.is_empty() {
                                    ui.indent(&option.0, |ui| {
                                        ui.label(RichText::new(&option.1).small().color(MUTED));
                                    });
                                }
                            }
                            if question.options.is_empty() {
                                answer.other = true;
                            } else if question.allow_other {
                                if question.multi_select {
                                    ui.checkbox(&mut answer.other, &question.other_label);
                                } else if ui.radio(answer.other, &question.other_label).clicked() {
                                    answer.other = true;
                                    answer.selected.clear();
                                }
                            }
                            if answer.other {
                                ui.add(
                                    TextEdit::singleline(&mut answer.text)
                                        .password(question.secret)
                                        .hint_text("你的回答")
                                        .desired_width(f32::INFINITY),
                                );
                            }
                        });
                        let values = answer.values();
                        complete &= !values.is_empty();
                        payload.insert(
                            question.id.clone(),
                            if record.app_id == "codex" {
                                json!({"answers":values})
                            } else {
                                json!(values.join(", "))
                            },
                        );
                    }
                    if questions.is_empty() {
                        ui.label("无法显示此问题的格式，请在执行记录中查看详情。");
                    }
                    if ui
                        .add_enabled(!self.busy && complete, egui::Button::new("提交回答"))
                        .clicked()
                    {
                        self.mutate(
                            ui.ctx(),
                            format!("/api/v1/workflows/{}/answer", record.id),
                            json!({"request_id":request_id,"answers":payload}),
                            false,
                        );
                    }
                }
                egui::CollapsingHeader::new("请求详情")
                    .id_salt(event.seq)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(
                                    serde_json::to_string_pretty(data).unwrap_or_default(),
                                )
                                .monospace()
                                .small(),
                            )
                            .selectable(true)
                            .wrap(),
                        );
                    });
            });
        ui.add_space(8.);
    }
    fn render_apps(&mut self, ui: &mut Ui) {
        ui.heading("应用");
        ui.label(
            RichText::new("保留各应用的官方执行引擎、登录方式与更新机制")
                .small()
                .color(MUTED),
        );
        ui.add_space(12.);
        let apps = self.apps.clone();
        ScrollArea::vertical()
            .id_salt("studio-apps")
            .show(ui, |ui| {
                for app in apps {
                    let id = app_id(&app).to_owned();
                    Frame::new()
                        .fill(PANEL)
                        .stroke(Stroke::new(1.0_f32, BORDER))
                        .corner_radius(8)
                        .inner_margin(16.)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                icons::glyph(ui, icons::Icon::Code, 20., app_color(&id));
                                ui.label(RichText::new(app_name(&app)).strong().size(16.));
                                pill(
                                    ui,
                                    if managed(&app) {
                                        "受管任务"
                                    } else {
                                        "交互终端"
                                    },
                                    if managed(&app) { ACCENT } else { MUTED },
                                );
                                if app
                                    .pointer("/source/model_vendor_official")
                                    .and_then(Value::as_bool)
                                    == Some(false)
                                {
                                    pill(ui, "第三方工具", MUTED);
                                }
                            });
                            if let Some(notes) = app
                                .get("notes")
                                .and_then(Value::as_str)
                                .filter(|v| !v.is_empty())
                            {
                                ui.label(RichText::new(notes).color(MUTED).size(12.));
                            }
                            let controls = wonderland::native_executor::capabilities(&id);
                            if managed(&app) {
                                ui.horizontal_wrapped(|ui| {
                                    if controls.read_only {
                                        pill(ui, "只读讨论", MUTED);
                                    }
                                    if controls.permissions {
                                        pill(ui, "逐次权限确认", MUTED);
                                    }
                                    if controls.questions {
                                        pill(ui, "交互提问", MUTED);
                                    }
                                    if !controls.reasoning_efforts.is_empty() {
                                        pill(ui, "可选推理档位", MUTED);
                                    }
                                });
                            }
                            if let Some(diagnostic) = self.diagnostics.get(&id) {
                                let installed = diagnostic["installed"].as_bool().unwrap_or(false);
                                ui.horizontal_wrapped(|ui| {
                                    pill(
                                        ui,
                                        if installed {
                                            "已找到程序"
                                        } else {
                                            "未检测到程序"
                                        },
                                        if installed { ACCENT } else { MUTED },
                                    );
                                    if let Some(version) = diagnostic["version"].as_str() {
                                        ui.label(RichText::new(version).small().color(MUTED));
                                    }
                                });
                                if let Some(path) = diagnostic["path"].as_str() {
                                    ui.label(RichText::new(path).small().color(MUTED));
                                }
                                ui.label(
                                    RichText::new("账号与模型可用性将在任务启动时验证")
                                        .small()
                                        .color(MUTED),
                                );
                                if diagnostic
                                    .pointer("/identity/matches_requested")
                                    .and_then(Value::as_bool)
                                    == Some(false)
                                {
                                    ui.label(
                                        RichText::new("程序身份与所选应用不符，请检查命令路径。")
                                            .small()
                                            .color(MUTED),
                                    );
                                }
                                if diagnostic
                                    .pointer("/version_probe/status")
                                    .and_then(Value::as_str)
                                    == Some("failed")
                                {
                                    ui.label(
                                        RichText::new("版本检测未通过，展开诊断查看原因。")
                                            .small()
                                            .color(MUTED),
                                    );
                                }
                                if !installed {
                                    if let Some(hint) = diagnostic["install_hint"].as_str() {
                                        ui.label(RichText::new(hint).small().color(MUTED));
                                    }
                                }
                                egui::CollapsingHeader::new("诊断详情")
                                    .id_salt(("app-diagnostic", &id))
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(
                                                serde_json::to_string_pretty(diagnostic)
                                                    .unwrap_or_default(),
                                            )
                                            .monospace()
                                            .small(),
                                        );
                                        if ui.small_button("复制诊断").clicked() {
                                            ui.ctx().copy_text(
                                                serde_json::to_string_pretty(diagnostic)
                                                    .unwrap_or_default(),
                                            );
                                        }
                                    });
                            }
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .add_enabled(
                                        self.connected && self.probing.is_none(),
                                        egui::Button::new("检测安装"),
                                    )
                                    .clicked()
                                {
                                    self.probe_app(ui.ctx(), &id);
                                }
                                if self.probing.as_deref() == Some(&id) {
                                    ui.spinner();
                                }
                                if managed(&app) && ui.button("创建任务").clicked() {
                                    self.page = Page::Work;
                                    self.composing = true;
                                    if self.app_id != id {
                                        self.model.clear();
                                    }
                                    self.app_id = id.clone();
                                    self.selected = None;
                                }
                                if icons::button(ui, icons::Icon::Terminal, "打开终端", 104., false)
                                    .clicked()
                                {
                                    self.launch_cli = Some(id.clone());
                                }
                                if let Some(url) = app
                                    .pointer("/source/website_url")
                                    .and_then(Value::as_str)
                                    .filter(|u| u.starts_with("https://"))
                                {
                                    ui.hyperlink_to("官方文档 ↗", url);
                                }
                            });
                        });
                    ui.add_space(9.);
                }
                if self.apps.is_empty() {
                    ui.label(RichText::new("等待服务返回应用目录…").color(MUTED));
                }
            });
    }
    fn probe_app(&mut self, ctx: &egui::Context, app_id: &str) {
        self.probing = Some(app_id.into());
        let (tx, source, ctx, app_id) = (
            self.tx.clone(),
            self.source.clone(),
            ctx.clone(),
            app_id.to_owned(),
        );
        thread::spawn(move || {
            let result = Runtime::new()
                .map_err(|error| error.to_string())
                .and_then(|runtime| {
                    runtime.block_on(post_json(
                        &source,
                        &format!("/api/v1/apps/{app_id}/probe"),
                        json!({}),
                    ))
                });
            let _ = tx.send(Reply::AppProbe {
                source,
                app_id,
                result,
            });
            ctx.request_repaint();
        });
    }
    fn render_projects(&mut self, ui: &mut Ui, cwd: &str) {
        ui.horizontal(|ui| {
            ui.heading("项目");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icons::button(ui, icons::Icon::Folder, "打开文件夹", 122., false).clicked()
                {
                    self.choose_project = true;
                }
            });
        });
        ui.add_space(14.);
        let mut projects: Vec<(String, String)> = self
            .projects
            .iter()
            .map(|p| (p.name.clone(), p.cwd.clone()))
            .collect();
        for path in std::iter::once(cwd).chain(self.records.iter().map(|r| r.cwd.as_str())) {
            if !projects.iter().any(|(_, p)| same_project(p, path)) {
                projects.push((
                    std::path::Path::new(path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    path.into(),
                ));
            }
        }
        ScrollArea::vertical()
            .id_salt("studio-projects")
            .show(ui, |ui| {
                for (name, project) in projects {
                    let tasks = self
                        .records
                        .iter()
                        .filter(|r| same_project(&r.cwd, &project))
                        .count();
                    Frame::new()
                        .fill(PANEL)
                        .corner_radius(8)
                        .inner_margin(16.)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                icons::glyph(ui, icons::Icon::Folder, 20., ACCENT);
                                ui.label(RichText::new(name).strong().size(16.));
                                ui.label(
                                    RichText::new(format!("{tasks} 个任务"))
                                        .small()
                                        .color(MUTED),
                                );
                                if same_project(cwd, &project) {
                                    pill(ui, "当前项目", ACCENT);
                                }
                                if ui.button("打开项目").clicked() {
                                    self.open_project = Some(project.clone());
                                }
                                if ui
                                    .add_enabled(
                                        self.connected && !self.busy,
                                        egui::Button::new("创建任务"),
                                    )
                                    .clicked()
                                {
                                    self.page = Page::Work;
                                    self.composing = true;
                                    self.selected = None;
                                    self.cwd = project.clone();
                                }
                                if !self.projects.iter().any(|p| same_project(&p.cwd, &project))
                                    && ui
                                        .add_enabled(
                                            self.connected && !self.busy,
                                            egui::Button::new("保存项目"),
                                        )
                                        .clicked()
                                {
                                    self.save_project(ui.ctx(), &project);
                                }
                            });
                            ui.label(RichText::new(&project).monospace().small().color(MUTED));
                        });
                    ui.add_space(9.);
                }
            });
    }
    fn save_project(&mut self, ctx: &egui::Context, cwd: &str) {
        if self.busy {
            return;
        }
        self.busy = true;
        let (tx, source, ctx, cwd) = (
            self.tx.clone(),
            self.source.clone(),
            ctx.clone(),
            cwd.to_owned(),
        );
        thread::spawn(move || {
            let result = Runtime::new().map_err(|e| e.to_string()).and_then(|rt| {
                rt.block_on(async {
                    let value =
                        post_json(&source, "/api/v1/projects", json!({"cwd":cwd,"name":null}))
                            .await?;
                    serde_json::from_value(value).map_err(|e| e.to_string())
                })
            });
            let _ = tx.send(Reply::Project { source, result });
            ctx.request_repaint();
        });
    }
    #[cfg(feature = "ui-snapshots")]
    pub fn prepare_snapshot(&mut self, mode: &str, cwd: &str) {
        self.fixture = true;
        self.connected = true;
        self.polling = false;
        self.source = "fixture".into();
        self.apps = vec![
            json!({"id":"codex","name":"Codex","capabilities":{"structured_runner":true,"notes":"OpenAI 官方编码应用"}}),
            json!({"id":"kimi-cli","name":"Kimi CLI","capabilities":{"structured_runner":true,"notes":"Moonshot AI 官方应用"}}),
            json!({"id":"claude","name":"Claude Code","capabilities":{"structured_runner":true,"notes":"Anthropic 官方原生应用"}}),
        ];
        self.cwd = cwd.into();
        self.model = "gpt-5.4".into();
        for (n, (title, status, app, model)) in [
            (
                "修复分页边界并添加测试",
                WorkflowStatus::Verifying,
                "codex",
                "gpt-5.4",
            ),
            (
                "梳理模块之间的依赖",
                WorkflowStatus::Running,
                "kimi-cli",
                "kimi-k2.5",
            ),
            (
                "检查 API 错误处理",
                WorkflowStatus::Draft,
                "codex",
                "gpt-5.4",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            self.records.push(WorkflowRecord{id:format!("fixture-{n}"),title:title.into(),prompt:"检查分页在空结果与最后一页的行为，并补充测试。".into(),cwd:cwd.into(),mode:"work".into(),app_id:app.into(),model:model.into(),read_only:false,max_duration_secs:1800,acceptance:vec!["现有测试通过".into(),"分页边界有回归覆盖".into()],status,created_at:"2026-09-21T09:30:00Z".into(),updated_at:"2026-09-21T09:35:00Z".into(),output:if n==0 {"已完成修改，等待验收。\n\n- 修正最后一页的边界计算\n- 添加空结果与最后一页测试\n\n请在项目中核对变更与测试记录。".into()}else{String::new()},error:None,native_session_id:None});
        }
        self.notice = "界面展示样例 · 非真实执行结果".into();
        self.composing = false;
        self.board = true;
        match mode {
            "studio-teams" | "studio-teams-new" => {
                self.page = Page::Teams;
                self.teams.prepare_snapshot(cwd, mode == "studio-teams-new");
            }
            "studio-apps" => {
                self.page = Page::Apps;
                self.apps = local_apps();
                self.diagnostics.insert("claude".into(), json!({"installed":true,"version":"2.1.193","path":"C:/Apps/Claude/bin/claude.exe","identity":{"matches_requested":true},"version_probe":{"status":"completed"},"authentication":{"status":"unknown"},"fixture":true}));
            }
            "studio-projects" => self.page = Page::Projects,
            "studio-task" => {
                self.board = false;
                self.selected = Some("fixture-0".into());
            }
            "studio-new" => {
                self.composing = true;
                self.board = false;
            }
            "studio-chat" => {
                self.page = Page::Chat;
                self.composing = true;
                self.board = false;
            }
            "studio-question" => {
                self.board = false;
                self.selected = Some("fixture-1".into());
                self.records[1].status = WorkflowStatus::WaitingInput;
                self.events.insert("fixture-1".into(),vec![WorkflowEvent{seq:1,workflow_id:"fixture-1".into(),kind:"question_requested".into(),data:json!({"request_id":"q1","data":{"questions":[{"question":"本次需要检查哪些模块？","header":"检查范围","options":[{"label":"全部模块","description":"检查整个项目中的模块依赖"},{"label":"仅核心模块","description":"先检查运行时与模型适配层"}],"multi_select":false}]}}),created_at:String::new()}]);
            }
            _ => {}
        }
    }
}

fn app_id(app: &Value) -> &str {
    app.get("id").and_then(Value::as_str).unwrap_or("")
}
fn local_apps() -> Vec<Value> {
    wonderland::desktop_bridge::official_apps()
        .into_iter()
        .filter_map(|app| serde_json::to_value(app).ok())
        .collect()
}
fn app_name(app: &Value) -> &str {
    app.get("name")
        .and_then(Value::as_str)
        .unwrap_or_else(|| app_id(app))
}
fn managed(app: &Value) -> bool {
    let id = app_id(app);
    wonderland::native_executor::supports_native(id)
        && app.get("enabled").and_then(Value::as_bool).unwrap_or(true)
        && app
            .pointer("/capabilities/structured_runner")
            .or_else(|| app.get("structured_runner"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
}
fn app_label(id: &str) -> String {
    match id {
        "codex" => "Codex",
        "kimi-cli" => "Kimi CLI",
        "kimi-code" => "Kimi Code",
        "claude" => "Claude Code",
        "deepseek" => "DeepSeek Harness",
        _ => id,
    }
    .into()
}
fn same_project(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        let normalize = |path: &str| {
            path.replace('/', "\\")
                .trim_start_matches("\\\\?\\")
                .trim_end_matches('\\')
                .to_lowercase()
        };
        normalize(left) == normalize(right)
    }
    #[cfg(not(windows))]
    {
        left.trim_end_matches('/') == right.trim_end_matches('/')
    }
}
fn app_color(id: &str) -> Color32 {
    match id {
        "codex" => ACCENT,
        "kimi-cli" | "kimi-code" => Color32::from_rgb(145, 189, 245),
        "claude" => Color32::from_rgb(232, 172, 145),
        "deepseek" => Color32::from_rgb(116, 160, 247),
        _ => MUTED,
    }
}
fn icon_button(ui: &mut Ui, icon: icons::Icon, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, 4, CARD);
    }
    icons::draw(ui, rect.shrink(5.), icon, MUTED);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response.on_hover_text(label)
}
fn lane_for(s: WorkflowStatus) -> usize {
    match s {
        WorkflowStatus::Draft => 0,
        WorkflowStatus::Running => 1,
        WorkflowStatus::WaitingInput | WorkflowStatus::Verifying | WorkflowStatus::Blocked => 2,
        _ => 3,
    }
}
fn status_text(s: WorkflowStatus) -> &'static str {
    match s {
        WorkflowStatus::Draft => "草稿",
        WorkflowStatus::Running => "执行中",
        WorkflowStatus::WaitingInput => "等待确认",
        WorkflowStatus::Verifying => "待验收",
        WorkflowStatus::Succeeded => "已验收",
        WorkflowStatus::Failed => "执行失败",
        WorkflowStatus::Cancelled => "已取消",
        WorkflowStatus::Blocked => "受阻",
        WorkflowStatus::Interrupted => "已中断",
    }
}
fn status_chip(ui: &mut Ui, s: WorkflowStatus) {
    pill(
        ui,
        status_text(s),
        match s {
            WorkflowStatus::Succeeded => ACCENT,
            WorkflowStatus::WaitingInput | WorkflowStatus::Verifying | WorkflowStatus::Blocked => {
                Color32::from_rgb(236, 204, 136)
            }
            WorkflowStatus::Failed => Color32::from_rgb(241, 156, 157),
            _ => MUTED,
        },
    );
}
fn pill(ui: &mut Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.1))
        .corner_radius(5)
        .inner_margin(egui::Margin::symmetric(7, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.).color(color));
        });
}
fn request_id(data: &Value) -> String {
    data.get("request_id")
        .or_else(|| data.get("id"))
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default()
}
fn is_permission(kind: &str, data: &Value) -> bool {
    kind.contains("approval")
        || kind.contains("permission")
        || data
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains("approval") || s.contains("permission"))
}
fn pending_requests(events: &[WorkflowEvent]) -> Vec<WorkflowEvent> {
    let mut pending = HashMap::new();
    for event in events {
        let id = request_id(&event.data);
        if id.is_empty() {
            continue;
        }
        let k = event.kind.as_str();
        if k == "control_submitted"
            || k.contains("resolved")
            || k.contains("answered")
            || k.contains("decision")
            || k.contains("approved")
            || k.contains("denied")
        {
            pending.remove(&id);
        } else if k.contains("request")
            && (is_permission(k, &event.data) || k.contains("input") || k.contains("question"))
        {
            pending.insert(id, event.clone());
        }
    }
    let mut values: Vec<_> = pending.into_values().collect();
    values.sort_by_key(|e| e.seq);
    values
}
fn event_label(kind: &str) -> &str {
    match kind {
        "created" => "创建任务",
        "started" => "开始执行",
        "completed" => "执行完成",
        "output" | "text_delta" => "应用输出",
        "reasoning_delta" => "推理",
        "tool_call" | "tool_activity" => "工具调用",
        "approval_requested" | "permission_requested" => "权限请求",
        "input_requested" | "question_requested" => "等待输入",
        "control_submitted" => "已提交答复",
        "permission_resolved" => "权限已处理",
        "accepted" | "human_verification" => "完成验收",
        "session_started" => "会话已建立",
        "status" => "状态",
        "usage" => "用量",
        _ => kind,
    }
}
fn event_summary(data: &Value) -> String {
    for key in [
        "message",
        "text",
        "description",
        "summary",
        "title",
        "reason",
        "output",
        "method",
    ] {
        if let Some(s) = data.get(key).and_then(Value::as_str) {
            return s.into();
        }
    }
    serde_json::to_string(data).unwrap_or_default()
}

#[derive(Default)]
struct Answer {
    selected: Vec<String>,
    other: bool,
    text: String,
}
impl Answer {
    fn values(&self) -> Vec<String> {
        let mut values = self.selected.clone();
        if self.other && !self.text.trim().is_empty() {
            values.push(self.text.trim().into());
        }
        values
    }
}
struct Question {
    id: String,
    header: String,
    question: String,
    body: String,
    options: Vec<(String, String)>,
    multi_select: bool,
    allow_other: bool,
    other_label: String,
    secret: bool,
}
fn request_questions(data: &Value, app_id: &str) -> Vec<Question> {
    let payload = data.get("data").unwrap_or(data);
    let Some(questions) = payload.get("questions").and_then(Value::as_array) else {
        return vec![];
    };
    questions
        .iter()
        .filter_map(|q| {
            let question = q.get("question")?.as_str()?.to_owned();
            // Kimi Wire keys answers by question text; Codex keys them by question ID.
            let id = if app_id == "codex" {
                q.get("id")?.as_str()?.to_owned()
            } else {
                question.clone()
            };
            let string = |key: &str| q.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|o| {
                    o.iter()
                        .filter_map(|v| {
                            Some((
                                v.get("label")?.as_str()?.into(),
                                v.get("description")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .into(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Question {
                id,
                header: string("header"),
                question,
                body: string("body"),
                options,
                multi_select: q
                    .get("multi_select")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                allow_other: app_id != "codex"
                    || q.get("isOther").and_then(Value::as_bool).unwrap_or(false),
                other_label: q
                    .get("other_label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("其他回答")
                    .into(),
                secret: q.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}
async fn get_json<T: serde::de::DeserializeOwned>(source: &str, path: &str) -> Result<T, String> {
    let response = wonderland::connection::service_client()
        .get(format!("{}{path}", source.trim_end_matches('/')))
        .timeout(Duration::from_secs(12))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    parse_response(response)
        .await
        .and_then(|v| serde_json::from_value(v).map_err(|e| format!("任务服务响应格式错误：{e}")))
}
async fn post_json(source: &str, path: &str, body: Value) -> Result<Value, String> {
    let response = wonderland::connection::service_client()
        .post(format!("{}{path}", source.trim_end_matches('/')))
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    parse_response(response).await
}
async fn parse_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .or_else(|| v.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| truncate(&text, 500));
        return Err(format!("{status} · {message}"));
    }
    if text.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn execution_completion_never_claims_acceptance() {
        assert_eq!(status_text(WorkflowStatus::Verifying), "待验收");
        assert_ne!(
            lane_for(WorkflowStatus::Verifying),
            lane_for(WorkflowStatus::Succeeded)
        );
    }
    #[test]
    fn resolved_permissions_are_not_actionable() {
        let make = |seq, kind: &str| WorkflowEvent {
            seq,
            workflow_id: "w".into(),
            kind: kind.into(),
            data: json!({"request_id":"a"}),
            created_at: String::new(),
        };
        assert_eq!(pending_requests(&[make(1, "approval_requested")]).len(), 1);
        assert!(
            pending_requests(&[make(1, "approval_requested"), make(2, "approval_resolved")])
                .is_empty()
        );
        assert!(
            pending_requests(&[make(1, "question_requested"), make(2, "control_submitted")])
                .is_empty()
        );
    }
    #[test]
    fn native_question_keys_match_each_official_protocol() {
        let payload = json!({"request_id":"r","data":{"questions":[{"id":"scope","question":"Which modules?","options":[{"label":"Core","description":"Runtime"}],"isOther":true}]}});
        let codex = request_questions(&payload, "codex");
        let kimi = request_questions(&payload, "kimi-cli");
        let claude = request_questions(&payload, "claude");
        assert_eq!(codex[0].id, "scope");
        assert_eq!(kimi[0].id, "Which modules?");
        assert_eq!(claude[0].id, "Which modules?");
        assert!(codex[0].allow_other);
        assert_eq!(codex[0].options[0].0, "Core");
    }
    #[test]
    fn composing_directory_is_not_overwritten_by_repaints() {
        let mut studio = Studio::new("original");
        studio.cwd = "typed-directory".into();
        studio.project_context("original");
        studio.project_context("new-workspace");
        assert_eq!(studio.cwd, "typed-directory");
        studio.cwd = "new-workspace".into();
        studio.project_context("third-workspace");
        assert_eq!(studio.cwd, "third-workspace");
    }
}

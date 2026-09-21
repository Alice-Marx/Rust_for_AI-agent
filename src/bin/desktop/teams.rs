//! Service-backed collaboration plans and verification evidence.
use super::*;
use wonderland::team_store::{
    CheckSpec, ExecutorBinding, NodeSpec, NodeStatus, TeamCreate, TeamEvent, TeamRecord,
    TeamStatus, TeamStrategy,
};

const AMBER: Color32 = Color32::from_rgb(236, 204, 136);
const RED: Color32 = Color32::from_rgb(241, 156, 157);

enum TeamReply {
    Snapshot {
        source: String,
        records: Result<Vec<TeamRecord>, String>,
        selected: Option<(
            String,
            Result<TeamRecord, String>,
            Result<Vec<TeamEvent>, String>,
        )>,
        pricing: Result<Value, String>,
    },
    Mutation {
        source: String,
        result: Result<TeamRecord, String>,
    },
    Created {
        source: String,
        record: TeamRecord,
    },
    Pricing {
        source: String,
        result: Result<Value, String>,
    },
    Directory(String),
}

struct CheckDraft {
    program: String,
    args: String,
    timeout_secs: u64,
}
impl Default for CheckDraft {
    fn default() -> Self {
        Self {
            program: String::new(),
            args: "[]".into(),
            timeout_secs: 300,
        }
    }
}

pub(super) struct Teams {
    pub selected: Option<String>,
    pub open_workflow: Option<String>,
    pub open_project: Option<String>,
    records: Vec<TeamRecord>,
    events: HashMap<String, Vec<TeamEvent>>,
    pricing: Option<Value>,
    tx: mpsc::Sender<TeamReply>,
    rx: mpsc::Receiver<TeamReply>,
    source: String,
    polling: bool,
    busy: bool,
    refreshing: bool,
    connected: bool,
    next_poll: Instant,
    notice: String,
    composing: bool,
    workspace: String,
    cwd: String,
    title: String,
    prompt: String,
    app_id: String,
    model: String,
    effort: String,
    strategy: TeamStrategy,
    candidates: String,
    nodes: String,
    checks: Vec<CheckDraft>,
    parallel: usize,
    minutes: u64,
    attempts: u32,
    budget: String,
    search: String,
}

impl Teams {
    pub fn new(cwd: &str) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            selected: None,
            open_workflow: None,
            open_project: None,
            records: vec![],
            events: HashMap::new(),
            pricing: None,
            tx,
            rx,
            source: String::new(),
            polling: false,
            busy: false,
            refreshing: false,
            connected: false,
            next_poll: Instant::now(),
            notice: String::new(),
            composing: false,
            workspace: cwd.into(),
            cwd: cwd.into(),
            title: String::new(),
            prompt: String::new(),
            app_id: "codex".into(),
            model: String::new(),
            effort: String::new(),
            strategy: TeamStrategy::Fixed,
            candidates: "[]".into(),
            nodes: String::new(),
            checks: vec![CheckDraft::default()],
            parallel: 2,
            minutes: 60,
            attempts: 2,
            budget: String::new(),
            search: String::new(),
        }
    }

    pub fn project_context(&mut self, cwd: &str) {
        if self.workspace != cwd {
            if self.cwd == self.workspace || self.cwd.is_empty() {
                self.cwd = cwd.into();
            }
            self.workspace = cwd.into();
        }
    }

    fn merge(&mut self, record: TeamRecord) {
        if let Some(old) = self.records.iter_mut().find(|old| old.id == record.id) {
            if record.revision >= old.revision {
                *old = record;
            }
        } else {
            self.records.push(record);
        }
        self.records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    }

    pub fn poll(&mut self, ctx: &egui::Context, source: &str, active: bool) {
        if self.source != source {
            if !self.source.is_empty() {
                self.selected = None;
            }
            self.source = source.into();
            self.records.clear();
            self.events.clear();
            self.pricing = None;
            self.polling = false;
            self.busy = false;
            self.refreshing = false;
            self.connected = false;
            self.next_poll = Instant::now();
            self.notice.clear();
        }
        while let Ok(reply) = self.rx.try_recv() {
            match reply {
                TeamReply::Snapshot {
                    source,
                    records,
                    selected,
                    pricing,
                } if source == self.source => {
                    self.polling = false;
                    match records {
                        Ok(records) => {
                            self.connected = true;
                            for record in records {
                                self.merge(record);
                            }
                        }
                        Err(error) => {
                            self.connected = false;
                            self.notice = error;
                        }
                    }
                    if let Some((id, record, events)) = selected {
                        match record {
                            Ok(record) => self.merge(record),
                            Err(error) => self.notice = error,
                        }
                        match events {
                            Ok(events) => {
                                let cache = self.events.entry(id).or_default();
                                for event in events {
                                    if !cache.iter().any(|old| old.seq == event.seq) {
                                        cache.push(event);
                                    }
                                }
                                cache.sort_by_key(|event| event.seq);
                                if cache.len() > 1000 {
                                    cache.drain(..cache.len() - 1000);
                                }
                            }
                            Err(error) => self.notice = error,
                        }
                    }
                    if let Ok(pricing) = pricing {
                        self.pricing = Some(pricing);
                    }
                    self.next_poll = Instant::now() + Duration::from_secs(2);
                }
                TeamReply::Mutation { source, result } if source == self.source => {
                    self.busy = false;
                    match result {
                        Ok(record) => {
                            self.selected = Some(record.id.clone());
                            self.composing = false;
                            self.merge(record);
                            self.notice.clear();
                        }
                        Err(error) => self.notice = error,
                    }
                    self.next_poll = Instant::now();
                }
                TeamReply::Created { source, record } if source == self.source => {
                    self.selected = Some(record.id.clone());
                    self.composing = false;
                    self.merge(record);
                }
                TeamReply::Pricing { source, result } if source == self.source => {
                    self.refreshing = false;
                    match result {
                        Ok(pricing) => {
                            self.pricing = Some(pricing);
                            self.notice =
                                "已刷新官方价格来源；订阅费用与自动路由条件仍独立核验。".into();
                        }
                        Err(error) => self.notice = error,
                    }
                    self.next_poll = Instant::now();
                }
                TeamReply::Directory(path) => self.cwd = path,
                _ => {}
            }
        }
        if !active || self.polling || Instant::now() < self.next_poll {
            return;
        }
        self.polling = true;
        let selected = self.selected.clone().map(|id| {
            let after = self
                .events
                .get(&id)
                .and_then(|events| events.last())
                .map_or(0, |event| event.seq);
            (id, after)
        });
        let (tx, source, ctx) = (self.tx.clone(), self.source.clone(), ctx.clone());
        thread::spawn(move || {
            let result = Runtime::new()
                .map_err(|error| error.to_string())
                .map(|runtime| {
                    runtime.block_on(async {
                        let selected = async {
                            if let Some((id, after)) = selected {
                                let record_path = format!("/api/v1/teams/{id}");
                                let events_path =
                                    format!("/api/v1/teams/{id}/events?after={after}");
                                let (record, events) = tokio::join!(
                                    get_json::<TeamRecord>(&source, &record_path),
                                    get_json::<Vec<TeamEvent>>(&source, &events_path)
                                );
                                Some((id, record, events))
                            } else {
                                None
                            }
                        };
                        tokio::join!(
                            get_json::<Vec<TeamRecord>>(&source, "/api/v1/teams"),
                            selected,
                            get_json::<Value>(&source, "/api/v1/pricing")
                        )
                    })
                });
            let (records, selected, pricing) =
                result.unwrap_or_else(|error| (Err(error.clone()), None, Err(error)));
            let _ = tx.send(TeamReply::Snapshot {
                source,
                records,
                selected,
                pricing,
            });
            ctx.request_repaint();
        });
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
                .map_err(|error| error.to_string())
                .and_then(|runtime| {
                    runtime.block_on(async {
                        let value = post_json(&source, &path, body).await?;
                        let record: TeamRecord = serde_json::from_value(value)
                            .map_err(|error| format!("协作记录格式错误：{error}"))?;
                        if !start_after {
                            return Ok(record);
                        }
                        let _ = tx.send(TeamReply::Created {
                            source: source.clone(),
                            record: record.clone(),
                        });
                        ctx.request_repaint();
                        post_json(
                            &source,
                            &format!("/api/v1/teams/{}/start", record.id),
                            json!({}),
                        )
                        .await
                        .and_then(|value| {
                            serde_json::from_value(value).map_err(|error| error.to_string())
                        })
                        .map_err(|error| {
                            format!("协作计划 {} 已保存，启动未完成：{error}", record.id)
                        })
                    })
                });
            let _ = tx.send(TeamReply::Mutation { source, result });
            ctx.request_repaint();
        });
    }

    fn refresh_prices(&mut self, ctx: &egui::Context) {
        self.refreshing = true;
        let (tx, source, ctx) = (self.tx.clone(), self.source.clone(), ctx.clone());
        thread::spawn(move || {
            let result = Runtime::new()
                .map_err(|error| error.to_string())
                .and_then(|runtime| {
                    runtime.block_on(async {
                        let response = wonderland::connection::service_client()
                            .post(format!(
                                "{}/api/v1/pricing/refresh",
                                source.trim_end_matches('/')
                            ))
                            .timeout(Duration::from_secs(90))
                            .json(&json!({}))
                            .send()
                            .await
                            .map_err(|error| error.to_string())?;
                        parse_response(response).await?;
                        get_json::<Value>(&source, "/api/v1/pricing").await
                    })
                });
            let _ = tx.send(TeamReply::Pricing { source, result });
            ctx.request_repaint();
        });
    }

    pub fn render(&mut self, ui: &mut Ui, apps: &[Value]) {
        ui.horizontal(|ui| {
            icons::glyph(ui, icons::Icon::Link, 25., ACCENT);
            ui.heading("Teams");
            pill(ui, "协作空间", MUTED);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("＋ 新协作").clicked() {
                    self.composing = true;
                    self.selected = None;
                }
                if ui
                    .selectable_label(!self.composing && self.selected.is_none(), "全部协作")
                    .clicked()
                {
                    self.composing = false;
                    self.selected = None;
                }
                if self.polling || self.busy {
                    ui.add(egui::Spinner::new().size(16.));
                }
            });
        });
        ui.label(RichText::new("把目标拆成有依赖、有负责人、可验证的工作。").color(MUTED));
        ui.add_space(10.);
        if !self.connected {
            ui.label(RichText::new("正在连接协作服务…").small().color(AMBER));
        }
        if !self.notice.is_empty() {
            Frame::new()
                .fill(CARD)
                .corner_radius(8)
                .inner_margin(12.)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(&self.notice).color(AMBER));
                        if ui.small_button("关闭").clicked() {
                            self.notice.clear();
                        }
                    });
                });
        }
        if self.composing {
            self.render_composer(ui, apps);
        } else if let Some(record) = self
            .selected
            .as_ref()
            .and_then(|id| self.records.iter().find(|record| &record.id == id))
            .cloned()
        {
            self.render_detail(ui, &record);
        } else if self.selected.is_some() {
            ui.spinner();
            ui.label("正在读取协作记录…");
        } else {
            self.render_list(ui);
        }
    }

    fn render_list(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            for (label, count, color) in [
                (
                    "进行中",
                    self.records
                        .iter()
                        .filter(|record| record.status.is_active())
                        .count(),
                    ACCENT,
                ),
                (
                    "需要处理",
                    self.records
                        .iter()
                        .filter(|record| {
                            matches!(
                                record.status,
                                TeamStatus::Blocked | TeamStatus::Failed | TeamStatus::WaitingInput
                            )
                        })
                        .count(),
                    AMBER,
                ),
                (
                    "验证通过",
                    self.records
                        .iter()
                        .filter(|record| record.status == TeamStatus::Succeeded)
                        .count(),
                    MUTED,
                ),
            ] {
                pill(ui, &format!("{label}  {count}"), color);
            }
            ui.add(
                TextEdit::singleline(&mut self.search)
                    .hint_text("搜索协作目标")
                    .desired_width(230.),
            );
        });
        ui.add_space(12.);
        let needle = self.search.to_lowercase();
        let records: Vec<_> = self
            .records
            .iter()
            .filter(|record| {
                needle.is_empty() || team_title(record).to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();
        ScrollArea::vertical().id_salt("teams-list").show(ui, |ui| {
            if records.is_empty() {
                Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1_f32, BORDER))
                    .corner_radius(14)
                    .inner_margin(28.)
                    .show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 2.).max(0.));
                        icons::glyph(ui, icons::Icon::Branch, 32., ACCENT);
                        ui.add_space(8.);
                        ui.label(
                            RichText::new(if self.records.is_empty() {
                                "从一个清晰的目标开始"
                            } else {
                                "没有匹配的协作"
                            })
                            .size(20.)
                            .strong(),
                        );
                        ui.label(
                            RichText::new(
                                "选择官方执行器，写下目标和测试方式，再跟踪每个节点的结果。",
                            )
                            .color(MUTED),
                        );
                        ui.add_space(10.);
                        if ui.button("创建协作计划  →").clicked() {
                            self.composing = true;
                        }
                    });
            }
            for record in records {
                Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1_f32, BORDER))
                    .corner_radius(10)
                    .inner_margin(16.)
                    .show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 2.).max(0.));
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(team_title(&record)).size(16.).strong(),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                self.selected = Some(record.id.clone());
                                self.next_poll = Instant::now();
                            }
                            team_chip(ui, record.status);
                        });
                        ui.label(RichText::new(truncate(&record.request.prompt, 160)).color(MUTED));
                        ui.horizontal_wrapped(|ui| {
                            pill(
                                ui,
                                &format!(
                                    "{} · {}",
                                    app_label(&record.request.planner.app_id),
                                    record.request.planner.model
                                ),
                                app_color(&record.request.planner.app_id),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} 个节点 · {} 项验证",
                                    record.nodes.len(),
                                    record.request.checks.len()
                                ))
                                .small()
                                .color(MUTED),
                            );
                            ui.label(RichText::new(&record.updated_at).small().color(MUTED));
                        });
                        if let Some(error) = &record.error {
                            ui.label(RichText::new(truncate(error, 200)).small().color(AMBER));
                        }
                    });
                ui.add_space(8.);
            }
            self.render_pricing(ui);
        });
    }

    fn render_composer(&mut self, ui: &mut Ui, apps: &[Value]) {
        ScrollArea::vertical().id_salt("teams-composer").show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(960.));
            section(ui, "01", "定义目标", "仓库、任务边界与预期结果");
            ui.add(TextEdit::singleline(&mut self.title).hint_text("协作名称（可选）").desired_width(f32::INFINITY));
            ui.add(TextEdit::multiline(&mut self.prompt).hint_text("要完成什么，以及什么结果代表完成…").desired_rows(4).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                ui.label(RichText::new("仓库").color(MUTED));
                ui.add(TextEdit::singleline(&mut self.cwd).desired_width((ui.available_width() - 45.).max(80.)));
                if icon_button(ui, icons::Icon::Folder, "选择协作仓库").clicked() {
                    let (tx, ctx, cwd) = (self.tx.clone(), ui.ctx().clone(), self.cwd.clone());
                    thread::spawn(move || {
                        if let Some(path) = rfd::FileDialog::new().set_title("选择协作仓库").set_directory(cwd).pick_folder() {
                            let _ = tx.send(TeamReply::Directory(path.to_string_lossy().into_owned())); ctx.request_repaint();
                        }
                    });
                }
            });
            section(ui, "02", "指定执行器", "执行过程保留官方工具的协议与权限确认");
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut self.strategy, TeamStrategy::Fixed, "固定官方执行器");
                ui.selectable_value(&mut self.strategy, TeamStrategy::Assigned, "逐节点指定模型"); ui.selectable_value(&mut self.strategy, TeamStrategy::Automatic, "Automatic · 条件检查");
            });
            ui.horizontal_wrapped(|ui| {
                let previous = self.app_id.clone();
                egui::ComboBox::from_id_salt("team-planner").selected_text(app_label(&self.app_id)).width(165.).show_ui(ui, |ui| {
                    for app in apps { if managed(app) { ui.selectable_value(&mut self.app_id, app_id(app).into(), app_name(app)); } }
                });
                if previous != self.app_id { self.model.clear(); self.effort.clear(); }
                ui.add(TextEdit::singleline(&mut self.model).hint_text("精确模型 ID").desired_width(220.));
                if self.app_id == "codex" {
                    egui::ComboBox::from_id_salt("team-effort").selected_text(if self.effort.is_empty() { "默认推理档位" } else { &self.effort }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.effort, String::new(), "默认推理档位");
                        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"] { ui.selectable_value(&mut self.effort, effort.into(), effort); }
                    });
                }
            });
            ui.label(RichText::new("填写该官方账号实际可用的模型 ID；执行中不会自动替换模型。").small().color(MUTED));
            if self.strategy == TeamStrategy::Automatic {
                Frame::new().fill(AMBER.gamma_multiply(0.07)).corner_radius(8).inner_margin(12.).show(ui, |ui| {
                    ui.label(RichText::new("Automatic 尚未就绪").strong().color(AMBER));
                    ui.label("官方计费渠道、精确模型与 benchmark 身份尚未全部验证，提交后可能标为受阻。刷新价格不会自动解除这些限制。");
                    ui.label(RichText::new("候选执行器 JSON").small().color(MUTED));
                    ui.add(TextEdit::multiline(&mut self.candidates).code_editor().desired_rows(3).desired_width(f32::INFINITY));
                });
            }
            if self.strategy == TeamStrategy::Assigned {
                ui.label(RichText::new("为每个节点明确指定官方工具与模型；请在下方节点计划中填写 executor。价格不参与此模式的选择。").small().color(MUTED));
                ui.label("允许的执行器 JSON");
                ui.add(TextEdit::multiline(&mut self.candidates).code_editor().desired_rows(3).desired_width(f32::INFINITY));
            }
            section(ui, "03", "验证完成", "程序与参数分别填写，参数使用 JSON 数组");
            let mut remove = None;
            for (index, check) in self.checks.iter_mut().enumerate() {
                ui.push_id(("team-check", index), |ui| {
                    Frame::new().fill(PANEL).stroke(Stroke::new(1_f32, BORDER)).corner_radius(8).inner_margin(12.).show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("验证 {}", index + 1)).strong());
                            ui.label(RichText::new("超时").small().color(MUTED));
                            ui.add(egui::DragValue::new(&mut check.timeout_secs).range(1..=3600).suffix(" 秒"));
                            if index > 0 && ui.small_button("移除").clicked() { remove = Some(index); }
                        });
                        ui.add(TextEdit::singleline(&mut check.program).hint_text("可执行程序，例如 cargo、node 或程序的完整路径").desired_width(f32::INFINITY));
                        ui.add(TextEdit::singleline(&mut check.args).font(egui::TextStyle::Monospace).hint_text("例如 [\"test\"] 或 [\"--test\", \"tests/sum.test.js\"]").desired_width(f32::INFINITY));
                    });
                });
            }
            if let Some(index) = remove { self.checks.remove(index); }
            if self.checks.len() < 32 && ui.small_button("＋ 添加验证命令").clicked() { self.checks.push(CheckDraft::default()); }
            egui::CollapsingHeader::new("执行范围与节点计划").default_open(false).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("并行节点"); ui.add(egui::DragValue::new(&mut self.parallel).range(1..=8));
                    ui.label("总时限"); ui.add(egui::DragValue::new(&mut self.minutes).range(1..=1440).suffix(" 分钟"));
                    ui.label("最多尝试"); ui.add(egui::DragValue::new(&mut self.attempts).range(1..=5));
                });
                ui.horizontal_wrapped(|ui| { ui.label("预算上限 USD（可选）"); ui.add(TextEdit::singleline(&mut self.budget).desired_width(130.)); });
                ui.label(RichText::new("提供 DAG JSON 可明确每个节点的依赖、写入范围、验收标准及执行器；留空由服务规划。").small().color(MUTED));
                ui.add(TextEdit::multiline(&mut self.nodes).code_editor().hint_text("[{\"id\":\"implementation\",\"objective\":\"完成目标\",\"dependencies\":[],\"write_paths\":[\"src\"],\"acceptance\":[\"测试通过\"]}]").desired_rows(5).desired_width(f32::INFINITY));
            });
            ui.add_space(8.); ui.separator();
            let request = self.build_request();
            if let Err(error) = &request { ui.label(RichText::new(error).small().color(MUTED)); }
            ui.horizontal_wrapped(|ui| {
                let enabled = self.connected && !self.busy && request.is_ok() && apps.iter().any(|app| app_id(app) == self.app_id && managed(app));
                if ui.add_enabled(enabled, egui::Button::new(RichText::new(if self.strategy == TeamStrategy::Automatic { "创建并检查条件  →" } else { "创建并开始协作  →" }).strong().color(BG)).fill(ACCENT)).clicked() {
                    if let Ok(request) = &request { self.mutate(ui.ctx(), "/api/v1/teams".into(), json!(request), true); }
                }
                if ui.add_enabled(enabled, egui::Button::new("保存计划")).clicked() {
                    if let Ok(request) = &request { self.mutate(ui.ctx(), "/api/v1/teams".into(), json!(request), false); }
                }
                if self.busy { ui.spinner(); }
            });
            ui.add_space(12.);
        });
    }

    fn build_request(&self) -> Result<TeamCreate, String> {
        let checks = self
            .checks
            .iter()
            .enumerate()
            .map(|(index, check)| {
                let args: Vec<String> = serde_json::from_str(&check.args).map_err(|_| {
                    format!(
                        "验证 {} 的参数需要是 JSON 字符串数组，例如 [\"test\"]",
                        index + 1
                    )
                })?;
                Ok(CheckSpec {
                    program: check.program.trim().into(),
                    args,
                    timeout_secs: check.timeout_secs,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let nodes: Vec<NodeSpec> = if self.nodes.trim().is_empty() {
            vec![]
        } else {
            serde_json::from_str(&self.nodes)
                .map_err(|error| format!("节点计划 JSON 无效：{error}"))?
        };
        let candidates = if self.strategy != TeamStrategy::Fixed {
            serde_json::from_str(&self.candidates)
                .map_err(|error| format!("候选执行器 JSON 无效：{error}"))?
        } else {
            vec![]
        };
        if self.strategy == TeamStrategy::Assigned && nodes.is_empty() {
            return Err("逐节点指定模式需要填写节点计划，每个节点都要指定 executor".into());
        }
        let budget_usd = if self.budget.trim().is_empty() {
            None
        } else {
            Some(
                self.budget
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| "预算需要是有效的 USD 数值".to_string())?,
            )
        };
        let request = TeamCreate {
            title: self.title.trim().into(),
            prompt: self.prompt.trim().into(),
            cwd: self.cwd.trim().into(),
            strategy: self.strategy,
            planner: ExecutorBinding {
                app_id: self.app_id.clone(),
                model: self.model.trim().into(),
                reasoning_effort: (!self.effort.is_empty() && self.app_id == "codex")
                    .then(|| self.effort.clone()),
            },
            candidates,
            nodes,
            checks,
            max_parallel: self.parallel,
            max_duration_secs: self.minutes * 60,
            max_attempts: self.attempts,
            budget_usd,
        };
        if request.prompt.is_empty() {
            return Err("填写任务目标、精确模型 ID 和至少一项验证命令后即可创建。".into());
        }
        wonderland::team_store::validate_create(&request).map_err(|error| error.to_string())?;
        Ok(request)
    }

    fn render_detail(&mut self, ui: &mut Ui, record: &TeamRecord) {
        ui.horizontal_wrapped(|ui| {
            if ui.small_button("← 全部协作").clicked() {
                self.selected = None;
            }
            team_chip(ui, record.status);
            ui.label(
                RichText::new(format!("修订 {}", record.revision))
                    .small()
                    .color(MUTED),
            );
            if matches!(record.status, TeamStatus::Planned | TeamStatus::Blocked)
                && ui
                    .add_enabled(
                        !self.busy && self.connected,
                        egui::Button::new(if record.status == TeamStatus::Blocked {
                            "重新检查并开始"
                        } else {
                            "开始协作"
                        }),
                    )
                    .clicked()
            {
                self.mutate(
                    ui.ctx(),
                    format!("/api/v1/teams/{}/start", record.id),
                    json!({}),
                    false,
                );
            }
            if !record.status.is_terminal()
                && ui
                    .add_enabled(!self.busy && self.connected, egui::Button::new("取消协作"))
                    .clicked()
            {
                self.mutate(
                    ui.ctx(),
                    format!("/api/v1/teams/{}/cancel", record.id),
                    json!({}),
                    false,
                );
            }
        });
        ui.label(RichText::new(team_title(record)).size(22.).strong());
        ui.horizontal_wrapped(|ui| {
            pill(
                ui,
                if record.request.strategy == TeamStrategy::Fixed {
                    "固定执行器"
                } else if record.request.strategy == TeamStrategy::Assigned {
                    "逐节点指定模型"
                } else {
                    "Automatic · 条件核验"
                },
                MUTED,
            );
            pill(
                ui,
                &format!(
                    "{} · {}",
                    app_label(&record.request.planner.app_id),
                    record.request.planner.model
                ),
                app_color(&record.request.planner.app_id),
            );
            if ui
                .small_button("复制协作 ID")
                .on_hover_text(&record.id)
                .clicked()
            {
                ui.ctx().copy_text(record.id.clone());
            }
        });
        ScrollArea::vertical()
            .id_salt(("teams-detail", &record.id))
            .show(ui, |ui| {
                ui.label(&record.request.prompt);
                ui.label(
                    RichText::new(&record.request.cwd)
                        .monospace()
                        .small()
                        .color(MUTED),
                );
                if let Some(workspace) = &record.run_workspace {
                    let workspace = display_workspace(workspace);
                    ui.label(
                        RichText::new(format!("执行目录  {workspace}"))
                            .monospace()
                            .small()
                            .color(MUTED),
                    );
                    ui.horizontal(|ui| {
                        if ui.small_button("打开结果项目").clicked() {
                            self.open_project = Some(workspace.clone());
                        }
                        if ui.small_button("复制结果路径").clicked() {
                            ui.ctx().copy_text(workspace.clone());
                        }
                    });
                }
                if let Some(error) = &record.error {
                    Frame::new()
                        .fill(AMBER.gamma_multiply(0.08))
                        .corner_radius(8)
                        .inner_margin(12.)
                        .show(ui, |ui| {
                            ui.label(RichText::new("需要处理").strong().color(AMBER));
                            ui.label(error);
                        });
                }
                if let Some(events) = self.events.get(&record.id) {
                    for event in events
                        .iter()
                        .filter(|event| event.kind == "planner_workflow")
                    {
                        if let Some(id) = event.data.get("workflow_id").and_then(Value::as_str) {
                            ui.horizontal_wrapped(|ui| {
                                icons::glyph(ui, icons::Icon::Spark, 16., ACCENT);
                                ui.label(RichText::new("规划任务").strong());
                                if ui.button("查看规划 / 处理问题  →").clicked() {
                                    self.open_workflow = Some(id.into());
                                }
                                ui.label(RichText::new(id).monospace().small().color(MUTED));
                            });
                        }
                    }
                }
                section(
                    ui,
                    "01",
                    "协作节点",
                    "依赖完成后继续，等待授权时打开对应子任务",
                );
                if record.nodes.is_empty() {
                    ui.label(RichText::new("尚未生成节点计划。").color(MUTED));
                }
                if !record.nodes.is_empty() {
                    render_dag(ui, record);
                }
                for node in &record.nodes {
                    Frame::new()
                        .fill(PANEL)
                        .stroke(Stroke::new(1_f32, BORDER))
                        .corner_radius(10)
                        .inner_margin(14.)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                icons::glyph(ui, icons::Icon::Branch, 15., node_color(node.status));
                                ui.label(RichText::new(&node.spec.id).strong());
                                pill(ui, node_status(node.status), node_color(node.status));
                            });
                            ui.label(&node.spec.objective);
                            ui.label(
                                RichText::new(if node.spec.dependencies.is_empty() {
                                    "无前置依赖".into()
                                } else {
                                    format!("依赖  {}", node.spec.dependencies.join("  ·  "))
                                })
                                .small()
                                .color(MUTED),
                            );
                            if !node.spec.write_paths.is_empty() {
                                ui.label(
                                    RichText::new(format!(
                                        "写入范围  {}",
                                        node.spec.write_paths.join(", ")
                                    ))
                                    .small()
                                    .color(MUTED),
                                );
                            }
                            if let Some(error) = &node.error {
                                ui.label(RichText::new(error).color(RED));
                            }
                            for attempt in &node.attempts {
                                ui.separator();
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "尝试 {} · {} · {}",
                                            attempt.number,
                                            app_label(&attempt.executor.app_id),
                                            attempt.executor.model
                                        ))
                                        .small()
                                        .color(MUTED),
                                    );
                                    pill(
                                        ui,
                                        node_status(attempt.status),
                                        node_color(attempt.status),
                                    );
                                    if let Some(id) = &attempt.workflow_id {
                                        if ui
                                            .button(if attempt.status == NodeStatus::WaitingInput {
                                                "处理授权 / 问题  →"
                                            } else {
                                                "打开子任务  →"
                                            })
                                            .on_hover_text(id)
                                            .clicked()
                                        {
                                            self.open_workflow = Some(id.clone());
                                        }
                                        ui.label(
                                            RichText::new(id).monospace().small().color(MUTED),
                                        );
                                    }
                                });
                                if let Some(error) = &attempt.error {
                                    ui.label(RichText::new(error).small().color(RED));
                                }
                                if let Some(artifact) = &attempt.artifact {
                                    egui::CollapsingHeader::new(format!(
                                        "尝试 {} 的结果记录",
                                        attempt.number
                                    ))
                                    .id_salt((&record.id, &node.spec.id, attempt.number))
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(
                                                serde_json::to_string_pretty(artifact)
                                                    .unwrap_or_default(),
                                            )
                                            .monospace()
                                            .small(),
                                        );
                                    });
                                }
                            }
                            egui::CollapsingHeader::new("节点验收标准")
                                .id_salt((&record.id, &node.spec.id, "acceptance"))
                                .show(ui, |ui| {
                                    for criterion in &node.spec.acceptance {
                                        ui.label(format!("• {criterion}"));
                                    }
                                });
                        });
                    ui.add_space(6.);
                }
                section(ui, "02", "验证记录", "保留每条命令的退出状态、耗时与输出");
                for (index, check) in record.request.checks.iter().enumerate() {
                    let results: Vec<_> = record
                        .verification
                        .iter()
                        .filter(|result| result.check_index == index)
                        .collect();
                    Frame::new()
                        .fill(PANEL)
                        .corner_radius(8)
                        .inner_margin(12.)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                icons::glyph(ui, icons::Icon::Terminal, 16., MUTED);
                                ui.label(
                                    RichText::new(format!(
                                        "{}  {}",
                                        check.program,
                                        serde_json::to_string(&check.args).unwrap_or_default()
                                    ))
                                    .monospace(),
                                );
                                if results.is_empty() {
                                    pill(ui, "尚未执行", MUTED);
                                }
                            });
                            for result in results {
                                ui.horizontal_wrapped(|ui| {
                                    pill(
                                        ui,
                                        if result.timed_out {
                                            "超时"
                                        } else if result.success {
                                            "通过"
                                        } else {
                                            "失败"
                                        },
                                        if result.success { ACCENT } else { RED },
                                    );
                                    ui.label(
                                        RichText::new(format!(
                                            "退出码 {} · {:.2} 秒",
                                            result
                                                .exit_code
                                                .map_or("未知".into(), |value| value.to_string()),
                                            result.duration_ms as f64 / 1000.
                                        ))
                                        .small()
                                        .color(MUTED),
                                    );
                                });
                                egui::CollapsingHeader::new("命令输出")
                                    .id_salt((&record.id, index, result.duration_ms))
                                    .show(ui, |ui| {
                                        ui.label(RichText::new(&result.output).monospace().small());
                                    });
                            }
                        });
                }
                if let Some(budget) = record.request.budget_usd {
                    ui.label(
                        RichText::new(format!(
                            "预算上限 ${budget:.4}；实际费用以服务核验记录和供应商账单为准。"
                        ))
                        .small()
                        .color(MUTED),
                    );
                }
                egui::CollapsingHeader::new("协作事件").show(ui, |ui| {
                    if let Some(events) = self.events.get(&record.id) {
                        for event in events.iter().rev().take(100) {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(&event.created_at).small().color(MUTED));
                                ui.label(RichText::new(&event.kind).small().strong());
                            });
                            ui.label(
                                RichText::new(truncate(&event_summary(&event.data), 1600))
                                    .small()
                                    .color(MUTED),
                            );
                            ui.separator();
                        }
                    }
                });
                self.render_pricing(ui);
            });
    }

    fn render_pricing(&mut self, ui: &mut Ui) {
        egui::CollapsingHeader::new("官方价格来源与自动选择条件").show(ui, |ui| {
            ui.label(RichText::new("API 标价仅供核对；订阅额度不能换算为免费 API 用量。Automatic 尚未开放自动价格调度。").small().color(MUTED));
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!self.refreshing && self.connected, egui::Button::new("刷新官方价格")).clicked() { self.refresh_prices(ui.ctx()); }
                if self.refreshing { ui.spinner(); ui.label("核对官方来源中…"); }
            });
            if let Some(pricing) = &self.pricing {
                if pricing.get("cache_is_stale").and_then(Value::as_bool).unwrap_or(true) { pill(ui, "价格缓存缺失或过期", AMBER); }
                if let Some(snapshot) = pricing.get("latest_snapshot").filter(|value| !value.is_null()) {
                    ui.label(RichText::new(format!("最近核对  {}", snapshot.get("checked_at").and_then(Value::as_str).unwrap_or("未知"))).small().color(MUTED));
                    if let Some(sources) = snapshot.get("sources").and_then(Value::as_array) {
                        for source in sources {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(source.get("provider").and_then(Value::as_str).unwrap_or("来源"));
                                let status = source.get("status").and_then(Value::as_str).unwrap_or("unknown");
                                pill(ui, status, if status == "verified" { ACCENT } else { AMBER });
                                if let Some(reason) = source.get("reason").and_then(Value::as_str) { ui.label(RichText::new(reason).small().color(MUTED)); }
                            });
                        }
                    }
                }
                if let Some(error) = pricing.pointer("/latest_failure/error").and_then(Value::as_str) { ui.label(RichText::new(error).small().color(AMBER)); }
            } else { ui.label(RichText::new("尚未读取价格快照。").small().color(MUTED)); }
        });
    }

    #[cfg(feature = "ui-snapshots")]
    pub fn prepare_snapshot(&mut self, cwd: &str, composing: bool) {
        use wonderland::team_store::{CheckResult, NodeAttempt, NodeRecord};
        self.connected = true;
        self.composing = composing;
        self.cwd = cwd.into();
        self.notice.clear();
        self.title = "完善分页体验与边界测试".into();
        self.prompt = "修复 API 与界面分页行为，覆盖空结果和最后一页，保留现有交互。".into();
        self.app_id = "kimi-cli".into();
        self.model = "kimi-for-coding".into();
        self.checks[0].program = "node".into();
        self.checks[0].args = r#"["--test", "tests/pagination.test.js"]"#.into();
        let mut request = self.build_request().expect("valid UI fixture");
        let make =
            |id: &str, objective: &str, dependencies: Vec<&str>, paths: Vec<&str>| NodeSpec {
                id: id.into(),
                objective: objective.into(),
                dependencies: dependencies.into_iter().map(str::to_owned).collect(),
                write_paths: paths.into_iter().map(str::to_owned).collect(),
                acceptance: vec!["分页边界有回归覆盖".into()],
                executor: None,
            };
        request.nodes = vec![
            make("api", "修复接口分页边界", vec![], vec!["src/api"]),
            make("ui", "完善最后一页的交互", vec![], vec!["src/ui"]),
            make(
                "review",
                "核对组合结果与回归测试",
                vec!["api", "ui"],
                vec![],
            ),
        ];
        let timestamp = "2026-09-21T10:30:00Z".to_owned();
        let nodes = request
            .nodes
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let status = [
                    NodeStatus::Succeeded,
                    NodeStatus::WaitingInput,
                    NodeStatus::Pending,
                ][index];
                NodeRecord {
                    spec: spec.clone(),
                    status,
                    error: None,
                    attempts: if index == 2 {
                        vec![]
                    } else {
                        vec![NodeAttempt {
                            number: 1,
                            executor: request.planner.clone(),
                            status,
                            workflow_id: Some(format!(
                                "fixture-{}",
                                if index == 0 { 0 } else { 1 }
                            )),
                            workspace: Some(cwd.into()),
                            reservation_id: None,
                            started_at: timestamp.clone(),
                            finished_at: (index == 0).then(|| timestamp.clone()),
                            error: None,
                            artifact: None,
                        }]
                    },
                }
            })
            .collect();
        self.records = vec![TeamRecord {
            id: "fixture-team".into(),
            request,
            status: TeamStatus::WaitingInput,
            revision: 7,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            error: None,
            starting_revision: Some("fixture-revision".into()),
            run_workspace: Some(cwd.into()),
            nodes,
            reservations: vec![],
            verification: vec![CheckResult {
                check_index: 0,
                success: false,
                exit_code: Some(1),
                timed_out: false,
                output: "界面样例：最后一页分页测试尚未通过，等待 UI 节点完成。".into(),
                duration_ms: 843,
            }],
        }];
        self.selected = (!composing).then(|| "fixture-team".into());
    }
}

fn render_dag(ui: &mut Ui, record: &TeamRecord) {
    let specs: Vec<_> = record.nodes.iter().map(|node| node.spec.clone()).collect();
    let layers = dependency_layers(&specs);
    let rows = layers.iter().map(Vec::len).max().unwrap_or(1);
    let tile = Vec2::new(194., 70.);
    let gap = Vec2::new(42., 20.);
    let size = Vec2::new(
        layers.len() as f32 * (tile.x + gap.x) - gap.x + 16.,
        rows as f32 * (tile.y + gap.y) - gap.y + 16.,
    );
    ScrollArea::horizontal()
        .id_salt(("team-dag", &record.id))
        .show(ui, |ui| {
            let (canvas, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            let mut boxes = HashMap::new();
            for (column, layer) in layers.iter().enumerate() {
                let top = (rows - layer.len()) as f32 * (tile.y + gap.y) / 2.;
                for (row, id) in layer.iter().enumerate() {
                    let rect = egui::Rect::from_min_size(
                        canvas.min
                            + Vec2::new(
                                8. + column as f32 * (tile.x + gap.x),
                                8. + top + row as f32 * (tile.y + gap.y),
                            ),
                        tile,
                    );
                    boxes.insert(id.clone(), rect);
                }
            }
            for node in &record.nodes {
                let Some(to) = boxes.get(&node.spec.id) else {
                    continue;
                };
                for dependency in &node.spec.dependencies {
                    let Some(from) = boxes.get(dependency) else {
                        continue;
                    };
                    let start = from.right_center();
                    let end = to.left_center();
                    let middle = (start.x + end.x) / 2.;
                    ui.painter().add(egui::Shape::line(
                        vec![
                            start,
                            egui::pos2(middle, start.y),
                            egui::pos2(middle, end.y),
                            egui::pos2(end.x - 7., end.y),
                        ],
                        Stroke::new(1.5_f32, BORDER),
                    ));
                    ui.painter().arrow(
                        egui::pos2(end.x - 9., end.y),
                        Vec2::new(8., 0.),
                        Stroke::new(1.5_f32, MUTED),
                    );
                }
            }
            for node in &record.nodes {
                let Some(rect) = boxes.get(&node.spec.id).copied() else {
                    continue;
                };
                let color = node_color(node.status);
                ui.painter().rect_filled(rect, 8, PANEL);
                ui.painter().rect_stroke(
                    rect,
                    8,
                    Stroke::new(1_f32, color.gamma_multiply(0.5)),
                    egui::StrokeKind::Inside,
                );
                ui.painter()
                    .circle_filled(rect.min + Vec2::new(13., 16.), 3., color);
                ui.painter().text(
                    rect.min + Vec2::new(24., 16.),
                    egui::Align2::LEFT_CENTER,
                    truncate(&node.spec.id, 14),
                    egui::FontId::proportional(12.),
                    TEXT,
                );
                ui.painter().text(
                    rect.right_top() + Vec2::new(-10., 16.),
                    egui::Align2::RIGHT_CENTER,
                    node_status(node.status),
                    egui::FontId::proportional(10.),
                    color,
                );
                ui.painter().text(
                    rect.min + Vec2::new(11., 43.),
                    egui::Align2::LEFT_CENTER,
                    truncate(&node.spec.objective, 13),
                    egui::FontId::proportional(12.),
                    MUTED,
                );
                ui.interact(
                    rect,
                    ui.id().with((&record.id, &node.spec.id, "dag")),
                    egui::Sense::hover(),
                )
                .on_hover_text(format!(
                    "{}\n{}\n依赖：{}",
                    node.spec.id,
                    node.spec.objective,
                    node.spec.dependencies.join(", ")
                ));
            }
        });
    ui.add_space(6.);
}

fn section(ui: &mut Ui, number: &str, title: &str, detail: &str) {
    ui.add_space(14.);
    ui.horizontal_wrapped(|ui| {
        pill(ui, number, ACCENT);
        ui.label(RichText::new(title).size(16.).strong());
        ui.label(RichText::new(detail).small().color(MUTED));
    });
    ui.add_space(4.);
}
fn team_title(record: &TeamRecord) -> String {
    if record.request.title.trim().is_empty() {
        truncate(&record.request.prompt, 64)
    } else {
        record.request.title.clone()
    }
}
fn display_workspace(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    }
}
fn team_status(status: TeamStatus) -> &'static str {
    match status {
        TeamStatus::Planned => "计划待执行",
        TeamStatus::Running => "协作中",
        TeamStatus::WaitingInput => "等待确认",
        TeamStatus::Verifying => "运行验证",
        TeamStatus::Succeeded => "验证通过",
        TeamStatus::Failed => "执行失败",
        TeamStatus::Cancelled => "已取消",
        TeamStatus::Blocked => "条件未满足",
        TeamStatus::Interrupted => "已中断",
    }
}
fn team_chip(ui: &mut Ui, status: TeamStatus) {
    pill(
        ui,
        team_status(status),
        match status {
            TeamStatus::Succeeded | TeamStatus::Running => ACCENT,
            TeamStatus::Blocked | TeamStatus::WaitingInput | TeamStatus::Verifying => AMBER,
            TeamStatus::Failed => RED,
            _ => MUTED,
        },
    );
}
fn node_status(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "待依赖",
        NodeStatus::Running => "执行中",
        NodeStatus::WaitingInput => "等待确认",
        NodeStatus::Verifying => "待验证",
        NodeStatus::Succeeded => "已完成",
        NodeStatus::Failed => "失败",
        NodeStatus::Cancelled => "已取消",
        NodeStatus::Blocked => "受阻",
        NodeStatus::Interrupted => "已中断",
    }
}
fn node_color(status: NodeStatus) -> Color32 {
    match status {
        NodeStatus::Succeeded | NodeStatus::Running => ACCENT,
        NodeStatus::WaitingInput | NodeStatus::Verifying | NodeStatus::Blocked => AMBER,
        NodeStatus::Failed => RED,
        _ => MUTED,
    }
}

/// Stable topological layers for the compact dependency overview. Invalid
/// server data is still displayed once, never allowed to loop the renderer.
fn dependency_layers(nodes: &[NodeSpec]) -> Vec<Vec<String>> {
    let mut remaining: Vec<_> = nodes.iter().collect();
    let mut seen = std::collections::HashSet::new();
    let mut layers = vec![];
    while !remaining.is_empty() {
        let ready: Vec<_> = remaining
            .iter()
            .filter(|node| node.dependencies.iter().all(|id| seen.contains(id)))
            .map(|node| node.id.clone())
            .collect();
        if ready.is_empty() {
            layers.push(remaining.iter().map(|node| node.id.clone()).collect());
            break;
        }
        for id in &ready {
            seen.insert(id.clone());
        }
        remaining.retain(|node| !seen.contains(&node.id));
        layers.push(ready);
    }
    layers
}

#[cfg(test)]
mod tests {
    use super::*;
    fn draft() -> Teams {
        let mut teams = Teams::new(std::env::current_dir().unwrap().to_str().unwrap());
        teams.prompt = "Verify the project".into();
        teams.model = "gpt-5".into();
        teams.checks[0].program = "node".into();
        teams.checks[0].args = r#"["--test", "tests/path with spaces.js", "$(literal)"]"#.into();
        teams
    }
    #[test]
    fn check_arguments_keep_literal_boundaries_and_reject_shell_input() {
        let mut teams = draft();
        let request = teams.build_request().unwrap();
        assert_eq!(
            request.checks[0].args,
            ["--test", "tests/path with spaces.js", "$(literal)"]
        );
        teams.checks[0].args = "--test tests/file.js".into();
        assert!(teams.build_request().is_err());
        teams.checks[0].args = "[]".into();
        teams.checks[0].program = "powershell.exe".into();
        assert!(teams.build_request().is_err());
    }
    #[test]
    fn automatic_needs_explicit_candidates_and_never_renders_as_success() {
        let mut teams = draft();
        teams.strategy = TeamStrategy::Automatic;
        assert!(teams.build_request().is_err());
        teams.candidates = r#"[{"app_id":"codex","model":"gpt-5"}]"#.into();
        assert_eq!(
            teams.build_request().unwrap().strategy,
            TeamStrategy::Automatic
        );
        assert_ne!(
            team_status(TeamStatus::Blocked),
            team_status(TeamStatus::Succeeded)
        );
    }
    #[test]
    fn dag_layers_respect_dependencies_even_when_json_is_out_of_order() {
        let node = |id: &str, dependencies: Vec<&str>| NodeSpec {
            id: id.into(),
            objective: id.into(),
            dependencies: dependencies.into_iter().map(str::to_owned).collect(),
            write_paths: vec![],
            acceptance: vec!["test".into()],
            executor: None,
        };
        let nodes = vec![
            node("verify", vec!["api", "ui"]),
            node("api", vec!["plan"]),
            node("plan", vec![]),
            node("ui", vec!["plan"]),
        ];
        assert_eq!(
            dependency_layers(&nodes),
            vec![vec!["plan"], vec!["api", "ui"], vec!["verify"]]
        );
        assert_eq!(
            dependency_layers(&[node("cyclic", vec!["cyclic"])]),
            vec![vec!["cyclic"]]
        );
    }

    fn record(revision: u64, status: TeamStatus) -> TeamRecord {
        TeamRecord {
            id: "team-fixture".into(),
            request: draft().build_request().unwrap(),
            status,
            revision,
            created_at: "2026-09-21T00:00:00Z".into(),
            updated_at: "2026-09-21T00:00:00Z".into(),
            error: None,
            starting_revision: None,
            run_workspace: None,
            nodes: vec![],
            reservations: vec![],
            verification: vec![],
        }
    }

    #[test]
    fn late_snapshots_cannot_replace_newer_revisions_or_other_services() {
        let mut teams = draft();
        teams.source = "http://service-b".into();
        teams.merge(record(7, TeamStatus::Verifying));
        teams.merge(record(6, TeamStatus::Running));
        assert_eq!(teams.records[0].status, TeamStatus::Verifying);
        teams
            .tx
            .send(TeamReply::Mutation {
                source: "http://service-a".into(),
                result: Ok(record(8, TeamStatus::Succeeded)),
            })
            .unwrap();
        teams.poll(&egui::Context::default(), "http://service-b", false);
        assert_eq!(teams.records[0].status, TeamStatus::Verifying);
        assert_eq!(teams.selected, None);
    }

    #[test]
    fn teams_page_renders_compose_and_unverified_detail_without_a_service() {
        let ctx = egui::Context::default();
        let mut teams = draft();
        teams.connected = true;
        teams.records = vec![record(7, TeamStatus::Verifying)];
        for compose in [true, false] {
            teams.composing = compose;
            teams.selected = (!compose).then(|| "team-fixture".into());
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(900., 650.),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| teams.render(ui, &local_apps()));
                },
            );
            assert!(!output.shapes.is_empty());
        }
        assert_eq!(team_status(teams.records[0].status), "运行验证");
        assert!(teams.records[0].verification.is_empty());
    }
}

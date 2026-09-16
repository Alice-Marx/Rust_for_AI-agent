use std::{sync::mpsc, thread};

use eframe::egui::{
    self, Align, Color32, Frame, Layout, RichText, ScrollArea, Stroke, TextEdit, Ui, Vec2, Visuals,
};
use reqwest::Client;
use rust_ai_agent::model::{AgentRequest, AgentResponse};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use uuid::Uuid;

const BG: Color32 = Color32::from_rgb(20, 20, 23);
const PANEL: Color32 = Color32::from_rgb(28, 28, 32);
const CARD: Color32 = Color32::from_rgb(38, 38, 43);
const ACCENT: Color32 = Color32::from_rgb(25, 195, 125);
const MUTED: Color32 = Color32::from_rgb(160, 160, 170);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Planning,
    Parallel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum MessageRole {
    User,
    Agent,
}

#[derive(Serialize, Deserialize)]
struct ChatMessage {
    role: MessageRole,
    text: String,
}

#[derive(Serialize, Deserialize)]
struct Task {
    id: String,
    session_id: String,
    title: String,
    messages: Vec<ChatMessage>,
    plan: Vec<String>,
    reflection: Option<String>,
    running: bool,
}

enum UiEvent {
    Completed {
        task_id: String,
        response: Box<AgentResponse>,
    },
    Failed {
        task_id: String,
        message: String,
    },
}

struct DesktopApp {
    server_url: String,
    user_id: String,
    tasks: Vec<Task>,
    active_task: usize,
    view: ViewMode,
    input: String,
    status: String,
    event_tx: mpsc::Sender<UiEvent>,
    event_rx: mpsc::Receiver<UiEvent>,
}

impl DesktopApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(Visuals::dark());
        let (event_tx, event_rx) = mpsc::channel();
        let tasks = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "tasks"))
            .filter(|tasks: &Vec<Task>| !tasks.is_empty())
            .unwrap_or_else(|| vec![Task::new("新任务")]);
        let server_url = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "server_url"))
            .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
        let user_id = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "user_id"))
            .unwrap_or_else(|| "local-user".to_string());
        Self {
            server_url,
            user_id,
            tasks,
            active_task: 0,
            view: ViewMode::Planning,
            input: String::new(),
            status: "就绪".to_string(),
            event_tx,
            event_rx,
        }
    }

    fn add_task(&mut self) {
        let number = self.tasks.len() + 1;
        self.tasks.push(Task::new(&format!("任务 {number}")));
        self.active_task = self.tasks.len() - 1;
        self.view = ViewMode::Planning;
        self.status = "已创建新任务".to_string();
    }

    fn send_active(&mut self) {
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return;
        }
        let Some(task) = self.tasks.get_mut(self.active_task) else {
            return;
        };
        if task.running {
            return;
        }
        let task_id = task.id.clone();
        let session_id = task.session_id.clone();
        if task.title == "新任务" || task.title.starts_with("任务 ") {
            task.title = title_from_input(&input);
        }
        task.messages.push(ChatMessage {
            role: MessageRole::User,
            text: input.clone(),
        });
        task.running = true;
        self.input.clear();
        self.status = "Agent 正在规划并执行...".to_string();

        let request = AgentRequest {
            session_id,
            user_id: Some(self.user_id.clone()),
            input,
        };
        spawn_agent_request(
            self.event_tx.clone(),
            self.server_url.clone(),
            task_id,
            request,
        );
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                UiEvent::Completed { task_id, response } => {
                    if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
                        task.running = false;
                        task.plan = response
                            .plan
                            .steps
                            .iter()
                            .map(|step| step.description.clone())
                            .collect();
                        task.reflection = Some(response.reflection.critique.clone());
                        task.messages.push(ChatMessage {
                            role: MessageRole::Agent,
                            text: response.output,
                        });
                        self.status = format!("完成 · 评分 {:.0}", response.evaluation.total_score);
                    }
                }
                UiEvent::Failed { task_id, message } => {
                    if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
                        task.running = false;
                        task.messages.push(ChatMessage {
                            role: MessageRole::Agent,
                            text: format!("请求失败：{message}"),
                        });
                    }
                    self.status = "请求失败".to_string();
                }
            }
        }
    }

    fn render_header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Rust AI Agent").strong());
            ui.label(RichText::new("桌面工作台").color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(&self.status).color(MUTED));
                ui.separator();
                if ui
                    .selectable_label(self.view == ViewMode::Parallel, "多任务并排")
                    .clicked()
                {
                    self.view = ViewMode::Parallel;
                }
                if ui
                    .selectable_label(self.view == ViewMode::Planning, "聊天式规划")
                    .clicked()
                {
                    self.view = ViewMode::Planning;
                }
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("服务").color(MUTED));
            ui.add(TextEdit::singleline(&mut self.server_url).desired_width(250.0));
            ui.label(RichText::new("用户").color(MUTED));
            ui.add(TextEdit::singleline(&mut self.user_id).desired_width(150.0));
        });
    }

    fn render_task_list(&mut self, ui: &mut Ui) {
        ui.heading("任务");
        if ui
            .add_sized(
                [ui.available_width(), 32.0],
                egui::Button::new("＋ 新建任务"),
            )
            .clicked()
        {
            self.add_task();
        }
        ui.add_space(10.0);
        ScrollArea::vertical().show(ui, |ui| {
            for (index, task) in self.tasks.iter().enumerate() {
                let active = index == self.active_task;
                let preview = task
                    .messages
                    .last()
                    .map(|message| message.text.lines().next().unwrap_or("").to_string())
                    .unwrap_or_else(|| "开始一个新的 Agent 任务".to_string());
                let response = ui.add_sized(
                    [ui.available_width(), 58.0],
                    egui::Button::new(
                        egui::RichText::new(format!("{}\n{}", task.title, truncate(&preview, 34)))
                            .size(13.0),
                    )
                    .fill(if active { CARD } else { PANEL })
                    .stroke(if active {
                        Stroke::new(1.0_f32, ACCENT)
                    } else {
                        Stroke::NONE
                    }),
                );
                if response.clicked() {
                    self.active_task = index;
                }
                ui.add_space(5.0);
            }
        });
    }

    fn render_planning(&mut self, ui: &mut Ui) {
        let Some(task) = self.tasks.get(self.active_task) else {
            return;
        };
        ui.heading(&task.title);
        ui.label(
            RichText::new(format!("session: {}", task.session_id))
                .color(MUTED)
                .small(),
        );
        ui.add_space(8.0);
        ScrollArea::vertical()
            .id_salt("planning-messages")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.render_messages(ui, self.active_task);
            });
        self.render_input(ui);
    }

    fn render_messages(&self, ui: &mut Ui, task_index: usize) {
        let Some(task) = self.tasks.get(task_index) else {
            return;
        };
        for message in &task.messages {
            let (label, color, alignment) = match message.role {
                MessageRole::User => ("你", ACCENT, Layout::right_to_left(Align::Min)),
                MessageRole::Agent => (
                    "Agent",
                    Color32::from_rgb(110, 165, 255),
                    Layout::left_to_right(Align::Min),
                ),
            };
            ui.with_layout(alignment, |ui| {
                Frame::new()
                    .fill(if message.role == MessageRole::User {
                        Color32::from_rgb(35, 64, 57)
                    } else {
                        CARD
                    })
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.label(RichText::new(label).color(color).strong());
                        ui.add_space(4.0);
                        ui.label(&message.text);
                    });
            });
            ui.add_space(10.0);
        }
        if task.messages.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("今天想完成什么？");
                ui.label(
                    RichText::new("Agent 会先生成计划，再执行、反思并保存长期记忆。").color(MUTED),
                );
            });
        }
    }

    fn render_input(&mut self, ui: &mut Ui) {
        ui.add_space(10.0);
        Frame::new()
            .fill(CARD)
            .corner_radius(12.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let task_running = self
                        .tasks
                        .get(self.active_task)
                        .map(|task| task.running)
                        .unwrap_or(false);
                    let edit = ui.add_enabled(
                        !task_running,
                        TextEdit::multiline(&mut self.input)
                            .hint_text("输入任务、问题或指令...")
                            .desired_rows(3)
                            .desired_width(ui.available_width() - 90.0),
                    );
                    if ui
                        .add_enabled(!task_running, egui::Button::new("发送"))
                        .clicked()
                        || (edit.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                    {
                        self.send_active();
                    }
                });
                ui.label(
                    RichText::new("Enter 发送 · 任务会自动写入长期记忆")
                        .color(MUTED)
                        .small(),
                );
            });
    }

    fn render_plan_panel(&self, ctx: &egui::Context) {
        let Some(task) = self.tasks.get(self.active_task) else {
            return;
        };
        egui::SidePanel::right("plan-panel")
            .default_width(260.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading("执行计划");
                ui.add_space(8.0);
                if task.plan.is_empty() {
                    ui.label(RichText::new("发送任务后，这里会显示 Agent 的计划。").color(MUTED));
                } else {
                    for (index, step) in task.plan.iter().enumerate() {
                        Frame::new()
                            .fill(PANEL)
                            .corner_radius(8.0)
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!("{}  {}", index + 1, step)).color(ACCENT),
                                );
                            });
                        ui.add_space(6.0);
                    }
                }
                if let Some(reflection) = &task.reflection {
                    ui.separator();
                    ui.label(RichText::new("反思").strong());
                    ui.label(RichText::new(reflection).color(MUTED));
                }
            });
    }

    fn render_parallel(&mut self, ui: &mut Ui) {
        ui.heading("多任务并排");
        ui.label(
            RichText::new("每个任务共享 Agent 服务，但拥有独立 session 和长期记忆上下文。")
                .color(MUTED),
        );
        ui.add_space(12.0);
        let mut offset = 0;
        while offset < self.tasks.len() {
            let row_count = (self.tasks.len() - offset).min(3);
            let mut selected = None;
            ui.columns(row_count, |columns| {
                for (column_offset, column) in columns.iter_mut().enumerate() {
                    let index = offset + column_offset;
                    let Some(task) = self.tasks.get(index) else {
                        continue;
                    };
                    let active = index == self.active_task;
                    Frame::new()
                        .fill(if active { CARD } else { PANEL })
                        .corner_radius(10.0)
                        .inner_margin(12.0)
                        .show(column, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&task.title).strong());
                                if task.running {
                                    ui.label(RichText::new("执行中").color(ACCENT));
                                }
                                if ui.small_button("打开").clicked() {
                                    selected = Some(index);
                                }
                            });
                            ui.separator();
                            let latest = task
                                .messages
                                .last()
                                .map(|message| message.text.as_str())
                                .unwrap_or("等待输入...");
                            ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                                ui.label(latest);
                            });
                            ui.add_space(10.0);
                            ui.label(
                                RichText::new(format!("{} 个计划步骤", task.plan.len()))
                                    .color(MUTED),
                            );
                        });
                }
            });
            if let Some(index) = selected {
                self.active_task = index;
                self.view = ViewMode::Planning;
            }
            offset += row_count;
            ui.add_space(10.0);
        }
        ui.add_space(12.0);
        ui.label(RichText::new("当前任务输入").strong());
        self.render_input(ui);
    }
}

impl eframe::App for DesktopApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        for task in &mut self.tasks {
            task.running = false;
        }
        eframe::set_value(storage, "tasks", &self.tasks);
        eframe::set_value(storage, "server_url", &self.server_url);
        eframe::set_value(storage, "user_id", &self.user_id);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        egui::TopBottomPanel::top("top-bar")
            .frame(
                Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(18, 14)),
            )
            .show(ctx, |ui| self.render_header(ui));
        egui::SidePanel::left("task-list")
            .default_width(245.0)
            .resizable(true)
            .frame(Frame::new().fill(PANEL).inner_margin(14.0))
            .show(ctx, |ui| self.render_task_list(ui));

        if self.view == ViewMode::Planning {
            self.render_plan_panel(ctx);
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(18.0))
            .show(ctx, |ui| match self.view {
                ViewMode::Planning => self.render_planning(ui),
                ViewMode::Parallel => self.render_parallel(ui),
            });
    }
}

impl Task {
    fn new(title: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session_id: Uuid::new_v4().to_string(),
            title: title.to_string(),
            messages: Vec::new(),
            plan: Vec::new(),
            reflection: None,
            running: false,
        }
    }
}

fn spawn_agent_request(
    event_tx: mpsc::Sender<UiEvent>,
    server_url: String,
    task_id: String,
    request: AgentRequest,
) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime.block_on(async move {
                    Client::new()
                        .post(format!("{}/v1/agent/run", server_url.trim_end_matches('/')))
                        .json(&request)
                        .send()
                        .await
                        .map_err(|error| error.to_string())?
                        .error_for_status()
                        .map_err(|error| error.to_string())?
                        .json::<AgentResponse>()
                        .await
                        .map_err(|error| error.to_string())
                })
            });
        let event = match result {
            Ok(response) => UiEvent::Completed {
                task_id,
                response: Box::new(response),
            },
            Err(message) => UiEvent::Failed { task_id, message },
        };
        let _ = event_tx.send(event);
    });
}

fn title_from_input(input: &str) -> String {
    truncate(input.lines().next().unwrap_or("新任务"), 24)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let result: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(Vec2::new(1280.0, 820.0))
            .with_min_inner_size(Vec2::new(940.0, 620.0)),
        ..Default::default()
    };
    eframe::run_native(
        "Rust AI Agent",
        options,
        Box::new(|cc| Ok(Box::new(DesktopApp::new(cc)))),
    )
}

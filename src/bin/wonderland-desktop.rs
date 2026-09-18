use std::{
    sync::{mpsc, Arc},
    thread,
};

use eframe::egui::{
    self, Align, Color32, Frame, Layout, RichText, ScrollArea, Stroke, TextEdit, Ui, Vec2, Visuals,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use uuid::Uuid;
use wonderland::cliproxy::{
    CliProxyAccount, CliProxyLoginStart, CliProxyLoginStatus, CliProxyModel, CliProxyVerification,
};
use wonderland::model::{AgentRequest, AgentResponse};

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
    /// 正在流式输出的回答正文（完成后并入 messages）。
    #[serde(default)]
    stream_text: String,
    /// 本轮工具执行进度（最新的在最后）。
    #[serde(default)]
    tool_log: Vec<String>,
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
    /// 助手文本增量。
    Delta {
        task_id: String,
        text: String,
    },
    /// 思维链增量（显示在工具进度区）。
    Reasoning {
        task_id: String,
        text: String,
    },
    /// 工具执行进度。
    ToolProgress {
        task_id: String,
        label: String,
    },
    LoginStarted(CliProxyLoginStart),
    LoginStatus(CliProxyLoginStatus),
    ModelsLoaded(Vec<CliProxyModel>),
    ModelVerified(CliProxyVerification),
    ToolsLoaded(Vec<serde_json::Value>),
    McpLoaded(Vec<serde_json::Value>),
    AccountsLoaded(Vec<CliProxyAccount>),
    HealthLoaded(serde_json::Value),
    ApiFailed(String),
}

struct DesktopApp {
    server_url: String,
    user_id: String,
    tasks: Vec<Task>,
    active_task: usize,
    view: ViewMode,
    input: String,
    status: String,
    login_provider: String,
    login_state: Option<String>,
    login_url: Option<String>,
    models: Vec<CliProxyModel>,
    selected_model: String,
    api_request_running: bool,
    event_tx: mpsc::Sender<UiEvent>,
    event_rx: mpsc::Receiver<UiEvent>,
    /// 当前后端 provider 名称与可用 wire 协议。
    provider_name: String,
    protocols: Vec<String>,
    /// 推理档位（发送给后端的请求级覆盖）。
    reasoning_effort: String,
    /// 已注册工具、MCP 服务器与订阅账号。
    tools: Vec<serde_json::Value>,
    mcp_servers: Vec<serde_json::Value>,
    accounts: Vec<CliProxyAccount>,
    /// 是否展开连接与工具面板。
    show_conn_panel: bool,
}

impl DesktopApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_fonts(chinese_font_definitions());
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
        let server_url_for_probe = server_url.clone();
        let event_tx_for_probe = event_tx.clone();
        let app = Self {
            server_url,
            user_id,
            tasks,
            active_task: 0,
            view: ViewMode::Planning,
            input: String::new(),
            status: "就绪".to_string(),
            login_provider: "codex".to_string(),
            login_state: None,
            login_url: None,
            models: Vec::new(),
            selected_model: String::new(),
            api_request_running: false,
            event_tx,
            event_rx,
            provider_name: "unknown".to_string(),
            protocols: Vec::new(),
            reasoning_effort: "medium".to_string(),
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            accounts: Vec::new(),
            show_conn_panel: false,
        };
        // 启动即抓一份后端能力信息（provider / 协议栈 / 工具 / MCP / 账号）。
        spawn_health_request(event_tx_for_probe, server_url_for_probe);
        app
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

        let reasoning_effort = match self.reasoning_effort.as_str() {
            "off" | "" => None,
            other => Some(other.to_string()),
        };
        let request = AgentRequest {
            session_id,
            user_id: Some(self.user_id.clone()),
            model: (!self.selected_model.trim().is_empty()).then(|| self.selected_model.clone()),
            skills: Vec::new(),
            mode: None,
            cwd: None,
            reasoning_effort,
            input,
        };
        spawn_agent_request(
            self.event_tx.clone(),
            self.server_url.clone(),
            task_id,
            request,
        );
    }

    /// 拉取工具 / MCP / 账号 / 健康信息。
    fn refresh_connection_panel(&mut self) {
        spawn_health_request(self.event_tx.clone(), self.server_url.clone());
        spawn_tools_request(self.event_tx.clone(), self.server_url.clone());
        spawn_mcp_request(self.event_tx.clone(), self.server_url.clone());
        spawn_accounts_request(self.event_tx.clone(), self.server_url.clone());
        self.status = "正在刷新连接与工具信息".to_string();
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
                        task.stream_text.clear();
                        task.tool_log.clear();
                        self.status = format!(
                            "完成 · 评分 {:.0} · {} 轮 · {} 次工具调用 · tokens {}/{}",
                            response.evaluation.total_score,
                            response.turns,
                            response.tool_calls,
                            response.usage.input_tokens,
                            response.usage.output_tokens,
                        );
                    }
                }
                UiEvent::Delta { task_id, text } => {
                    if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
                        task.stream_text.push_str(&text);
                    }
                }
                UiEvent::Reasoning { task_id, text } => {
                    if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
                        let line = text.lines().last().unwrap_or_default().trim();
                        if !line.is_empty() {
                            push_tool_note(task, format!("思考：{line}"));
                        }
                    }
                }
                UiEvent::ToolProgress { task_id, label } => {
                    if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
                        push_tool_note(task, label);
                    }
                }
                UiEvent::ToolsLoaded(tools) => {
                    self.tools = tools;
                }
                UiEvent::McpLoaded(servers) => {
                    self.mcp_servers = servers;
                }
                UiEvent::AccountsLoaded(accounts) => {
                    self.accounts = accounts;
                }
                UiEvent::HealthLoaded(value) => {
                    self.provider_name = value
                        .get("provider")
                        .and_then(|provider| provider.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    self.protocols = value
                        .get("protocols")
                        .and_then(|protocols| protocols.as_array())
                        .map(|entries| {
                            entries
                                .iter()
                                .filter_map(|entry| entry.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                }
                UiEvent::Failed { task_id, message } => {
                    if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
                        task.running = false;
                        task.stream_text.clear();
                        task.tool_log.clear();
                        task.messages.push(ChatMessage {
                            role: MessageRole::Agent,
                            text: format!("请求失败：{message}"),
                        });
                    }
                    self.status = "请求失败".to_string();
                }
                UiEvent::LoginStarted(response) => {
                    self.api_request_running = false;
                    self.login_state = response.state.clone();
                    self.login_url = response.url.clone();
                    self.status = "OAuth 授权链接已生成，请在浏览器完成登录".to_string();
                }
                UiEvent::LoginStatus(response) => {
                    self.api_request_running = false;
                    self.status = if response.authenticated {
                        "账号验证成功".to_string()
                    } else if let Some(error) = response.error {
                        format!("登录失败：{error}")
                    } else {
                        format!("登录状态：{}", response.status)
                    };
                }
                UiEvent::ModelsLoaded(models) => {
                    self.api_request_running = false;
                    if self.selected_model.is_empty() {
                        self.selected_model = models
                            .first()
                            .map(|model| model.id.clone())
                            .unwrap_or_default();
                    }
                    self.models = models;
                    self.status = format!("已读取 {} 个可用模型", self.models.len());
                }
                UiEvent::ModelVerified(response) => {
                    self.api_request_running = false;
                    self.status = match response.selected_model_available {
                        Some(true) => {
                            format!("模型 {} 可用", response.selected_model.unwrap_or_default())
                        }
                        Some(false) => format!(
                            "模型 {} 不在当前订阅模型列表",
                            response.selected_model.unwrap_or_default()
                        ),
                        None => format!("CLIProxyAPI 在线，共 {} 个模型", response.model_count),
                    };
                }
                UiEvent::ApiFailed(message) => {
                    self.api_request_running = false;
                    self.status = format!("API 操作失败：{}", friendly_api_error(&message));
                }
            }
        }
    }

    fn start_login(&mut self) {
        if self.api_request_running {
            return;
        }
        self.api_request_running = true;
        self.status = "正在向 CLIProxyAPI 请求 OAuth 授权链接...".to_string();
        spawn_login_request(
            self.event_tx.clone(),
            self.server_url.clone(),
            self.login_provider.clone(),
        );
    }

    fn check_login(&mut self) {
        let Some(state) = self.login_state.clone() else {
            self.status = "请先点击账号登录".to_string();
            return;
        };
        if self.api_request_running {
            return;
        }
        self.api_request_running = true;
        self.status = "正在检查账号登录状态...".to_string();
        spawn_login_status_request(self.event_tx.clone(), self.server_url.clone(), state);
    }

    fn load_models(&mut self) {
        if self.api_request_running {
            return;
        }
        self.api_request_running = true;
        self.status = "正在读取订阅模型...".to_string();
        spawn_models_request(self.event_tx.clone(), self.server_url.clone());
    }

    fn verify_model(&mut self) {
        if self.api_request_running {
            return;
        }
        self.api_request_running = true;
        self.status = "正在验证模型...".to_string();
        spawn_verify_request(
            self.event_tx.clone(),
            self.server_url.clone(),
            self.selected_model.clone(),
        );
    }

    fn render_header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Wonderland").strong());
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
            ui.label(RichText::new("推理").color(MUTED));
            egui::ComboBox::from_id_salt("reasoning-effort")
                .selected_text(&self.reasoning_effort)
                .width(90.0)
                .show_ui(ui, |ui| {
                    for effort in ["off", "low", "medium", "high"] {
                        ui.selectable_value(&mut self.reasoning_effort, effort.to_string(), effort);
                    }
                });
            if ui
                .selectable_label(self.show_conn_panel, "连接与工具")
                .clicked()
            {
                self.show_conn_panel = !self.show_conn_panel;
                if self.show_conn_panel {
                    self.refresh_connection_panel();
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("API 登录").color(MUTED));
            egui::ComboBox::from_id_salt("login-provider")
                .selected_text(&self.login_provider)
                .show_ui(ui, |ui| {
                    for provider in [
                        "codex",
                        "claude",
                        "antigravity",
                        "kimi",
                        "xai",
                        "devin",
                        "meta",
                    ] {
                        ui.selectable_value(
                            &mut self.login_provider,
                            provider.to_string(),
                            provider,
                        );
                    }
                });
            if ui
                .add_enabled(!self.api_request_running, egui::Button::new("账号登录"))
                .clicked()
            {
                self.start_login();
            }
            if ui
                .add_enabled(
                    !self.api_request_running && self.login_state.is_some(),
                    egui::Button::new("检查登录"),
                )
                .clicked()
            {
                self.check_login();
            }
            if ui
                .add_enabled(!self.api_request_running, egui::Button::new("刷新模型"))
                .clicked()
            {
                self.load_models();
            }
            ui.label(RichText::new("模型").color(MUTED));
            if self.models.is_empty() {
                ui.add(TextEdit::singleline(&mut self.selected_model).desired_width(150.0));
            } else {
                egui::ComboBox::from_id_salt("model-picker")
                    .selected_text(if self.selected_model.is_empty() {
                        "选择模型"
                    } else {
                        &self.selected_model
                    })
                    .show_ui(ui, |ui| {
                        for model in &self.models {
                            ui.selectable_value(
                                &mut self.selected_model,
                                model.id.clone(),
                                &model.id,
                            );
                        }
                    });
            }
            if ui
                .add_enabled(
                    !self.api_request_running && !self.selected_model.trim().is_empty(),
                    egui::Button::new("验证模型"),
                )
                .clicked()
            {
                self.verify_model();
            }
        });
        if let Some(url) = &self.login_url {
            ui.horizontal(|ui| {
                ui.label(RichText::new("OAuth").color(MUTED));
                ui.hyperlink_to("打开浏览器授权", url);
                if let Some(state) = &self.login_state {
                    ui.label(RichText::new(format!("state: {}", truncate(state, 18))).color(MUTED));
                }
            });
        }
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
        if task.messages.is_empty() && task.stream_text.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("今天想完成什么？");
                ui.label(
                    RichText::new("Agent 会先生成计划，再执行、反思并保存长期记忆。").color(MUTED),
                );
            });
        }
        // 正在流式输出的回答：直接在气泡里逐字增长。
        if !task.stream_text.is_empty() {
            ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                Frame::new()
                    .fill(CARD)
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Agent")
                                .color(Color32::from_rgb(110, 165, 255))
                                .strong(),
                        );
                        ui.add_space(4.0);
                        ui.label(&task.stream_text);
                    });
            });
        }
        if !task.tool_log.is_empty() {
            ui.add_space(6.0);
            Frame::new()
                .fill(Color32::from_rgb(32, 32, 38))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.label(RichText::new("执行进度").color(MUTED).small());
                    for line in &task.tool_log {
                        ui.label(RichText::new(truncate(line, 120)).color(MUTED).small());
                    }
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

    /// 右侧连接与工具面板：provider/协议栈、工具、MCP、订阅账号。
    fn render_connection_panel(&mut self, ctx: &egui::Context) {
        if !self.show_conn_panel {
            return;
        }
        let mut open = self.show_conn_panel;
        egui::SidePanel::right("connection-panel")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("连接与工具");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("关闭").clicked() {
                            open = false;
                        }
                        if ui.button("刷新").clicked() {
                            self.refresh_connection_panel();
                        }
                    });
                });
                ui.add_space(6.0);
                ui.label(RichText::new(format!("后端 provider：{}", self.provider_name)).color(MUTED));
                ui.label(
                    RichText::new(format!(
                        "可用协议：{}",
                        if self.protocols.is_empty() {
                            "（未上报）".to_string()
                        } else {
                            self.protocols.join(" / ")
                        }
                    ))
                    .color(MUTED),
                );
                ui.label(
                    RichText::new(
                        "请求会按模型族自动走 chat.completions / Responses / Anthropic Messages。",
                    )
                    .color(MUTED)
                    .small(),
                );
                ui.separator();

                ui.collapsing(format!("工具（{}）", self.tools.len()), |ui| {
                    ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                        for tool in &self.tools {
                            let name = tool
                                .get("name")
                                .and_then(|value| value.as_str())
                                .unwrap_or("?");
                            let source = tool
                                .get("source")
                                .and_then(|value| value.as_str())
                                .unwrap_or("builtin");
                            let read_only = tool
                                .get("read_only")
                                .and_then(|value| value.as_bool())
                                .unwrap_or(false);
                            ui.label(
                                RichText::new(format!(
                                    "{name}  [{source}{}]",
                                    if read_only { " · 只读" } else { "" }
                                ))
                                .small(),
                            );
                        }
                    });
                });

                ui.collapsing(format!("MCP 服务器（{}）", self.mcp_servers.len()), |ui| {
                    if self.mcp_servers.is_empty() {
                        ui.label(
                            RichText::new(
                                "未连接。可在项目 .mcp.json 或 .wonderland/settings.json 的 mcpServers 中配置。",
                            )
                            .color(MUTED)
                            .small(),
                        );
                    }
                    for server in &self.mcp_servers {
                        let name = server
                            .get("name")
                            .and_then(|value| value.as_str())
                            .unwrap_or("?");
                        let count = server
                            .get("tools")
                            .and_then(|value| value.as_array())
                            .map(|tools| tools.len())
                            .unwrap_or(0);
                        ui.label(RichText::new(format!("{name}（{count} 个工具）")).small());
                    }
                });

                ui.collapsing(format!("订阅账号（{}）", self.accounts.len()), |ui| {
                    if self.accounts.is_empty() {
                        ui.label(
                            RichText::new("还没有登录订阅账号；先在上方选择服务并点击账号登录。")
                                .color(MUTED)
                                .small(),
                        );
                    }
                    for account in &self.accounts {
                        ui.label(
                            RichText::new(format!(
                                "{}  {}{}",
                                account.name,
                                account.provider.clone().unwrap_or_default(),
                                if account.disabled { "（已禁用）" } else { "" }
                            ))
                            .small(),
                        );
                    }
                });
            });
        self.show_conn_panel = open;
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
        self.render_connection_panel(ctx);
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(18.0))
            .show(ctx, |ui| match self.view {
                ViewMode::Planning => self.render_planning(ui),
                ViewMode::Parallel => self.render_parallel(ui),
            });
    }
}

/// 进度区只保留最近 6 条，避免长任务把面板撑爆。
fn push_tool_note(task: &mut Task, label: String) {
    task.tool_log.push(label);
    let overflow = task.tool_log.len().saturating_sub(6);
    if overflow > 0 {
        task.tool_log.drain(..overflow);
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
            stream_text: String::new(),
            tool_log: Vec::new(),
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
        let runtime = match Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = event_tx.send(UiEvent::Failed {
                    task_id,
                    message: error.to_string(),
                });
                return;
            }
        };
        runtime.block_on(async move {
            use futures_util::StreamExt;

            let url = format!("{}/v1/agent/stream", server_url.trim_end_matches('/'));
            let response = match Client::new().post(url).json(&request).send().await {
                Ok(response) => response,
                Err(error) => {
                    let _ = event_tx.send(UiEvent::Failed {
                        task_id,
                        message: format!("无法连接后端：{error}"),
                    });
                    return;
                }
            };
            let status = response.status();
            if !status.is_success() {
                let raw = response.text().await.unwrap_or_default();
                let _ = event_tx.send(UiEvent::Failed {
                    task_id,
                    message: format!("HTTP {status}: {raw}"),
                });
                return;
            }

            let mut stream = response.bytes_stream();
            let mut decoder = wonderland::provider::SseBuffer::new();
            let mut final_response: Option<AgentResponse> = None;
            let mut failure: Option<String> = None;
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else {
                    break;
                };
                for payload in decoder.push_bytes(&chunk) {
                    let trimmed = payload.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(frame) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                        continue;
                    };
                    match frame
                        .get("frame")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                    {
                        "event" => {
                            let event = &frame["event"];
                            let kind = event
                                .get("type")
                                .and_then(|value| value.as_str())
                                .unwrap_or_default();
                            let text = || {
                                event
                                    .get("text")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or_default()
                                    .to_string()
                            };
                            match kind {
                                "text_delta" => {
                                    let _ = event_tx.send(UiEvent::Delta {
                                        task_id: task_id.clone(),
                                        text: text(),
                                    });
                                }
                                "reasoning_delta" => {
                                    let _ = event_tx.send(UiEvent::Reasoning {
                                        task_id: task_id.clone(),
                                        text: text(),
                                    });
                                }
                                "tool_call" => {
                                    let name = event
                                        .get("name")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("?");
                                    let _ = event_tx.send(UiEvent::ToolProgress {
                                        task_id: task_id.clone(),
                                        label: format!("调用工具 {name}"),
                                    });
                                }
                                "tool_result" => {
                                    let error = event
                                        .get("is_error")
                                        .and_then(|value| value.as_bool())
                                        .unwrap_or(false);
                                    let head = event
                                        .get("content")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or_default()
                                        .lines()
                                        .next()
                                        .unwrap_or_default()
                                        .to_string();
                                    let _ = event_tx.send(UiEvent::ToolProgress {
                                        task_id: task_id.clone(),
                                        label: format!(
                                            "工具结果{}：{head}",
                                            if error { "（失败）" } else { "" }
                                        ),
                                    });
                                }
                                "failed" => {
                                    failure = event
                                        .get("message")
                                        .and_then(|value| value.as_str())
                                        .map(str::to_string);
                                }
                                _ => {}
                            }
                        }
                        "response" => {
                            final_response = serde_json::from_value(frame["response"].clone()).ok();
                        }
                        "error" => {
                            failure = frame
                                .get("message")
                                .and_then(|value| value.as_str())
                                .map(str::to_string);
                        }
                        _ => {}
                    }
                }
            }

            let event = match final_response {
                Some(response) => UiEvent::Completed {
                    task_id,
                    response: Box::new(response),
                },
                None => UiEvent::Failed {
                    task_id,
                    message: failure.unwrap_or_else(|| "流式响应提前结束".to_string()),
                },
            };
            let _ = event_tx.send(event);
        });
    });
}

/// 读取 /health：provider 名称与可用 wire 协议。
fn spawn_health_request(event_tx: mpsc::Sender<UiEvent>, server_url: String) {
    spawn_json_request(
        event_tx,
        format!("{}/health", server_url.trim_end_matches('/')),
        UiEvent::HealthLoaded,
    );
}

/// 读取 /v1/tools：内置 + MCP 工具清单。
fn spawn_tools_request(event_tx: mpsc::Sender<UiEvent>, server_url: String) {
    spawn_json_array_request(
        event_tx,
        format!("{}/v1/tools", server_url.trim_end_matches('/')),
        UiEvent::ToolsLoaded,
    );
}

/// 读取 /v1/mcp/servers。
fn spawn_mcp_request(event_tx: mpsc::Sender<UiEvent>, server_url: String) {
    spawn_json_array_request(
        event_tx,
        format!("{}/v1/mcp/servers", server_url.trim_end_matches('/')),
        UiEvent::McpLoaded,
    );
}

/// 读取 /v1/providers/cliproxyapi/accounts。
fn spawn_accounts_request(event_tx: mpsc::Sender<UiEvent>, server_url: String) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime.block_on(async move {
                    Client::new()
                        .get(format!(
                            "{}/v1/providers/cliproxyapi/accounts",
                            server_url.trim_end_matches('/')
                        ))
                        .send()
                        .await
                        .map_err(|error| error.to_string())?
                        .error_for_status()
                        .map_err(|error| error.to_string())?
                        .json::<Vec<CliProxyAccount>>()
                        .await
                        .map_err(|error| error.to_string())
                })
            });
        let event = match result {
            Ok(accounts) => UiEvent::AccountsLoaded(accounts),
            Err(message) => UiEvent::ApiFailed(message),
        };
        let _ = event_tx.send(event);
    });
}

fn spawn_json_request(
    event_tx: mpsc::Sender<UiEvent>,
    url: String,
    wrap: fn(serde_json::Value) -> UiEvent,
) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime.block_on(async move {
                    Client::new()
                        .get(url)
                        .send()
                        .await
                        .map_err(|error| error.to_string())?
                        .error_for_status()
                        .map_err(|error| error.to_string())?
                        .json::<serde_json::Value>()
                        .await
                        .map_err(|error| error.to_string())
                })
            });
        let event = match result {
            Ok(value) => wrap(value),
            Err(message) => UiEvent::ApiFailed(message),
        };
        let _ = event_tx.send(event);
    });
}

fn spawn_json_array_request(
    event_tx: mpsc::Sender<UiEvent>,
    url: String,
    wrap: fn(Vec<serde_json::Value>) -> UiEvent,
) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime.block_on(async move {
                    Client::new()
                        .get(url)
                        .send()
                        .await
                        .map_err(|error| error.to_string())?
                        .error_for_status()
                        .map_err(|error| error.to_string())?
                        .json::<Vec<serde_json::Value>>()
                        .await
                        .map_err(|error| error.to_string())
                })
            });
        let event = match result {
            Ok(values) => wrap(values),
            Err(message) => UiEvent::ApiFailed(message),
        };
        let _ = event_tx.send(event);
    });
}

fn spawn_login_request(event_tx: mpsc::Sender<UiEvent>, server_url: String, provider: String) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime.block_on(async move {
                    Client::new()
                        .post(format!(
                            "{}/v1/providers/cliproxyapi/login",
                            server_url.trim_end_matches('/')
                        ))
                        .json(&serde_json::json!({ "provider": provider }))
                        .send()
                        .await
                        .map_err(|error| error.to_string())?
                        .error_for_status()
                        .map_err(|error| error.to_string())?
                        .json::<CliProxyLoginStart>()
                        .await
                        .map_err(|error| error.to_string())
                })
            });
        let event = result
            .map(UiEvent::LoginStarted)
            .unwrap_or_else(UiEvent::ApiFailed);
        let _ = event_tx.send(event);
    });
}

fn spawn_login_status_request(event_tx: mpsc::Sender<UiEvent>, server_url: String, state: String) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime.block_on(async move {
                    Client::new()
                        .get(format!(
                            "{}/v1/providers/cliproxyapi/login/status",
                            server_url.trim_end_matches('/')
                        ))
                        .query(&[("state", state)])
                        .send()
                        .await
                        .map_err(|error| error.to_string())?
                        .error_for_status()
                        .map_err(|error| error.to_string())?
                        .json::<CliProxyLoginStatus>()
                        .await
                        .map_err(|error| error.to_string())
                })
            });
        let event = result
            .map(UiEvent::LoginStatus)
            .unwrap_or_else(UiEvent::ApiFailed);
        let _ = event_tx.send(event);
    });
}

fn spawn_models_request(event_tx: mpsc::Sender<UiEvent>, server_url: String) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime.block_on(async move {
                    Client::new()
                        .get(format!(
                            "{}/v1/providers/cliproxyapi/models",
                            server_url.trim_end_matches('/')
                        ))
                        .send()
                        .await
                        .map_err(|error| error.to_string())?
                        .error_for_status()
                        .map_err(|error| error.to_string())?
                        .json::<Vec<CliProxyModel>>()
                        .await
                        .map_err(|error| error.to_string())
                })
            });
        let event = result
            .map(UiEvent::ModelsLoaded)
            .unwrap_or_else(UiEvent::ApiFailed);
        let _ = event_tx.send(event);
    });
}

fn spawn_verify_request(event_tx: mpsc::Sender<UiEvent>, server_url: String, model: String) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|error| error.to_string())
            .and_then(|runtime| {
                runtime.block_on(async move {
                    Client::new()
                        .post(format!(
                            "{}/v1/providers/cliproxyapi/verify",
                            server_url.trim_end_matches('/')
                        ))
                        .json(&serde_json::json!({ "model": model }))
                        .send()
                        .await
                        .map_err(|error| error.to_string())?
                        .error_for_status()
                        .map_err(|error| error.to_string())?
                        .json::<CliProxyVerification>()
                        .await
                        .map_err(|error| error.to_string())
                })
            });
        let event = result
            .map(UiEvent::ModelVerified)
            .unwrap_or_else(UiEvent::ApiFailed);
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

fn chinese_font_definitions() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto-sans-sc".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/NotoSansSC-VF.ttf"
        ))),
    );
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .expect("egui must provide the proportional font family")
        .insert(0, "noto-sans-sc".to_owned());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .expect("egui must provide the monospace font family")
        .insert(0, "noto-sans-sc".to_owned());
    fonts
}

fn friendly_api_error(message: &str) -> String {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("/v1/providers/cliproxyapi")
        && normalized.contains("503 service unavailable")
    {
        return "CLIProxyAPI 尚未就绪。请关闭后重新打开 Wonderland 桌面版；若仍失败，请查看 %LOCALAPPDATA%\\WonderlandData\\launcher.log。".to_string();
    }
    if normalized.contains("/v1/providers/cliproxyapi")
        && normalized.contains("500 internal server error")
    {
        return "CLIProxyAPI 启动或本机连接失败。请查看 %LOCALAPPDATA%\\WonderlandData\\launcher.log。".to_string();
    }
    if normalized.contains("connection refused") || normalized.contains("error sending request") {
        return "无法连接 Wonderland 后端。请关闭后重新打开桌面版。".to_string();
    }
    message.to_string()
}

fn main() -> eframe::Result {
    let icon = egui::IconData {
        rgba: include_bytes!("../../assets/icons/icon.rgba").to_vec(),
        width: 256,
        height: 256,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(Vec2::new(1280.0, 820.0))
            .with_min_inner_size(Vec2::new(940.0, 620.0))
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    eframe::run_native(
        "Wonderland",
        options,
        Box::new(|cc| Ok(Box::new(DesktopApp::new(cc)))),
    )
}

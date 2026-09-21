#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{
    sync::{mpsc, Arc},
    thread,
};

use eframe::egui::{
    self, Align, Color32, Frame, Layout, RichText, ScrollArea, Stroke, TextEdit, Ui, Vec2, Visuals,
};

use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use uuid::Uuid;
use wonderland::cliproxy::{
    CliProxyAccount, CliProxyLoginStart, CliProxyLoginStatus, CliProxyModel, CliProxyVerification,
};
use wonderland::model::{AgentRequest, AgentResponse};
#[path = "desktop/ui.rs"]
mod desktop_ui;
#[path = "desktop/icons.rs"]
mod icons;
#[path = "desktop/studio.rs"]
mod studio;
#[path = "desktop/workbench.rs"]
mod workbench;

const BG: Color32 = Color32::from_rgb(16, 19, 24);
const PANEL: Color32 = Color32::from_rgb(21, 25, 32);
const CARD: Color32 = Color32::from_rgb(29, 35, 44);
const ACCENT: Color32 = Color32::from_rgb(166, 235, 207);
const MUTED: Color32 = Color32::from_rgb(149, 160, 176);
const BORDER: Color32 = Color32::from_rgb(44, 53, 65);
const TEXT: Color32 = Color32::from_rgb(231, 237, 244);

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

#[derive(Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: MessageRole,
    text: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Task {
    id: String,
    session_id: String,
    #[serde(default)]
    cwd: String,
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
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    usage: wonderland::provider::Usage,
}

#[derive(Clone)]
struct PendingTaskSwitch {
    task_id: String,
    cwd: String,
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
    McpLogin(serde_json::Value),
    SearchLoaded(Vec<serde_json::Value>),
    ConnectionLoaded(serde_json::Value),
    SessionLoaded(wonderland::session::Session),
    Approval(serde_json::Value),
    ApiFailed(String),
}

struct DesktopApp {
    #[cfg(feature = "ui-snapshots")]
    snapshot_frames: usize,
    cancellations: std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>,
    server_url: String,
    user_id: String,
    tasks: Vec<Task>,
    active_task: usize,
    pending_task_switch: Option<PendingTaskSwitch>,
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
    working_dir: String,
    mcp_login: Option<serde_json::Value>,
    search_query: String,
    search_hits: Vec<serde_json::Value>,
    connection: wonderland::connection::ConnectionSettings,
    has_api_key: bool,
    permission_mode: wonderland::permissions::PermissionMode,
    approvals: Vec<serde_json::Value>,
    workbench: workbench::Workbench,
    studio: studio::Studio,
    closing_requested: bool,
    allow_close: bool,
}

impl DesktopApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_fonts(chinese_font_definitions());
        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        cc.egui_ctx.set_visuals(Visuals::dark());
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = Visuals::dark();
        style.visuals.override_text_color = Some(TEXT);
        style.spacing.item_spacing = Vec2::new(10.0, 9.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        style.visuals.panel_fill = PANEL;
        style.visuals.window_fill = PANEL;
        style.visuals.extreme_bg_color = BG;
        style.spacing.interact_size.y = 32.0;
        style.visuals.selection.bg_fill = Color32::from_rgb(43, 69, 63);
        style.visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
        style.visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        for widget in [
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            widget.corner_radius = egui::CornerRadius::same(8);
            widget.bg_stroke = Stroke::new(1.0_f32, BORDER);
            widget.fg_stroke = Stroke::new(1.5_f32, TEXT);
        }
        style.visuals.widgets.inactive.bg_fill = CARD;
        style.visuals.widgets.inactive.weak_bg_fill = CARD;
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(40, 50, 61);
        style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(40, 50, 61);
        style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.7));
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(40, 66, 59);
        style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(40, 66, 59);
        style.visuals.widgets.open.bg_fill = CARD;
        style.visuals.widgets.open.weak_bg_fill = CARD;
        style.visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
        cc.egui_ctx.set_style(style);
        let (event_tx, event_rx) = mpsc::channel();
        let snapshot =
            cfg!(feature = "ui-snapshots") && std::env::var_os("WONDERLAND_SNAPSHOT").is_some();
        let mut tasks = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "tasks"))
            .filter(|tasks: &Vec<Task>| !tasks.is_empty())
            .unwrap_or_else(|| vec![Task::new("新任务")]);
        if snapshot {
            tasks = vec![Task::new("新任务")];
        }
        for task in &mut tasks {
            task.running = false;
        }
        let server_url = std::env::var("AGENT_SERVER_URL")
            .ok()
            .or_else(|| {
                cc.storage
                    .and_then(|storage| eframe::get_value(storage, "server_url"))
            })
            .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
        let user_id = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, "user_id"))
            .unwrap_or_else(|| "local-user".to_string());
        let server_url_for_probe = server_url.clone();
        let event_tx_for_probe = event_tx.clone();
        let fallback_dir = std::env::current_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .display()
            .to_string();
        let startup_dir = std::env::var("AGENT_CWD").unwrap_or_else(|_| fallback_dir.clone());
        let storage = cc.storage.filter(|_| !snapshot);
        let preferred_dir: String = storage
            .and_then(|storage| eframe::get_value(storage, "working_dir"))
            .unwrap_or(startup_dir);
        let saved_active: Option<String> =
            storage.and_then(|storage| eframe::get_value(storage, "active_task_id"));
        let (active_task, working_dir, restore_notice) = restore_task_workspace(
            &mut tasks,
            saved_active.as_deref(),
            &preferred_dir,
            &fallback_dir,
        );
        let mut app = Self {
            #[cfg(feature = "ui-snapshots")]
            snapshot_frames: 0,
            cancellations: Default::default(),
            server_url,
            user_id,
            tasks,
            active_task,
            pending_task_switch: None,
            view: ViewMode::Planning,
            input: String::new(),
            status: restore_notice.unwrap_or_else(|| "就绪".to_string()),
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
            reasoning_effort: "auto".to_string(),
            tools: Vec::new(),
            mcp_servers: Vec::new(),
            accounts: Vec::new(),
            show_conn_panel: snapshot && std::env::var_os("WONDERLAND_SNAPSHOT_SETTINGS").is_some(),
            working_dir: working_dir.clone(),
            mcp_login: None,
            search_query: String::new(),
            search_hits: Vec::new(),
            connection: wonderland::connection::ConnectionSettings {
                provider: "subscription".into(),
                ..Default::default()
            },
            has_api_key: false,
            permission_mode: wonderland::permissions::PermissionMode::Default,
            approvals: Vec::new(),
            workbench: workbench::Workbench::new(&working_dir),
            studio: studio::Studio::new(&working_dir),
            closing_requested: false,
            allow_close: false,
        };
        if let Some(storage) = cc.storage.filter(|_| !snapshot) {
            app.selected_model = eframe::get_value(storage, "selected_model").unwrap_or_default();
            app.reasoning_effort =
                eframe::get_value(storage, "reasoning_effort").unwrap_or_else(|| "auto".into());
            app.workbench.restore(storage);
            app.studio.restore(storage, &app.server_url);
        }
        spawn_json_request(
            app.event_tx.clone(),
            format!("{}/v1/connection", app.server_url.trim_end_matches('/')),
            UiEvent::ConnectionLoaded,
        );
        // 启动即抓一份后端能力信息（provider / 协议栈 / 工具 / MCP / 账号）。
        spawn_health_request(event_tx_for_probe, server_url_for_probe);
        app
    }

    fn add_task(&mut self) {
        if self.workbench.has_pending_root() {
            self.status = "请先确认或取消工作区切换".into();
            return;
        }
        self.pending_task_switch = None;
        let number = self.tasks.len() + 1;
        let mut task = Task::new(&format!("任务 {number}"));
        task.cwd = self.working_dir.clone();
        self.tasks.push(task);
        self.active_task = self.tasks.len() - 1;
        self.view = ViewMode::Planning;
        self.status = "已创建新任务".to_string();
    }

    fn select_task(&mut self, index: usize) {
        self.sync_workspace_change();
        let Some(task) = self.tasks.get(index) else {
            return;
        };
        if existing_workspace(&task.cwd).is_none() {
            self.status = format!(
                "任务目录已不存在：{}。请先恢复该目录；历史任务已保留。",
                task.cwd
            );
            return;
        }
        if same_workspace(&task.cwd, &self.workbench.root.display().to_string()) {
            if self.workbench.has_pending_root() {
                self.status = "请先确认或取消工作区切换".into();
                return;
            }
            self.pending_task_switch = None;
            self.active_task = index;
            self.working_dir = self.workbench.root.display().to_string();
            self.view = ViewMode::Planning;
            self.workbench.pane = workbench::Pane::Chat;
            return;
        }
        self.pending_task_switch = Some(PendingTaskSwitch {
            task_id: task.id.clone(),
            cwd: task.cwd.clone(),
        });
        self.workbench
            .request_root(std::path::PathBuf::from(&task.cwd));
        self.sync_workspace_change();
    }

    fn sync_workspace_change(&mut self) {
        if let Some(root) = self.workbench.changed_root.take() {
            self.working_dir = root;
            let selected = self.pending_task_switch.take().and_then(|pending| {
                task_selected_for_root(&self.tasks, &pending, &self.working_dir)
            });
            if let Some(index) = selected {
                // This root change was requested by selecting an existing task.
                // Keep its session and running request instead of creating a task.
                self.active_task = index;
                self.view = ViewMode::Planning;
                self.workbench.pane = workbench::Pane::Chat;
            } else {
                apply_manual_workspace(&mut self.tasks, &mut self.active_task, &self.working_dir);
            }
        } else if self.pending_task_switch.is_some() && !self.workbench.has_pending_root() {
            // The user cancelled the workbench's unsaved-file confirmation.
            self.pending_task_switch = None;
        }
    }

    fn send_active(&mut self) {
        self.sync_workspace_change();
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return;
        }
        let Some(task) = self.tasks.get(self.active_task) else {
            return;
        };
        let cwd = match request_task_cwd(
            task,
            &self.workbench.root,
            self.workbench.has_pending_root(),
        ) {
            Ok(cwd) => cwd,
            Err(message) => {
                self.status = message;
                return;
            }
        };
        let task = &mut self.tasks[self.active_task];
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
        task.stream_text.clear();
        task.reasoning.clear();
        task.tool_log.clear();
        self.input.clear();
        self.status = "Agent 正在规划并执行...".to_string();

        let reasoning_effort = match self.reasoning_effort.as_str() {
            "" | "auto" => None,
            other => Some(other.to_string()),
        };
        let request = AgentRequest {
            session_id,
            user_id: Some(self.user_id.clone()),
            model: (!self.selected_model.trim().is_empty()).then(|| self.selected_model.clone()),
            skills: Vec::new(),
            mode: Some(self.permission_mode),
            cwd: Some(cwd),
            reasoning_effort,
            input,
        };
        self.cancellations.insert(
            task_id.clone(),
            spawn_agent_request(
                self.event_tx.clone(),
                self.server_url.clone(),
                task_id,
                request,
            ),
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
                    self.cancellations.remove(&task_id);
                    self.approvals.retain(|p| p["task_id"] != task_id);
                    if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
                        task.running = false;
                        task.usage = response.usage;
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
                        task.reasoning.push_str(&text);
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
                UiEvent::McpLogin(value) => {
                    if value["status"] == "ok" {
                        self.status = "MCP 登录成功，请点击重新连接加载工具".into();
                    }
                    self.mcp_login = Some(value);
                }
                UiEvent::SearchLoaded(hits) => {
                    self.search_hits = hits;
                }
                UiEvent::Approval(value) => {
                    self.approvals.push(value);
                }
                UiEvent::ConnectionLoaded(value) => {
                    self.api_request_running = false;
                    self.connection.provider =
                        value["provider"].as_str().unwrap_or("offline").into();
                    self.connection.base_url =
                        value["base_url"].as_str().unwrap_or_default().into();
                    self.connection.model = value["model"].as_str().unwrap_or_default().into();
                    self.connection.wire = serde_json::from_value(value["wire"].clone()).ok();
                    self.has_api_key = value["has_api_key"].as_bool().unwrap_or(false);
                    self.connection.api_key.clear();
                    if !self.connection.model.is_empty() {
                        self.selected_model = self.connection.model.clone();
                    }
                    self.models.clear();
                    if !self.selected_model.is_empty() {
                        self.models.push(CliProxyModel {
                            id: self.selected_model.clone(),
                            object: "model".into(),
                            owned_by: Some(self.connection.provider.clone()),
                        });
                    }
                    self.load_models();
                    self.status = "连接配置已就绪".into();
                }
                UiEvent::SessionLoaded(session) => {
                    let index = if let Some(index) =
                        self.tasks.iter().position(|t| t.session_id == session.id)
                    {
                        index
                    } else {
                        let mut task = Task::new("历史会话");
                        // The server's legacy session format does not record a cwd.
                        // Bind imported sessions explicitly to the current workspace.
                        task.cwd = self.workbench.root.display().to_string();
                        task.session_id = session.id;
                        task.usage = session.usage;
                        task.messages = session
                            .messages
                            .iter()
                            .filter_map(|m| {
                                let text = m.text();
                                if text.is_empty() {
                                    return None;
                                }
                                Some(ChatMessage {
                                    role: if m.role == wonderland::provider::Role::User {
                                        MessageRole::User
                                    } else {
                                        MessageRole::Agent
                                    },
                                    text,
                                })
                            })
                            .collect();
                        if let Some(first) = task.messages.first() {
                            task.title = truncate(&first.text, 24);
                        }
                        self.tasks.push(task);
                        self.tasks.len() - 1
                    };
                    self.select_task(index);
                }
                UiEvent::AccountsLoaded(accounts) => {
                    self.accounts = accounts;
                }
                UiEvent::HealthLoaded(value) => {
                    if self.selected_model.is_empty() {
                        self.selected_model = value["model"].as_str().unwrap_or_default().into();
                    }
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
                    self.cancellations.remove(&task_id);
                    self.approvals.retain(|p| p["task_id"] != task_id);
                    if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
                        task.running = false;
                        if !task.stream_text.is_empty() {
                            task.messages.push(ChatMessage {
                                role: MessageRole::Agent,
                                text: std::mem::take(&mut task.stream_text),
                            });
                        }
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
                    let subscription = matches!(
                        self.connection.provider.as_str(),
                        "subscription" | "cliproxyapi"
                    );
                    if self.selected_model.is_empty()
                        || (subscription
                            && !models.is_empty()
                            && !models.iter().any(|m| m.id == self.selected_model))
                    {
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
        self.status = "正在读取当前连接的模型目录...".to_string();
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

    fn render_task_list(&mut self, ui: &mut Ui) {
        use icons::{glyph, Icon};
        let available = ui.available_height();
        if ui
            .add_sized(
                [ui.available_width(), 40.0],
                egui::Button::new(RichText::new("＋  新建任务").strong().color(BG))
                    .fill(ACCENT)
                    .corner_radius(10),
            )
            .clicked()
        {
            self.add_task();
        }
        ui.add_space(24.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("最近任务").size(11.0).color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(self.tasks.len().to_string())
                        .small()
                        .color(MUTED),
                );
            });
        });
        ui.add_space(6.0);
        let mut selected = None;
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height((available - 350.0).max(120.0))
            .min_scrolled_height((available - 350.0).max(120.0))
            .show(ui, |ui| {
                for (index, task) in self.tasks.iter().enumerate() {
                    let active = index == self.active_task;
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 72.0),
                        egui::Sense::click(),
                    );
                    let painter = ui.painter();
                    painter.rect_filled(
                        rect,
                        10,
                        if active {
                            CARD
                        } else if response.hovered() {
                            Color32::from_rgb(25, 30, 38)
                        } else {
                            Color32::TRANSPARENT
                        },
                    );
                    if active {
                        painter.rect_filled(
                            egui::Rect::from_min_size(
                                rect.min + Vec2::new(0., 18.),
                                Vec2::new(3., 36.),
                            ),
                            2,
                            ACCENT,
                        );
                    }
                    icons::draw(
                        ui,
                        egui::Rect::from_min_size(rect.min + Vec2::new(14., 16.), Vec2::splat(16.)),
                        Icon::Chat,
                        if active { ACCENT } else { MUTED },
                    );
                    painter.text(
                        rect.min + Vec2::new(40., 24.),
                        egui::Align2::LEFT_CENTER,
                        truncate(&task.title, 13),
                        egui::FontId::proportional(13.0),
                        TEXT,
                    );
                    let preview = if task.running {
                        "正在执行…"
                    } else {
                        task.messages
                            .last()
                            .map(|m| m.text.lines().next().unwrap_or(""))
                            .unwrap_or("等待你的第一个想法")
                    };
                    painter.text(
                        rect.min + Vec2::new(40., 49.),
                        egui::Align2::LEFT_CENTER,
                        truncate(preview, 15),
                        egui::FontId::proportional(11.0),
                        MUTED,
                    );
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::SelectableLabel,
                            true,
                            &task.title,
                        )
                    });
                    if response.clicked() {
                        selected = Some(index);
                    }
                    ui.add_space(3.0);
                }
            });
        if let Some(index) = selected {
            self.select_task(index);
        }
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            glyph(ui, Icon::Folder, 16.0, MUTED);
            ui.label(RichText::new("当前工作区").small().color(MUTED));
        });
        let folder = std::path::Path::new(&self.working_dir)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.working_dir);
        ui.label(RichText::new(truncate(folder, 22)).size(12.0))
            .on_hover_text(&self.working_dir);
        ui.add_space(14.0);
        Frame::new()
            .fill(BG)
            .corner_radius(10)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_min_width((ui.available_width() - 24.0).max(80.0));
                ui.horizontal(|ui| {
                    glyph(ui, Icon::Link, 16.0, ACCENT);
                    ui.label(
                        RichText::new(if self.provider_name == "unknown" {
                            "正在连接"
                        } else if self.provider_name == "offline" {
                            "离线演示"
                        } else {
                            "模型已连接"
                        })
                        .size(12.0)
                        .color(ACCENT),
                    );
                });
                ui.label(
                    RichText::new(if self.selected_model.is_empty() {
                        "在设置中连接你的模型"
                    } else {
                        &self.selected_model
                    })
                    .small()
                    .color(MUTED),
                );
            });
    }

    fn render_connection_panel(&mut self, ctx: &egui::Context) {
        if !self.show_conn_panel {
            return;
        }
        let mut open = self.show_conn_panel;
        egui::SidePanel::right("connection-panel")
            .resizable(true)
            .default_width(370.0)
            .min_width(330.0)
            .frame(Frame::new().fill(PANEL).inner_margin(20.0))
            .show(ctx, |ui| {
                ScrollArea::vertical().show(ui,|ui|{
                ui.horizontal(|ui| {
                    ui.heading("工作区设置");
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
                self.render_provider_settings(ui);
                ui.separator();
                ui.collapsing("连接诊断", |ui| {
                ui.label(RichText::new(format!("服务：{}", self.provider_name)).color(MUTED));
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
                });
                ui.separator();
                let profile = wonderland::model_profile::ModelProfile::resolve(&self.selected_model);
                ui.collapsing("模型能力档案", |ui| {
                    ui.label(format!("模型：{}", self.selected_model));
                    ui.label(format!("档案：{}", profile.name));
                    ui.label(format!("上下文：{} tokens", profile.context_window));
                    ui.label(format!("输出上限：{} tokens", profile.max_output_tokens));
                    ui.label(format!("协议：{}", profile.protocol.as_str()));
                    ui.label(format!("缓存：{:?} · 推理：{:?}", profile.cache, profile.reasoning));
                });
                ui.collapsing("检索历史会话", |ui| {
                    ui.text_edit_singleline(&mut self.search_query);
                    if ui.button("搜索").clicked() {
                        let mut url = reqwest::Url::parse(&format!("{}/v1/sessions/search",self.server_url.trim_end_matches('/'))).ok();
                        if let Some(url) = url.as_mut() {
                            url.query_pairs_mut().append_pair("q", &self.search_query).append_pair("user_id", &self.user_id);
                            spawn_json_array_request(self.event_tx.clone(),url.to_string(),UiEvent::SearchLoaded);
                        }
                    }
                    ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                        for hit in &self.search_hits {
                            let id=hit["id"].as_str().unwrap_or_default();
                            if ui.link(id).clicked(){spawn_session_request(self.event_tx.clone(),self.server_url.clone(),id.to_string());}
                            ui.label(RichText::new(hit["snippet"].as_str().unwrap_or_default()).small());
                            ui.separator();
                        }
                    });
                });

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
                    if ui.button("重新连接 / 加载配置").clicked() {
                        spawn_mcp_post(self.event_tx.clone(), self.server_url.clone(), "/v1/mcp/reload".into(), self.working_dir.clone(), false);
                    }
                    if let Some(login) = &self.mcp_login {
                        if let Some(url) = login["url"].as_str() { ui.hyperlink_to("打开 MCP 授权页面", url); }
                        if let Some(state) = login["state"].as_str() {
                            if ui.button("检查 MCP 登录").clicked() { spawn_json_request(self.event_tx.clone(),format!("{}/v1/mcp/login/status?state={}",self.server_url.trim_end_matches('/'),state),UiEvent::McpLogin); }
                        }
                        if let Some(error) = login["error"].as_str() { ui.colored_label(egui::Color32::LIGHT_RED,error); }
                    }
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
                        if let Some(error) = server["error"].as_str() { ui.colored_label(egui::Color32::LIGHT_RED,error); }
                        if ui.small_button(format!("OAuth 登录 {name}")).clicked() {
                            let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
                            spawn_mcp_post(self.event_tx.clone(),self.server_url.clone(),format!("/v1/mcp/{encoded}/login"),self.working_dir.clone(),true);
                        }
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
            });
        self.show_conn_panel = open;
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
                self.select_task(index);
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
    fn persist_egui_memory(&self) -> bool {
        !(cfg!(feature = "ui-snapshots") && std::env::var_os("WONDERLAND_SNAPSHOT").is_some())
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        #[cfg(feature = "ui-snapshots")]
        if std::env::var_os("WONDERLAND_SNAPSHOT").is_some() {
            return;
        }
        let mut tasks = self.tasks.clone();
        for task in &mut tasks {
            task.running = false;
        }
        eframe::set_value(storage, "tasks", &tasks);
        eframe::set_value(storage, "server_url", &self.server_url);
        eframe::set_value(storage, "user_id", &self.user_id);
        eframe::set_value(storage, "selected_model", &self.selected_model);
        eframe::set_value(storage, "reasoning_effort", &self.reasoning_effort);
        eframe::set_value(storage, "working_dir", &self.working_dir);
        if let Some(task) = self.tasks.get(self.active_task) {
            eframe::set_value(storage, "active_task_id", &task.id);
        }
        self.workbench.save(storage);
        self.studio.save(storage);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.workbench.poll(ctx);
        self.sync_workspace_change();
        self.studio.project_context(&self.working_dir);
        self.studio.poll(ctx, &self.server_url);
        if let Some(app_id) = self.studio.launch_cli.take() {
            if let Some(profile) = self
                .workbench
                .profiles
                .iter()
                .find(|p| p.id == app_id)
                .cloned()
            {
                self.workbench.show_terminal = true;
                self.workbench.launch(Some(profile), false);
            } else {
                self.studio.notice =
                    format!("应用 {app_id} 尚无本地终端配置，请在 CLI 工作台添加入口。");
            }
        }
        if let Some(project) = self.studio.open_project.take() {
            self.workbench
                .request_root(std::path::PathBuf::from(project));
        }
        if std::mem::take(&mut self.studio.choose_project) {
            self.workbench.choose_root();
        }
        if let Some(context) = self.workbench.chat_context.take() {
            self.input = context;
            self.view = ViewMode::Planning;
            self.studio.page = studio::Page::Api;
            self.workbench.pane = workbench::Pane::Chat;
        }
        let snapshot =
            cfg!(feature = "ui-snapshots") && std::env::var_os("WONDERLAND_SNAPSHOT").is_some();
        if !snapshot
            && !self.allow_close
            && ctx.input(|i| i.viewport().close_requested())
            && (self.workbench.has_unsaved()
                || self.workbench.has_running()
                || self.tasks.iter().any(|t| t.running))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing_requested = true;
        }
        #[cfg(feature = "ui-snapshots")]
        self.render_snapshot(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        egui::TopBottomPanel::top("top-bar")
            .frame(
                Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ctx, |ui| self.render_header(ui));
        let previous_pane = self.workbench.pane;
        egui::TopBottomPanel::top("workspace-toolbar")
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| self.workbench.toolbar(ui));
        if self.workbench.pane != previous_pane {
            self.studio.page = studio::Page::Api;
        }
        self.workbench
            .terminal_panel(ctx, self.closing_requested || !self.approvals.is_empty());
        self.workbench.dialogs(ctx);
        self.sync_workspace_change();
        if self.closing_requested {
            egui::Modal::new(egui::Id::new("close-workspace-dialog")).show(ctx, |ui| {
                ui.heading("关闭工作区");
                ui.label(
                    "存在未保存编辑或正在运行的会话。退出会丢弃未保存编辑，并停止本窗口的终端和 API 对话。后台任务会继续运行。",
                );
                ui.horizontal(|ui| {
                    if ui.button("继续工作").clicked() {
                        self.closing_requested = false;
                    }
                    if ui.button("停止并退出").clicked() {
                        self.workbench.stop_all();
                        self.cancellations.clear();
                        self.allow_close = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        }
        let studio_mode = matches!(
            self.studio.page,
            studio::Page::Work | studio::Page::Chat | studio::Page::Apps | studio::Page::Projects
        );
        // Give the conversation room when settings are open on a small screen.
        if !studio_mode
            && self.workbench.pane == workbench::Pane::Chat
            && (!self.show_conn_panel || ctx.screen_rect().width() >= 1180.0)
        {
            egui::SidePanel::left("task-list")
                .default_width(236.0)
                .min_width(210.0)
                .max_width(320.0)
                .resizable(true)
                .frame(Frame::new().fill(PANEL).inner_margin(16.0))
                .show(ctx, |ui| self.render_task_list(ui));
        }
        if studio_mode
            && matches!(self.studio.page, studio::Page::Work | studio::Page::Chat)
            && (!self.show_conn_panel || ctx.screen_rect().width() >= 1180.0)
        {
            egui::SidePanel::left("studio-task-list")
                .default_width(236.0)
                .min_width(210.0)
                .max_width(320.0)
                .resizable(true)
                .frame(Frame::new().fill(PANEL).inner_margin(16.0))
                .show(ctx, |ui| self.studio.sidebar(ui));
        }

        self.render_connection_panel(ctx);
        self.render_approval(ctx);
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(24.0))
            .show(ctx, |ui| {
                if studio_mode {
                    self.studio.render(ui, &self.working_dir);
                } else if self.workbench.pane != workbench::Pane::Chat {
                    self.workbench.render_pane(ui);
                } else {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.view, ViewMode::Planning, "对话");
                        ui.selectable_value(&mut self.view, ViewMode::Parallel, "多任务");
                    });
                    ui.add_space(8.0);
                    match self.view {
                        ViewMode::Planning => self.render_planning(ui),
                        ViewMode::Parallel => self.render_parallel(ui),
                    }
                }
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

fn existing_workspace(path: &str) -> Option<std::path::PathBuf> {
    if path.trim().is_empty() {
        return None;
    }
    let path = std::path::Path::new(path).canonicalize().ok()?;
    path.is_dir().then_some(path)
}

fn same_workspace(left: &str, right: &str) -> bool {
    match (existing_workspace(left), existing_workspace(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn task_selected_for_root(
    tasks: &[Task],
    pending: &PendingTaskSwitch,
    root: &str,
) -> Option<usize> {
    same_workspace(&pending.cwd, root)
        .then(|| {
            tasks
                .iter()
                .position(|task| task.id == pending.task_id && same_workspace(&task.cwd, root))
        })
        .flatten()
}

fn restore_task_workspace(
    tasks: &mut Vec<Task>,
    saved_active: Option<&str>,
    preferred: &str,
    fallback: &str,
) -> (usize, String, Option<String>) {
    let mut notice = None;
    let root = if existing_workspace(preferred).is_some() {
        preferred.to_owned()
    } else {
        notice = Some(format!(
            "上次工作区已不存在：{preferred}。已打开当前目录，历史任务保持原目录。"
        ));
        fallback.to_owned()
    };
    if tasks.is_empty() {
        tasks.push(Task::new("新任务"));
    }
    for task in tasks.iter_mut() {
        if task.cwd.is_empty() {
            task.cwd = root.clone();
        }
    }
    let active = saved_active
        .and_then(|id| tasks.iter().position(|task| task.id == id))
        .unwrap_or(0);
    if existing_workspace(&tasks[active].cwd).is_some() {
        (active, tasks[active].cwd.clone(), notice)
    } else {
        notice = Some(format!(
            "历史任务目录已不存在：{}。已保留历史，并在当前工作区新建任务。",
            tasks[active].cwd
        ));
        let mut task = Task::new("新任务");
        task.cwd = root.clone();
        tasks.push(task);
        (tasks.len() - 1, root, notice)
    }
}

fn apply_manual_workspace(tasks: &mut Vec<Task>, active: &mut usize, root: &str) {
    let preserve_current = tasks.get(*active).is_some_and(|task| {
        (task.running || !task.messages.is_empty()) && !same_workspace(&task.cwd, root)
    });
    if preserve_current || tasks.get(*active).is_none() {
        let mut task = Task::new(&format!("任务 {}", tasks.len() + 1));
        task.cwd = root.into();
        tasks.push(task);
        *active = tasks.len() - 1;
    } else if let Some(task) = tasks.get_mut(*active) {
        task.cwd = root.into();
    }
}

fn request_task_cwd(
    task: &Task,
    root: &std::path::Path,
    root_pending: bool,
) -> Result<String, String> {
    if root_pending {
        return Err("请先确认或取消工作区切换，再发送任务".into());
    }
    let current = root.display().to_string();
    if existing_workspace(&task.cwd).is_none() {
        return Err(format!("任务目录不存在：{}。未发送请求。", task.cwd));
    }
    if !same_workspace(&task.cwd, &current) {
        return Err("任务目录与当前工作区不一致，请重新选择该任务后发送".into());
    }
    Ok(task.cwd.clone())
}

#[cfg(test)]
mod workspace_task_tests {
    use super::*;

    fn task_at(root: &std::path::Path, running: bool) -> Task {
        let mut task = Task::new("existing task");
        task.cwd = root.display().to_string();
        task.running = running;
        task.messages.push(ChatMessage {
            role: MessageRole::User,
            text: "existing history".into(),
        });
        task
    }

    #[test]
    fn task_selection_waits_for_matching_root_and_preserves_running_session() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let tasks = vec![task_at(first.path(), false), task_at(second.path(), true)];
        let pending = PendingTaskSwitch {
            task_id: tasks[1].id.clone(),
            cwd: tasks[1].cwd.clone(),
        };
        assert_eq!(
            task_selected_for_root(&tasks, &pending, &tasks[0].cwd),
            None
        );
        assert_eq!(
            task_selected_for_root(&tasks, &pending, &tasks[1].cwd),
            Some(1)
        );
        assert!(tasks[1].running);
        assert_eq!(tasks.len(), 2);
        let reordered = vec![tasks[1].clone(), tasks[0].clone()];
        assert_eq!(
            task_selected_for_root(&reordered, &pending, &tasks[1].cwd),
            Some(0)
        );
    }

    #[test]
    fn manual_workspace_preserves_history_but_same_root_does_not_duplicate_running_tasks() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let original = task_at(first.path(), true);
        let original_id = original.id.clone();
        let original_cwd = original.cwd.clone();
        let mut tasks = vec![original];
        let mut active = 0;
        apply_manual_workspace(
            &mut tasks,
            &mut active,
            &first.path().join(".").display().to_string(),
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, original_id);
        apply_manual_workspace(
            &mut tasks,
            &mut active,
            &second.path().display().to_string(),
        );
        assert_eq!(active, 1);
        assert_eq!(tasks.len(), 2);
        assert!(same_workspace(&tasks[0].cwd, &original_cwd));
        assert!(tasks[0].running);
        assert!(tasks[1].messages.is_empty());
    }

    #[test]
    fn send_rejects_pending_missing_or_wrong_workspace_without_retargeting_task() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut task = task_at(first.path(), false);
        assert!(request_task_cwd(&task, first.path(), true).is_err());
        assert!(request_task_cwd(&task, second.path(), false).is_err());
        assert_eq!(
            request_task_cwd(&task, first.path(), false).unwrap(),
            task.cwd
        );
        task.cwd = first.path().join("missing").display().to_string();
        assert!(request_task_cwd(&task, first.path(), false).is_err());
    }

    #[test]
    fn restoring_missing_directory_preserves_history_and_selects_valid_workspace() {
        let fallback = tempfile::tempdir().unwrap();
        let missing = fallback.path().join("deleted-project");
        let historical = task_at(&missing, false);
        let historical_id = historical.id.clone();
        let historical_cwd = historical.cwd.clone();
        let mut tasks = vec![historical];
        let (active, root, notice) = restore_task_workspace(
            &mut tasks,
            Some(&historical_id),
            &historical_cwd,
            &fallback.path().display().to_string(),
        );
        assert_eq!(active, 1);
        assert_eq!(tasks[0].cwd, historical_cwd);
        assert_eq!(tasks[0].messages.len(), 1);
        assert_eq!(root, fallback.path().display().to_string());
        assert!(notice.is_some());
        assert!(request_task_cwd(&tasks[active], fallback.path(), false).is_ok());
    }

    #[test]
    fn restoring_selected_task_uses_its_directory_and_keeps_other_task_directories() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut tasks = vec![task_at(first.path(), false), task_at(second.path(), false)];
        let id = tasks[1].id.clone();
        let first_cwd = tasks[0].cwd.clone();
        let (active, root, _) =
            restore_task_workspace(&mut tasks, Some(&id), &first_cwd, &first_cwd);
        assert_eq!(active, 1);
        assert_eq!(root, tasks[1].cwd);
        assert_eq!(tasks[0].cwd, first_cwd);
        assert_eq!(tasks.len(), 2);
    }
}

impl Task {
    fn new(title: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session_id: Uuid::new_v4().to_string(),
            cwd: String::new(),
            title: title.to_string(),
            messages: Vec::new(),
            plan: Vec::new(),
            reflection: None,
            running: false,
            stream_text: String::new(),
            tool_log: Vec::new(),
            reasoning: String::new(),
            usage: Default::default(),
        }
    }
}

fn spawn_agent_request(
    event_tx: mpsc::Sender<UiEvent>,
    server_url: String,
    task_id: String,
    request: AgentRequest,
) -> tokio::sync::oneshot::Sender<()> {
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
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
        let cancel_events = event_tx.clone();
        let cancel_id = task_id.clone();
        runtime.block_on(async move {
            let run=async move {
            use futures_util::StreamExt;

            let url = format!("{}/v1/agent/stream", server_url.trim_end_matches('/'));
            let response = match wonderland::connection::service_client().post(url).header("x-wonderland-interactive","true").json(&request).send().await {
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
                                "permission_request" => {let mut prompt=event.clone();prompt["task_id"]=serde_json::json!(task_id);let _=event_tx.send(UiEvent::Approval(prompt));}
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
            };
            tokio::select! {
                _=run=>{},
                _=cancel_rx=>{let _=cancel_events.send(UiEvent::Failed{task_id:cancel_id,message:"已取消当前任务".into()});}
            }
        });
    });
    cancel_tx
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
fn spawn_session_request(tx: mpsc::Sender<UiEvent>, server: String, id: String) {
    thread::spawn(move || {
        if let Ok(runtime) = Runtime::new() {
            let result = runtime.block_on(async {
                wonderland::connection::service_client()
                    .get(format!(
                        "{}/v1/sessions/{}",
                        server.trim_end_matches('/'),
                        url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>()
                    ))
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<wonderland::session::Session>()
                    .await
            });
            let _ = tx.send(match result {
                Ok(session) => UiEvent::SessionLoaded(session),
                Err(e) => UiEvent::ApiFailed(e.to_string()),
            });
        }
    });
}

fn spawn_mcp_post(
    event_tx: mpsc::Sender<UiEvent>,
    server_url: String,
    path: String,
    cwd: String,
    login: bool,
) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|e| e.to_string())
            .and_then(|runtime| {
                runtime.block_on(async {
                    let response = wonderland::connection::service_client()
                        .post(format!("{}{path}", server_url.trim_end_matches('/')))
                        .json(&serde_json::json!({"cwd":cwd}))
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    let value: serde_json::Value =
                        response.json().await.map_err(|e| e.to_string())?;
                    if let Some(error) = value["error"].as_str() {
                        return Err(error.to_string());
                    }
                    Ok(value)
                })
            });
        let event = match result {
            Ok(value) if login => UiEvent::McpLogin(value),
            Ok(value) => {
                UiEvent::McpLoaded(value["servers"].as_array().cloned().unwrap_or_default())
            }
            Err(error) => UiEvent::ApiFailed(error),
        };
        let _ = event_tx.send(event);
    });
}

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
                    wonderland::connection::service_client()
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
                    wonderland::connection::service_client()
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
                    wonderland::connection::service_client()
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
                    wonderland::connection::service_client()
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
                    wonderland::connection::service_client()
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
                    wonderland::connection::service_client()
                        .get(format!("{}/v1/models", server_url.trim_end_matches('/')))
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
                    wonderland::connection::service_client()
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
            "../../assets/fonts/NotoSansSC-Regular.ttf"
        ))),
    );
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .expect("egui must provide the proportional font family")
        .push("noto-sans-sc".to_owned());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .expect("egui must provide the monospace font family")
        .push("noto-sans-sc".to_owned());
    fonts
}

fn friendly_api_error(message: &str) -> String {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("/v1/providers/cliproxyapi")
        && normalized.contains("503 service unavailable")
    {
        return "CLIProxyAPI 尚未就绪。请关闭后重新打开 Wonderland 桌面版；若仍失败，请查看 %LOCALAPPDATA%\\WonderlandData\\wonderland.err.log。".to_string();
    }
    if normalized.contains("/v1/providers/cliproxyapi")
        && normalized.contains("500 internal server error")
    {
        return "CLIProxyAPI 启动或本机连接失败。请查看 %LOCALAPPDATA%\\WonderlandData\\wonderland.err.log。".to_string();
    }
    if normalized.contains("connection refused") || normalized.contains("error sending request") {
        return "无法连接 Wonderland 后端。请关闭后重新打开桌面版。".to_string();
    }
    message.to_string()
}

fn main() -> eframe::Result {
    let initial_size = Vec2::new(1280.0, 820.0);
    #[cfg(feature = "ui-snapshots")]
    let initial_size = if std::env::var_os("WONDERLAND_SNAPSHOT_COMPACT").is_some() {
        Vec2::new(940.0, 620.0)
    } else {
        initial_size
    };
    let icon = egui::IconData {
        rgba: include_bytes!("../../assets/icons/icon.rgba").to_vec(),
        width: 256,
        height: 256,
    };
    let options = eframe::NativeOptions {
        persist_window: !(cfg!(feature = "ui-snapshots")
            && std::env::var_os("WONDERLAND_SNAPSHOT").is_some()),
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(initial_size)
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

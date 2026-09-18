use super::*;

impl DesktopApp {
    /// Development-only rendering fixture. Capture egui's own framebuffer, never other windows.
    #[cfg(feature = "ui-snapshots")]
    pub(super) fn render_snapshot(&mut self, ctx: &egui::Context) {
        let Some(path) = std::env::var_os("WONDERLAND_SNAPSHOT") else {
            return;
        };
        self.snapshot_frames += 1;
        if self.snapshot_frames == 1 {
            if let Ok(mode) = std::env::var("WONDERLAND_SNAPSHOT_PANE") {
                self.workbench.prepare_snapshot(&mode);
            }
        }
        if self.snapshot_frames == 1 && std::env::var_os("WONDERLAND_SNAPSHOT_COMPACT").is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(940.0, 620.0)));
        }
        let capture_frame = if std::env::var_os("WONDERLAND_SNAPSHOT_PANE").is_some() {
            70
        } else {
            8
        };
        if self.snapshot_frames == capture_frame {
            if std::env::var_os("WONDERLAND_SNAPSHOT_CONVERSATION").is_some() {
                self.tasks[0].title = "修复求和函数".into();
                self.tasks[0].messages=vec![ChatMessage{role:MessageRole::User,text:"检查 sum.js 中的求和逻辑，修复后验证结果。".into()},ChatMessage{role:MessageRole::Agent,text:"已修复 sum(a, b) 的运算符，并验证正数与负数输入。\n\n```javascript\nfunction sum(a, b) {\n  return a + b;\n}\n```\n\n- sum(2, 3) → 5\n- sum(-2, 5) → 3".into()}];
                self.tasks[0].tool_log = vec![
                    "读取 sum.js".into(),
                    "修改 1 处代码".into(),
                    "重新读取，验证通过".into(),
                ];
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        for event in ctx.input(|i| i.events.clone()) {
            if let egui::Event::Screenshot { image, .. } = event {
                let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                image::save_buffer(
                    &path,
                    &bytes,
                    image.width() as u32,
                    image.height() as u32,
                    image::ColorType::Rgba8,
                )
                .expect("save UI snapshot");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
    pub(super) fn render_header(&mut self, ui: &mut Ui) {
        use icons::{brand, button, Icon};
        ui.horizontal(|ui| {
            brand(ui, 32.0);
            ui.add_space(2.0);
            ui.label(RichText::new("Wonderland").size(20.0).strong());
            ui.add_space(12.0);
            ui.label(
                RichText::new("/  你的 AI 编码工作区")
                    .size(12.0)
                    .color(MUTED),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, Icon::Settings, "设置", 88.0, self.show_conn_panel).clicked() {
                    self.show_conn_panel = !self.show_conn_panel;
                    if self.show_conn_panel {
                        self.refresh_connection_panel();
                    }
                }
                ui.add_space(8.0);
                if button(
                    ui,
                    Icon::Grid,
                    "多任务",
                    100.0,
                    self.view == ViewMode::Parallel,
                )
                .clicked()
                {
                    self.view = ViewMode::Parallel;
                    self.workbench.pane = workbench::Pane::Chat;
                }
                if button(
                    ui,
                    Icon::Chat,
                    "对话",
                    88.0,
                    self.view == ViewMode::Planning,
                )
                .clicked()
                {
                    self.view = ViewMode::Planning;
                    self.workbench.pane = workbench::Pane::Chat;
                }
            });
        });
    }

    pub(super) fn render_welcome(&mut self, ui: &mut Ui) {
        use icons::Icon;
        ui.add_space(if ui.available_height() > 390.0 {
            46.0
        } else {
            16.0
        });
        ui.vertical_centered(|ui| {
            let (tile, _) = ui.allocate_exact_size(Vec2::splat(64.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(tile, 18, Color32::from_rgb(27, 46, 41));
            ui.painter().rect_stroke(
                tile,
                18,
                Stroke::new(1.0_f32, Color32::from_rgb(52, 79, 69)),
                egui::StrokeKind::Inside,
            );
            icons::draw(ui, tile.shrink(16.0), Icon::Spark, ACCENT);
            ui.add_space(18.0);
            ui.label(
                RichText::new("让想法，落到代码里。")
                    .size(32.0)
                    .strong()
                    .color(TEXT),
            );
            ui.add_space(10.0);
            ui.label(
                RichText::new("从理解项目到完成构建，和你熟悉的模型一起工作。")
                    .size(14.0)
                    .color(MUTED),
            );
            ui.add_space(22.0);
            ui.horizontal_wrapped(|ui| {
                let models = [
                    ("OpenAI", Color32::from_rgb(170, 218, 202)),
                    ("Claude", Color32::from_rgb(229, 177, 148)),
                    ("Kimi", Color32::from_rgb(164, 187, 250)),
                    ("DeepSeek", Color32::from_rgb(157, 186, 236)),
                ];
                let total = 360.0;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                for (name, color) in models {
                    Frame::new()
                        .fill(PANEL)
                        .corner_radius(16)
                        .inner_margin(egui::Margin::symmetric(10, 5))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("•").color(color));
                                ui.label(RichText::new(name).size(11.0).color(MUTED));
                            });
                        });
                }
            });
        });
        ui.add_space(32.0);
        let actions = [
            (
                Icon::Folder,
                "理解项目",
                "梳理架构与关键入口",
                "请阅读当前项目，梳理目录结构、主要模块及运行方式。",
            ),
            (
                Icon::Code,
                "实现功能",
                "把需求变成可运行的代码",
                "请先分析项目现状，再帮我实现以下功能：",
            ),
            (
                Icon::Search,
                "检查代码",
                "定位问题，给出可靠修复",
                "请检查当前项目中的潜在缺陷，给出定位依据并修复。",
            ),
        ];
        let mut selected = None;
        let mut card = |ui: &mut Ui, index: usize| {
            let (icon, title, desc, prompt) = actions[index];
            let width = ui.available_width();
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(width, 114.0), egui::Sense::click());
            let painter = ui.painter();
            painter.rect_filled(rect, 12, if response.hovered() { CARD } else { PANEL });
            painter.rect_stroke(
                rect,
                12,
                Stroke::new(
                    1.0_f32,
                    if response.hovered() {
                        ACCENT.gamma_multiply(0.5)
                    } else {
                        BORDER
                    },
                ),
                egui::StrokeKind::Inside,
            );
            icons::draw(
                ui,
                egui::Rect::from_min_size(rect.min + Vec2::new(16., 16.), Vec2::splat(21.)),
                icon,
                ACCENT,
            );
            painter.text(
                rect.min + Vec2::new(16., 57.),
                egui::Align2::LEFT_CENTER,
                title,
                egui::FontId::proportional(14.),
                TEXT,
            );
            painter.text(
                rect.min + Vec2::new(16., 84.),
                egui::Align2::LEFT_CENTER,
                desc,
                egui::FontId::proportional(11.),
                MUTED,
            );
            icons::draw(
                ui,
                egui::Rect::from_min_size(
                    rect.right_top() + Vec2::new(-32., 18.),
                    Vec2::splat(15.),
                ),
                Icon::Arrow,
                MUTED,
            );
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title));
            if response.clicked() {
                selected = Some(prompt.to_owned());
            }
        };
        if ui.available_width() >= 550.0 {
            ui.columns(3, |cols| {
                for (i, col) in cols.iter_mut().enumerate() {
                    card(col, i);
                }
            });
        } else {
            for i in 0..3 {
                card(ui, i);
                ui.add_space(6.0);
            }
        }
        if let Some(prompt) = selected {
            self.input = prompt;
        }
    }

    pub(super) fn render_planning(&mut self, ui: &mut Ui) {
        let Some(task) = self.tasks.get(self.active_task) else {
            return;
        };
        ui.horizontal(|ui| {
            ui.heading(RichText::new(&task.title).size(18.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let usage = task.usage;
                if usage.input_tokens + usage.output_tokens > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{} 输入 · {} 输出 · {} 缓存",
                            usage.input_tokens, usage.output_tokens, usage.cache_read_tokens
                        ))
                        .small()
                        .color(MUTED),
                    );
                }
            });
        });
        ui.add_space(8.0);
        let reserve = if ui.available_width() < 640.0 {
            245.0
        } else {
            215.0
        };
        let height = (ui.available_height() - reserve).max(80.0);
        ScrollArea::vertical()
            .id_salt(("conversation", &task.id))
            .auto_shrink([false, false])
            .max_height(height)
            .min_scrolled_height(height)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let width = ui.available_width().min(940.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| self.render_messages(ui, self.active_task),
                );
            });
        self.render_input(ui);
    }

    pub(super) fn render_messages(&mut self, ui: &mut Ui, task_index: usize) {
        let Some(task) = self.tasks.get(task_index) else {
            return;
        };
        if task.messages.is_empty() && task.stream_text.is_empty() {
            self.render_welcome(ui);
            return;
        }
        for (index, message) in task.messages.iter().enumerate() {
            ui.push_id((task_index, index), |ui| {
                let is_user = message.role == MessageRole::User;
                Frame::new()
                    .fill(if is_user { CARD } else { PANEL })
                    .corner_radius(12.0)
                    .inner_margin(18.0)
                    .show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 36.0).max(100.0));
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(if is_user { "你" } else { "Wonderland" })
                                    .strong()
                                    .color(if is_user { MUTED } else { ACCENT }),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("复制").clicked() {
                                    ui.ctx().copy_text(message.text.clone());
                                }
                            });
                        });
                        ui.add_space(8.0);
                        markdown(ui, &message.text);
                    });
                ui.add_space(12.0);
            });
        }
        if !task.reasoning.is_empty() {
            ui.collapsing(
                if task.running {
                    "思考中"
                } else {
                    "推理摘要"
                },
                |ui| {
                    ui.label(RichText::new(&task.reasoning).small().color(MUTED));
                },
            );
        }
        if !task.stream_text.is_empty() {
            Frame::new()
                .fill(PANEL)
                .corner_radius(12.0)
                .inner_margin(18.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Wonderland").strong().color(ACCENT));
                        if task.running {
                            ui.spinner();
                        }
                    });
                    ui.add_space(8.0);
                    markdown(ui, &task.stream_text);
                });
        }
        if !task.tool_log.is_empty() {
            ui.add_space(10.0);
            egui::CollapsingHeader::new(format!("执行记录 · {}", task.tool_log.len()))
                .default_open(task.running)
                .show(ui, |ui| {
                    for line in &task.tool_log {
                        ui.label(RichText::new(line).small().color(MUTED));
                    }
                });
        }
        if !task.plan.is_empty() {
            ui.collapsing("任务计划", |ui| {
                for (i, step) in task.plan.iter().enumerate() {
                    ui.label(format!("{}. {step}", i + 1));
                }
            });
        }
    }

    pub(super) fn render_input(&mut self, ui: &mut Ui) {
        let running = self.tasks.get(self.active_task).is_some_and(|t| t.running);
        ui.add_space(12.0);
        Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(14.0)
            .inner_margin(18.0)
            .show(ui, |ui| {
                let edit = ui.add(
                    TextEdit::multiline(&mut self.input)
                        .hint_text("你想构建什么？描述任务，或粘贴代码…")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .frame(false),
                );
                let shortcut = edit.has_focus()
                    && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    egui::ComboBox::from_id_salt("composer-model")
                        .width(175.0)
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
                            if self.models.is_empty() {
                                ui.label("在设置中添加 API 或登录订阅");
                            }
                        });
                    if self.reasoning_effort != "auto"
                        && !wonderland::model_profile::supported_reasoning_efforts(
                            &self.selected_model,
                        )
                        .contains(&self.reasoning_effort.as_str())
                    {
                        self.reasoning_effort = "auto".into();
                    }
                    egui::ComboBox::from_id_salt("composer-reasoning")
                        .width(85.0)
                        .selected_text(effort_label(&self.reasoning_effort))
                        .show_ui(ui, |ui| {
                            for effort in std::iter::once("auto").chain(
                                wonderland::model_profile::supported_reasoning_efforts(
                                    &self.selected_model,
                                )
                                .iter()
                                .copied(),
                            ) {
                                ui.selectable_value(
                                    &mut self.reasoning_effort,
                                    effort.into(),
                                    effort_label(effort),
                                );
                            }
                        });
                    use wonderland::permissions::PermissionMode;
                    egui::ComboBox::from_id_salt("permission-mode")
                        .width(90.0)
                        .selected_text(match self.permission_mode {
                            PermissionMode::Plan => "只读计划",
                            PermissionMode::AcceptEdits => "允许编辑",
                            _ => "逐项确认",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.permission_mode,
                                PermissionMode::Default,
                                "逐项确认",
                            );
                            ui.selectable_value(
                                &mut self.permission_mode,
                                PermissionMode::Plan,
                                "只读计划",
                            );
                            ui.selectable_value(
                                &mut self.permission_mode,
                                PermissionMode::AcceptEdits,
                                "允许编辑",
                            );
                        });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !running && !self.input.trim().is_empty(),
                                egui::Button::new(RichText::new("发送  ↑").strong().color(BG))
                                    .min_size(Vec2::new(70.0, 34.0))
                                    .fill(ACCENT),
                            )
                            .clicked()
                            || (!running && shortcut)
                        {
                            self.send_active();
                        }
                        if running && ui.button("停止").clicked() {
                            if let Some(task) = self.tasks.get(self.active_task) {
                                if let Some(cancel) = self.cancellations.remove(&task.id) {
                                    let _ = cancel.send(());
                                }
                            }
                        }
                    });
                });
            });
        ui.horizontal(|ui| {
            icons::glyph(ui, icons::Icon::Shield, 13.0, MUTED);
            ui.label(
                RichText::new("Ctrl + Enter 发送 · Enter 换行")
                    .small()
                    .color(MUTED),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(truncate(&self.status, 70))
                        .small()
                        .color(MUTED),
                );
            });
        });
    }

    pub(super) fn render_provider_settings(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("模型与账号").strong().size(17.0));
        let before = self.connection.provider.clone();
        egui::ComboBox::from_id_salt("connection-provider")
            .width(ui.available_width() - 10.0)
            .selected_text(provider_label(&before))
            .show_ui(ui, |ui| {
                for p in [
                    "subscription",
                    "openai",
                    "anthropic",
                    "deepseek",
                    "kimi",
                    "qwen",
                    "glm",
                    "gemini",
                    "openrouter",
                    "ollama",
                    "offline",
                ] {
                    ui.selectable_value(&mut self.connection.provider, p.into(), provider_label(p));
                }
            });
        if before != self.connection.provider {
            self.connection.base_url.clear();
            self.connection.model.clear();
            self.connection.api_key.clear();
            self.has_api_key = false;
        }
        if self.connection.provider == "subscription" || self.connection.provider == "cliproxyapi" {
            ui.label(
                RichText::new("使用订阅账号登录。账号轮换、刷新与额度由内置 CLIProxyAPI 处理。")
                    .small()
                    .color(MUTED),
            );
        } else if self.connection.provider != "offline" {
            ui.label("API 地址");
            ui.add(
                TextEdit::singleline(&mut self.connection.base_url)
                    .hint_text("留空使用厂商官方地址")
                    .desired_width(f32::INFINITY),
            );
            ui.label("API 密钥");
            ui.add(
                TextEdit::singleline(&mut self.connection.api_key)
                    .password(true)
                    .hint_text(if self.has_api_key {
                        "已保存 · 留空保留"
                    } else {
                        "输入 API key"
                    })
                    .desired_width(f32::INFINITY),
            );
            ui.label("默认模型");
            ui.add(
                TextEdit::singleline(&mut self.connection.model)
                    .hint_text("例如 deepseek-chat / gpt-5")
                    .desired_width(f32::INFINITY),
            );
            ui.collapsing("高级协议设置", |ui| {
                use wonderland::model_profile::WireProtocol;
                egui::ComboBox::from_id_salt("connection-wire")
                    .selected_text(self.connection.wire.map(|w| w.as_str()).unwrap_or("自动"))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.connection.wire, None, "自动");
                        for wire in [
                            WireProtocol::ChatCompletions,
                            WireProtocol::Responses,
                            WireProtocol::AnthropicMessages,
                        ] {
                            ui.selectable_value(
                                &mut self.connection.wire,
                                Some(wire),
                                wire.as_str(),
                            );
                        }
                    });
            });
        }
        if ui
            .add_enabled(
                !self.api_request_running,
                egui::Button::new(RichText::new("保存并应用连接").color(BG).strong())
                    .fill(ACCENT)
                    .min_size(Vec2::new(170.0, 36.0)),
            )
            .clicked()
        {
            self.api_request_running = true;
            self.status = "正在应用连接…".into();
            save_connection(
                self.event_tx.clone(),
                self.server_url.clone(),
                self.connection.clone(),
            );
        }
        ui.add_space(10.0);
        ui.collapsing("订阅账号登录", |ui| {
            egui::ComboBox::from_id_salt("account-provider")
                .selected_text(provider_label(&self.login_provider))
                .show_ui(ui, |ui| {
                    for p in [
                        "codex",
                        "claude",
                        "kimi",
                        "antigravity",
                        "xai",
                        "devin",
                        "meta",
                    ] {
                        ui.selectable_value(&mut self.login_provider, p.into(), provider_label(p));
                    }
                });
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(!self.api_request_running, egui::Button::new("登录账号"))
                    .clicked()
                {
                    self.start_login();
                }
                if ui
                    .add_enabled(
                        self.login_state.is_some() && !self.api_request_running,
                        egui::Button::new("检查登录"),
                    )
                    .clicked()
                {
                    self.check_login();
                }
            });
            if let Some(url) = &self.login_url {
                ui.hyperlink_to("在浏览器中继续授权", url);
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("刷新模型").clicked() {
                    self.load_models();
                }
                if ui.button("验证连接").clicked() {
                    self.verify_model();
                }
            });
        });
        ui.collapsing("工作区与服务", |ui| {
            ui.label("工作目录");
            ui.label(RichText::new(&self.working_dir).small());
            if ui.button("选择项目目录…").clicked() {
                self.workbench.choose_root();
            }
            ui.label("服务地址");
            ui.add(TextEdit::singleline(&mut self.server_url).desired_width(f32::INFINITY));
            ui.label("用户标识");
            ui.text_edit_singleline(&mut self.user_id);
            ui.label("本次模型");
            ui.text_edit_singleline(&mut self.selected_model);
        });
    }

    pub(super) fn render_approval(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.approvals.first().cloned() else {
            return;
        };
        let mut answer = None;
        egui::Window::new("工具执行确认")
            .collapsible(false)
            .resizable(true)
            .default_width(540.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.heading(prompt["tool"].as_str().unwrap_or("工具"));
                ui.label("请检查这次操作的完整参数。");
                ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                serde_json::to_string_pretty(&prompt["input"]).unwrap_or_default(),
                            )
                            .monospace(),
                        )
                        .selectable(true),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button("拒绝").clicked() {
                        answer = Some(false);
                    }
                    if ui.button("允许这一次").clicked() {
                        answer = Some(true);
                    }
                });
            });
        if let Some(allow) = answer {
            self.approvals.remove(0);
            let url = format!(
                "{}/v1/permissions/{}",
                self.server_url.trim_end_matches('/'),
                prompt["id"].as_str().unwrap_or_default()
            );
            let tx = self.event_tx.clone();
            thread::spawn(move || {
                if let Ok(runtime) = Runtime::new() {
                    let result = runtime.block_on(async {
                        wonderland::connection::service_client()
                            .post(url)
                            .json(&serde_json::json!({"allow":allow}))
                            .send()
                            .await?
                            .error_for_status()
                    });
                    if let Err(e) = result {
                        let _ = tx.send(UiEvent::ApiFailed(e.to_string()));
                    }
                }
            });
        }
    }
}

fn effort_label(effort: &str) -> &str {
    match effort {
        "on" => "推理开启",
        "minimal" => "最少推理",
        "xhigh" => "更高推理",
        "max" => "最大推理",
        "ultra" => "Ultra",
        "off" => "推理关闭",
        "low" => "低推理",
        "medium" => "中推理",
        "high" => "高推理",
        _ => "自动推理",
    }
}
fn provider_label(provider: &str) -> &str {
    match provider {
        "subscription" | "cliproxyapi" => "订阅账号 · CLIProxyAPI",
        "openai" | "codex" => "OpenAI / Codex",
        "anthropic" | "claude" => "Anthropic / Claude",
        "deepseek" => "DeepSeek",
        "kimi" => "Kimi / Moonshot",
        "offline" => "离线演示",
        other => other,
    }
}

fn save_connection(
    tx: mpsc::Sender<UiEvent>,
    server: String,
    settings: wonderland::connection::ConnectionSettings,
) {
    thread::spawn(move || {
        let result = Runtime::new()
            .map_err(|e| e.to_string())
            .and_then(|runtime| {
                runtime.block_on(async {
                    let response = wonderland::connection::service_client()
                        .put(format!("{}/v1/connection", server.trim_end_matches('/')))
                        .json(&settings)
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    let value: serde_json::Value =
                        response.json().await.map_err(|e| e.to_string())?;
                    if let Some(e) = value["error"].as_str() {
                        return Err(e.to_string());
                    }
                    Ok(value)
                })
            });
        let _ = tx.send(match result {
            Ok(value) => UiEvent::ConnectionLoaded(value),
            Err(e) => UiEvent::ApiFailed(e),
        });
        spawn_health_request(tx, server);
    });
}

/// Readable Markdown with selectable text, fenced code and one-click code copying.
fn markdown(ui: &mut Ui, text: &str) {
    let mut in_code = false;
    let mut code = String::new();
    let mut language = String::new();
    for line in text.lines() {
        if let Some(fence) = line.strip_prefix("```") {
            if in_code {
                code_block(ui, &language, &code);
                code.clear();
            } else {
                language = fence.trim().into();
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
            continue;
        }
        if line.trim().is_empty() {
            ui.add_space(6.0);
            continue;
        }
        let (size, line) = if let Some(t) = line.strip_prefix("### ") {
            (17.0, t)
        } else if let Some(t) = line.strip_prefix("## ") {
            (20.0, t)
        } else if let Some(t) = line.strip_prefix("# ") {
            (24.0, t)
        } else {
            (15.0, line)
        };
        let line = line
            .strip_prefix("- ")
            .map(|t| format!("•  {t}"))
            .unwrap_or_else(|| line.into());
        ui.add(
            egui::Label::new(RichText::new(line).size(size))
                .wrap()
                .selectable(true),
        );
    }
    if !code.is_empty() {
        code_block(ui, &language, &code);
    }
}
fn code_block(ui: &mut Ui, language: &str, code: &str) {
    Frame::new()
        .fill(BG)
        .corner_radius(8.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if language.is_empty() {
                        "代码"
                    } else {
                        language
                    })
                    .small()
                    .color(MUTED),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("复制代码").clicked() {
                        ui.ctx().copy_text(code.into());
                    }
                });
            });
            ScrollArea::horizontal()
                .id_salt(ui.next_auto_id())
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(code.trim_end()).monospace().size(13.0))
                            .selectable(true),
                    );
                });
        });
}

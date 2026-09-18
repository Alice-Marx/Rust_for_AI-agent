//! Native PTY sessions for the desktop. Commands run as the current user; this
//! is a terminal, not the agent's restricted code-execution sandbox.
//!
//! The reader and writer never block the UI. Output queues and scrollback are
//! bounded, and closing a tab terminates its process group / Windows Job.

use anyhow::{anyhow, bail, Context, Result};
use eframe::egui::{self, Color32, FontId, Key, Modifiers, Pos2, Rect, Stroke, Vec2};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    collections::BTreeMap,
    hash::Hash,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::Duration,
};

const SCROLLBACK_LINES: usize = 5_000;
const OUTPUT_CHUNKS: usize = 64;
const MAX_INPUT_BYTES: usize = 1_048_576;
const BACKGROUND: Color32 = Color32::from_rgb(13, 19, 25);
const FOREGROUND: Color32 = Color32::from_rgb(214, 225, 233);
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct TerminalCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    Running,
    Exited { code: u32 },
    Stopped,
    Failed(String),
}

enum Output {
    Data(Vec<u8>),
    Error(String),
    End,
}

#[derive(Default)]
pub struct TerminalViewResponse {
    pub has_focus: bool,
    pub error: Option<String>,
}

pub struct TerminalSession {
    id: u64,
    command: TerminalCommand,
    process_id: Option<u32>,
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    containment: Option<crate::process_tree::TerminalProcessTree>,
    output: Option<Receiver<Output>>,
    input: Option<SyncSender<Vec<u8>>>,
    buffer: TerminalBuffer,
    status: TerminalStatus,
    selection: Option<((u16, u16), (u16, u16))>,
    preedit: String,
    last_mouse_cell: Option<(u16, u16)>,
    scroll_remainder: f32,
    completed_output: Option<String>,
}

impl TerminalSession {
    pub fn spawn(command: TerminalCommand, rows: u16, cols: u16) -> Result<Self> {
        anyhow::ensure!(
            command.cwd.is_dir(),
            "终端工作目录不存在：{}",
            command.cwd.display()
        );
        let size = terminal_size(rows, cols);
        let pair = native_pty_system()
            .openpty(size)
            .context("创建原生 PTY 失败")?;
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let mut cmd = CommandBuilder::new(&command.program);
        cmd.args(&command.args);
        cmd.cwd(&command.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "Wonderland");
        cmd.env_remove("NO_COLOR");
        for (name, value) in &command.env {
            cmd.env(name, value);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("无法启动 {}", command.program.display()))?;
        let process_id = child.process_id();
        let containment = if let Some(pid) = process_id {
            match crate::process_tree::TerminalProcessTree::attach(pid) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    if child.try_wait()?.is_none() {
                        let _ = child.kill();
                        return Err(error.context("无法建立终端进程生命周期管理"));
                    }
                    None
                }
            }
        } else {
            None
        };
        drop(pair.slave);
        let (output_tx, output) = mpsc::sync_channel(OUTPUT_CHUNKS);
        let (input, input_rx) = mpsc::sync_channel::<Vec<u8>>(16);
        let error_tx = output_tx.clone();
        thread::Builder::new()
            .name("wonderland-pty-input".into())
            .spawn(move || {
                while let Ok(bytes) = input_rx.recv() {
                    if let Err(error) = writer.write_all(&bytes).and_then(|_| writer.flush()) {
                        let _ = error_tx.send(Output::Error(format!("终端输入失败：{error}")));
                        break;
                    }
                }
            })?;
        thread::Builder::new()
            .name("wonderland-pty-output".into())
            .spawn(move || {
                let mut bytes = [0_u8; 8192];
                let mut forwarding = true;
                loop {
                    match reader.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(n) => {
                            // Continue draining after the UI disconnects: Windows
                            // ClosePseudoConsole can wait for its output pipe.
                            if forwarding
                                && output_tx.send(Output::Data(bytes[..n].to_vec())).is_err()
                            {
                                forwarding = false;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        // Unix PTYs commonly report EIO when the slave closes.
                        #[cfg(unix)]
                        Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                        Err(error) => {
                            if forwarding {
                                let _ =
                                    output_tx.send(Output::Error(format!("终端读取失败：{error}")));
                            }
                            break;
                        }
                    }
                }
                let _ = output_tx.send(Output::End);
            })?;
        Ok(Self {
            id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            command,
            process_id,
            child: Some(child),
            master: Some(pair.master),
            containment,
            output: Some(output),
            input: Some(input),
            buffer: TerminalBuffer::new(size.rows, size.cols),
            status: TerminalStatus::Running,
            selection: None,
            preedit: String::new(),
            last_mouse_cell: None,
            scroll_remainder: 0.0,
            completed_output: None,
        })
    }

    pub fn title(&self) -> &str {
        &self.command.title
    }
    /// Stable for the lifetime of a tab, including when neighboring tabs close.
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn cwd(&self) -> &Path {
        &self.command.cwd
    }
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }
    pub fn status(&self) -> &TerminalStatus {
        &self.status
    }
    /// An I/O failure can leave the process alive even when its display status
    /// is Failed. Exit confirmation must reflect ownership of that process.
    pub fn has_live_process(&self) -> bool {
        self.child.is_some()
    }
    /// Also true while an exited process still has buffered output to drain.
    pub fn needs_poll(&self) -> bool {
        self.child.is_some() || self.output.is_some()
    }
    pub fn screen(&self) -> &vt100::Screen {
        self.buffer.parser.screen()
    }
    pub fn screen_text(&self) -> String {
        self.screen().contents()
    }
    pub fn completed_output(&self) -> Option<&str> {
        self.completed_output.as_deref()
    }

    /// Poll even hidden tabs to keep output moving; work per poll is bounded.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..32 {
            let output = self.output.as_ref().map(|rx| rx.try_recv());
            match output {
                Some(Ok(Output::Data(bytes))) => {
                    for reply in self.buffer.process(&bytes) {
                        let _ = self.queue_input(reply);
                    }
                    changed = true;
                }
                Some(Ok(Output::Error(message))) => {
                    if matches!(self.status, TerminalStatus::Running) {
                        self.status = TerminalStatus::Failed(message);
                    }
                    changed = true;
                }
                Some(Ok(Output::End)) | Some(Err(TryRecvError::Disconnected)) => {
                    self.output = None;
                    break;
                }
                Some(Err(TryRecvError::Empty)) | None => break,
            }
        }
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    self.status = TerminalStatus::Exited {
                        code: exit.exit_code(),
                    };
                    self.child = None;
                    // Descendants must not outlive the shell that owns the tab.
                    self.containment = None;
                    self.input = None;
                    if let Some(master) = self.master.take() {
                        // ConPTY closes its output pipe only when its master is
                        // closed. Keep polling to render all queued final bytes.
                        let _ = thread::Builder::new()
                            .name("wonderland-pty-exit".into())
                            .spawn(move || drop(master));
                    }
                    changed = true;
                }
                Ok(None) => {}
                Err(error) => {
                    self.status = TerminalStatus::Failed(format!("读取进程状态失败：{error}"));
                    changed = true;
                }
            }
        }
        if self.child.is_none() && self.output.is_none() && self.completed_output.is_none() {
            self.completed_output = Some(self.buffer.transcript());
        }
        changed
    }

    fn queue_input(&self, bytes: Vec<u8>) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(bytes.len() <= MAX_INPUT_BYTES, "单次粘贴不能超过 1 MiB");
        let input = self.input.as_ref().ok_or_else(|| anyhow!("终端已关闭"))?;
        match input.try_send(bytes) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => bail!("终端正忙，请稍后重试输入"),
            Err(TrySendError::Disconnected(_)) => bail!("终端输入通道已关闭"),
        }
    }

    pub fn send_input(&mut self, bytes: &[u8]) -> Result<()> {
        anyhow::ensure!(
            matches!(self.status, TerminalStatus::Running),
            "终端进程已经结束"
        );
        self.queue_input(bytes.to_vec())?;
        self.buffer.parser.screen_mut().set_scrollback(0);
        self.selection = None;
        Ok(())
    }

    pub fn paste(&mut self, text: &str) -> Result<()> {
        self.send_input(&paste_bytes(text, self.screen().bracketed_paste()))
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        // vt100 resizing truncates rows/columns. After a process exits there
        // is nobody left to repaint, so freeze its final grid until captured.
        if self.child.is_none() {
            return Ok(());
        }
        let size = terminal_size(rows, cols);
        if self.screen().size() != (size.rows, size.cols) {
            if let Some(master) = &self.master {
                master.resize(size)?;
            }
            self.buffer
                .parser
                .screen_mut()
                .set_size(size.rows, size.cols);
            self.selection = None;
        }
        Ok(())
    }

    /// Positive values move towards older output. Alternate screens belong to
    /// the application and intentionally have no local scrollback.
    pub fn scroll(&mut self, lines: isize) {
        if !self.screen().alternate_screen() {
            let next = self.screen().scrollback().saturating_add_signed(lines);
            self.buffer
                .parser
                .screen_mut()
                .set_scrollback(next.min(SCROLLBACK_LINES));
            self.selection = None;
        }
    }

    pub fn stop(&mut self) {
        if self.child.is_none() && self.completed_output.is_none() {
            self.poll();
            self.completed_output = Some(self.buffer.transcript());
        }
        self.input = None;
        self.output = None;
        self.containment = None;
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        let child = self.child.take();
        let master = self.master.take();
        if child.is_some() || master.is_some() {
            // Closing a ConPTY is allowed to block; never do it on egui's thread.
            let _ = thread::Builder::new()
                .name("wonderland-pty-close".into())
                .spawn(move || {
                    if let Some(mut child) = child {
                        let _ = child.wait();
                    }
                    drop(master);
                });
        }
        if matches!(
            self.status,
            TerminalStatus::Running | TerminalStatus::Failed(_)
        ) {
            self.status = TerminalStatus::Stopped;
        }
    }

    /// Render an interactive ANSI screen. Shift+drag selects when a TUI captures the
    /// mouse; Ctrl+Shift+C copies; Ctrl+C interrupts when nothing is selected.
    pub fn render(&mut self, ui: &mut egui::Ui, id: impl Hash) -> TerminalViewResponse {
        self.poll();
        let widget_id = ui.make_persistent_id((id, self.id));
        if let Some(output) = self.completed_output.as_deref() {
            let mut text = output;
            let mut focused = false;
            egui::ScrollArea::both()
                .id_salt(widget_id.with("completed-output"))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let response = ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .id(widget_id)
                            .font(FontId::monospace(13.0))
                            .text_color(FOREGROUND)
                            .frame(false)
                            .desired_width(f32::INFINITY),
                    );
                    focused = response.has_focus();
                    response.context_menu(|ui| {
                        if ui.button("复制全部终端输出").clicked() {
                            ui.ctx().copy_text(output.to_owned());
                            ui.close_menu();
                        }
                    });
                });
            return TerminalViewResponse {
                has_focus: focused,
                error: None,
            };
        }
        let font = FontId::monospace(13.0);
        let cell_width = ui.fonts(|f| f.glyph_width(&font, 'M')).max(6.0);
        let cell_height = ui.fonts(|f| f.row_height(&font)).ceil().max(17.0);
        let available = ui.available_size().max(Vec2::new(80.0, 50.0));
        let (rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
        let response = ui.interact(rect, widget_id, egui::Sense::click_and_drag());
        let content = rect.shrink(10.0);
        let rows = (content.height() / cell_height).floor().max(1.0) as u16;
        let cols = (content.width() / cell_width).floor().max(2.0) as u16;
        let mut result = TerminalViewResponse {
            has_focus: response.has_focus() && ui.is_enabled(),
            error: None,
        };
        if let Err(error) = self.resize(rows, cols) {
            result.error = Some(error.to_string());
        }
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        result.has_focus = response.has_focus() && ui.is_enabled();
        if !result.has_focus {
            // IME Disabled/Commit can be delivered to the next focused widget.
            // Do not retain a stale candidate that blocks later terminal keys.
            self.preedit.clear();
        }
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
        let to_cell = |pos: Pos2| -> (u16, u16) {
            (
                ((pos.y - content.top()) / cell_height)
                    .floor()
                    .clamp(0.0, rows.saturating_sub(1) as f32) as u16,
                ((pos.x - content.left()) / cell_width)
                    .floor()
                    .clamp(0.0, cols.saturating_sub(1) as f32) as u16,
            )
        };
        let shift = ui.input(|i| i.modifiers.shift);
        let capture_mouse = ui.is_enabled()
            && self.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
            && !shift;
        if !capture_mouse {
            if response.drag_started() {
                if let Some(pos) = ui.input(|i| i.pointer.press_origin()) {
                    let cell = to_cell(pos);
                    self.selection = Some((cell, cell));
                }
            }
            if response.dragged() {
                if let (Some((start, _)), Some(pos)) =
                    (self.selection, response.interact_pointer_pos())
                {
                    self.selection = Some((start, to_cell(pos)));
                }
            }
            if response.clicked() {
                self.selection = None;
            }
        }
        if response.hovered() && ui.is_enabled() {
            let delta = ui.input(|i| i.smooth_scroll_delta.y);
            if delta.abs() > 0.5 {
                self.scroll_remainder += delta / cell_height;
                let lines = self.scroll_remainder.trunc();
                self.scroll_remainder -= lines;
                if capture_mouse {
                    let cell = ui
                        .input(|i| i.pointer.hover_pos())
                        .map(to_cell)
                        .unwrap_or((0, 0));
                    for _ in 0..lines.abs().min(12.0) as usize {
                        let bytes = mouse_bytes(
                            self.screen().mouse_protocol_encoding(),
                            if delta > 0.0 { 64 } else { 65 },
                            cell,
                            false,
                        );
                        let _ = self.queue_input(bytes);
                    }
                } else if self.screen().alternate_screen() {
                    for _ in 0..lines.abs().min(12.0) as usize {
                        let key = if delta > 0.0 {
                            Key::ArrowUp
                        } else {
                            Key::ArrowDown
                        };
                        let _ = self.send_input(
                            &key_bytes(key, Modifiers::NONE, self.screen().application_cursor())
                                .unwrap(),
                        );
                    }
                } else {
                    self.scroll(lines as isize);
                }
            }
        }
        if result.has_focus {
            let events = ui.input(|i| i.events.clone());
            let has_copy = events.iter().any(|e| matches!(e, egui::Event::Copy));
            let has_cut = events.iter().any(|e| matches!(e, egui::Event::Cut));
            let composing = !self.preedit.is_empty()
                || events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Ime(egui::ImeEvent::Commit(_))));
            for event in events {
                let operation = match event {
                    egui::Event::Copy => {
                        if let Some(text) = self.selected_text() {
                            ui.ctx().copy_text(text);
                            Ok(())
                        } else if ui.input(|i| i.modifiers.shift || i.modifiers.mac_cmd) {
                            ui.ctx().copy_text(self.screen_text());
                            Ok(())
                        } else {
                            self.send_input(&[3])
                        }
                    }
                    egui::Event::Cut => self.send_input(&[24]),
                    egui::Event::Paste(text) => self.paste(&text),
                    egui::Event::Text(text) if self.preedit.is_empty() => {
                        let alt = ui.input(|i| i.modifiers.alt && !i.modifiers.ctrl);
                        let mut bytes = if alt { vec![0x1b] } else { Vec::new() };
                        bytes.extend(text.as_bytes());
                        self.send_input(&bytes)
                    }
                    egui::Event::Ime(egui::ImeEvent::Preedit(text)) => {
                        self.preedit = text;
                        Ok(())
                    }
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                        self.preedit.clear();
                        self.send_input(text.as_bytes())
                    }
                    egui::Event::Ime(egui::ImeEvent::Disabled) => {
                        self.preedit.clear();
                        Ok(())
                    }
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        // Enter used to accept a Chinese/Japanese IME candidate
                        // must not submit the terminal command prematurely.
                        if composing {
                            continue;
                        }
                        if (has_copy && key == Key::C) || (has_cut && key == Key::X) {
                            continue;
                        }
                        if modifiers.shift && modifiers.ctrl && key == Key::C {
                            ui.ctx().copy_text(
                                self.selected_text().unwrap_or_else(|| self.screen_text()),
                            );
                            Ok(())
                        } else if modifiers.shift && matches!(key, Key::PageUp | Key::PageDown) {
                            self.scroll(if key == Key::PageUp {
                                rows as isize
                            } else {
                                -(rows as isize)
                            });
                            Ok(())
                        } else if let Some(bytes) =
                            key_bytes(key, modifiers, self.screen().application_cursor())
                        {
                            self.send_input(&bytes)
                        } else {
                            Ok(())
                        }
                    }
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers,
                    } if capture_mouse && rect.contains(pos) => {
                        if !pressed
                            && self.screen().mouse_protocol_mode()
                                == vt100::MouseProtocolMode::Press
                        {
                            continue;
                        }
                        let button = match button {
                            egui::PointerButton::Primary => 0,
                            egui::PointerButton::Middle => 1,
                            egui::PointerButton::Secondary => 2,
                            _ => continue,
                        };
                        let code = button
                            + if modifiers.alt { 8 } else { 0 }
                            + if modifiers.ctrl { 16 } else { 0 };
                        self.queue_input(mouse_bytes(
                            self.screen().mouse_protocol_encoding(),
                            code,
                            to_cell(pos),
                            !pressed,
                        ))
                    }
                    egui::Event::PointerMoved(pos) if capture_mouse && rect.contains(pos) => {
                        let cell = to_cell(pos);
                        let dragging = ui.input(|i| i.pointer.primary_down());
                        let mode = self.screen().mouse_protocol_mode();
                        if self.last_mouse_cell != Some(cell)
                            && (mode == vt100::MouseProtocolMode::AnyMotion
                                || (dragging && mode == vt100::MouseProtocolMode::ButtonMotion))
                        {
                            self.last_mouse_cell = Some(cell);
                            self.queue_input(mouse_bytes(
                                self.screen().mouse_protocol_encoding(),
                                if dragging { 32 } else { 35 },
                                cell,
                                false,
                            ))
                        } else {
                            Ok(())
                        }
                    }
                    _ => Ok(()),
                };
                if let Err(error) = operation {
                    result.error = Some(error.to_string());
                }
            }
        }
        response.context_menu(|ui| {
            if ui.button("复制选中内容 / 当前屏幕").clicked() {
                ui.ctx()
                    .copy_text(self.selected_text().unwrap_or_else(|| self.screen_text()));
                ui.close_menu();
            }
            if ui.button("粘贴").clicked() {
                response.request_focus();
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                ui.close_menu();
            }
            if ui.button("中断进程  Ctrl+C").clicked() {
                if let Err(error) = self.send_input(&[3]) {
                    result.error = Some(error.to_string());
                }
                ui.close_menu();
            }
            if ui.button("返回最新输出").clicked() {
                self.buffer.parser.screen_mut().set_scrollback(0);
                ui.close_menu();
            }
        });
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 6.0, BACKGROUND);
        let screen = self.screen();
        let (actual_rows, actual_cols) = screen.size();
        let selection = self
            .selection
            .map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        for row in 0..actual_rows {
            for col in 0..actual_cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let point =
                    content.min + Vec2::new(col as f32 * cell_width, row as f32 * cell_height);
                let cell_rect = Rect::from_min_size(
                    point,
                    Vec2::new(
                        cell_width * if cell.is_wide() { 2.0 } else { 1.0 },
                        cell_height,
                    ),
                );
                let mut fg = ansi_color(cell.fgcolor(), FOREGROUND, cell.bold());
                let mut bg = ansi_color(cell.bgcolor(), BACKGROUND, false);
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if selection.is_some_and(|(start, end)| (row, col) >= start && (row, col) <= end) {
                    bg = Color32::from_rgb(43, 76, 87);
                }
                if bg != BACKGROUND {
                    painter.rect_filled(cell_rect, 0.0, bg);
                }
                if cell.has_contents() {
                    painter.text(
                        point,
                        egui::Align2::LEFT_TOP,
                        cell.contents(),
                        font.clone(),
                        fg,
                    );
                    if cell.underline() {
                        painter.line_segment(
                            [
                                cell_rect.left_bottom() - Vec2::new(0.0, 2.0),
                                cell_rect.right_bottom() - Vec2::new(0.0, 2.0),
                            ],
                            Stroke::new(1.0_f32, fg),
                        );
                    }
                }
            }
        }
        let (cursor_row, cursor_col) = screen.cursor_position();
        let cursor = Rect::from_min_size(
            content.min
                + Vec2::new(
                    cursor_col as f32 * cell_width,
                    cursor_row as f32 * cell_height,
                ),
            Vec2::new(cell_width, cell_height),
        );
        if !screen.hide_cursor()
            && screen.scrollback() == 0
            && matches!(self.status, TerminalStatus::Running)
        {
            painter.rect_stroke(
                cursor,
                0.0,
                Stroke::new(
                    1.0_f32,
                    if result.has_focus {
                        Color32::from_rgb(113, 224, 191)
                    } else {
                        Color32::from_gray(90)
                    },
                ),
                egui::StrokeKind::Inside,
            );
        }
        if result.has_focus {
            ui.output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect,
                    cursor_rect: cursor,
                })
            });
            if !self.preedit.is_empty() {
                painter.rect_filled(
                    Rect::from_min_size(
                        cursor.min,
                        Vec2::new(
                            self.preedit.chars().count() as f32 * cell_width * 2.0,
                            cell_height,
                        ),
                    ),
                    0.0,
                    BACKGROUND,
                );
                painter.text(
                    cursor.min,
                    egui::Align2::LEFT_TOP,
                    &self.preedit,
                    font,
                    Color32::from_rgb(113, 224, 191),
                );
            }
        }
        if screen.scrollback() > 0 {
            painter.text(
                content.right_top(),
                egui::Align2::RIGHT_TOP,
                format!("↑ {} 行 · 输入返回底部", screen.scrollback()),
                FontId::proportional(11.0),
                Color32::from_rgb(113, 224, 191),
            );
        }
        if self.needs_poll() {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
        result
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let (_, cols) = self.screen().size();
        let mut text = String::new();
        for row in start.0..=end.0 {
            let begin = if row == start.0 { start.1 } else { 0 };
            let finish = if row == end.0 {
                end.1.saturating_add(1).min(cols)
            } else {
                cols
            };
            let mut line = String::new();
            for col in begin..finish {
                if let Some(cell) = self.screen().cell(row, col) {
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    if cell.has_contents() {
                        line.push_str(&cell.contents());
                    } else {
                        line.push(' ');
                    }
                }
            }
            text.push_str(line.trim_end());
            if row < end.0 && !self.screen().row_wrapped(row) {
                text.push('\n');
            }
        }
        Some(text)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn terminal_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows: rows.clamp(1, 200),
        cols: cols.clamp(2, 500),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    // A pasted ESC must never terminate a bracketed paste or inject a control
    // sequence. Newlines remain paste content in bracketed mode.
    let clean = text
        .replace('\0', "")
        .replace('\u{1b}', "")
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if bracketed {
        format!("\u{1b}[200~{clean}\u{1b}[201~").into_bytes()
    } else {
        clean.replace('\n', "\r").into_bytes()
    }
}

fn key_bytes(key: Key, modifiers: Modifiers, application_cursor: bool) -> Option<Vec<u8>> {
    if modifiers.mac_cmd || ((modifiers.ctrl || modifiers.command) && key == Key::V) {
        return None;
    }
    let modifier = 1
        + usize::from(modifiers.shift)
        + 2 * usize::from(modifiers.alt)
        + 4 * usize::from(modifiers.ctrl);
    let mut bytes = match key {
        Key::ArrowUp | Key::ArrowDown | Key::ArrowRight | Key::ArrowLeft | Key::Home | Key::End => {
            let final_char = match key {
                Key::ArrowUp => 'A',
                Key::ArrowDown => 'B',
                Key::ArrowRight => 'C',
                Key::ArrowLeft => 'D',
                Key::Home => 'H',
                _ => 'F',
            };
            if modifier > 1 {
                format!("\u{1b}[1;{modifier}{final_char}").into_bytes()
            } else {
                format!(
                    "\u{1b}{}{final_char}",
                    if application_cursor { 'O' } else { '[' }
                )
                .into_bytes()
            }
        }
        Key::Insert
        | Key::Delete
        | Key::PageUp
        | Key::PageDown
        | Key::F5
        | Key::F6
        | Key::F7
        | Key::F8
        | Key::F9
        | Key::F10
        | Key::F11
        | Key::F12 => {
            let n = match key {
                Key::Insert => 2,
                Key::Delete => 3,
                Key::PageUp => 5,
                Key::PageDown => 6,
                Key::F5 => 15,
                Key::F6 => 17,
                Key::F7 => 18,
                Key::F8 => 19,
                Key::F9 => 20,
                Key::F10 => 21,
                Key::F11 => 23,
                _ => 24,
            };
            if modifier > 1 {
                format!("\u{1b}[{n};{modifier}~").into_bytes()
            } else {
                format!("\u{1b}[{n}~").into_bytes()
            }
        }
        Key::F1 | Key::F2 | Key::F3 | Key::F4 => {
            let c = match key {
                Key::F1 => 'P',
                Key::F2 => 'Q',
                Key::F3 => 'R',
                _ => 'S',
            };
            if modifier > 1 {
                format!("\u{1b}[1;{modifier}{c}").into_bytes()
            } else {
                format!("\u{1b}O{c}").into_bytes()
            }
        }
        Key::Enter => vec![b'\r'],
        Key::Tab if modifiers.shift => b"\x1b[Z".to_vec(),
        Key::Tab => vec![b'\t'],
        Key::Backspace => vec![if modifiers.ctrl { 8 } else { 127 }],
        Key::Escape => vec![27],
        _ if modifiers.ctrl && !modifiers.alt => {
            let name = key.name();
            let c = name.as_bytes();
            if c.len() == 1 && c[0].is_ascii_alphabetic() {
                vec![c[0].to_ascii_uppercase() - b'A' + 1]
            } else {
                match key {
                    Key::Space | Key::Num2 => vec![0],
                    Key::OpenBracket => vec![27],
                    Key::Backslash => vec![28],
                    Key::CloseBracket => vec![29],
                    Key::Num6 => vec![30],
                    Key::Minus => vec![31],
                    _ => return None,
                }
            }
        }
        _ => return None,
    };
    if modifiers.alt && matches!(key, Key::Enter | Key::Tab | Key::Backspace | Key::Escape) {
        bytes.insert(0, 27);
    }
    Some(bytes)
}

fn mouse_bytes(
    encoding: vt100::MouseProtocolEncoding,
    button: u8,
    cell: (u16, u16),
    released: bool,
) -> Vec<u8> {
    let (row, col) = cell;
    if encoding == vt100::MouseProtocolEncoding::Sgr {
        format!(
            "\u{1b}[<{button};{};{}{}",
            col + 1,
            row + 1,
            if released { 'm' } else { 'M' }
        )
        .into_bytes()
    } else {
        let mut bytes = b"\x1b[M".to_vec();
        let code = if released { 3 } else { button };
        if encoding == vt100::MouseProtocolEncoding::Utf8 {
            for value in [
                u32::from(code) + 32,
                u32::from(col) + 33,
                u32::from(row) + 33,
            ] {
                if let Some(c) = char::from_u32(value) {
                    let mut buf = [0; 4];
                    bytes.extend(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        } else {
            bytes.extend([code + 32, col.min(222) as u8 + 33, row.min(222) as u8 + 33]);
        }
        bytes
    }
}

fn ansi_color(color: vt100::Color, default: Color32, bold: bool) -> Color32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        vt100::Color::Idx(index) => {
            let index = if bold && index < 8 { index + 8 } else { index };
            const COLORS: [[u8; 3]; 16] = [
                [28, 35, 43],
                [224, 108, 117],
                [133, 202, 144],
                [226, 191, 112],
                [114, 163, 225],
                [187, 145, 222],
                [113, 206, 205],
                [212, 220, 228],
                [103, 116, 129],
                [245, 133, 139],
                [163, 225, 173],
                [246, 215, 139],
                [145, 190, 247],
                [214, 172, 246],
                [151, 233, 230],
                [242, 246, 250],
            ];
            let [r, g, b] = if index < 16 {
                COLORS[index as usize]
            } else if index >= 232 {
                [8 + 10 * (index - 232); 3]
            } else {
                let n = index - 16;
                let level = |v| if v == 0 { 0 } else { 55 + 40 * v };
                [level(n / 36), level(n / 6 % 6), level(n % 6)]
            };
            Color32::from_rgb(r, g, b)
        }
    }
}

/// vt100 handles drawing modes; this small bounded scanner answers terminal
/// queries so official TUIs do not stall waiting for cursor/capability replies.
struct TerminalBuffer {
    parser: vt100::Parser,
    query: Vec<u8>,
}
impl TerminalBuffer {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            query: Vec::new(),
        }
    }
    fn transcript(&mut self) -> String {
        let original = self.parser.screen().scrollback();
        let (rows, cols) = self.parser.screen().size();
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let mut remaining = self.parser.screen().scrollback();
        let mut output = String::new();
        loop {
            self.parser.screen_mut().set_scrollback(remaining);
            let take = if remaining == 0 {
                usize::from(rows)
            } else {
                remaining.min(usize::from(rows))
            };
            for (row, text) in self.parser.screen().rows(0, cols).take(take).enumerate() {
                output.push_str(&text);
                if !self.parser.screen().row_wrapped(row as u16) {
                    output.push('\n');
                }
            }
            if remaining == 0 {
                break;
            }
            remaining = remaining.saturating_sub(usize::from(rows));
        }
        self.parser.screen_mut().set_scrollback(original);
        output.truncate(output.trim_end_matches('\n').len());
        output
    }
    fn process(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut replies = Vec::new();
        let mut parsed = 0;
        for (i, &byte) in bytes.iter().enumerate() {
            if self.query.is_empty() {
                if byte == 27 {
                    self.query.push(byte);
                }
                continue;
            }
            self.query.push(byte);
            let complete = match self.query.get(1) {
                Some(b'[') => self.query.len() > 2 && (0x40..=0x7e).contains(&byte),
                Some(b']') => byte == 7 || self.query.ends_with(b"\x1b\\"),
                _ => self.query.len() > 1,
            };
            if complete {
                self.parser.process(&bytes[parsed..=i]);
                parsed = i + 1;
                let screen = self.parser.screen();
                let (r, c) = screen.cursor_position();
                let (rows, cols) = screen.size();
                let reply = match self.query.as_slice() {
                    b"\x1b[5n" => Some(b"\x1b[0n".to_vec()),
                    b"\x1b[6n" => Some(format!("\x1b[{};{}R", r + 1, c + 1).into_bytes()),
                    b"\x1b[?6n" => Some(format!("\x1b[?{};{}R", r + 1, c + 1).into_bytes()),
                    b"\x1b[c" | b"\x1b[0c" => Some(b"\x1b[?1;2c".to_vec()),
                    b"\x1b[>c" | b"\x1b[>0c" => Some(b"\x1b[>0;0;0c".to_vec()),
                    b"\x1b[18t" => Some(format!("\x1b[8;{rows};{cols}t").into_bytes()),
                    b"\x1b]10;?\x07" | b"\x1b]10;?\x1b\\" => {
                        Some(b"\x1b]10;rgb:d6d6/e1e1/e9e9\x1b\\".to_vec())
                    }
                    b"\x1b]11;?\x07" | b"\x1b]11;?\x1b\\" => {
                        Some(b"\x1b]11;rgb:0d0d/1313/1919\x1b\\".to_vec())
                    }
                    _ => None,
                };
                if let Some(reply) = reply {
                    replies.push(reply);
                }
                self.query.clear();
            } else if self.query.len() > 128 {
                self.query.clear();
            }
        }
        self.parser.process(&bytes[parsed..]);
        replies
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn ansi_unicode_alternate_screen_queries_and_resize() {
        let mut buffer = TerminalBuffer::new(10, 60);
        buffer.process("\x1b[31m你好 Rust\x1b[0m".as_bytes());
        assert!(buffer.parser.screen().contents().contains("你好 Rust"));
        assert_eq!(
            buffer.parser.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        buffer.process(b"\x1b[3;7H\x1b[");
        assert_eq!(buffer.process(b"6n"), vec![b"\x1b[3;7R".to_vec()]);
        buffer.process(b"\x1b[?1049h\x1b[?2004h\x1b[2J\x1b[HAPP");
        assert!(buffer.parser.screen().alternate_screen());
        assert!(buffer.parser.screen().bracketed_paste());
        assert_eq!(buffer.parser.screen().contents(), "APP");
        buffer.process(b"\x1b[?1049l");
        assert!(buffer.parser.screen().contents().contains("你好 Rust"));
        buffer.parser.screen_mut().set_size(15, 80);
        assert_eq!(buffer.process(b"\x1b[18t"), vec![b"\x1b[8;15;80t".to_vec()]);
    }

    #[test]
    fn terminal_input_preserves_controls_and_brackets_unicode_paste() {
        assert_eq!(key_bytes(Key::C, Modifiers::CTRL, false), Some(vec![3]));
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::NONE, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(Key::ArrowLeft, Modifiers::CTRL, false),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            key_bytes(Key::Enter, Modifiers::NONE, false),
            Some(vec![13])
        );
        assert_eq!(
            paste_bytes("你好\r\nRust", true),
            "\x1b[200~你好\nRust\x1b[201~".as_bytes()
        );
        assert_eq!(
            paste_bytes("x\x1b[201~y\0", true),
            b"\x1b[200~x[201~y\x1b[201~"
        );
    }

    fn shell_command(cwd: &Path) -> TerminalCommand {
        #[cfg(windows)]
        let (program, args) = (
            PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
                .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
            vec!["-NoLogo".into(), "-NoProfile".into(), "-NoExit".into()],
        );
        #[cfg(not(windows))]
        let (program, args) = (PathBuf::from("/bin/sh"), vec!["-i".into()]);
        TerminalCommand {
            program,
            args,
            cwd: cwd.to_owned(),
            env: BTreeMap::new(),
            title: "test".into(),
        }
    }

    fn poll_until(
        session: &mut TerminalSession,
        mut predicate: impl FnMut(&TerminalSession) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            session.poll();
            if predicate(session) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "terminal timed out: {:?}\n{}",
            session.status(),
            session.screen_text()
        );
    }

    #[test]
    fn native_pty_shell_accepts_input_unicode_cwd_and_exits() {
        let directory = tempfile::Builder::new()
            .prefix("terminal workspace ")
            .tempdir()
            .unwrap();
        let mut session = TerminalSession::spawn(shell_command(directory.path()), 24, 110).unwrap();
        // Wait for a real prompt before injecting a command (ConPTY requests
        // cursor coordinates during startup and needs poll() to answer it).
        poll_until(&mut session, |s| !s.screen_text().trim().is_empty());
        #[cfg(windows)]
        let command="[Console]::WriteLine('PTY-' + 'READY'); [Console]::WriteLine((Get-Location).Path); [Console]::WriteLine(([char]20320).ToString() + [char]22909)\r";
        #[cfg(not(windows))]
        let command = "printf 'PTY-%s\\n' READY; pwd; printf '\\344\\275\\240\\345\\245\\275\\n'\r";
        session.send_input(command.as_bytes()).unwrap();
        poll_until(&mut session, |s| {
            let text = s.screen_text();
            text.contains("PTY-READY") && text.contains("你好")
        });
        assert!(session
            .screen_text()
            .contains(directory.path().file_name().unwrap().to_str().unwrap()));
        session.resize(30, 100).unwrap();
        assert_eq!(session.screen().size(), (30, 100));
        #[cfg(windows)]
        let waiting = "[Console]::WriteLine('WAIT-' + 'START'); Start-Sleep -Seconds 30\r";
        #[cfg(not(windows))]
        let waiting = "printf 'WAIT-%s\\n' START; sleep 30\r";
        session.send_input(waiting.as_bytes()).unwrap();
        poll_until(&mut session, |s| s.screen_text().contains("WAIT-START"));
        session.send_input(&[3]).unwrap();
        poll_until(&mut session, |s| {
            s.screen_text()
                .rsplit_once("WAIT-START")
                .is_some_and(|(_, tail)| {
                    tail.contains('>') || tail.contains('$') || tail.contains('#')
                })
        });
        // A separate round trip ensures Ctrl+C reached the foreground job,
        // instead of merely testing that bytes can be written to the PTY.
        #[cfg(windows)]
        let resumed = "[Console]::WriteLine('CTRL-' + 'RESUMED')\r";
        #[cfg(not(windows))]
        let resumed = "printf 'CTRL-%s\\n' RESUMED\r";
        session.send_input(resumed.as_bytes()).unwrap();
        poll_until(&mut session, |s| s.screen_text().contains("CTRL-RESUMED"));
        session.send_input(b"exit\r").unwrap();
        poll_until(&mut session, |s| {
            matches!(s.status(), TerminalStatus::Exited { code: 0 })
        });
        poll_until(&mut session, |s| !s.needs_poll());
        let completed = session.completed_output().unwrap().to_owned();
        assert!(completed.contains("你好") && completed.contains("CTRL-RESUMED"));
        session.resize(1, 2).unwrap();
        assert_eq!(session.completed_output(), Some(completed.as_str()));
    }

    #[test]
    fn scrollback_is_bounded_and_ansi_queries_do_not_accumulate() {
        let mut buffer = TerminalBuffer::new(4, 20);
        for _ in 0..SCROLLBACK_LINES + 100 {
            buffer.process(b"line\r\n");
        }
        buffer.parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(buffer.parser.screen().scrollback(), SCROLLBACK_LINES);
        buffer.process(b"\x1b]");
        buffer.process(&vec![b'x'; 8192]);
        assert!(buffer.query.len() <= 128);
    }

    #[test]
    fn completed_transcript_includes_history_without_duplicates() {
        let mut buffer = TerminalBuffer::new(4, 40);
        for n in 0..13 {
            buffer.process(format!("LINE-{n:02}\r\n").as_bytes());
        }
        buffer.parser.screen_mut().set_scrollback(2);
        assert_eq!(
            buffer.transcript(),
            (0..13)
                .map(|n| format!("LINE-{n:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(buffer.parser.screen().scrollback(), 2);
    }

    /// Optional local verification of unmodified vendor TUI startup; it sends
    /// no prompts, accepts no workspace trust dialog, and prints no account data.
    #[test]
    #[ignore = "requires an installed official CLI; set WONDERLAND_TEST_CLI_ID"]
    fn installed_cli_starts_in_native_pty() {
        let id = std::env::var("WONDERLAND_TEST_CLI_ID").unwrap_or_else(|_| "codex".into());
        let profile = crate::desktop_bridge::default_cli_profiles()
            .into_iter()
            .find(|profile| profile.id == id)
            .expect("known CLI profile");
        let directory = tempfile::tempdir().unwrap();
        let launch = crate::desktop_bridge::prepare_cli(&profile, directory.path()).unwrap();
        let mut session = TerminalSession::spawn(
            TerminalCommand {
                program: launch.executable,
                args: launch.args,
                cwd: launch.cwd,
                env: launch.env,
                title: launch.label,
            },
            32,
            110,
        )
        .unwrap();
        poll_until(&mut session, |s| {
            s.screen_text()
                .chars()
                .filter(|c| !c.is_whitespace())
                .count()
                > 80
        });
        let settled = Instant::now() + Duration::from_secs(2);
        while Instant::now() < settled {
            session.poll();
            assert_eq!(session.status(), &TerminalStatus::Running);
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(session.status(), &TerminalStatus::Running);
        println!(
            "{id}: native TUI started with {} visible characters; no prompt submitted",
            session.screen_text().chars().count()
        );
        session.stop();
    }

    #[test]
    fn close_terminal_terminates_owned_process_without_blocking_ui() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = TerminalSession::spawn(shell_command(directory.path()), 24, 80).unwrap();
        poll_until(&mut session, |s| !s.screen_text().trim().is_empty());
        assert!(session.has_live_process());
        session.status = TerminalStatus::Failed("simulated input channel failure".into());
        assert!(
            session.has_live_process(),
            "I/O status must not hide a living process"
        );
        let pid = session.process_id().unwrap();
        let started = Instant::now();
        session.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(session.status(), &TerminalStatus::Stopped);
        assert!(!session.has_live_process());
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::{
                Foundation::CloseHandle,
                System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
            };
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if !process.is_null() {
                assert_eq!(WaitForSingleObject(process, 5000), 0);
                CloseHandle(process);
            }
        }
        #[cfg(unix)]
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while unsafe { libc::kill(pid as i32, 0) } == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0);
        }
    }

    #[test]
    fn ime_focus_loss_does_not_block_subsequent_terminal_input() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = TerminalSession::spawn(shell_command(directory.path()), 24, 100).unwrap();
        poll_until(&mut session, |s| !s.screen_text().trim().is_empty());
        let context = egui::Context::default();
        let frame = |session: &mut TerminalSession, focused, events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 440.0))),
                focused,
                events,
                ..Default::default()
            };
            let _ = context.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if focused {
                        let id = ui.make_persistent_id(("ime-focus-test", session.id()));
                        ui.memory_mut(|memory| memory.request_focus(id));
                    }
                    session.render(ui, "ime-focus-test");
                });
            });
        };
        frame(
            &mut session,
            true,
            vec![egui::Event::Ime(egui::ImeEvent::Preedit("候选".into()))],
        );
        assert_eq!(session.preedit, "候选");
        // No Disabled event reaches this terminal after focus moves elsewhere.
        frame(&mut session, false, vec![]);
        assert!(session.preedit.is_empty());
        #[cfg(windows)]
        let command = "[Console]::WriteLine('IME-' + 'RESUMED')";
        #[cfg(not(windows))]
        let command = "printf 'IME-%s\\n' RESUMED";
        frame(
            &mut session,
            true,
            vec![
                egui::Event::Text(command.into()),
                egui::Event::Key {
                    key: Key::Enter,
                    physical_key: Some(Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                },
            ],
        );
        poll_until(&mut session, |s| s.screen_text().contains("IME-RESUMED"));
        session.stop();
    }
}

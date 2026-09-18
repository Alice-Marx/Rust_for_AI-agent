//! Desktop-owned workbench: project files, Git review and unmodified CLI processes.
use super::*;
use std::path::{Path, PathBuf};
use wonderland::{
    desktop_bridge as bridge, desktop_terminal as terminal, desktop_workspace as workspace,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Chat,
    Files,
    Changes,
    Cli,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(path: &str, dirty: bool) -> Editor {
        Editor {
            file: workspace::WorkspaceFile {
                relative_path: PathBuf::from(path),
                content: "original".into(),
                revision: "revision".into(),
                language: "Text".into(),
            },
            text: if dirty { "edited" } else { "original" }.into(),
            saving: false,
        }
    }

    #[test]
    fn pending_close_tracks_file_when_other_tabs_move() {
        let mut workbench = Workbench::unstarted(".");
        workbench.generation = 4;
        workbench.editors = vec![editor("a", false), editor("b", true), editor("c", true)];
        workbench.active_editor = 2;
        workbench.request_close_editor(1);
        workbench.request_close_editor(0);
        workbench.finish_close_editor(true);
        assert_eq!(workbench.editors.len(), 1);
        assert_eq!(workbench.editors[0].file.relative_path, Path::new("c"));
        assert!(workbench.editors[0].dirty());
        assert_eq!(workbench.active_editor, 0);
        workbench.request_close_editor(0);
        workbench.generation += 1;
        workbench.finish_close_editor(true);
        assert_eq!(workbench.editors.len(), 1);
    }

    #[test]
    fn old_async_results_cannot_replace_new_selections() {
        let mut workbench = Workbench::unstarted(".");
        workbench.generation = 3;
        workbench.directory_request = 2;
        workbench.file_request = 2;
        workbench.git_request = 2;
        workbench.diff_request = 2;
        workbench.diff_path = Some(PathBuf::from("new.rs"));
        workbench.diff_loading = true;
        let entry = workspace::WorkspaceEntry {
            relative_path: "new/file".into(),
            name: "file".into(),
            is_dir: false,
            size: 0,
        };
        workbench
            .tx
            .send(Event::Directory(
                3,
                2,
                "new".into(),
                Ok(vec![entry.clone()]),
            ))
            .unwrap();
        workbench
            .tx
            .send(Event::Directory(3, 1, "old".into(), Ok(vec![])))
            .unwrap();
        workbench
            .tx
            .send(Event::Diff(3, 2, "new.rs".into(), Ok("new diff".into())))
            .unwrap();
        workbench
            .tx
            .send(Event::Diff(3, 1, "old.rs".into(), Ok("old diff".into())))
            .unwrap();
        workbench
            .tx
            .send(Event::File(3, 1, Ok(editor("old.rs", false).file)))
            .unwrap();
        workbench
            .tx
            .send(Event::Git(
                3,
                2,
                Ok(workspace::GitStatus {
                    branch: "new".into(),
                    entries: vec![],
                }),
            ))
            .unwrap();
        workbench
            .tx
            .send(Event::Git(
                3,
                1,
                Ok(workspace::GitStatus {
                    branch: "old".into(),
                    entries: vec![],
                }),
            ))
            .unwrap();
        workbench.poll(&egui::Context::default());
        assert_eq!(workbench.directory, Path::new("new"));
        assert_eq!(workbench.entries, vec![entry]);
        assert_eq!(workbench.diff, "new diff");
        assert!(!workbench.diff_loading);
        assert!(workbench.editors.is_empty());
        assert_eq!(workbench.git.as_ref().unwrap().branch, "new");
        workbench
            .tx
            .send(Event::Diff(
                3,
                2,
                "new.rs".into(),
                Err("read failed".into()),
            ))
            .unwrap();
        workbench.poll(&egui::Context::default());
        assert!(workbench.diff.is_empty());
        assert_eq!(workbench.notice, "read failed");
    }

    #[test]
    fn changing_workspace_clears_old_directory_and_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let mut workbench = Workbench::unstarted(".");
        workbench.editors.push(editor("old.rs", true));
        workbench.request_close_editor(0);
        workbench.entries.push(workspace::WorkspaceEntry {
            relative_path: "old.rs".into(),
            name: "old.rs".into(),
            is_dir: false,
            size: 0,
        });
        workbench.languages.push(workspace::ProjectLanguage {
            language: "Rust".into(),
            toolchain: "cargo".into(),
            manifests: vec![],
        });
        workbench.diff = "old".into();
        workbench.diff_path = Some("old.rs".into());
        workbench.diff_loading = true;
        workbench.apply_root(temp.path().to_path_buf());
        assert!(workbench.editors.is_empty());
        assert!(workbench.entries.is_empty());
        assert!(workbench.languages.is_empty());
        assert!(workbench.close_editor.is_none());
        assert!(workbench.diff_path.is_none());
        assert!(workbench.diff.is_empty());
        assert!(!workbench.diff_loading);
    }
}
struct Editor {
    file: workspace::WorkspaceFile,
    text: String,
    saving: bool,
}
#[derive(Clone)]
struct PendingEditorClose {
    generation: u64,
    path: PathBuf,
}
impl Editor {
    fn dirty(&self) -> bool {
        self.text != self.file.content
    }
}
enum Event {
    Root(PathBuf),
    Import(Vec<bridge::CliProfile>),
    Directory(
        u64,
        u64,
        PathBuf,
        Result<Vec<workspace::WorkspaceEntry>, String>,
    ),
    File(u64, u64, Result<workspace::WorkspaceFile, String>),
    Saved(u64, PathBuf, Result<workspace::WorkspaceFile, String>),
    Git(u64, u64, Result<workspace::GitStatus, String>),
    Diff(u64, u64, PathBuf, Result<String, String>),
    Languages(u64, Vec<workspace::ProjectLanguage>),
    Detected(Vec<bridge::CliStatus>),
    Terminal(Result<terminal::TerminalSession, String>),
    Notice(Result<String, String>),
}
pub struct Workbench {
    pub pane: Pane,
    pub show_terminal: bool,
    pub terminal_expanded: bool,
    pub profiles: Vec<bridge::CliProfile>,
    pub recent: Vec<String>,
    pub root: PathBuf,
    pub changed_root: Option<String>,
    pub chat_context: Option<String>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    generation: u64,
    directory_request: u64,
    file_request: u64,
    git_request: u64,
    diff_request: u64,
    directory: PathBuf,
    entries: Vec<workspace::WorkspaceEntry>,
    listing: bool,
    editors: Vec<Editor>,
    active_editor: usize,
    close_editor: Option<PendingEditorClose>,
    pending_root: Option<PathBuf>,
    git: Option<workspace::GitStatus>,
    diff: String,
    diff_path: Option<PathBuf>,
    diff_loading: bool,
    languages: Vec<workspace::ProjectLanguage>,
    statuses: Vec<bridge::CliStatus>,
    scanning: bool,
    selected_cli: usize,
    args_json: String,
    profile_open: bool,
    terminals: Vec<terminal::TerminalSession>,
    active_terminal: usize,
    starting: bool,
    close_terminal: Option<usize>,
    pub notice: String,
}
impl Workbench {
    #[cfg(feature = "ui-snapshots")]
    pub fn prepare_snapshot(&mut self, mode: &str) {
        match mode {
            "cli" => self.pane = Pane::Cli,
            "files" => {
                self.pane = Pane::Files;
                self.read_directory(PathBuf::from("src"));
                self.open_file(PathBuf::from("src/lib.rs"));
            }
            "changes" => {
                self.pane = Pane::Changes;
                self.load_git();
                self.load_diff(PathBuf::from("Cargo.toml"));
            }
            "terminal" => {
                self.pane = Pane::Cli;
                self.show_terminal = true;
                self.launch(None, false);
            }
            _ => {}
        }
    }
    pub fn new(root: &str) -> Self {
        let mut result = Self::unstarted(root);
        result.reload_root();
        result.scan_clis();
        result
    }
    fn unstarted(root: &str) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            pane: Pane::Chat,
            show_terminal: false,
            terminal_expanded: false,
            profiles: bridge::default_cli_profiles(),
            recent: vec![],
            root: PathBuf::from(root),
            changed_root: None,
            chat_context: None,
            tx,
            rx,
            generation: 0,
            directory_request: 0,
            file_request: 0,
            git_request: 0,
            diff_request: 0,
            directory: PathBuf::new(),
            entries: vec![],
            listing: false,
            editors: vec![],
            active_editor: 0,
            close_editor: None,
            pending_root: None,
            git: None,
            diff: String::new(),
            diff_path: None,
            diff_loading: false,
            languages: vec![],
            statuses: vec![],
            scanning: false,
            selected_cli: 0,
            args_json: "[]".into(),
            profile_open: false,
            terminals: vec![],
            active_terminal: 0,
            starting: false,
            close_terminal: None,
            notice: String::new(),
        }
    }
    pub fn has_unsaved(&self) -> bool {
        self.editors.iter().any(Editor::dirty)
    }
    pub fn has_pending_root(&self) -> bool {
        self.pending_root.is_some()
    }
    pub fn has_running(&self) -> bool {
        self.terminals
            .iter()
            .any(terminal::TerminalSession::has_live_process)
    }
    pub fn stop_all(&mut self) {
        for terminal in &mut self.terminals {
            terminal.stop();
        }
    }
    pub fn restore(&mut self, storage: &dyn eframe::Storage) {
        if let Some(profiles) =
            eframe::get_value::<Vec<bridge::CliProfile>>(storage, "cli_profiles")
        {
            if !profiles.is_empty() {
                self.profiles = profiles;
            }
        }
        self.recent = eframe::get_value(storage, "recent_workspaces").unwrap_or_default();
        self.scanning = false;
        self.scan_clis();
    }
    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "cli_profiles", &self.profiles);
        eframe::set_value(storage, "recent_workspaces", &self.recent);
    }
    pub fn choose_root(&self) {
        let tx = self.tx.clone();
        let root = self.root.clone();
        thread::spawn(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("打开项目文件夹")
                .set_directory(root)
                .pick_folder()
            {
                let _ = tx.send(Event::Root(path));
            }
        });
    }
    pub fn request_root(&mut self, path: PathBuf) {
        if path == self.root {
            return;
        }
        if self.has_unsaved() {
            self.pending_root = Some(path);
        } else {
            self.apply_root(path);
        }
    }

    /// A root change invalidates every in-flight workspace response. Request
    /// counters are bumped as well as the generation so queued events cannot
    /// match a newly-issued operation by coincidence.
    fn invalidate_workspace_requests(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.directory_request = self.directory_request.wrapping_add(1);
        self.file_request = self.file_request.wrapping_add(1);
        self.git_request = self.git_request.wrapping_add(1);
        self.diff_request = self.diff_request.wrapping_add(1);
    }
    fn apply_root(&mut self, path: PathBuf) {
        if !path.is_dir() {
            self.notice = "工作区目录不存在".into();
            return;
        }
        self.root = path;
        self.invalidate_workspace_requests();
        self.changed_root = Some(self.root.display().to_string());
        self.recent
            .retain(|p| p != &self.root.display().to_string());
        self.recent.insert(0, self.root.display().to_string());
        self.recent.truncate(12);
        self.editors.clear();
        self.active_editor = 0;
        self.close_editor = None;
        self.pending_root = None;
        self.directory.clear();
        self.entries.clear();
        self.languages.clear();
        self.git = None;
        self.diff.clear();
        self.diff_path = None;
        self.diff_loading = false;
        self.notice.clear();
        self.reload_root();
    }
    fn reload_root(&mut self) {
        self.generation += 1;
        self.read_directory(PathBuf::new());
        let (root, tx, generation) = (self.root.clone(), self.tx.clone(), self.generation);
        thread::spawn(move || {
            let languages = workspace::detect_project(&root).unwrap_or_default();
            let _ = tx.send(Event::Languages(generation, languages));
        });
    }
    fn read_directory(&mut self, path: PathBuf) {
        self.listing = true;
        self.directory_request += 1;
        let request = self.directory_request;
        self.directory = path.clone();
        self.entries.clear();
        let (root, tx, generation) = (self.root.clone(), self.tx.clone(), self.generation);
        thread::spawn(move || {
            let result = workspace::list_directory(&root, &path).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Directory(generation, request, path, result));
        });
    }
    fn open_file(&mut self, path: PathBuf) {
        self.file_request += 1;
        let request = self.file_request;
        if let Some(index) = self
            .editors
            .iter()
            .position(|e| e.file.relative_path == path)
        {
            self.active_editor = index;
            self.pane = Pane::Files;
            return;
        }
        let (root, tx, generation) = (self.root.clone(), self.tx.clone(), self.generation);
        thread::spawn(move || {
            let _ = tx.send(Event::File(
                generation,
                request,
                workspace::open_file(&root, &path).map_err(|e| format!("{e:#}")),
            ));
        });
    }
    fn save_editor(&mut self, index: usize) {
        let Some(editor) = self.editors.get_mut(index) else {
            return;
        };
        if editor.saving || !editor.dirty() {
            return;
        }
        editor.saving = true;
        let (root, tx, generation) = (self.root.clone(), self.tx.clone(), self.generation);
        let (path, revision, text) = (
            editor.file.relative_path.clone(),
            editor.file.revision.clone(),
            editor.text.clone(),
        );
        thread::spawn(move || {
            let result =
                workspace::save_file(&root, &path, &revision, &text).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Saved(generation, path, result));
        });
    }
    fn remove_editor(&mut self, index: usize) {
        self.editors.remove(index);
        if index < self.active_editor {
            self.active_editor -= 1;
        }
        self.active_editor = self.active_editor.min(self.editors.len().saturating_sub(1));
    }
    fn request_close_editor(&mut self, index: usize) {
        let Some(editor) = self.editors.get(index) else {
            return;
        };
        if editor.dirty() {
            self.close_editor = Some(PendingEditorClose {
                generation: self.generation,
                path: editor.file.relative_path.clone(),
            });
        } else {
            self.remove_editor(index);
        }
    }
    fn finish_close_editor(&mut self, discard: bool) {
        let Some(pending) = self.close_editor.take() else {
            return;
        };
        if discard && pending.generation == self.generation {
            if let Some(index) = self
                .editors
                .iter()
                .position(|editor| editor.file.relative_path == pending.path)
            {
                self.remove_editor(index);
            }
        }
    }
    pub fn scan_clis(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        let profiles = self.profiles.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let handles: Vec<_> = profiles
                .into_iter()
                .map(|p| thread::spawn(move || bridge::detect_cli(&p)))
                .collect();
            let statuses = handles.into_iter().filter_map(|h| h.join().ok()).collect();
            let _ = tx.send(Event::Detected(statuses));
        });
    }
    pub fn launch(&mut self, profile: Option<bridge::CliProfile>, external: bool) {
        if self.starting {
            return;
        }
        self.starting = true;
        let (root, tx) = (self.root.clone(), self.tx.clone());
        if !external {
            self.show_terminal = true;
        }
        thread::spawn(move || {
            let spec = match profile {
                Some(profile) => bridge::prepare_cli(&profile, &root),
                None => bridge::prepare_shell(&root),
            };
            if external {
                let result = spec
                    .and_then(|s| bridge::launch_external(&s))
                    .map(|_| "已打开系统终端".into())
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::Notice(result));
            } else {
                let result = spec
                    .and_then(|s| {
                        terminal::TerminalSession::spawn(
                            terminal::TerminalCommand {
                                program: s.executable,
                                args: s.args,
                                cwd: s.cwd,
                                env: s.env,
                                title: s.label,
                            },
                            24,
                            100,
                        )
                    })
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::Terminal(result));
            }
        });
    }
    fn load_git(&mut self) {
        self.git_request += 1;
        let request = self.git_request;
        let (root, tx, generation) = (self.root.clone(), self.tx.clone(), self.generation);
        thread::spawn(move || {
            let result = Runtime::new().map_err(|e| e.to_string()).and_then(|rt| {
                rt.block_on(workspace::git_status(&root))
                    .map_err(|e| format!("{e:#}"))
            });
            let _ = tx.send(Event::Git(generation, request, result));
        });
    }
    fn load_diff(&mut self, path: PathBuf) {
        self.diff_loading = true;
        self.diff_request += 1;
        let request = self.diff_request;
        self.diff_path = Some(path.clone());
        self.diff.clear();
        let (root, tx, generation) = (self.root.clone(), self.tx.clone(), self.generation);
        thread::spawn(move || {
            let result = Runtime::new().map_err(|e| e.to_string()).and_then(|rt| {
                rt.block_on(workspace::git_diff(&root, Some(&path)))
                    .map_err(|e| format!("{e:#}"))
            });
            let _ = tx.send(Event::Diff(generation, request, path, result));
        });
    }
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Root(path) => self.request_root(path),
                Event::Import(profiles) => {
                    for profile in profiles {
                        if !self.profiles.iter().any(|p| p.id == profile.id) {
                            self.profiles.push(profile);
                        }
                    }
                    self.scan_clis();
                    self.pane = Pane::Cli;
                }
                Event::Directory(g, request, path, result)
                    if g == self.generation && request == self.directory_request =>
                {
                    self.listing = false;
                    match result {
                        Ok(entries) => {
                            self.directory = path;
                            self.entries = entries;
                        }
                        Err(e) => {
                            self.entries.clear();
                            self.notice = e;
                        }
                    }
                }
                Event::File(g, request, result)
                    if g == self.generation && request == self.file_request =>
                {
                    match result {
                        Ok(file) => {
                            if let Some(i) = self
                                .editors
                                .iter()
                                .position(|e| e.file.relative_path == file.relative_path)
                            {
                                self.active_editor = i;
                            } else {
                                self.editors.push(Editor {
                                    text: file.content.clone(),
                                    file,
                                    saving: false,
                                });
                                self.active_editor = self.editors.len() - 1;
                            }
                            self.pane = Pane::Files;
                        }
                        Err(e) => self.notice = e,
                    }
                }
                Event::Saved(g, path, result) if g == self.generation => {
                    if let Some(editor) = self
                        .editors
                        .iter_mut()
                        .find(|e| e.file.relative_path == path)
                    {
                        editor.saving = false;
                        match result {
                            Ok(file) => {
                                editor.file = file;
                                self.notice = "文件已保存".into();
                            }
                            Err(e) => self.notice = e,
                        }
                    }
                }
                Event::Git(g, request, result)
                    if g == self.generation && request == self.git_request =>
                {
                    match result {
                        Ok(status) => self.git = Some(status),
                        Err(e) => {
                            self.git = None;
                            self.notice = e;
                        }
                    }
                }
                Event::Diff(g, request, path, result)
                    if g == self.generation
                        && request == self.diff_request
                        && self.diff_path.as_ref() == Some(&path) =>
                {
                    self.diff_loading = false;
                    match result {
                        Ok(diff) => self.diff = diff,
                        Err(e) => {
                            self.diff.clear();
                            self.notice = e;
                        }
                    }
                }
                Event::Languages(g, languages) if g == self.generation => {
                    self.languages = languages
                }
                Event::Detected(statuses) => {
                    self.statuses = statuses;
                    self.scanning = false;
                }
                Event::Terminal(result) => {
                    self.starting = false;
                    match result {
                        Ok(terminal) => {
                            self.terminals.push(terminal);
                            self.active_terminal = self.terminals.len() - 1;
                        }
                        Err(e) => self.notice = e,
                    }
                }
                Event::Notice(result) => {
                    self.starting = false;
                    self.notice = match result {
                        Ok(s) | Err(s) => s,
                    };
                }
                _ => {}
            }
        }
        for terminal in &mut self.terminals {
            if terminal.needs_poll() {
                terminal.poll();
                ctx.request_repaint_after(std::time::Duration::from_millis(33));
            }
        }
        if self.scanning || self.starting || self.listing {
            ctx.request_repaint_after(std::time::Duration::from_millis(70));
        }
    }
    pub fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            if icons::button(ui, icons::Icon::Folder, "打开项目", 102., false).clicked() {
                self.choose_root();
            }
            let label = self
                .root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            ui.menu_button(truncate(&label, 20), |ui| {
                ui.label(self.root.display().to_string());
                if ui.button("在文件管理器打开").clicked() {
                    let root = self.root.clone();
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let _ = tx.send(Event::Notice(
                            bridge::open_workspace(&root)
                                .map(|_| "已打开文件管理器".into())
                                .map_err(|e| e.to_string()),
                        ));
                    });
                    ui.close_menu();
                }
                ui.separator();
                for path in self.recent.clone() {
                    if ui.button(&path).clicked() {
                        self.request_root(PathBuf::from(path));
                        ui.close_menu();
                    }
                }
            });
            ui.separator();
            for (pane, label, icon, width) in [
                (Pane::Chat, "对话", icons::Icon::Chat, 80.),
                (Pane::Files, "文件", icons::Icon::File, 80.),
                (Pane::Changes, "变更", icons::Icon::Branch, 80.),
                (Pane::Cli, "CLI 工作台", icons::Icon::Grid, 118.),
            ] {
                if icons::button(ui, icon, label, width, self.pane == pane).clicked() {
                    self.pane = pane;
                    if pane == Pane::Changes {
                        self.load_git();
                    }
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icons::button(ui, icons::Icon::Terminal, "终端", 82., self.show_terminal)
                    .clicked()
                {
                    self.show_terminal = !self.show_terminal;
                    if self.show_terminal && self.terminals.is_empty() {
                        self.launch(None, false);
                    }
                }
            });
        });
        if !self.notice.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&self.notice).small().color(ACCENT));
                if ui.small_button("×").clicked() {
                    self.notice.clear();
                }
            });
        }
    }
    pub fn render_pane(&mut self, ui: &mut Ui) {
        match self.pane {
            Pane::Files => self.render_files(ui),
            Pane::Changes => self.render_changes(ui),
            Pane::Cli => self.render_clis(ui),
            Pane::Chat => {}
        }
    }
    fn render_files(&mut self, ui: &mut Ui) {
        egui::SidePanel::left("explorer-inside")
            .default_width(200.0)
            .min_width(140.0)
            .max_width(360.0)
            .resizable(true)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("项目文件");
                    if ui.small_button("刷新").clicked() {
                        self.read_directory(self.directory.clone());
                    }
                });
                ui.label(
                    RichText::new(
                        self.languages
                            .iter()
                            .map(|l| l.language.as_str())
                            .collect::<Vec<_>>()
                            .join(" · "),
                    )
                    .small()
                    .color(MUTED),
                );
                if !self.directory.as_os_str().is_empty() && ui.button("← 上一级").clicked() {
                    self.read_directory(
                        self.directory
                            .parent()
                            .unwrap_or(Path::new(""))
                            .to_path_buf(),
                    );
                }
                if self.listing {
                    ui.spinner();
                }
                let mut open = None;
                ScrollArea::vertical().show(ui, |ui| {
                    for entry in &self.entries {
                        if ui
                            .selectable_label(
                                false,
                                format!("{} {}", if entry.is_dir { "▸" } else { "·" }, entry.name),
                            )
                            .clicked()
                        {
                            open = Some(entry.clone());
                        }
                    }
                });
                if let Some(entry) = open {
                    if entry.is_dir {
                        self.read_directory(entry.relative_path);
                    } else {
                        self.open_file(entry.relative_path);
                    }
                }
            });
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(16.0)
                    .corner_radius(12),
            )
            .show_inside(ui, |ui| {
                let mut close = None;
                ScrollArea::horizontal()
                    .id_salt("editor-tabs")
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (i, editor) in self.editors.iter().enumerate() {
                                ui.push_id(&editor.file.relative_path, |ui| {
                                    let name = editor
                                        .file
                                        .relative_path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy();
                                    if ui
                                        .selectable_label(
                                            self.active_editor == i,
                                            format!(
                                                "{name}{}",
                                                if editor.dirty() { " •" } else { "" }
                                            ),
                                        )
                                        .clicked()
                                    {
                                        self.active_editor = i;
                                    }
                                    if ui.small_button("×").on_hover_text("关闭文件").clicked()
                                    {
                                        close = Some(i);
                                    }
                                });
                            }
                        });
                    });
                if let Some(i) = close {
                    self.request_close_editor(i);
                }
                let mut save = false;
                let mut context = None;
                if let Some(editor) = self.editors.get_mut(self.active_editor) {
                    ui.add_space(4.);
                    ui.horizontal_wrapped(|ui| {
                        icons::glyph(ui, icons::Icon::File, 14., MUTED);
                        ui.label(
                            RichText::new(truncate(
                                &editor.file.relative_path.display().to_string(),
                                48,
                            ))
                            .small()
                            .color(MUTED),
                        );
                        ui.label(RichText::new(&editor.file.language).small().color(ACCENT));
                        if ui
                            .add_enabled(
                                editor.dirty() && !editor.saving,
                                egui::Button::new("保存 · Ctrl+S"),
                            )
                            .clicked()
                        {
                            save = true;
                        }
                        if ui.button("添加到对话").clicked() {
                            context = Some(format!(
                                "请查看 {}：\n```{}\n{}\n```",
                                editor.file.relative_path.display(),
                                editor.file.language,
                                editor.text.chars().take(16000).collect::<String>()
                            ));
                        }
                    });
                    ui.separator();
                    let height = (ui.available_height() - 30.).max(40.);
                    ScrollArea::both()
                        .id_salt(("code", &editor.file.relative_path))
                        .auto_shrink([false, false])
                        .max_height(height)
                        .show(ui, |ui| {
                            let response = ui.add(
                                TextEdit::multiline(&mut editor.text)
                                    .id_salt(("editor", &editor.file.relative_path))
                                    .font(egui::FontId::monospace(14.))
                                    .code_editor()
                                    .frame(false)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(25),
                            );
                            if response.has_focus()
                                && ui.input_mut(|i| {
                                    i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)
                                })
                            {
                                save = true;
                            }
                        });
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{} 行  ·  UTF-8", editor.text.lines().count()))
                                .small()
                                .color(MUTED),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new(if editor.saving {
                                    "正在保存…"
                                } else if editor.dirty() {
                                    "有未保存编辑"
                                } else {
                                    "已保存"
                                })
                                .small()
                                .color(if editor.dirty() {
                                    ACCENT
                                } else {
                                    MUTED
                                }),
                            );
                        });
                    });
                } else {
                    ui.add_space(65.);
                    icons::glyph(ui, icons::Icon::Code, 36., ACCENT);
                    ui.add_space(14.);
                    ui.heading("代码，就在对话旁边");
                    ui.label(
                        RichText::new("从左侧选择文件，查看、编辑或添加到对话。").color(MUTED),
                    );
                    ui.add_space(8.);
                    ui.label(
                        RichText::new("明确保存你的修改，自动检查外部变更。")
                            .small()
                            .color(MUTED),
                    );
                }
                if save {
                    self.save_editor(self.active_editor);
                }
                if let Some(context) = context {
                    self.chat_context = Some(context);
                    self.pane = Pane::Chat;
                }
            });
    }
    fn render_changes(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading("工作区变更");
            if ui.button("刷新").clicked() {
                self.load_git();
            }
            if let Some(git) = &self.git {
                ui.label(RichText::new(&git.branch).small().color(ACCENT));
            }
        });
        ui.separator();
        let mut select = None;
        egui::SidePanel::left("git-files")
            .default_width(230.0)
            .show_inside(ui, |ui| {
                ScrollArea::vertical().show(ui, |ui| {
                    if let Some(git) = &self.git {
                        if git.entries.is_empty() {
                            ui.label("工作区没有未提交变更");
                        }
                        for entry in &git.entries {
                            if ui
                                .selectable_label(
                                    self.diff_path.as_ref() == Some(&entry.path),
                                    format!(
                                        "{}{}  {}",
                                        entry.index_status,
                                        entry.worktree_status,
                                        entry.path.display()
                                    ),
                                )
                                .clicked()
                            {
                                select = Some(entry.path.clone());
                            }
                        }
                    } else {
                        ui.label("点击刷新读取 Git 状态");
                    }
                });
            });
        if let Some(path) = select {
            self.load_diff(path);
        }
        egui::CentralPanel::default()
            .frame(Frame::new().inner_margin(12.))
            .show_inside(ui, |ui| {
                if let Some(path) = &self.diff_path {
                    ui.label(RichText::new(path.display().to_string()).strong());
                    ui.separator();
                }
                if self.diff_loading {
                    ui.spinner();
                }
                ScrollArea::both().show(ui, |ui| {
                    if self.diff.is_empty() && !self.diff_loading {
                        let message = if self.diff_path.is_some() {
                            "此文件没有可显示的文本变更。"
                        } else {
                            "选择文件，查看暂存、未暂存及新增文件的改动。"
                        };
                        ui.label(RichText::new(message).color(MUTED));
                    }
                    for line in self.diff.lines() {
                        let color = if line.starts_with('+') {
                            ACCENT
                        } else if line.starts_with('-') {
                            Color32::from_rgb(243, 155, 145)
                        } else if line.starts_with('@') {
                            Color32::from_rgb(150, 186, 242)
                        } else {
                            TEXT
                        };
                        ui.add(
                            egui::Label::new(
                                RichText::new(line).monospace().size(12.).color(color),
                            )
                            .selectable(true),
                        );
                    }
                });
            });
    }
    fn render_clis(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            icons::glyph(ui, icons::Icon::Grid, 24., ACCENT);
            ui.label(RichText::new("CLI 工作台").size(24.).strong());
            if ui
                .add_enabled(!self.scanning, egui::Button::new("检测安装与版本"))
                .clicked()
            {
                self.scan_clis();
            }
            if self.scanning {
                ui.spinner();
            }
            if ui.button("导入源码目录").clicked() {
                let tx = self.tx.clone();
                thread::spawn(move || {
                    if let Some(root) = rfd::FileDialog::new()
                        .set_title("选择包含 codex、kimi-cli 等源码目录的父目录")
                        .pick_folder()
                    {
                        let _ = tx.send(Event::Import(bridge::checkout_cli_profiles(&root)));
                    }
                });
            }
            if ui.button("＋ 自定义程序").clicked() {
                self.profiles.push(bridge::CliProfile {
                    id: Uuid::new_v4().to_string(),
                    ..Default::default()
                });
                self.selected_cli = self.profiles.len() - 1;
                self.args_json = "[]".into();
                self.profile_open = true;
            }
        });
        ui.label(RichText::new("你熟悉的工具，在同一个工作区。").color(MUTED));
        ui.label(
            RichText::new("保留各工具自己的登录、配置与能力。API 和订阅聚合仍可在「对话」中使用。")
                .small()
                .color(MUTED),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let ready = self
                .statuses
                .iter()
                .filter(|s| s.executable.is_some() && s.error.is_none())
                .count();
            ui.label(
                RichText::new(format!("{ready} 个就绪  /  {} 个入口", self.profiles.len()))
                    .small()
                    .color(ACCENT),
            );
            ui.separator();
            icons::glyph(ui, icons::Icon::Folder, 14., MUTED);
            ui.label(
                RichText::new(truncate(&self.root.display().to_string(), 76))
                    .small()
                    .color(MUTED),
            );
        });
        ui.add_space(16.0);
        let mut action = None;
        let mut configure = None;
        let columns = if ui.available_width() >= 770. { 2 } else { 1 };
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for start in (0..self.profiles.len()).step_by(columns) {
                    ui.columns(columns, |cells| {
                        for (column, ui) in cells.iter_mut().enumerate() {
                            let i = start + column;
                            let Some(profile) = self.profiles.get(i) else {
                                continue;
                            };
                            let status = self.statuses.iter().find(|s| s.id == profile.id);
                            let available =
                                status.is_some_and(|s| s.executable.is_some() && s.error.is_none());
                            let (mark, tint, description) = cli_identity(&profile.id);
                            Frame::new()
                                .fill(PANEL)
                                .stroke(Stroke::new(1.0_f32, BORDER))
                                .corner_radius(12)
                                .inner_margin(18.)
                                .show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.set_min_height(196.);
                                    ui.horizontal(|ui| {
                                        let (rect, _) = ui.allocate_exact_size(
                                            Vec2::splat(42.),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(
                                            rect,
                                            12,
                                            tint.gamma_multiply(0.12),
                                        );
                                        ui.painter().text(
                                            rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            mark,
                                            egui::FontId::monospace(21.),
                                            tint,
                                        );
                                        ui.vertical(|ui| {
                                            ui.label(
                                                RichText::new(truncate(&profile.name, 32))
                                                    .size(16.)
                                                    .strong(),
                                            )
                                            .on_hover_text(&profile.name);
                                            ui.label(
                                                RichText::new(description).small().color(MUTED),
                                            );
                                        });
                                    });
                                    ui.add_space(9.);
                                    ui.horizontal(|ui| {
                                        let color = if available {
                                            ACCENT
                                        } else {
                                            Color32::from_rgb(218, 184, 127)
                                        };
                                        let (dot, _) = ui.allocate_exact_size(
                                            Vec2::splat(8.),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().circle_filled(dot.center(), 3., color);
                                        ui.label(
                                            RichText::new(if !profile.enabled {
                                                "已停用"
                                            } else if available {
                                                "可启动"
                                            } else if self.scanning {
                                                "正在检测"
                                            } else {
                                                "需要配置"
                                            })
                                            .small()
                                            .color(color),
                                        );
                                        if let Some(version) =
                                            status.and_then(|s| s.version.as_deref())
                                        {
                                            ui.label(
                                                RichText::new(truncate(version, 34))
                                                    .small()
                                                    .color(MUTED),
                                            )
                                            .on_hover_text(version);
                                        }
                                    });
                                    let path = status
                                        .and_then(|s| s.executable.as_ref())
                                        .map(|p| p.display().to_string())
                                        .unwrap_or_else(|| profile.executable.clone());
                                    ui.label(
                                        RichText::new(truncate(&path, 56))
                                            .monospace()
                                            .small()
                                            .color(MUTED),
                                    )
                                    .on_hover_text(path);
                                    ui.add_space(6.);
                                    ui.horizontal_wrapped(|ui| {
                                        if ui
                                            .add_enabled(
                                                profile.enabled && !self.starting,
                                                egui::Button::new(
                                                    RichText::new("打开终端  ↗").color(ACCENT),
                                                )
                                                .fill(Color32::from_rgb(36, 55, 51)),
                                            )
                                            .clicked()
                                        {
                                            action = Some((profile.clone(), false));
                                        }
                                        if ui
                                            .add_enabled(
                                                profile.enabled && !self.starting,
                                                egui::Button::new("系统终端"),
                                            )
                                            .clicked()
                                        {
                                            action = Some((profile.clone(), true));
                                        }
                                        if ui.button("配置").clicked() {
                                            configure = Some(i);
                                        }
                                    });
                                    ui.add_space(4.);
                                    ui.push_id(&profile.id, |ui| {
                                        ui.collapsing("安装与入口说明", |ui| {
                                            if let Some(error) =
                                                status.and_then(|s| s.error.as_deref())
                                            {
                                                ui.label(RichText::new(error).small().color(MUTED));
                                            }
                                            ui.label(
                                                RichText::new(&profile.install_hint)
                                                    .small()
                                                    .color(MUTED),
                                            );
                                            if ui.small_button("复制说明").clicked() {
                                                ui.ctx().copy_text(profile.install_hint.clone());
                                            }
                                        });
                                    });
                                });
                        }
                    });
                    ui.add_space(12.);
                }
            });
        if let Some((profile, external)) = action {
            self.launch(Some(profile), external);
        }
        if let Some(i) = configure {
            self.selected_cli = i;
            self.args_json =
                serde_json::to_string_pretty(&self.profiles[i].args).unwrap_or("[]".into());
            self.profile_open = true;
        }
    }
    pub fn terminal_panel(&mut self, ctx: &egui::Context, input_blocked: bool) {
        if !self.show_terminal {
            return;
        }
        let maximum_height = (ctx.screen_rect().height() - 150.).max(200.);
        let panel = egui::TopBottomPanel::bottom(if self.terminal_expanded {
            "workbench-terminal-expanded"
        } else {
            "workbench-terminal"
        })
        .resizable(true)
        .default_height(280.)
        .height_range(200.0..=maximum_height);
        let panel = if self.terminal_expanded {
            panel.exact_height(maximum_height)
        } else {
            panel
        };
        panel
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(13, 19, 25))
                    .inner_margin(10.),
            )
            .show(ctx, |ui| {
                // A modal dialog must not leave a focused PTY receiving keys in
                // the background. Keep the terminal visible for context, but
                // disable every terminal widget until the dialog is closed.
                let modal_open = input_blocked
                    || self.profile_open
                    || self.close_editor.is_some()
                    || self.pending_root.is_some()
                    || self.close_terminal.is_some();
                if modal_open {
                    ui.disable();
                }
                ui.horizontal(|ui| {
                    ui.strong(format!("终端 · {}", self.terminals.len()));
                    if ui
                        .add_enabled(!self.starting, egui::Button::new("＋ Shell"))
                        .clicked()
                    {
                        self.launch(None, false);
                    }
                    ui.menu_button("打开 CLI", |ui| {
                        let mut selected = None;
                        for profile in &self.profiles {
                            if profile.enabled && ui.button(&profile.name).clicked() {
                                selected = Some(profile.clone());
                                ui.close_menu();
                            }
                        }
                        if let Some(profile) = selected {
                            self.launch(Some(profile), false);
                        }
                    });
                    if ui.button("系统终端 ↗").clicked() {
                        self.launch(None, true);
                    }
                    if ui.button("隐藏").clicked() {
                        self.show_terminal = false;
                    }
                    if ui
                        .button(if self.terminal_expanded {
                            "还原"
                        } else {
                            "展开"
                        })
                        .clicked()
                    {
                        self.terminal_expanded = !self.terminal_expanded;
                    }
                    if !self.terminals.is_empty() && ui.button("关闭会话").clicked() {
                        self.close_terminal = Some(self.active_terminal);
                    }
                });
                // Tabs are kept in their own horizontal scroll area so a
                // number of active CLI sessions never steals the PTY height.
                egui::ScrollArea::horizontal()
                    .id_salt("terminal-tabs")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for (i, term) in self.terminals.iter().enumerate() {
                                let status = terminal_status_label(term.status());
                                if ui
                                    .selectable_label(
                                        self.active_terminal == i,
                                        format!("{} · {status}", term.title()),
                                    )
                                    .clicked()
                                {
                                    self.active_terminal = i;
                                }
                            }
                        });
                    });
                if self.starting {
                    ui.spinner();
                }
                if let Some(term) = self.terminals.get_mut(self.active_terminal) {
                    ui.label(
                        RichText::new(format!(
                            "{}  ·  点击终端输入 · Ctrl+C 中断 · Ctrl+Shift+C 复制选区",
                            term.cwd().display()
                        ))
                        .small()
                        .color(MUTED),
                    );
                    let response = term.render(ui, "terminal");
                    if let Some(error) = response.error {
                        self.notice = error;
                    }
                } else {
                    ui.add_space(22.);
                    ui.label("启动一个 Shell 或选择官方 CLI。终端在当前项目目录运行。");
                }
            });
    }
    pub fn dialogs(&mut self, ctx: &egui::Context) {
        if self.profile_open {
            let mut open = true;
            let mut saved = false;
            egui::Window::new("CLI 启动配置").open(&mut open).default_width(590.).resizable(true).show(ctx,|ui|{
                if let Some(profile)=self.profiles.get_mut(self.selected_cli){ui.label("名称");ui.text_edit_singleline(&mut profile.name);ui.label("程序或解释器路径");ui.add(TextEdit::singleline(&mut profile.executable).desired_width(f32::INFINITY));ui.label("参数数组（JSON，每个参数单独一项）");ui.add(TextEdit::multiline(&mut self.args_json).font(egui::TextStyle::Monospace).desired_rows(4).desired_width(f32::INFINITY));ui.label(RichText::new("例如 [\"E:/my-cli/dist/main.mjs\"] 配合 node；或使用已构建的 Rust 可执行文件。不会自动安装依赖或修改源码。").small().color(MUTED));ui.checkbox(&mut profile.enabled,"启用");if ui.button("保存配置").clicked(){match serde_json::from_str::<Vec<String>>(&self.args_json){Ok(args)=>{profile.args=args;profile.required_paths.clear();saved=true;},Err(e)=>self.notice=format!("参数 JSON 无效：{e}")}}}
            });
            self.profile_open = open && !saved;
            if saved {
                self.scan_clis();
            }
        }
        if let Some(pending) = self.close_editor.clone() {
            let mut decision = None;
            egui::Modal::new(egui::Id::new("unsaved-editor-dialog")).show(ctx, |ui| {
                ui.heading("文件尚未保存");
                ui.label(pending.path.display().to_string());
                ui.label("关闭会丢弃这个文件的未保存编辑。");
                ui.horizontal(|ui| {
                    if ui.button("保留编辑").clicked() {
                        decision = Some(false);
                    }
                    if ui.button("丢弃并关闭").clicked() {
                        decision = Some(true);
                    }
                });
            });
            if let Some(discard) = decision {
                self.finish_close_editor(discard);
            }
        }
        if self.pending_root.is_some() {
            let mut decision = None;
            egui::Modal::new(egui::Id::new("switch-workspace-dialog")).show(ctx, |ui| {
                ui.heading("切换工作区");
                ui.label("当前文件有未保存编辑。原有终端会保留在它们各自的项目目录。");
                ui.horizontal(|ui| {
                    if ui.button("返回保存").clicked() {
                        decision = Some(false);
                    }
                    if ui.button("丢弃编辑并切换").clicked() {
                        decision = Some(true);
                    }
                });
            });
            if let Some(discard) = decision {
                let path = self.pending_root.take().unwrap();
                if discard {
                    self.apply_root(path);
                }
            }
        }
        if let Some(index) = self.close_terminal {
            let mut decision = None;
            egui::Modal::new(egui::Id::new("close-terminal-dialog")).show(ctx, |ui| {
                ui.heading("关闭终端会话");
                ui.label("此终端中的进程及其子进程将停止。");
                ui.horizontal(|ui| {
                    if ui.button("继续运行").clicked() {
                        decision = Some(false);
                    }
                    if ui.button("停止并关闭").clicked() {
                        decision = Some(true);
                    }
                });
            });
            if let Some(close) = decision {
                self.close_terminal = None;
                if close && index < self.terminals.len() {
                    self.terminals.remove(index);
                    self.active_terminal = self
                        .active_terminal
                        .min(self.terminals.len().saturating_sub(1));
                }
            }
        }
    }
}

fn terminal_status_label(status: &terminal::TerminalStatus) -> String {
    match status {
        terminal::TerminalStatus::Running => "运行中".into(),
        terminal::TerminalStatus::Exited { code } => format!("已退出 {code}"),
        terminal::TerminalStatus::Stopped => "已停止".into(),
        terminal::TerminalStatus::Failed(error) => format!("失败：{}", truncate(error, 24)),
    }
}

fn cli_identity(id: &str) -> (&'static str, Color32, &'static str) {
    if id.contains("codex") {
        (">_", ACCENT, "OpenAI · 编码与推理")
    } else if id.contains("claude") {
        (
            "✳",
            Color32::from_rgb(232, 177, 147),
            "Anthropic · 代码协作",
        )
    } else if id.contains("kimi") {
        (
            "K",
            Color32::from_rgb(169, 180, 246),
            "Moonshot · 项目与工具",
        )
    } else if id.contains("deepseek") {
        ("D", Color32::from_rgb(139, 186, 245), "DeepSeek · Harness")
    } else if id == "wonderland" {
        ("W", ACCENT, "多模型 · API 与订阅")
    } else {
        ("/", MUTED, "自定义命令行程序")
    }
}

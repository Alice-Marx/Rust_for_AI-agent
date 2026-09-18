//! Local, model-independent workspace services for the desktop.
//!
//! Project detection only reads directory names. Git inspection disables external
//! diff drivers, text conversion, fsmonitor, hooks and optional lock writes. These
//! helpers guard normal desktop editing; they are not an OS security sandbox.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::{Duration, UNIX_EPOCH},
};
use tokio::io::AsyncReadExt;

pub const MAX_EDITOR_BYTES: usize = 1_048_576;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_GIT_OUTPUT_BYTES: usize = 1_048_576;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const HIDDEN_DIRECTORIES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub relative_path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceFile {
    pub relative_path: PathBuf,
    pub content: String,
    /// Includes content and modification time. Supply this exact value on save.
    pub revision: String,
    pub language: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectLanguage {
    pub language: String,
    pub toolchain: String,
    pub manifests: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitEntry {
    pub path: PathBuf,
    pub index_status: char,
    pub worktree_status: char,
    pub original_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitStatus {
    /// Branch, tracking and ahead/behind information reported by Git.
    pub branch: String,
    pub entries: Vec<GitEntry>,
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize().context("工作区目录不存在或无法访问")?;
    ensure!(root.is_dir(), "工作区必须是目录");
    Ok(root)
}

fn checked_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(
        !relative.is_absolute()
            && relative.components().all(|part| {
                !matches!(
                    part,
                    Component::ParentDir | Component::Prefix(_) | Component::RootDir
                )
            }),
        "请使用工作区内的相对路径，不能包含 .."
    );
    let path = root
        .join(relative)
        .canonicalize()
        .context("路径不存在或无法访问")?;
    ensure!(
        path.starts_with(root),
        "路径通过符号链接或目录联接指向工作区外部"
    );
    Ok(path)
}

/// Read a single directory on demand. External symlinks/junctions are omitted.
pub fn list_directory(root: &Path, relative: &Path) -> Result<Vec<WorkspaceEntry>> {
    let root = canonical_root(root)?;
    let directory = checked_path(&root, relative)?;
    ensure!(directory.is_dir(), "所选路径不是目录");
    let mut entries = Vec::new();
    for (index, result) in fs::read_dir(directory)?.enumerate() {
        ensure!(
            index < MAX_DIRECTORY_ENTRIES,
            "目录包含过多条目，请打开更小的目录"
        );
        let entry = result?;
        let name = entry.file_name();
        let path = match entry.path().canonicalize() {
            Ok(path) if path.starts_with(&root) => path,
            _ => continue,
        };
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_dir() && !metadata.is_file() {
            continue;
        }
        let name = name.to_string_lossy().into_owned();
        if metadata.is_dir() && HIDDEN_DIRECTORIES.contains(&name.as_str()) {
            continue;
        }
        // Preserve the displayed route through an internal symlink. It is checked
        // again when opened, rather than trusting this directory snapshot.
        entries.push(WorkspaceEntry {
            relative_path: relative.join(entry.file_name()),
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

fn read_snapshot(path: &Path) -> Result<(String, String)> {
    // Check before opening so a manually entered FIFO/device path cannot block
    // the editor waiting for a producer. Verify the open handle again below.
    ensure!(fs::metadata(path)?.is_file(), "仅支持打开普通文件");
    let file = File::open(path).context("无法读取文件")?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "仅支持打开普通文件");
    ensure!(
        before.len() <= MAX_EDITOR_BYTES as u64,
        "文件超过 1 MiB，请使用外部编辑器"
    );
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&file)
        .take(MAX_EDITOR_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_EDITOR_BYTES,
        "文件超过 1 MiB，请使用外部编辑器"
    );
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len() && before.modified().ok() == after.modified().ok(),
        "文件正在被其他程序修改，请重新打开"
    );
    ensure!(!bytes.contains(&0), "二进制文件不能在文本编辑器中打开");
    let mut digest = Sha256::new();
    digest.update(&bytes);
    if let Ok(modified) = after.modified() {
        let time = modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        digest.update(time.to_le_bytes());
    }
    let revision = format!("{:x}", digest.finalize());
    let content =
        String::from_utf8(bytes).context("文件不是 UTF-8 文本，请使用外部编辑器转换编码")?;
    Ok((content, revision))
}

pub fn open_file(root: &Path, relative: &Path) -> Result<WorkspaceFile> {
    let root = canonical_root(root)?;
    let path = checked_path(&root, relative)?;
    let (content, revision) = read_snapshot(&path)?;
    Ok(WorkspaceFile {
        relative_path: relative.to_path_buf(),
        content,
        revision,
        language: language_for_path(relative).into(),
    })
}

struct TemporarySave(PathBuf);
impl Drop for TemporarySave {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            temporary.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error()).context("无法原子替换文件，原文件保持不变");
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> Result<()> {
    fs::rename(temporary, destination).context("无法原子替换文件")
}

/// Explicitly save an existing file; refuse stale edits instead of overwriting.
/// Atomic replacement preserves permissions and never deletes the original as a
/// fallback. Like conventional editors this is not a transactional lock against
/// arbitrary concurrent writers between the last check and the final rename.
pub fn save_file(
    root: &Path,
    relative: &Path,
    expected_revision: &str,
    content: &str,
) -> Result<WorkspaceFile> {
    ensure!(
        content.len() <= MAX_EDITOR_BYTES,
        "文件超过 1 MiB，请使用外部编辑器"
    );
    ensure!(
        !content.as_bytes().contains(&0),
        "不能保存含有空字符的二进制内容"
    );
    let root = canonical_root(root)?;
    let path = checked_path(&root, relative)?;
    let (_, revision) = read_snapshot(&path)?;
    ensure!(
        revision == expected_revision,
        "文件已被其他程序修改；请重新打开并合并修改，尚未覆盖磁盘内容"
    );
    let permissions = fs::metadata(&path)?.permissions();
    ensure!(!permissions.readonly(), "文件为只读，尚未保存");
    let parent = path.parent().context("文件没有父目录")?;
    let temporary =
        TemporarySave(parent.join(format!(".wonderland-save-{}.tmp", uuid::Uuid::new_v4())));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary.0)
        .context("无法创建临时保存文件")?;
    output.write_all(content.as_bytes())?;
    output.set_permissions(permissions)?;
    output.sync_all()?;
    drop(output);
    ensure!(
        checked_path(&root, relative)? == path,
        "文件路径发生变化，请重新打开"
    );
    let (_, current_revision) = read_snapshot(&path)?;
    ensure!(
        current_revision == expected_revision,
        "保存前检测到外部修改，尚未覆盖磁盘内容"
    );
    replace_file(&temporary.0, &path)?;
    open_file(&root, relative)
}

pub fn language_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "Rust",
        "py" | "pyi" => "Python",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        "go" => "Go",
        "cs" => "C#",
        "fs" | "fsx" => "F#",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "c" | "h" => "C",
        "cpp" | "cc" | "cxx" | "hpp" => "C++",
        "rb" => "Ruby",
        "php" => "PHP",
        "swift" => "Swift",
        "dart" => "Dart",
        "html" | "htm" => "HTML",
        "css" | "scss" => "CSS",
        "json" | "jsonc" => "JSON",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "md" | "mdx" => "Markdown",
        "sh" | "bash" | "zsh" => "Shell",
        "ps1" | "psm1" => "PowerShell",
        "sql" => "SQL",
        "xml" | "csproj" | "fsproj" => "XML",
        _ => "Text",
    }
}

/// Detect common toolchains from manifest names. Does not import modules, run
/// package scripts, probe executables, or evaluate any project configuration.
pub fn detect_project(root: &Path) -> Result<Vec<ProjectLanguage>> {
    let entries = list_directory(root, Path::new(""))?;
    let names: Vec<&str> = entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| entry.name.as_str())
        .collect();
    let definitions: &[(&str, &str, &[&str])] = &[
        ("Rust", "cargo", &["Cargo.toml"]),
        (
            "Python",
            "python / uv / pip",
            &[
                "pyproject.toml",
                "requirements.txt",
                "Pipfile",
                "setup.py",
                "setup.cfg",
            ],
        ),
        ("JavaScript", "node / npm / pnpm / yarn", &["package.json"]),
        (
            "TypeScript",
            "node / tsc",
            &[
                "tsconfig.json",
                "tsconfig.base.json",
                "deno.json",
                "deno.jsonc",
            ],
        ),
        ("Go", "go", &["go.mod", "go.work"]),
        (
            "Java / Kotlin",
            "maven / gradle",
            &[
                "pom.xml",
                "build.gradle",
                "build.gradle.kts",
                "settings.gradle",
                "settings.gradle.kts",
            ],
        ),
        (
            "C / C++",
            "cmake / make / meson",
            &["CMakeLists.txt", "Makefile", "meson.build"],
        ),
        ("Ruby", "ruby / bundler", &["Gemfile"]),
        ("PHP", "php / composer", &["composer.json"]),
        ("Swift", "swift", &["Package.swift"]),
        ("Dart", "dart / flutter", &["pubspec.yaml"]),
    ];
    let mut languages = Vec::new();
    for (language, toolchain, manifests) in definitions {
        let found: Vec<String> = manifests
            .iter()
            .filter(|manifest| names.contains(manifest))
            .map(|value| (*value).to_string())
            .collect();
        if !found.is_empty() {
            languages.push(ProjectLanguage {
                language: (*language).into(),
                toolchain: (*toolchain).into(),
                manifests: found,
            });
        }
    }
    for (extension, language) in [
        ("csproj", "C#"),
        ("fsproj", "F#"),
        ("sln", ".NET"),
        ("slnx", ".NET"),
    ] {
        let found: Vec<String> = names
            .iter()
            .filter(|name| {
                Path::new(name)
                    .extension()
                    .is_some_and(|value| value == extension)
            })
            .map(|name| (*name).into())
            .collect();
        if !found.is_empty() {
            languages.push(ProjectLanguage {
                language: language.into(),
                toolchain: "dotnet".into(),
                manifests: found,
            });
        }
    }
    Ok(languages)
}

async fn read_git_pipe<R: tokio::io::AsyncRead + Unpin>(
    stream: R,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= limit,
        "Git 输出超过显示上限，请选择单个文件查看"
    );
    Ok(bytes)
}

fn git_executable(root: &Path) -> Result<PathBuf> {
    // Avoid Windows' implicit current-directory executable search: opening an
    // untrusted project that contains git.exe must not execute that file.
    let search = std::env::var_os("PATH").context("PATH 未配置，无法查找 Git")?;
    let executable = if cfg!(windows) { "git.exe" } else { "git" };
    for directory in std::env::split_paths(&search).filter(|directory| directory.is_absolute()) {
        if let Ok(candidate) = directory.join(executable).canonicalize() {
            if candidate.is_file() && !candidate.starts_with(root) {
                return Ok(candidate);
            }
        }
    }
    bail!("无法找到工作区外的 Git，请安装 Git 并加入 PATH")
}

fn base_git_command(root: &Path) -> Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new(git_executable(root)?);
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "submodule.recurse=false",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(variable);
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    Ok(command)
}

async fn capture_git(
    mut command: tokio::process::Command,
    empty_config_allowed: bool,
) -> Result<Vec<u8>> {
    let mut child = command
        .spawn()
        .context("无法启动 Git，请安装 Git 并加入 PATH")?;
    let stdout = child.stdout.take().context("无法读取 Git 输出")?;
    let stderr = child.stderr.take().context("无法读取 Git 错误")?;
    let operation = async {
        tokio::try_join!(
            async { child.wait().await.context("Git 进程异常") },
            read_git_pipe(stdout, MAX_GIT_OUTPUT_BYTES),
            read_git_pipe(stderr, 65_536)
        )
    };
    let (status, stdout, stderr) = tokio::time::timeout(GIT_TIMEOUT, operation)
        .await
        .context("读取 Git 超时，请缩小工作区或稍后重试")??;
    if !status.success() && !(empty_config_allowed && status.code() == Some(1) && stderr.is_empty())
    {
        bail!("Git: {}", String::from_utf8_lossy(&stderr).trim());
    }
    Ok(stdout)
}

async fn git_command(root: &Path, args: &[std::ffi::OsString]) -> Result<Vec<u8>> {
    // `git diff` and even `git status` may apply clean/process filters from
    // .gitattributes. Discover only their configuration *names* and disable them
    // for these commands. No configuration values or credentials are captured.
    let mut config = base_git_command(root)?;
    config.args([
        "config",
        "--null",
        "--name-only",
        "--get-regexp",
        "^filter[.].*[.](clean|process)$",
    ]);
    let names = capture_git(config, true).await?;
    let mut command = base_git_command(root)?;
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::str::from_utf8(name).context("Git 过滤器配置名称不是 UTF-8")?;
        command.arg("-c").arg(format!("{name}="));
    }
    command.args(args);
    capture_git(command, false).await
}

fn git_args(values: &[&str]) -> Vec<std::ffi::OsString> {
    values
        .iter()
        .map(|value| std::ffi::OsString::from(*value))
        .collect()
}

fn parse_git_status(bytes: &[u8], prefix: &str) -> Result<GitStatus> {
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    let mut branch = String::new();
    let mut entries = Vec::new();
    while let Some(record) = records.next() {
        let record =
            std::str::from_utf8(record).context("Git 文件名不是 UTF-8，无法在桌面中显示")?;
        if let Some(value) = record.strip_prefix("## ") {
            branch = value.to_string();
            continue;
        }
        ensure!(
            record.len() >= 4 && record.as_bytes()[2] == b' ',
            "无法解析 Git 状态"
        );
        let index_status = record.as_bytes()[0] as char;
        let worktree_status = record.as_bytes()[1] as char;
        let original_path =
            if matches!(index_status, 'R' | 'C') || matches!(worktree_status, 'R' | 'C') {
                let original = records.next().context("Git 重命名状态缺少原路径")?;
                let original = std::str::from_utf8(original).context("Git 文件名不是 UTF-8")?;
                original.strip_prefix(prefix).map(PathBuf::from)
            } else {
                None
            };
        if let Some(path) = record[3..].strip_prefix(prefix) {
            entries.push(GitEntry {
                path: PathBuf::from(path),
                index_status,
                worktree_status,
                original_path,
            });
        }
    }
    Ok(GitStatus { branch, entries })
}

pub async fn git_status(root: &Path) -> Result<GitStatus> {
    let root = canonical_root(root)?;
    let prefix = git_command(&root, &git_args(&["rev-parse", "--show-prefix"])).await?;
    let prefix = std::str::from_utf8(&prefix)
        .context("Git 工作区路径不是 UTF-8")?
        .trim_end_matches(['\n', '\r']);
    let output = git_command(
        &root,
        &git_args(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--",
            ".",
        ]),
    )
    .await?;
    parse_git_status(&output, prefix)
}

/// Review staged/unstaged changes and bounded previews of individual untracked
/// text files. External diff/textconv programs are disabled. A deleted path is
/// permitted only after checking its existing parent.
pub async fn git_diff(root: &Path, relative: Option<&Path>) -> Result<String> {
    let root = canonical_root(root)?;
    let path = relative.unwrap_or_else(|| Path::new("."));
    if root.join(path).exists() {
        checked_path(&root, path)?;
    } else {
        ensure!(
            !path.is_absolute()
                && !path.components().any(|part| matches!(
                    part,
                    Component::ParentDir | Component::Prefix(_) | Component::RootDir
                )),
            "差异路径必须位于工作区内"
        );
        let mut parent = path.parent().unwrap_or_else(|| Path::new(""));
        while !root.join(parent).exists() {
            parent = parent.parent().context("差异文件不属于工作区")?;
        }
        checked_path(&root, parent)?;
    }
    let mut unstaged = git_args(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=all",
        "--no-color",
        "--",
    ]);
    unstaged.push(path.as_os_str().to_owned());
    let mut staged = git_args(&[
        "diff",
        "--cached",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=all",
        "--no-color",
        "--",
    ]);
    staged.push(path.as_os_str().to_owned());
    let (unstaged, staged) =
        tokio::try_join!(git_command(&root, &unstaged), git_command(&root, &staged))?;
    let mut output = String::new();
    if !unstaged.is_empty() {
        output.push_str("--- 工作区修改 / Unstaged ---\n");
        output.push_str(&String::from_utf8_lossy(&unstaged));
    }
    if !staged.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("--- 暂存区修改 / Staged ---\n");
        output.push_str(&String::from_utf8_lossy(&staged));
    }
    if output.is_empty() && relative.is_some() && root.join(path).is_file() {
        let mut untracked = git_args(&["ls-files", "--others", "--exclude-standard", "-z", "--"]);
        untracked.push(path.as_os_str().to_owned());
        if !git_command(&root, &untracked).await?.is_empty() {
            let file = open_file(&root, path)?;
            output.push_str(&format!(
                "--- 新增文件预览 / Untracked: {} ---\n",
                path.display()
            ));
            if file.content.is_empty() {
                output.push_str("（空文件）\n");
            } else {
                for line in file.content.lines() {
                    output.push('+');
                    output.push_str(line);
                    output.push('\n');
                }
                if !file.content.ends_with('\n') {
                    output.push_str("\\ No newline at end of file\n");
                }
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_is_sorted_and_filters_generated_content() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["Zeta", "alpha", "target", ".git", "node_modules", ".venv"] {
            fs::create_dir(temp.path().join(name)).unwrap();
        }
        for name in ["z.rs", "B.py", "a.ts"] {
            fs::write(temp.path().join(name), "text").unwrap();
        }
        let entries = list_directory(temp.path(), Path::new("")).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "Zeta", "a.ts", "B.py", "z.rs"]
        );
        assert!(entries[0].is_dir);
        assert!(!entries[2].is_dir);
        assert!(list_directory(temp.path(), Path::new("..")).is_err());
        assert!(list_directory(temp.path(), temp.path()).is_err());
    }

    #[test]
    fn file_editor_refuses_binary_and_large_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("binary.bin"), [1, 0, 3]).unwrap();
        fs::write(temp.path().join("gbk.txt"), [0xff, 0xfe]).unwrap();
        fs::write(
            temp.path().join("large.txt"),
            vec![b'a'; MAX_EDITOR_BYTES + 1],
        )
        .unwrap();
        for name in ["binary.bin", "gbk.txt", "large.txt"] {
            assert!(open_file(temp.path(), Path::new(name)).is_err());
        }
    }

    #[test]
    fn save_is_explicit_and_preserves_external_edits() {
        let temp = tempfile::tempdir().unwrap();
        let path = Path::new("hello.rs");
        fs::write(temp.path().join(path), "fn main() {}\n").unwrap();
        let first = open_file(temp.path(), path).unwrap();
        assert_eq!(first.language, "Rust");
        let saved = save_file(
            temp.path(),
            path,
            &first.revision,
            "fn main() { /* 修改 */ }\n",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(path)).unwrap(),
            saved.content
        );
        assert_ne!(saved.revision, first.revision);
        fs::write(temp.path().join(path), "external edit\n").unwrap();
        assert!(save_file(temp.path(), path, &saved.revision, "overwrite").is_err());
        assert_eq!(
            fs::read_to_string(temp.path().join(path)).unwrap(),
            "external edit\n"
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn manifests_detect_languages_without_evaluation() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "Cargo.toml",
            "package.json",
            "tsconfig.json",
            "pyproject.toml",
            "go.mod",
            "App.csproj",
            "pom.xml",
        ] {
            fs::write(
                temp.path().join(name),
                "not valid manifest contents; must not execute",
            )
            .unwrap();
        }
        let languages = detect_project(temp.path()).unwrap();
        for name in [
            "Rust",
            "JavaScript",
            "TypeScript",
            "Python",
            "Go",
            "C#",
            "Java / Kotlin",
        ] {
            assert!(languages.iter().any(|item| item.language == name));
        }
    }

    #[test]
    fn git_status_parser_handles_renames_spaces_and_subdirectory() {
        let status = parse_git_status(b"## main...origin/main [ahead 1]\0R  pkg/new name.rs\0pkg/old name.rs\0 M other.rs\0?? pkg/new\nfile.py\0", "pkg/").unwrap();
        assert_eq!(status.entries.len(), 2);
        assert_eq!(status.entries[0].path, Path::new("new name.rs"));
        assert_eq!(
            status.entries[0].original_path.as_deref(),
            Some(Path::new("old name.rs"))
        );
        assert_eq!(status.entries[1].path, Path::new("new\nfile.py"));
    }

    #[cfg(unix)]
    #[test]
    fn external_symlink_is_not_traversed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(list_directory(root.path(), Path::new(""))
            .unwrap()
            .is_empty());
        assert!(open_file(root.path(), Path::new("escape/secret")).is_err());
        assert!(list_directory(root.path(), Path::new("escape")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn external_junction_is_not_traversed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        let link = root.path().join("escape");
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(list_directory(root.path(), Path::new(""))
            .unwrap()
            .is_empty());
        assert!(open_file(root.path(), Path::new("escape/secret")).is_err());
        assert!(list_directory(root.path(), Path::new("escape")).is_err());
        assert!(save_file(
            root.path(),
            Path::new("escape/secret"),
            "irrelevant",
            "overwrite"
        )
        .is_err());
        fs::remove_dir(link).unwrap();
        assert_eq!(
            fs::read_to_string(outside.path().join("secret")).unwrap(),
            "secret"
        );
    }

    #[tokio::test]
    async fn git_review_reads_staged_unstaged_and_deleted_files() {
        let temp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
        };
        let Ok(version) = run(&["--version"]) else {
            return;
        };
        if !version.status.success() {
            return;
        }
        assert!(run(&["init", "-q"]).unwrap().status.success());
        fs::write(temp.path().join("file.txt"), "first\n").unwrap();
        assert!(run(&["add", "file.txt"]).unwrap().status.success());
        fs::write(temp.path().join("file.txt"), "second\n").unwrap();
        let status = git_status(temp.path()).await.unwrap();
        assert_eq!(status.entries.len(), 1);
        assert_eq!(status.entries[0].index_status, 'A');
        assert_eq!(status.entries[0].worktree_status, 'M');
        let diff = git_diff(temp.path(), Some(Path::new("file.txt")))
            .await
            .unwrap();
        assert!(diff.contains("+second"));
        assert!(diff.contains("+first"));
        fs::remove_file(temp.path().join("file.txt")).unwrap();
        assert!(git_diff(temp.path(), Some(Path::new("file.txt")))
            .await
            .unwrap()
            .contains("-first"));
        assert!(git_diff(temp.path(), Some(Path::new("../outside")))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn git_review_does_not_execute_repository_filters() {
        let temp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
        };
        let Ok(version) = run(&["--version"]) else {
            return;
        };
        if !version.status.success() {
            return;
        }
        assert!(run(&["init", "-q"]).unwrap().status.success());
        fs::write(temp.path().join("file.txt"), "first\n").unwrap();
        assert!(run(&["add", "file.txt"]).unwrap().status.success());
        fs::write(temp.path().join(".gitattributes"), "*.txt filter=probe\n").unwrap();
        fs::write(temp.path().join("file.txt"), "second\n").unwrap();
        assert!(run(&[
            "config",
            "filter.probe.clean",
            "echo executed > filter-ran; cat"
        ])
        .unwrap()
        .status
        .success());
        assert!(run(&[
            "config",
            "filter.probe.process",
            "echo executed > process-ran; exit 1"
        ])
        .unwrap()
        .status
        .success());
        git_status(temp.path()).await.unwrap();
        assert!(git_diff(temp.path(), Some(Path::new("file.txt")))
            .await
            .unwrap()
            .contains("+second"));
        assert!(!temp.path().join("filter-ran").exists());
        assert!(!temp.path().join("process-ran").exists());
    }

    #[tokio::test]
    async fn git_review_includes_individual_untracked_files_and_bounds_the_preview() {
        let temp = tempfile::tempdir().unwrap();
        let Ok(output) = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(temp.path())
            .output()
        else {
            return;
        };
        assert!(output.status.success());
        fs::create_dir(temp.path().join("new directory")).unwrap();
        let path = Path::new("new directory/file.rs");
        fs::write(temp.path().join(path), "fn main() {}\n// 新增").unwrap();
        fs::write(temp.path().join("empty.txt"), "").unwrap();
        let status = git_status(temp.path()).await.unwrap();
        assert!(status
            .entries
            .iter()
            .any(|entry| entry.path == path && entry.index_status == '?'));
        let preview = git_diff(temp.path(), Some(path)).await.unwrap();
        assert!(preview.contains("+fn main() {}\n+// 新增\n"));
        assert!(preview.contains("No newline at end of file"));
        assert!(git_diff(temp.path(), Some(Path::new("empty.txt")))
            .await
            .unwrap()
            .contains("空文件"));
        fs::write(temp.path().join(path), [0, 1, 2]).unwrap();
        assert!(git_diff(temp.path(), Some(path)).await.is_err());
        fs::write(temp.path().join(path), vec![b'a'; MAX_EDITOR_BYTES + 1]).unwrap();
        assert!(git_diff(temp.path(), Some(path)).await.is_err());
    }
}

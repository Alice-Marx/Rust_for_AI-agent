//! Detached Git delivery workspaces. This is isolation of edits, not an OS sandbox.
//! Source branches are never checked out, reset, staged or committed by this module.
use crate::team_store::normalize_write_path;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_GIT_BYTES: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunWorkspace {
    pub run_id: String,
    pub source: PathBuf,
    pub source_revision: String,
    pub root: PathBuf,
    pub integration: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttemptWorkspace {
    pub run_id: String,
    pub node_id: String,
    pub attempt: u32,
    pub path: PathBuf,
    pub base_revision: String,
    pub integration: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeArtifact {
    pub run_id: String,
    pub node_id: String,
    pub attempt: u32,
    pub workspace: PathBuf,
    pub base_revision: String,
    pub commit_revision: String,
    pub changed_paths: Vec<String>,
    pub patch_path: PathBuf,
    pub patch_sha256: String,
    pub test_changes: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrationResult {
    pub revision: String,
    pub changed_paths: Vec<String>,
}

pub fn prepare_run(source: &Path, runs_root: &Path, run_id: &str) -> Result<RunWorkspace> {
    safe_id(run_id)?;
    let source = source
        .canonicalize()
        .context("source repository is missing")?;
    ensure!(source.is_dir(), "source is not a directory");
    let git_root =
        PathBuf::from(git_text(&source, &["rev-parse", "--show-toplevel"])?).canonicalize()?;
    ensure!(
        git_root == source,
        "team source must be the Git repository root"
    );
    assert_clean(&source)?;
    reject_worktree_links(&source)?;
    reject_unsafe_tree(&source, "HEAD")?;
    reject_custom_filters(&source)?;
    let source_revision = current_revision(&source)?;
    fs::create_dir_all(runs_root)?;
    let runs_root = runs_root.canonicalize()?;
    ensure!(
        !runs_root.starts_with(&source),
        "isolated team workspaces must be outside the user's checkout"
    );
    let root = runs_root.join(run_id);
    fs::create_dir(&root).context("run workspace already exists")?;
    let integration = root.join("integration");
    git(
        &source,
        &[
            "worktree",
            "add",
            "--detach",
            "--",
            &path_str(&integration)?,
            &source_revision,
        ],
    )?;
    let run = RunWorkspace {
        run_id: run_id.into(),
        source,
        source_revision,
        root: root.canonicalize()?,
        integration: integration.canonicalize()?,
    };
    write_new(
        &run.root.join("run.json"),
        &serde_json::to_vec_pretty(&run)?,
    )?;
    assert_source_unchanged(&run)?;
    Ok(run)
}

pub fn prepare_attempt(
    run: &RunWorkspace,
    node_id: &str,
    attempt: u32,
) -> Result<AttemptWorkspace> {
    validate_run(run)?;
    safe_id(node_id)?;
    ensure!(
        (1..=5).contains(&attempt),
        "attempt number must be between 1 and 5"
    );
    let _lock = IntegrationLock::acquire(&run.root)?;
    assert_source_unchanged(run)?;
    assert_clean(&run.integration)?;
    assert_detached(&run.integration)?;
    let base_revision = current_revision(&run.integration)?;
    let attempts = run.root.join("attempts");
    fs::create_dir_all(&attempts)?;
    let path = attempts.join(format!("{node_id}-{attempt}"));
    ensure!(!path.exists(), "attempt workspace already exists");
    git(
        &run.integration,
        &[
            "worktree",
            "add",
            "--detach",
            "--",
            &path_str(&path)?,
            &base_revision,
        ],
    )?;
    let workspace = AttemptWorkspace {
        run_id: run.run_id.clone(),
        node_id: node_id.into(),
        attempt,
        path: path.canonicalize()?,
        base_revision,
        integration: run.integration.clone(),
    };
    write_new(
        &attempt_metadata(&run.root, node_id, attempt),
        &serde_json::to_vec_pretty(&workspace)?,
    )?;
    Ok(workspace)
}

pub fn capture_attempt(
    attempt: &AttemptWorkspace,
    write_paths: &[String],
) -> Result<ChangeArtifact> {
    let run = validate_attempt(attempt)?;
    assert_source_unchanged(&run)?;
    assert_detached(&attempt.path)?;
    reject_custom_filters(&attempt.path)?;
    reject_worktree_links(&attempt.path)?;
    let scopes = write_paths
        .iter()
        .map(|p| normalize_write_path(p))
        .collect::<Result<Vec<_>>>()?;
    // Staging touches only this registered detached attempt worktree. All changes,
    // including agent-created commits, are compared with its pinned starting revision.
    git(&attempt.path, &["add", "--all", "--", "."])?;
    reject_unsafe_tree(&attempt.path, "")?;
    let changed_paths = changed(&attempt.path, &attempt.base_revision, None)?;
    ensure!(
        changed_paths.iter().all(|path| in_scope(path, &scopes)),
        "attempt wrote outside its declared paths: {}",
        changed_paths
            .iter()
            .filter(|p| !in_scope(p, &scopes))
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    let baseline = tracked_paths(&attempt.path, &attempt.base_revision)?;
    let test_changes: Vec<String> = changed_paths
        .iter()
        .filter(|path| protected_test_path(path))
        .cloned()
        .collect();
    ensure!(
        !test_changes.iter().any(|path| baseline.contains(path)),
        "attempt changed existing tests or test configuration; automatic acceptance is blocked: {}",
        test_changes.join(", ")
    );
    preserve_inline_tests(
        &attempt.path,
        &attempt.base_revision,
        &changed_paths,
        &baseline,
    )?;
    let current = current_revision(&attempt.path)?;
    let tree = git_text(&attempt.path, &["write-tree"])?;
    let commit_revision = commit_tree(
        &attempt.path,
        &tree,
        &current,
        &format!(
            "Wonderland node {} attempt {}",
            attempt.node_id, attempt.attempt
        ),
    )?;
    git(
        &attempt.path,
        &["update-ref", "HEAD", &commit_revision, &current],
    )?;
    let patch = git(
        &attempt.path,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            &attempt.base_revision,
            &commit_revision,
            "--",
        ],
    )?;
    let artifacts = run.root.join("artifacts");
    fs::create_dir_all(&artifacts)?;
    let patch_path = artifacts.join(format!("{}-{}.patch", attempt.node_id, attempt.attempt));
    write_new(&patch_path, &patch)?;
    let artifact = ChangeArtifact {
        run_id: attempt.run_id.clone(),
        node_id: attempt.node_id.clone(),
        attempt: attempt.attempt,
        workspace: attempt.path.clone(),
        base_revision: attempt.base_revision.clone(),
        commit_revision,
        changed_paths,
        patch_path,
        patch_sha256: hash(&patch),
        test_changes,
    };
    write_new(
        &artifacts.join(format!("{}-{}.json", attempt.node_id, attempt.attempt)),
        &serde_json::to_vec_pretty(&artifact)?,
    )?;
    assert_source_unchanged(&run)?;
    Ok(artifact)
}

pub fn integrate_attempt(
    run: &RunWorkspace,
    artifact: &ChangeArtifact,
) -> Result<IntegrationResult> {
    validate_run(run)?;
    safe_id(&artifact.node_id)?;
    ensure!(
        artifact.run_id == run.run_id,
        "artifact belongs to another run"
    );
    let expected: AttemptWorkspace = read_json(&attempt_metadata(
        &run.root,
        &artifact.node_id,
        artifact.attempt,
    ))?;
    validate_attempt(&expected)?;
    ensure!(
        artifact.workspace == expected.path && artifact.base_revision == expected.base_revision,
        "artifact workspace or base revision changed"
    );
    let canonical_artifact: ChangeArtifact = read_json(
        &run.root
            .join("artifacts")
            .join(format!("{}-{}.json", artifact.node_id, artifact.attempt)),
    )?;
    ensure!(
        &canonical_artifact == artifact,
        "artifact differs from captured manifest"
    );
    let expected_patch = run
        .root
        .join("artifacts")
        .join(format!("{}-{}.patch", artifact.node_id, artifact.attempt));
    ensure!(
        artifact.patch_path == expected_patch,
        "unexpected patch path"
    );
    reject_link(&expected_patch)?;
    let patch = read_bounded(&expected_patch)?;
    ensure!(hash(&patch) == artifact.patch_sha256, "patch hash mismatch");
    let expected_diff = git(
        &expected.path,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            &artifact.base_revision,
            &artifact.commit_revision,
            "--",
        ],
    )?;
    ensure!(
        patch == expected_diff,
        "patch does not match its Git commit"
    );
    ensure!(
        changed(
            &expected.path,
            &artifact.base_revision,
            Some(&artifact.commit_revision)
        )? == artifact.changed_paths,
        "artifact changed-path manifest mismatch"
    );
    let _lock = IntegrationLock::acquire(&run.root)?;
    assert_source_unchanged(run)?;
    assert_clean(&run.integration)?;
    assert_detached(&run.integration)?;
    let previous = current_revision(&run.integration)?;
    if patch.is_empty() {
        return Ok(IntegrationResult {
            revision: previous,
            changed_paths: vec![],
        });
    }
    git(
        &run.integration,
        &[
            "apply",
            "--check",
            "--index",
            "--",
            &path_str(&artifact.patch_path)?,
        ],
    )
    .context("node changes conflict with the integrated result")?;
    let result = (|| {
        git(
            &run.integration,
            &["apply", "--index", "--", &path_str(&artifact.patch_path)?],
        )?;
        let tree = git_text(&run.integration, &["write-tree"])?;
        let revision = commit_tree(
            &run.integration,
            &tree,
            &previous,
            &format!(
                "Integrate {} attempt {}",
                artifact.node_id, artifact.attempt
            ),
        )?;
        git(
            &run.integration,
            &["update-ref", "HEAD", &revision, &previous],
        )?;
        assert_clean(&run.integration)?;
        Ok(IntegrationResult {
            revision,
            changed_paths: artifact.changed_paths.clone(),
        })
    })();
    if result.is_err() {
        // Rollback is restricted to the registered detached integration worktree.
        // Never reset a user checkout or an arbitrary artifact-supplied path.
        git(&run.integration, &["reset", "--hard", &previous])
            .context("failed to restore isolated integration workspace after integration error")?;
    }
    assert_source_unchanged(run)?;
    result
}

pub fn assert_source_unchanged(run: &RunWorkspace) -> Result<()> {
    ensure!(
        current_revision(&run.source)? == run.source_revision,
        "source HEAD changed during the team run"
    );
    assert_clean(&run.source)
}
pub fn current_revision(workspace: &Path) -> Result<String> {
    let revision = git_text(workspace, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    ensure!(
        matches!(revision.len(), 40 | 64) && revision.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Git revision"
    );
    Ok(revision)
}
pub fn assert_clean(workspace: &Path) -> Result<()> {
    let status = git(
        workspace,
        &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
    )?;
    ensure!(
        status.is_empty(),
        "repository must be clean, including untracked files: {}",
        String::from_utf8_lossy(&status)
            .replace('\0', "; ")
            .chars()
            .take(2000)
            .collect::<String>()
    );
    Ok(())
}

fn validate_run(run: &RunWorkspace) -> Result<()> {
    safe_id(&run.run_id)?;
    ensure!(
        run.root.is_absolute() && run.integration.is_absolute() && run.source.is_absolute(),
        "workspace paths must be absolute"
    );
    ensure!(
        run.root.canonicalize()? == run.root
            && run.integration == run.root.join("integration")
            && run.integration.canonicalize()? == run.integration,
        "invalid integration workspace location"
    );
    ensure!(
        !run.root.starts_with(&run.source),
        "run workspace overlaps source checkout"
    );
    let stored: RunWorkspace = read_json(&run.root.join("run.json"))?;
    ensure!(&stored == run, "run workspace metadata mismatch");
    assert_detached(&run.integration)?;
    Ok(())
}
fn validate_attempt(attempt: &AttemptWorkspace) -> Result<RunWorkspace> {
    safe_id(&attempt.run_id)?;
    safe_id(&attempt.node_id)?;
    ensure!((1..=5).contains(&attempt.attempt), "invalid attempt number");
    let root = attempt
        .integration
        .parent()
        .context("integration root missing")?;
    let run: RunWorkspace = read_json(&root.join("run.json"))?;
    validate_run(&run)?;
    ensure!(
        run.run_id == attempt.run_id && run.integration == attempt.integration,
        "attempt belongs to another run"
    );
    let expected = run
        .root
        .join("attempts")
        .join(format!("{}-{}", attempt.node_id, attempt.attempt));
    ensure!(
        attempt.path == expected && attempt.path.canonicalize()? == expected,
        "invalid attempt location"
    );
    let stored: AttemptWorkspace = read_json(&attempt_metadata(
        &run.root,
        &attempt.node_id,
        attempt.attempt,
    ))?;
    ensure!(&stored == attempt, "attempt metadata mismatch");
    let top = PathBuf::from(git_text(&attempt.path, &["rev-parse", "--show-toplevel"])?)
        .canonicalize()?;
    ensure!(top == attempt.path, "attempt is not its own Git worktree");
    Ok(run)
}
fn attempt_metadata(root: &Path, node: &str, number: u32) -> PathBuf {
    root.join(format!("attempt-{node}-{number}.json"))
}
fn assert_detached(path: &Path) -> Result<()> {
    let (code, _) = git_status(path, &["symbolic-ref", "--quiet", "HEAD"])?;
    ensure!(code == 1, "team workspaces must remain on detached HEAD");
    Ok(())
}
fn safe_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
        "invalid workspace identifier"
    );
    normalize_write_path(value)?;
    Ok(())
}
fn path_str(path: &Path) -> Result<String> {
    let text = path.to_str().context("workspace path must be UTF-8")?;
    // Git for Windows does not accept Rust's verbatim canonical paths as argv.
    // Keep canonical paths internally for identity checks, normalize only argv.
    #[cfg(windows)]
    {
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return Ok(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return Ok(rest.to_owned());
        }
    }
    Ok(text.to_owned())
}
fn in_scope(path: &str, scopes: &[String]) -> bool {
    scopes
        .iter()
        .any(|scope| path == scope || path.starts_with(&(scope.clone() + "/")))
}
fn protected_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or("");
    lower.split('/').any(|p| {
        matches!(
            p,
            "test" | "tests" | "__tests__" | "spec" | "specs" | ".github"
        )
    }) || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.starts_with("jest.config.")
        || name.starts_with("vitest.config.")
        || matches!(
            name,
            "pytest.ini"
                | "tox.ini"
                | "cargo.toml"
                | "package.json"
                | "pyproject.toml"
                | "makefile"
                | "justfile"
                | "build.rs"
                | "conftest.py"
                | ".gitattributes"
                | ".gitmodules"
        )
}
fn preserve_inline_tests(
    workspace: &Path,
    base: &str,
    changed: &[String],
    baseline: &HashSet<String>,
) -> Result<()> {
    // Rust commonly keeps unit tests beside implementation. Preserve the existing
    // test section while allowing edits before it. This is conservative text evidence,
    // not a claim that arbitrary program semantics can be proved here.
    for path in changed
        .iter()
        .filter(|p| p.ends_with(".rs") && baseline.contains(*p))
    {
        let original = git(workspace, &["cat-file", "blob", &format!("{base}:{path}")])?;
        let Ok(text) = std::str::from_utf8(&original) else {
            continue;
        };
        let marker = ["#[cfg(test)]", "#[test]", "#[tokio::test"]
            .into_iter()
            .filter_map(|marker| text.find(marker))
            .min();
        if let Some(index) = marker {
            let current = git(workspace, &["cat-file", "blob", &format!(":{path}")])
                .context("existing inline test section was deleted")?;
            ensure!(
                current.ends_with(&original[index..]),
                "existing inline tests changed in {path}; automatic acceptance is blocked"
            );
        }
    }
    Ok(())
}
fn reject_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "symlink artifacts are not accepted"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "reparse-point artifacts are not accepted"
        );
    }
    Ok(())
}
fn reject_unsafe_tree(path: &Path, revision: &str) -> Result<()> {
    let bytes = if revision.is_empty() {
        git(path, &["ls-files", "--stage", "-z"])?
    } else {
        git(path, &["ls-tree", "-r", "-z", revision])?
    };
    for entry in bytes.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let entry = std::str::from_utf8(entry).context("non-UTF8 Git filename is unsupported")?;
        ensure!(
            !entry.starts_with("120000 ") && !entry.starts_with("160000 "),
            "team automation does not support symlinks or submodules"
        );
        if let Some((_, name)) = entry.split_once('\t') {
            normalize_write_path(name)?;
        }
    }
    Ok(())
}
fn reject_worktree_links(workspace: &Path) -> Result<()> {
    let paths = nul_paths(&git(
        workspace,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?)?;
    for path in paths {
        let mut component_path = workspace.to_path_buf();
        for part in path.split('/') {
            component_path.push(part);
            match fs::symlink_metadata(&component_path) {
                Ok(_) => reject_link(&component_path)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}
fn tracked_paths(path: &Path, revision: &str) -> Result<HashSet<String>> {
    nul_paths(&git(
        path,
        &["ls-tree", "-r", "--name-only", "-z", revision],
    )?)
    .map(|v| v.into_iter().collect())
}
fn changed(path: &Path, base: &str, commit: Option<&str>) -> Result<Vec<String>> {
    let bytes = if let Some(commit) = commit {
        git(
            path,
            &[
                "diff",
                "--name-only",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "-z",
                base,
                commit,
                "--",
            ],
        )?
    } else {
        git(
            path,
            &[
                "diff",
                "--cached",
                "--name-only",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "-z",
                base,
                "--",
            ],
        )?
    };
    let mut paths = nul_paths(&bytes)?;
    paths.sort();
    Ok(paths)
}
fn nul_paths(bytes: &[u8]) -> Result<Vec<String>> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let path = std::str::from_utf8(p).context("non-UTF8 filename is unsupported")?;
            normalize_write_path(path)
        })
        .collect()
}
fn reject_custom_filters(path: &Path) -> Result<()> {
    let (status, data) = git_status(
        path,
        &[
            "config",
            "--includes",
            "--get-regexp",
            "^filter\\..*\\.(clean|smudge|process)$",
        ],
    )?;
    ensure!(
        status == 1 || (status == 0 && data.is_empty()),
        "custom Git clean/smudge filters require manual workspace preparation"
    );
    Ok(())
}
fn commit_tree(path: &Path, tree: &str, parent: &str, message: &str) -> Result<String> {
    git_text(path, &["commit-tree", tree, "-p", parent, "-m", message])
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    reject_link(path)?;
    let mut data = vec![];
    File::open(path)?
        .take(MAX_GIT_BYTES as u64 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= MAX_GIT_BYTES, "artifact is too large");
    Ok(data)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read_bounded(path)?).context("invalid workspace metadata")
}

struct IntegrationLock {
    path: PathBuf,
    _file: File,
}
impl IntegrationLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join("integration.lock");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .context("integration workspace is already in use")?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self { path, _file: file })
    }
}
impl Drop for IntegrationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn git_text(path: &Path, args: &[&str]) -> Result<String> {
    String::from_utf8(git(path, args)?)
        .map(|v| v.trim().into())
        .context("invalid Git output")
}
fn git(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let (code, bytes) = git_status(path, args)?;
    ensure!(
        code == 0,
        "Git {} failed with exit code {code}: {}",
        args.first().unwrap_or(&"command"),
        String::from_utf8_lossy(&bytes)
            .chars()
            .take(2000)
            .collect::<String>()
    );
    Ok(bytes)
}
fn git_status(path: &Path, args: &[&str]) -> Result<(i32, Vec<u8>)> {
    // Honor only the effective inert line-ending settings. Disabling the global
    // config without preserving these makes ordinary Windows clean checkouts look
    // dirty, while inheriting all global config would re-enable executable filters.
    let mut line_endings = Vec::new();
    for (key, allowed) in [
        ("core.autocrlf", &["true", "false", "input"][..]),
        ("core.eol", &["lf", "crlf", "native"][..]),
        ("core.safecrlf", &["true", "false", "warn"][..]),
    ] {
        let (code, value) = git_status_inner(path, &["config", "--get", key], true, &[])?;
        if code == 0 {
            let value = String::from_utf8(value)?.trim().to_ascii_lowercase();
            ensure!(allowed.contains(&value.as_str()), "unsupported {key} value");
            line_endings.push(format!("{key}={value}"));
        } else {
            ensure!(
                code == 1,
                "cannot inspect safe Git line-ending configuration"
            );
        }
    }
    git_status_inner(path, args, false, &line_endings)
}
fn git_status_inner(
    path: &Path,
    args: &[&str],
    config_read: bool,
    line_endings: &[String],
) -> Result<(i32, Vec<u8>)> {
    let mut command = Command::new(git_executable(path)?);
    command.args([
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
        "-c",
        "gc.auto=0",
        "-c",
        "commit.gpgsign=false",
    ]);
    for value in line_endings {
        command.arg("-c").arg(value);
    }
    command
        .args(args)
        .current_dir(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let inherited: Vec<OsString> = std::env::vars_os()
        .filter_map(|(k, _)| {
            k.to_str()
                .is_some_and(|k| k.starts_with("GIT_"))
                .then_some(k)
        })
        .collect();
    for key in inherited {
        command.env_remove(key);
    }
    if !config_read {
        command.env("GIT_CONFIG_NOSYSTEM", "1").env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .env("GIT_AUTHOR_NAME", "Wonderland")
        .env("GIT_AUTHOR_EMAIL", "wonderland@localhost")
        .env("GIT_COMMITTER_NAME", "Wonderland")
        .env("GIT_COMMITTER_EMAIL", "wonderland@localhost");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("cannot start Git")?;
    let tree = match crate::process_tree::ProcessTree::attach(child.id()) {
        Ok(tree) => Some(tree),
        Err(error) => {
            if child.try_wait()?.is_some() {
                None
            } else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    };
    let stdout = child.stdout.take().context("Git stdout missing")?;
    let stderr = child.stderr.take().context("Git stderr missing")?;
    let out = thread::spawn(move || read_pipe(stdout));
    let err = thread::spawn(move || read_pipe(stderr));
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            drop(tree);
            let _ = child.kill();
            let _ = child.wait();
            let _ = out.join();
            let _ = err.join();
            bail!("Git command timed out");
        }
        thread::sleep(Duration::from_millis(20));
    };
    drop(tree);
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("Git output reader failed"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("Git error reader failed"))??;
    Ok((
        status.code().unwrap_or(-1),
        if status.success() { stdout } else { stderr },
    ))
}
fn read_pipe(mut pipe: impl Read) -> Result<Vec<u8>> {
    let mut data = vec![];
    let mut buffer = [0; 8192];
    let mut oversized = false;
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if data.len() + count <= MAX_GIT_BYTES {
            data.extend_from_slice(&buffer[..count]);
        } else {
            oversized = true;
        }
    }
    ensure!(!oversized, "Git output exceeds size limit");
    Ok(data)
}
fn git_executable(root: &Path) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    for directory in std::env::split_paths(&path).filter(|p| p.is_absolute()) {
        if let Ok(candidate) = directory
            .join(if cfg!(windows) { "git.exe" } else { "git" })
            .canonicalize()
        {
            if candidate.is_file()
                && !candidate.starts_with(root)
                && !candidate
                    .ancestors()
                    .skip(1)
                    .any(|directory| directory.join(".git").exists())
            {
                return Ok(candidate);
            }
        }
    }
    bail!("Git executable was not found outside the workspace")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn repository() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let runs = root.path().join("runs");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::create_dir_all(source.join("tests")).unwrap();
        fs::write(source.join("src/first.txt"), "first original\n").unwrap();
        fs::write(source.join("src/second.txt"), "second original\n").unwrap();
        fs::write(source.join("tests/check.py"), "assert 2 + 2 == 4\n").unwrap();
        git(&source, &["init", "--quiet"]).unwrap();
        git(&source, &["config", "--local", "core.autocrlf", "false"]).unwrap();
        git(&source, &["add", "--all", "--", "."]).unwrap();
        git(&source, &["commit", "--quiet", "-m", "fixture"]).unwrap();
        (root, source, runs)
    }
    #[test]
    fn delivery_changes_only_detached_workspaces() {
        let (_root, source, runs) = repository();
        let original = current_revision(&source).unwrap();
        let run = prepare_run(&source, &runs, "delivery").unwrap();
        let attempt = prepare_attempt(&run, "implement", 1).unwrap();
        fs::write(
            attempt.path.join("src/first.txt"),
            "updated implementation\n",
        )
        .unwrap();
        let artifact = capture_attempt(&attempt, &["src/first.txt".into()]).unwrap();
        assert_eq!(artifact.changed_paths, vec!["src/first.txt"]);
        let result = integrate_attempt(&run, &artifact).unwrap();
        assert_ne!(result.revision, original);
        assert_eq!(
            fs::read_to_string(run.integration.join("src/first.txt")).unwrap(),
            "updated implementation\n"
        );
        assert_eq!(
            fs::read_to_string(source.join("src/first.txt")).unwrap(),
            "first original\n"
        );
        assert_eq!(current_revision(&source).unwrap(), original);
        assert_source_unchanged(&run).unwrap();
    }
    #[test]
    fn windows_line_endings_preserve_git_test_identity() {
        let (_root, source, runs) = repository();
        git(&source, &["config", "--local", "core.autocrlf", "true"]).unwrap();
        fs::write(source.join("src/first.txt"), "first original\r\n").unwrap();
        git(&source, &["add", "--renormalize", "."]).unwrap();
        assert_clean(&source).unwrap();
        let run = prepare_run(&source, &runs, "line-endings").unwrap();
        let attempt = prepare_attempt(&run, "implement", 1).unwrap();
        fs::write(attempt.path.join("src/first.txt"), "updated\r\n").unwrap();
        let artifact = capture_attempt(&attempt, &["src/first.txt".into()]).unwrap();
        integrate_attempt(&run, &artifact).unwrap();
        assert_eq!(
            git_text(&source, &["rev-parse", "HEAD:tests/check.py"]).unwrap(),
            git_text(&run.integration, &["rev-parse", "HEAD:tests/check.py"]).unwrap()
        );
        assert_source_unchanged(&run).unwrap();
    }
    #[test]
    fn agent_commits_cannot_hide_undeclared_writes() {
        let (_root, source, runs) = repository();
        let run = prepare_run(&source, &runs, "scope").unwrap();
        let attempt = prepare_attempt(&run, "implement", 1).unwrap();
        fs::write(attempt.path.join("outside.txt"), "undeclared\n").unwrap();
        git(&attempt.path, &["add", "--all", "--", "."]).unwrap();
        git(&attempt.path, &["commit", "--quiet", "-m", "agent commit"]).unwrap();
        assert!(capture_attempt(&attempt, &["src".into()]).is_err());
        assert_clean(&run.integration).unwrap();
        assert_source_unchanged(&run).unwrap();
    }
    #[test]
    fn changed_existing_tests_are_blocked_but_new_tests_can_be_delivered() {
        let (_root, source, runs) = repository();
        let run = prepare_run(&source, &runs, "tests").unwrap();
        let bad = prepare_attempt(&run, "weaken", 1).unwrap();
        fs::write(bad.path.join("tests/check.py"), "pass\n").unwrap();
        assert!(capture_attempt(&bad, &["tests".into()]).is_err());
        let good = prepare_attempt(&run, "add", 1).unwrap();
        fs::write(good.path.join("tests/new_test.py"), "assert 3 + 3 == 6\n").unwrap();
        let artifact = capture_attempt(&good, &["tests".into()]).unwrap();
        assert_eq!(artifact.test_changes, vec!["tests/new_test.py"]);
        integrate_attempt(&run, &artifact).unwrap();
        assert_eq!(
            fs::read_to_string(run.integration.join("tests/check.py")).unwrap(),
            "assert 2 + 2 == 4\n"
        );
    }
    #[test]
    fn conflicting_delivery_preserves_prior_integration() {
        let (_root, source, runs) = repository();
        let run = prepare_run(&source, &runs, "conflicts").unwrap();
        let first = prepare_attempt(&run, "first", 1).unwrap();
        let second = prepare_attempt(&run, "second", 1).unwrap();
        fs::write(first.path.join("src/first.txt"), "first change\n").unwrap();
        fs::write(second.path.join("src/first.txt"), "second change\n").unwrap();
        let a = capture_attempt(&first, &["src".into()]).unwrap();
        let b = capture_attempt(&second, &["src".into()]).unwrap();
        let integrated = integrate_attempt(&run, &a).unwrap();
        assert!(integrate_attempt(&run, &b).is_err());
        assert_eq!(
            current_revision(&run.integration).unwrap(),
            integrated.revision
        );
        assert_eq!(
            fs::read_to_string(run.integration.join("src/first.txt")).unwrap(),
            "first change\n"
        );
        assert_clean(&run.integration).unwrap();
        assert_source_unchanged(&run).unwrap();
    }
    #[test]
    fn disjoint_changes_from_same_base_integrate_serially() {
        let (_root, source, runs) = repository();
        let run = prepare_run(&source, &runs, "parallel").unwrap();
        let first = prepare_attempt(&run, "first", 1).unwrap();
        let second = prepare_attempt(&run, "second", 1).unwrap();
        assert_eq!(first.base_revision, second.base_revision);
        fs::write(first.path.join("src/first.txt"), "first change\n").unwrap();
        fs::write(second.path.join("src/second.txt"), "second change\n").unwrap();
        let a = capture_attempt(&first, &["src/first.txt".into()]).unwrap();
        let b = capture_attempt(&second, &["src/second.txt".into()]).unwrap();
        integrate_attempt(&run, &a).unwrap();
        integrate_attempt(&run, &b).unwrap();
        assert_eq!(
            fs::read_to_string(run.integration.join("src/first.txt")).unwrap(),
            "first change\n"
        );
        assert_eq!(
            fs::read_to_string(run.integration.join("src/second.txt")).unwrap(),
            "second change\n"
        );
    }
    #[test]
    fn patch_tampering_is_rejected_before_any_integration() {
        let (_root, source, runs) = repository();
        let run = prepare_run(&source, &runs, "tamper").unwrap();
        let attempt = prepare_attempt(&run, "node", 1).unwrap();
        fs::write(attempt.path.join("src/first.txt"), "new\n").unwrap();
        let artifact = capture_attempt(&attempt, &["src".into()]).unwrap();
        fs::write(&artifact.patch_path, "malformed patch").unwrap();
        assert!(integrate_attempt(&run, &artifact).is_err());
        assert_eq!(
            current_revision(&run.integration).unwrap(),
            run.source_revision
        );
        assert_clean(&run.integration).unwrap();
    }
    #[test]
    fn dirty_source_and_source_changes_block_delivery() {
        let (_root, source, runs) = repository();
        fs::write(source.join("untracked.txt"), "local work\n").unwrap();
        assert!(prepare_run(&source, &runs, "dirty").is_err());
        fs::remove_file(source.join("untracked.txt")).unwrap();
        let run = prepare_run(&source, &runs, "clean").unwrap();
        fs::write(source.join("src/first.txt"), "user changed this\n").unwrap();
        assert!(prepare_attempt(&run, "node", 1).is_err());
        assert_eq!(
            fs::read_to_string(source.join("src/first.txt")).unwrap(),
            "user changed this\n"
        );
    }
    #[test]
    fn inline_rust_tests_are_preserved_while_implementation_can_change() {
        let (_root, source, runs) = repository();
        let tests =
            "#[cfg(test)]\nmod tests { #[test] fn checks() { assert_eq!(super::value(), 2); } }\n";
        fs::write(
            source.join("src/lib.rs"),
            format!("fn value()->u8{{1}}\n{tests}"),
        )
        .unwrap();
        git(&source, &["add", "--all", "--", "."]).unwrap();
        git(&source, &["commit", "--quiet", "-m", "inline tests"]).unwrap();
        let run = prepare_run(&source, &runs, "inline").unwrap();
        let good = prepare_attempt(&run, "fix", 1).unwrap();
        fs::write(
            good.path.join("src/lib.rs"),
            format!("fn value()->u8{{2}}\n{tests}"),
        )
        .unwrap();
        assert!(capture_attempt(&good, &["src/lib.rs".into()]).is_ok());
        let bad = prepare_attempt(&run, "weaken", 1).unwrap();
        fs::write(
            bad.path.join("src/lib.rs"),
            "fn value()->u8{1}\n#[cfg(test)]\nmod tests {}\n",
        )
        .unwrap();
        assert!(capture_attempt(&bad, &["src/lib.rs".into()]).is_err());
    }
}

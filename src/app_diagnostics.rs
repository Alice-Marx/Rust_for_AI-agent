//! Read-only diagnostics for registered CLI applications. Version probes run
//! with closed stdin through desktop_bridge's bounded process-tree probe.
//! No login/configuration file, credential value or model endpoint is read.

use crate::desktop_bridge::{self, AppMetadata, CliProfile};
use anyhow::{ensure, Context, Result};
use chrono::{SecondsFormat, Utc};
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const HASH_TIMEOUT: Duration = Duration::from_secs(8);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
static PROBE_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// The app ID must exactly match the built-in registry. No executable path,
/// arguments, installation request, credentials or prompt is accepted here.
/// Authentication/model availability intentionally remain `unknown`.
pub async fn probe(app_id: &str) -> Result<Value> {
    let (app, profile) = registered(app_id)?;
    let permit = PROBE_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .context("application diagnostics are busy; retry after the current probes finish")?;
    // Holding the permit inside the worker also bounds detached workers if an
    // unusual filesystem blocks beyond the outer async response deadline.
    tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            probe_blocking(app, profile)
        }),
    )
    .await
    .context("application diagnostics exceeded the 20-second deadline")?
    .context("application diagnostics worker failed")?
}

fn registered(app_id: &str) -> Result<(AppMetadata, CliProfile)> {
    ensure!(app_id.len() <= 64, "application ID is not registered");
    let app = desktop_bridge::official_apps()
        .into_iter()
        .find(|app| app.id == app_id)
        .context("application ID is not registered")?;
    let profile = desktop_bridge::default_cli_profiles()
        .into_iter()
        .find(|profile| profile.id == app_id)
        .context("application has no registered default CLI profile")?;
    ensure!(
        profile.args.is_empty() && profile.version_args == ["--version"],
        "default diagnostic profile contains unsupported arguments"
    );
    Ok((app, profile))
}

fn discover_profile(
    mut profile: CliProfile,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> (CliProfile, Option<PathBuf>, bool) {
    let mut found = resolve(&profile.executable);
    let fallback = found.is_none() && profile.id == "kimi-cli";
    if fallback {
        profile.executable = "kimi".into();
        found = resolve("kimi");
    }
    (profile, found, fallback)
}

struct Entry {
    path: PathBuf,
    scope: &'static str,
    package: Option<Value>,
    direct_probe: bool,
    note: &'static str,
}

fn probe_blocking(app: AppMetadata, profile: CliProfile) -> Result<Value> {
    let started = Instant::now();
    let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let (mut profile, launcher, fallback) =
        discover_profile(profile, desktop_bridge::find_executable);
    let caps = crate::native_executor::capabilities(&app.id);
    let mut report = json!({
        "app_id": app.id,
        "name": app.name,
        "provider": app.provider,
        "checked_at": checked_at,
        "installed": launcher.is_some(),
        "status": if launcher.is_some() { "found" } else { "not_installed" },
        "version": null,
        "path": launcher,
        "launcher_path": launcher,
        "identity_path": null,
        "sha256": null,
        "hash_scope": null,
        "version_probe": {"status":"not_run","timeout_seconds":5,"arguments":["--version"]},
        "identity": {"status":"unknown","observed_app_id":null,"matches_requested":null,"publisher_verified":false},
        "capabilities": caps,
        "capabilities_scope":"Implemented Wonderland adapter controls; protocol compatibility with this installed version has not been tested by this diagnostic",
        "authentication": {"status":"unknown","reason":"Credentials and account state were not read; a version banner does not prove login"},
        "model_availability": {"status":"unknown","models":[],"reason":"No model request or model listing was performed"},
        "billing_channel": {"status":"unknown","reason":"API and subscription billing cannot be inferred from installation"},
        "official_source": app.source,
        "install_hint": profile.install_hint,
        "used_kimi_command_fallback":fallback,
        "limitations":["Local paths, package metadata, version text and hashes are observations, not publisher signatures or remote model attestations", "Only the identified file is hashed; interpreters, dynamically loaded modules and plugins are outside this digest"],
    });
    let Some(launcher) = launcher else {
        report["elapsed_ms"] = json!(started.elapsed().as_millis());
        return Ok(report);
    };
    let entry = match resolve_entry(&app.id, &launcher) {
        Ok(entry) => entry,
        Err(_) => {
            report["status"] = json!("identity_resolution_failed");
            report["identity"]["reason"] = json!("Installed package metadata or entrypoint could not be validated; no wrapper was executed");
            report["elapsed_ms"] = json!(started.elapsed().as_millis());
            return Ok(report);
        }
    };
    report["identity_path"] = json!(entry.path);
    report["path"] = json!(entry.path);
    report["hash_scope"] = json!(entry.scope);
    report["package"] = json!(entry.package);
    report["entrypoint_note"] = json!(entry.note);
    // Native npm entrypoints, especially Claude's bin/claude.exe, are probed
    // directly. This avoids invoking an .exe through an npm Node wrapper.
    profile.executable = if entry.direct_probe {
        entry.path.clone()
    } else {
        launcher
    }
    .to_string_lossy()
    .into_owned();
    let status = desktop_bridge::detect_cli(&profile);
    let package_name = entry
        .package
        .as_ref()
        .and_then(|package| package["name"].as_str());
    let observation = observe_identity(&app.id, status.version.as_deref(), package_name);
    report["version"] = observation["version"].clone();
    report["identity"] = observation["identity"].clone();
    if app.id == "kimi-code"
        && status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("当前 kimi 命令属于 Python Kimi CLI"))
    {
        report["identity"] = json!({"status":"observed","observed_app_id":"kimi-cli","matches_requested":false,"publisher_verified":false,"evidence":"desktop_bridge rejected a Python Kimi CLI banner for the Node Kimi profile"});
    }
    report["version_probe"] = json!({
        "status":if status.error.is_some() {"failed"} else {"completed"},
        "timeout_seconds":5,
        "arguments":["--version"],
        "executable":profile.executable,
        // Raw stdout/stderr is never returned: an unexpected executable might
        // print environment/configuration values even for --version.
        "error":status.error.as_ref().map(|error|if error.contains("超时") {"version probe timed out"}else{"version probe failed or returned an unexpected application identity"}),
    });
    report["status"] = json!(if report["identity"]["matches_requested"] == false {
        "identity_mismatch"
    } else if status.error.is_some() {
        "version_probe_failed"
    } else {
        "observed"
    });
    match executable_digest(
        &entry.path,
        MAX_EXECUTABLE_BYTES,
        Instant::now() + HASH_TIMEOUT,
    ) {
        Ok((digest, bytes)) => {
            report["sha256"] = json!(digest);
            report["hashed_bytes"] = json!(bytes);
            report["hash_status"] = json!("recorded");
        }
        Err(_) => {
            report["hash_status"] = json!("unavailable");
            report["hash_error"] = json!(
                "File could not be hashed within its size/time bound, or changed while reading"
            );
        }
    }
    report["elapsed_ms"] = json!(started.elapsed().as_millis());
    Ok(report)
}

fn resolve_entry(app_id: &str, launcher: &Path) -> Result<Entry> {
    let canonical = launcher
        .canonicalize()
        .context("installed launcher is unavailable")?;
    let native = native_header(&canonical)?;
    // Python console-script EXEs are launchers; their hash cannot attest the
    // imported kimi_cli package. They are still the actual invoked PE file.
    if native {
        return Ok(Entry {
            path: canonical,
            scope: if app_id == "kimi-cli" {
                "python_console_launcher"
            } else {
                "native_executable"
            },
            package: None,
            direct_probe: true,
            note: if app_id == "kimi-cli" {
                "Digest covers the Python console launcher, not the installed Python package"
            } else {
                "Resolved native executable; publisher signature not checked"
            },
        });
    }
    if matches!(app_id, "claude" | "codex" | "kimi-code") {
        if let Some(entry) = npm_entry(app_id, launcher, &canonical)? {
            return Ok(entry);
        }
    }
    Ok(Entry {path:canonical,scope:"launcher_only",package:None,direct_probe:false,note:"Underlying runtime or application entrypoint could not be resolved from the supported package layouts; digest covers the launcher only"})
}

fn native_header(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "entrypoint is not a regular file"
    );
    let mut magic = [0u8; 4];
    let count = file.read(&mut magic)?;
    Ok((count >= 2 && &magic[..2] == b"MZ")
        || (count == 4
            && (magic == *b"\x7fELF"
                || matches!(
                    magic,
                    [0xfe, 0xed, 0xfa, 0xce]
                        | [0xfe, 0xed, 0xfa, 0xcf]
                        | [0xce, 0xfa, 0xed, 0xfe]
                        | [0xcf, 0xfa, 0xed, 0xfe]
                        | [0xca, 0xfe, 0xba, 0xbe]
                        | [0xbe, 0xba, 0xfe, 0xca]
                ))))
}

fn npm_entry(app_id: &str, launcher: &Path, canonical: &Path) -> Result<Option<Entry>> {
    let (package_name, folder, command) = match app_id {
        "claude" => ("@anthropic-ai/claude-code", "claude-code", "claude"),
        "codex" => ("@openai/codex", "codex", "codex"),
        "kimi-code" => ("@moonshot-ai/kimi-code", "kimi-code", "kimi"),
        _ => return Ok(None),
    };
    let mut roots = Vec::new();
    for path in [launcher, canonical] {
        if let Some(parent) = path.parent() {
            roots.push(parent.join("node_modules").join(package_name));
            roots.push(parent.join("../lib/node_modules").join(package_name));
            for ancestor in parent.ancestors().take(4) {
                if ancestor.file_name().is_some_and(|name| name == folder) {
                    roots.push(ancestor.to_path_buf());
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    for root in roots {
        if !seen.insert(root.clone()) || !root.join("package.json").is_file() {
            continue;
        }
        let package = read_json(&root.join("package.json"))?;
        ensure!(
            package["name"].as_str() == Some(package_name),
            "npm package identity mismatch"
        );
        let entry = package["bin"]
            .get(command)
            .and_then(Value::as_str)
            .or_else(|| package["bin"].as_str())
            .context("npm bin mapping unavailable")?;
        let allowed = match app_id {
            "claude" => ["bin/claude.exe", "bin/claude", "cli.js", "cli-wrapper.cjs"].as_slice(),
            "codex" => ["bin/codex.js"].as_slice(),
            "kimi-code" => ["dist/main.mjs"].as_slice(),
            _ => unreachable!(),
        };
        ensure!(
            allowed.contains(&entry),
            "unsupported npm entrypoint mapping"
        );
        let root = root.canonicalize()?;
        let target = root.join(entry).canonicalize()?;
        ensure!(
            target.starts_with(&root),
            "npm entrypoint leaves package root"
        );
        // A colocated unrelated package must not hijack diagnostics for a
        // different same-name command. Accept only the package entry itself or
        // a bounded shim containing its literal package path.
        if !canonical.starts_with(&root) {
            let shim = read_small(launcher)?;
            let shim = String::from_utf8_lossy(&shim).replace('\\', "/");
            ensure!(
                shim.contains(package_name),
                "launcher does not reference the expected npm package"
            );
        }
        let metadata = json!({"name":package_name,"version":package["version"].as_str().filter(|value|valid_version(value)),"manifest_path":root.join("package.json"),"evidence":"Local unsigned package metadata"});
        if native_header(&target)? {
            return Ok(Some(Entry {path:target,scope:"native_executable",package:Some(metadata),direct_probe:true,note:"Resolved native npm bin entry; package metadata and publisher signature are not cryptographic attestations"}));
        }
        if app_id == "claude" && matches!(entry, "bin/claude.exe" | "bin/claude") {
            anyhow::bail!("Claude native entry is not a native executable")
        }
        if app_id == "codex" {
            if let Some(binary) = codex_native(&root)? {
                return Ok(Some(Entry {path:binary,scope:"native_executable",package:Some(metadata),direct_probe:true,note:"Resolved installed Codex platform binary using known vendor layouts; package metadata is unsigned"}));
            }
        }
        return Ok(Some(Entry {
            path: target,
            scope: "script_entrypoint",
            package: Some(metadata),
            direct_probe: false,
            note: "Digest covers the npm application script, not Node or its imported dependencies",
        }));
    }
    Ok(None)
}

fn codex_native(root: &Path) -> Result<Option<PathBuf>> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some(("win32-x64", "x86_64-pc-windows-msvc", "codex.exe")),
        ("windows", "aarch64") => Some(("win32-arm64", "aarch64-pc-windows-msvc", "codex.exe")),
        ("linux", "x86_64") => Some(("linux-x64", "x86_64-unknown-linux-musl", "codex")),
        ("linux", "aarch64") => Some(("linux-arm64", "aarch64-unknown-linux-musl", "codex")),
        ("macos", "x86_64") => Some(("darwin-x64", "x86_64-apple-darwin", "codex")),
        ("macos", "aarch64") => Some(("darwin-arm64", "aarch64-apple-darwin", "codex")),
        _ => None,
    };
    let Some((platform, triple, name)) = platform else {
        return Ok(None);
    };
    let package_name = format!("@openai/codex-{platform}");
    let mut packages = vec![root.join("node_modules").join(&package_name)];
    if let Some(scope) = root.parent() {
        packages.push(scope.join(format!("codex-{platform}")));
    }
    for package in packages {
        if !package.join("package.json").is_file() {
            continue;
        }
        let metadata = read_json(&package.join("package.json"))?;
        ensure!(
            metadata["name"].as_str() == Some(package_name.as_str()),
            "Codex platform package name mismatch"
        );
        let package = package.canonicalize()?;
        for middle in ["bin", "codex"] {
            let binary = package.join("vendor").join(triple).join(middle).join(name);
            if binary.is_file() {
                let binary = binary.canonicalize()?;
                ensure!(
                    binary.starts_with(&package) && native_header(&binary)?,
                    "Codex platform binary invalid"
                );
                return Ok(Some(binary));
            }
        }
    }
    for middle in ["bin", "codex"] {
        let binary = root.join("vendor").join(triple).join(middle).join(name);
        if binary.is_file() {
            let binary = binary.canonicalize()?;
            ensure!(
                binary.starts_with(root) && native_header(&binary)?,
                "Codex vendor binary invalid"
            );
            return Ok(Some(binary));
        }
    }
    Ok(None)
}

fn read_small(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_METADATA_BYTES as u64,
        "metadata or shim too large"
    );
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_METADATA_BYTES,
        "metadata or shim too large"
    );
    Ok(bytes)
}
fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&read_small(path)?).context("invalid local package metadata")
}

fn executable_digest(path: &Path, max_bytes: u64, deadline: Instant) -> Result<(String, u64)> {
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.len() > 0 && before.len() <= max_bytes,
        "executable size outside bounds"
    );
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure!(
            Instant::now() < deadline,
            "executable hash deadline exceeded"
        );
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .context("executable size overflow")?;
        ensure!(bytes <= max_bytes, "executable grew beyond size bound");
        hash.update(&buffer[..count]);
    }
    let after = file.metadata()?;
    ensure!(
        bytes == before.len()
            && after.len() == before.len()
            && before.modified().ok() == after.modified().ok(),
        "executable changed while hashing"
    );
    Ok((format!("{:x}", hash.finalize()), bytes))
}

fn valid_version(value: &str) -> bool {
    value.len() <= 96
        && Regex::new(
            r"^[0-9]{1,6}\.[0-9]{1,6}(?:\.[0-9]{1,6})?(?:[-+][A-Za-z0-9]+(?:[.-][A-Za-z0-9]+)*)?$",
        )
        .is_ok_and(|regex| regex.is_match(value))
}

fn observe_identity(requested: &str, banner: Option<&str>, package: Option<&str>) -> Value {
    let mut observed = None;
    let mut version = None;
    // Accept complete, recognized lines only; don't return arbitrary stdout,
    // errors, credential-looking strings or environment output.
    let patterns = [
        (
            "codex",
            r"(?i)^codex(?:-cli)?\s+([0-9][A-Za-z0-9.+-]{1,95})$",
        ),
        (
            "claude",
            r"(?i)^([0-9][A-Za-z0-9.+-]{1,95})\s+\(Claude Code\)$",
        ),
        (
            "kimi-cli",
            r"(?i)^(?:kimi|kimi-cli), version ([0-9][A-Za-z0-9.+-]{1,95})$",
        ),
        (
            "kimi-code",
            r"(?i)^(?:kimi-code|kimi code)\s+([0-9][A-Za-z0-9.+-]{1,95})$",
        ),
    ];
    if let Some(banner) = banner {
        for line in banner
            .lines()
            .take(16)
            .map(str::trim)
            .filter(|line| line.len() <= 160)
        {
            for (identity, pattern) in patterns {
                if let Ok(regex) = Regex::new(pattern) {
                    if let Some(capture) = regex.captures(line) {
                        if valid_version(&capture[1]) {
                            observed = Some(identity);
                            version = Some(capture[1].to_owned());
                            break;
                        }
                    }
                }
            }
            if observed.is_some() {
                break;
            }
            if valid_version(line) {
                version = Some(line.to_owned())
            }
        }
    }
    let package_identity = match package {
        Some("@anthropic-ai/claude-code") => Some("claude"),
        Some("@openai/codex") => Some("codex"),
        Some("@moonshot-ai/kimi-code") => Some("kimi-code"),
        _ => None,
    };
    let observed = observed.or(package_identity.filter(|_| version.is_some()));
    json!({"version":version,"identity":{"status":if observed.is_some(){"observed"}else{"unknown"},"observed_app_id":observed,"matches_requested":observed.map(|identity|identity==requested),"publisher_verified":false,"evidence":if package.is_some(){"Local package metadata and recognized version text"}else{"Recognized local version text only"}}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_application_cannot_supply_commands_or_paths() {
        for id in [
            "custom",
            "claude --print",
            "../codex",
            "C:/temp/tool.exe",
            "codex\n--help",
            "",
        ] {
            assert!(registered(id).is_err());
        }
        for app in desktop_bridge::official_apps() {
            let (_, profile) = registered(&app.id).unwrap();
            assert!(profile.args.is_empty());
            assert_eq!(profile.version_args, vec!["--version"]);
        }
    }

    #[test]
    fn python_kimi_fallback_preserves_requested_identity() {
        let (_, profile) = registered("kimi-cli").unwrap();
        let (profile, path, fallback) = discover_profile(profile, |name| {
            (name == "kimi").then(|| PathBuf::from("/installed/kimi"))
        });
        assert!(fallback);
        assert_eq!(profile.id, "kimi-cli");
        assert_eq!(profile.executable, "kimi");
        assert_eq!(path, Some(PathBuf::from("/installed/kimi")));
        let (_, profile) = registered("kimi-cli").unwrap();
        let (_, _, fallback) = discover_profile(profile, |name| Some(PathBuf::from(name)));
        assert!(!fallback);
    }

    #[test]
    fn banner_does_not_confuse_python_kimi_and_node_kimi_or_expose_other_output() {
        let result = observe_identity(
            "kimi-code",
            Some("kimi, version 1.28.0\nTOKEN=hidden-value"),
            None,
        );
        assert_eq!(result["identity"]["observed_app_id"], "kimi-cli");
        assert_eq!(result["identity"]["matches_requested"], false);
        assert!(!result.to_string().contains("hidden-value"));
        let result = observe_identity(
            "claude",
            Some("2.1.193 (Claude Code)"),
            Some("@anthropic-ai/claude-code"),
        );
        assert_eq!(result["version"], "2.1.193");
        assert_eq!(result["identity"]["publisher_verified"], false);
        let result = observe_identity("codex", Some("sk-proj-this-is-not-version-output"), None);
        assert!(result["version"].is_null());
        assert_eq!(result["identity"]["status"], "unknown");
        let result = observe_identity("kimi-code", Some("2.0.1"), Some("@moonshot-ai/kimi-code"));
        assert_eq!(result["identity"]["matches_requested"], true);
    }

    fn claude_fixture(directory: &Path, entry: &str, name: &str) -> PathBuf {
        let root = directory.join("node_modules/@anthropic-ai/claude-code");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(
            root.join("package.json"),
            serde_json::to_vec(&json!({"name":name,"version":"2.1.193","bin":{"claude":entry}}))
                .unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("bin/claude.exe"), b"MZnative-fixture").unwrap();
        let shim = directory.join("claude.cmd");
        std::fs::write(
            &shim,
            b"@echo off\n\"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe\" %*\n",
        )
        .unwrap();
        shim
    }

    #[test]
    fn claude_native_npm_bin_is_the_digest_and_probe_target() {
        let directory = tempfile::tempdir().unwrap();
        let shim = claude_fixture(
            directory.path(),
            "bin/claude.exe",
            "@anthropic-ai/claude-code",
        );
        let entry = resolve_entry("claude", &shim).unwrap();
        assert_eq!(entry.scope, "native_executable");
        assert!(entry.direct_probe);
        assert_eq!(entry.path.file_name().unwrap(), "claude.exe");
        let (digest, _) =
            executable_digest(&entry.path, 1024, Instant::now() + Duration::from_secs(1)).unwrap();
        let (wrapper, _) =
            executable_digest(&shim, 1024, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_ne!(digest, wrapper);
    }

    #[test]
    fn altered_npm_mapping_or_unrelated_wrapper_fails_closed() {
        for (entry, name) in [
            ("../../elsewhere.exe", "@anthropic-ai/claude-code"),
            ("bin/claude.exe", "unrelated-package"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let shim = claude_fixture(directory.path(), entry, name);
            assert!(resolve_entry("claude", &shim).is_err());
        }
        let directory = tempfile::tempdir().unwrap();
        let shim = claude_fixture(
            directory.path(),
            "bin/claude.exe",
            "@anthropic-ai/claude-code",
        );
        std::fs::write(&shim, b"echo unrelated").unwrap();
        assert!(resolve_entry("claude", &shim).is_err());
    }

    #[test]
    fn file_hash_is_bounded_and_deadline_aware() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("binary");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(
            executable_digest(&file, 3, Instant::now() + Duration::from_secs(1)).unwrap(),
            (
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
                3
            )
        );
        assert!(executable_digest(&file, 2, Instant::now() + Duration::from_secs(1)).is_err());
        assert!(executable_digest(&file, 3, Instant::now()).is_err());
        assert!(executable_digest(
            directory.path(),
            1024,
            Instant::now() + Duration::from_secs(1)
        )
        .is_err());
    }

    #[test]
    fn package_metadata_is_bounded_and_versions_are_not_arbitrary_output() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("package.json");
        std::fs::write(&file, vec![b' '; MAX_METADATA_BYTES + 1]).unwrap();
        assert!(read_json(&file).is_err());
        for version in ["0.154.0-alpha.6.2", "2.1.193", "1.28.0"] {
            assert!(valid_version(version));
        }
        for version in [
            "1.secret-credential",
            "2.1.3 token",
            "2.1.3\nsecret",
            "version 2.1.3",
        ] {
            assert!(!valid_version(version));
        }
    }

    #[tokio::test]
    async fn unregistered_probe_fails_before_spawning() {
        assert!(probe("powershell").await.is_err());
    }

    #[tokio::test]
    #[ignore = "explicit installed-CLI version-only smoke test; no model or login calls"]
    async fn installed_application_probe() {
        let app = std::env::var("WONDERLAND_TEST_CLI_ID").expect("set a registered app ID");
        let result = probe(&app).await.unwrap();
        assert_eq!(result["authentication"]["status"], "unknown");
        assert_eq!(result["model_availability"]["status"], "unknown");
        assert_eq!(result["installed"], true);
        assert_eq!(result["version_probe"]["status"], "completed");
        assert!(result["sha256"]
            .as_str()
            .is_some_and(|value| value.len() == 64));
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    }
}

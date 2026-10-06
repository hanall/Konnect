use crate::hierarchy::{discover_sheet_tree, SheetEntry};
use crate::render::{render_all, render_some};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

const QUIET_WINDOW: Duration = Duration::from_millis(600);
const MAX_BURST_WAIT: Duration = Duration::from_secs(5);
const BROWSER_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize)]
pub struct PublicSheet {
    pub id: usize,
    pub name: String,
    pub file: String,
    pub depth: usize,
    pub rendered: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicViewerState {
    pub revision: u64,
    pub root_file: String,
    pub status: &'static str,
    pub source: &'static str,
    pub sheets: Vec<PublicSheet>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

struct ViewerData {
    sheets: Vec<SheetEntry>,
    cache: HashMap<String, String>,
    errors: Vec<String>,
    warnings: Vec<String>,
    status: &'static str,
}

#[derive(Default)]
struct BrowserEvidence {
    page: bool,
    state: bool,
    svg: bool,
    ui_ready: bool,
}

impl BrowserEvidence {
    fn is_ready(&self) -> bool {
        self.page && self.state && self.svg && self.ui_ready
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderHealth {
    pub total_sheets: usize,
    pub rendered_sheets: usize,
    pub render_errors: usize,
    pub render_warnings: usize,
}

enum WatchMessage {
    Paths(Vec<PathBuf>),
    Error(String),
}

pub struct ViewerState {
    root: PathBuf,
    cli: String,
    data: Mutex<ViewerData>,
    revision: AtomicU64,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    watched_dirs: Mutex<HashSet<PathBuf>>,
    render_lock: Mutex<()>,
    browser_evidence: Mutex<BrowserEvidence>,
    browser_seen_signal: Condvar,
    shutdown: AtomicBool,
    last_authenticated_request: Mutex<Instant>,
}

impl ViewerState {
    pub fn new(root: &Path, cli: String) -> Result<Arc<Self>, String> {
        let sheets = discover_sheet_tree(root)?;
        let root = PathBuf::from(
            sheets
                .first()
                .ok_or_else(|| "Schematic hierarchy is empty".to_string())?
                .file
                .clone(),
        );
        let report = render_all(&cli, &root, &sheets);
        let root_key = root.to_string_lossy().to_string();
        if !report.cache.contains_key(&root_key) {
            return Err(format!(
                "Failed to render root schematic from an isolated snapshot: {}",
                report.errors.join("; ")
            ));
        }
        let watched_dirs = compute_watch_dirs(&sheets);
        let state = Arc::new(Self {
            root,
            cli,
            data: Mutex::new(ViewerData {
                sheets,
                cache: report.cache,
                errors: report.errors,
                warnings: report.warnings,
                status: "ready",
            }),
            revision: AtomicU64::new(1),
            watcher: Mutex::new(None),
            watched_dirs: Mutex::new(watched_dirs.clone()),
            render_lock: Mutex::new(()),
            browser_evidence: Mutex::new(BrowserEvidence::default()),
            browser_seen_signal: Condvar::new(),
            shutdown: AtomicBool::new(false),
            last_authenticated_request: Mutex::new(Instant::now()),
        });
        let watcher = build_watcher(&state, watched_dirs)?;
        *state.watcher.lock().unwrap() = Some(watcher);
        Ok(state)
    }

    pub fn public_state(&self) -> PublicViewerState {
        let data = self.data.lock().unwrap();
        PublicViewerState {
            revision: self.revision.load(Ordering::Acquire),
            root_file: self.root.to_string_lossy().to_string(),
            status: data.status,
            source: "isolated_snapshot",
            sheets: data
                .sheets
                .iter()
                .enumerate()
                .map(|(id, entry)| PublicSheet {
                    id,
                    name: entry.name.clone(),
                    file: entry.file.clone(),
                    depth: entry.depth,
                    rendered: data.cache.contains_key(&entry.file),
                })
                .collect(),
            errors: data.errors.clone(),
            warnings: data.warnings.clone(),
        }
    }

    pub fn svg(&self, sheet_index: usize) -> Result<String, String> {
        let data = self.data.lock().unwrap();
        let sheet = data
            .sheets
            .get(sheet_index)
            .ok_or_else(|| format!("Unknown sheet index: {sheet_index}"))?;
        data.cache.get(&sheet.file).cloned().ok_or_else(|| {
            format!(
                "Sheet '{}' has no current render; inspect viewer errors",
                sheet.name
            )
        })
    }

    pub fn refresh_sheet(&self, sheet_index: usize) -> Result<(), String> {
        let _render_guard = self.render_lock.lock().unwrap();
        let (sheets, file, name) = {
            let mut data = self.data.lock().unwrap();
            let sheet = data
                .sheets
                .get(sheet_index)
                .ok_or_else(|| format!("Unknown sheet index: {sheet_index}"))?;
            let file = sheet.file.clone();
            let name = sheet.name.clone();
            data.status = "rendering";
            data.errors.clear();
            self.revision.fetch_add(1, Ordering::AcqRel);
            (data.sheets.clone(), file, name)
        };
        let subset: HashSet<String> = [file.clone()].into_iter().collect();
        let report = render_some(&self.cli, &self.root, &sheets, Some(&subset));
        let mut data = self.data.lock().unwrap();
        data.status = if report.errors.is_empty() {
            "ready"
        } else {
            "error"
        };
        data.errors = report.errors;
        data.warnings = report.warnings;
        data.cache.remove(&file);
        if let Some(svg) = report.cache.get(&file) {
            data.cache.insert(file, svg.clone());
        }
        self.revision.fetch_add(1, Ordering::AcqRel);
        if data.cache.contains_key(&data.sheets[sheet_index].file) {
            Ok(())
        } else {
            Err(format!(
                "Failed to refresh sheet '{name}': {}",
                data.errors.join("; ")
            ))
        }
    }

    pub fn open_in_kicad(&self, sheet_index: usize) -> Result<(), String> {
        let active = {
            let data = self.data.lock().unwrap();
            PathBuf::from(
                &data
                    .sheets
                    .get(sheet_index)
                    .ok_or_else(|| format!("Unknown sheet index: {sheet_index}"))?
                    .file,
            )
        };
        let project = self.root.with_extension("kicad_pro");
        let target = if project.exists() { project } else { active };
        let lock = kicad_lock_path(&target);
        if lock.exists() {
            return Err(format!(
                "'{}' is already open in another KiCad window",
                target.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        let binary = resolve_kicad_binary()?;
        Command::new(&binary)
            .arg(&target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                format!(
                    "Failed to launch KiCad {} for {}: {error}",
                    binary.display(),
                    target.display()
                )
            })?;
        Ok(())
    }

    pub fn mark_browser_page(&self) {
        self.update_browser_evidence(|evidence| evidence.page = true);
    }

    pub fn mark_browser_state(&self) {
        self.update_browser_evidence(|evidence| evidence.state = true);
    }

    pub fn mark_browser_svg(&self) {
        self.update_browser_evidence(|evidence| evidence.svg = true);
    }

    pub fn mark_browser_ui_ready(&self) {
        self.update_browser_evidence(|evidence| evidence.ui_ready = true);
    }

    fn update_browser_evidence(&self, update: impl FnOnce(&mut BrowserEvidence)) {
        let mut evidence = self.browser_evidence.lock().unwrap();
        update(&mut evidence);
        if evidence.is_ready() {
            self.browser_seen_signal.notify_all();
        }
    }

    pub fn wait_for_browser(&self, timeout: Duration) -> bool {
        let evidence = self.browser_evidence.lock().unwrap();
        if evidence.is_ready() {
            return true;
        }
        let (evidence, _) = self
            .browser_seen_signal
            .wait_timeout_while(evidence, timeout, |evidence| !evidence.is_ready())
            .unwrap();
        evidence.is_ready()
    }

    pub fn browser_seen(&self) -> bool {
        self.browser_evidence.lock().unwrap().page
    }

    pub fn browser_ready(&self) -> bool {
        self.browser_evidence.lock().unwrap().is_ready()
    }

    pub fn watcher_active(&self) -> bool {
        self.watcher.lock().unwrap().is_some()
    }

    pub fn touch_authenticated_request(&self) {
        *self.last_authenticated_request.lock().unwrap() = Instant::now();
    }

    pub fn authenticated_idle_for(&self) -> Duration {
        self.last_authenticated_request.lock().unwrap().elapsed()
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn render_health(&self) -> RenderHealth {
        let data = self.data.lock().unwrap();
        RenderHealth {
            total_sheets: data.sheets.len(),
            rendered_sheets: data
                .sheets
                .iter()
                .filter(|sheet| data.cache.contains_key(&sheet.file))
                .count(),
            render_errors: data.errors.len(),
            render_warnings: data.warnings.len(),
        }
    }

    fn report_watcher_error(&self, error: String) {
        let mut data = self.data.lock().unwrap();
        data.status = "error";
        data.errors = vec![error];
        self.revision.fetch_add(1, Ordering::AcqRel);
    }
}

fn compute_watch_dirs(entries: &[SheetEntry]) -> HashSet<PathBuf> {
    entries
        .iter()
        .filter_map(|entry| Path::new(&entry.file).parent().map(Path::to_path_buf))
        .collect()
}

fn is_relevant_schematic_change(path: &Path) -> bool {
    if path.extension().and_then(|value| value.to_str()) != Some("kicad_sch") {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    !(name.starts_with("_autosave-") || name.starts_with('~'))
}

fn build_watcher(
    state: &Arc<ViewerState>,
    initial_dirs: HashSet<PathBuf>,
) -> Result<notify::RecommendedWatcher, String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let weak = Arc::downgrade(state);
    spawn_render_worker(weak, receiver);
    let mut watcher = notify::recommended_watcher(
        move |result: Result<notify::Event, notify::Error>| match result {
            Ok(event) => {
                if !matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                ) {
                    return;
                }
                let paths: Vec<PathBuf> = event
                    .paths
                    .into_iter()
                    .filter(|path| is_relevant_schematic_change(path))
                    .collect();
                if !paths.is_empty() {
                    let _ = sender.send(WatchMessage::Paths(paths));
                }
            }
            Err(error) => {
                let _ = sender.send(WatchMessage::Error(format!("File watcher failed: {error}")));
            }
        },
    )
    .map_err(|error| format!("Failed to create file watcher: {error}"))?;
    for directory in initial_dirs {
        watcher
            .watch(&directory, RecursiveMode::NonRecursive)
            .map_err(|error| format!("Failed to watch {}: {error}", directory.display()))?;
    }
    Ok(watcher)
}

fn note_changed(changed: &mut HashSet<PathBuf>, paths: Vec<PathBuf>) {
    for path in paths {
        if let Ok(canonical) = path.canonicalize() {
            changed.insert(canonical);
        }
        changed.insert(path);
    }
}

fn drain_until_quiet(
    receiver: &Receiver<WatchMessage>,
    changed: &mut HashSet<PathBuf>,
) -> Result<bool, String> {
    let started = Instant::now();
    loop {
        if started.elapsed() >= MAX_BURST_WAIT {
            return Ok(true);
        }
        match receiver.recv_timeout(QUIET_WINDOW) {
            Ok(WatchMessage::Paths(paths)) => note_changed(changed, paths),
            Ok(WatchMessage::Error(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => return Ok(true),
            Err(RecvTimeoutError::Disconnected) => return Ok(false),
        }
    }
}

fn files_needing_render(
    entries: &[SheetEntry],
    changed: &HashSet<PathBuf>,
    cache: &HashMap<String, String>,
) -> HashSet<String> {
    let mut need = HashSet::new();
    for entry in entries {
        if cache.contains_key(&entry.file) {
            let raw = PathBuf::from(&entry.file);
            let canonical = raw.canonicalize().unwrap_or_else(|_| raw.clone());
            if !changed.contains(&raw) && !changed.contains(&canonical) {
                continue;
            }
        }
        need.insert(entry.file.clone());
    }
    need
}

fn spawn_render_worker(state: Weak<ViewerState>, receiver: Receiver<WatchMessage>) {
    std::thread::spawn(move || loop {
        let first = match receiver.recv() {
            Ok(WatchMessage::Paths(paths)) => paths,
            Ok(WatchMessage::Error(error)) => {
                if let Some(state) = state.upgrade() {
                    state.report_watcher_error(error);
                    continue;
                }
                return;
            }
            Err(_) => return,
        };
        let Some(state) = state.upgrade() else {
            return;
        };
        let _render_guard = state.render_lock.lock().unwrap();
        {
            let mut data = state.data.lock().unwrap();
            data.status = "rendering";
            data.errors.clear();
            state.revision.fetch_add(1, Ordering::AcqRel);
        }
        let mut changed = HashSet::new();
        note_changed(&mut changed, first);
        match drain_until_quiet(&receiver, &mut changed) {
            Ok(false) => return,
            Err(error) => {
                state.report_watcher_error(error);
                continue;
            }
            Ok(true) => {}
        }
        let new_sheets = match discover_sheet_tree(&state.root) {
            Ok(sheets) => sheets,
            Err(error) => {
                state.report_watcher_error(format!(
                    "Hierarchy refresh refused; prior render retained: {error}"
                ));
                continue;
            }
        };
        let old_cache = state.data.lock().unwrap().cache.clone();
        let need = files_needing_render(&new_sheets, &changed, &old_cache);
        let report = render_some(&state.cli, &state.root, &new_sheets, Some(&need));
        let mut cache = HashMap::new();
        for entry in &new_sheets {
            if let Some(svg) = report.cache.get(&entry.file) {
                cache.insert(entry.file.clone(), svg.clone());
            } else if !need.contains(&entry.file) {
                if let Some(svg) = old_cache.get(&entry.file) {
                    cache.insert(entry.file.clone(), svg.clone());
                }
            }
        }

        let new_dirs = compute_watch_dirs(&new_sheets);
        let mut watch_errors = Vec::new();
        if let Some(watcher) = state.watcher.lock().unwrap().as_mut() {
            let mut watched = state.watched_dirs.lock().unwrap();
            let added: Vec<PathBuf> = new_dirs.difference(&watched).cloned().collect();
            for directory in added {
                match watcher.watch(&directory, RecursiveMode::NonRecursive) {
                    Ok(()) => {
                        watched.insert(directory.clone());
                    }
                    Err(error) => watch_errors.push(format!(
                        "Failed to watch new sheet directory {}: {error}",
                        directory.display()
                    )),
                }
            }
            let obsolete: Vec<PathBuf> = watched.difference(&new_dirs).cloned().collect();
            for directory in obsolete {
                match watcher.unwatch(&directory) {
                    Ok(()) => {
                        watched.remove(&directory);
                    }
                    Err(error) => watch_errors.push(format!(
                        "Failed to stop watching obsolete directory {}: {error}",
                        directory.display()
                    )),
                }
            }
        }

        let mut data = state.data.lock().unwrap();
        data.sheets = new_sheets;
        data.cache = cache;
        data.errors = report.errors;
        data.errors.extend(watch_errors);
        data.warnings = report.warnings;
        data.status = if data.errors.is_empty() {
            "ready"
        } else {
            "error"
        };
        state.revision.fetch_add(1, Ordering::AcqRel);
    });
}

fn kicad_lock_path(target: &Path) -> PathBuf {
    let directory = target.parent().unwrap_or(Path::new("."));
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    directory.join(format!("~{name}.lck"))
}

fn resolve_kicad_binary() -> Result<PathBuf, String> {
    let candidates: Vec<PathBuf> = if cfg!(target_os = "windows") {
        vec![
            PathBuf::from(r"C:\KiCad\10.0\bin\kicad.exe"),
            PathBuf::from(r"C:\Program Files\KiCad\10.0\bin\kicad.exe"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![PathBuf::from(
            "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad",
        )]
    } else {
        vec![
            PathBuf::from("/usr/bin/kicad"),
            PathBuf::from("/usr/local/bin/kicad"),
        ]
    };
    candidates
        .into_iter()
        .find(|candidate| trusted_regular_executable(candidate))
        .ok_or_else(|| {
            "KiCad GUI executable was not found in a trusted system location".to_string()
        })
}

#[derive(Serialize)]
struct Readiness<'a> {
    url: &'a str,
    pid: u32,
    health: ReadinessHealth<'a>,
}

#[derive(Serialize)]
struct ReadinessHealth<'a> {
    bind: &'a str,
    browser_connected: bool,
    root_rendered: bool,
    auto_refresh_watcher: bool,
    source: &'a str,
    total_sheets: usize,
    rendered_sheets: usize,
    render_errors: usize,
    render_warnings: usize,
}

pub fn run_from_env() -> Result<(), String> {
    let (cli_override, schematic) = parse_args(std::env::args().skip(1))?;
    let cli = resolve_kicad_cli(cli_override)?;
    let state = ViewerState::new(&schematic, cli)?;
    let server = crate::server::start_server(Arc::clone(&state), Duration::from_secs(120))?;
    let url = server.url().to_string();
    if let Err(error) = open_browser(&url) {
        state.request_shutdown();
        let _ = server.wait();
        return Err(error);
    }
    if !state.wait_for_browser(BROWSER_CONNECT_TIMEOUT) {
        state.request_shutdown();
        let _ = server.wait();
        return Err(format!(
            "Browser launcher returned but no browser requested the capability URL within {} seconds",
            BROWSER_CONNECT_TIMEOUT.as_secs()
        ));
    }
    let readiness = Readiness {
        url: &url,
        pid: std::process::id(),
        health: {
            let render = state.render_health();
            ReadinessHealth {
                bind: "127.0.0.1",
                browser_connected: state.browser_ready(),
                root_rendered: state.svg(0).is_ok(),
                auto_refresh_watcher: state.watcher_active(),
                source: "isolated_snapshot",
                total_sheets: render.total_sheets,
                rendered_sheets: render.rendered_sheets,
                render_errors: render.render_errors,
                render_warnings: render.render_warnings,
            }
        },
    };
    let readiness = serde_json::to_string(&readiness)
        .map_err(|error| format!("Failed to serialize viewer readiness: {error}"))?;
    eprintln!("KONNECT_VIEWER_READY {readiness}");
    server.wait()
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<(Option<String>, PathBuf), String> {
    let mut cli = None;
    let mut schematic = None;
    let mut args = args.peekable();
    while let Some(argument) = args.next() {
        if argument == "--kicad-cli" {
            let value = args
                .next()
                .ok_or_else(|| "--kicad-cli requires a path".to_string())?;
            if value.is_empty() {
                return Err("--kicad-cli path must not be empty".to_string());
            }
            cli = Some(value);
        } else if argument.starts_with('-') {
            return Err(format!("Unknown viewer option: {argument}"));
        } else if schematic.replace(PathBuf::from(&argument)).is_some() {
            return Err("Exactly one root .kicad_sch path is required".to_string());
        }
    }
    let schematic = schematic.ok_or_else(|| "Root .kicad_sch path is required".to_string())?;
    Ok((cli, schematic))
}

fn resolve_kicad_cli(override_path: Option<String>) -> Result<String, String> {
    let candidates: Vec<PathBuf> = if cfg!(target_os = "windows") {
        vec![
            PathBuf::from(r"C:\KiCad\10.0\bin\kicad-cli.exe"),
            PathBuf::from(r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"),
            PathBuf::from("/usr/local/bin/kicad-cli"),
        ]
    } else {
        vec![
            PathBuf::from("/usr/bin/kicad-cli"),
            PathBuf::from("/usr/local/bin/kicad-cli"),
        ]
    };
    let resolved = resolve_kicad_cli_with_environment(
        override_path.as_deref(),
        std::env::var_os("PATH").as_deref(),
        &candidates,
    )?;
    resolved.into_os_string().into_string().map_err(|path| {
        format!(
            "Resolved kicad-cli path is not valid UTF-8: {}",
            PathBuf::from(path).display()
        )
    })
}

fn resolve_kicad_cli_with_environment(
    configured: Option<&str>,
    path_environment: Option<&std::ffi::OsStr>,
    official_candidates: &[PathBuf],
) -> Result<PathBuf, String> {
    if let Some(configured) = configured {
        let configured_path = Path::new(configured);
        if is_bare_executable_name(configured_path) {
            if let Some(path) = resolve_bare_executable(configured_path, path_environment) {
                return Ok(path);
            }
            if matches!(configured, "kicad-cli" | "kicad-cli.exe") {
                if let Some(path) = resolve_official_executable(official_candidates) {
                    return Ok(path);
                }
            }
            return Err(format!(
                "Configured bare kicad-cli executable '{configured}' was not found in an absolute PATH entry or trusted official location"
            ));
        }

        if !trusted_regular_executable(configured_path) {
            return Err(format!(
                "Configured kicad-cli is not a trusted regular executable: {}",
                configured_path.display()
            ));
        }
        return configured_path
            .canonicalize()
            .map_err(|error| format!("Failed to resolve configured kicad-cli: {error}"));
    }

    resolve_official_executable(official_candidates)
        .ok_or_else(|| "kicad-cli was not found in a trusted system location".to_string())
}

fn is_bare_executable_name(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

fn resolve_bare_executable(
    name: &Path,
    path_environment: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let path_environment = path_environment?;
    for directory in std::env::split_paths(path_environment) {
        // Empty PATH components and relative directories invoke cwd-dependent
        // process search semantics. A configured bare name must never gain
        // that implicit authority.
        if !directory.is_absolute() {
            continue;
        }
        if let Some(executable) = canonical_trusted_executable(&directory.join(name)) {
            return Some(executable);
        }
    }
    None
}

fn resolve_official_executable(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find_map(|candidate| canonical_trusted_executable(candidate))
}

fn canonical_trusted_executable(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    if canonical.is_absolute() && trusted_regular_executable(&canonical) {
        Some(canonical)
    } else {
        None
    }
}

fn open_browser(url: &str) -> Result<(), String> {
    let mut command = if cfg!(target_os = "windows") {
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .ok_or_else(|| "SystemRoot is unavailable".to_string())?;
        let binary = system_root.join("System32").join("cmd.exe");
        let mut command = Command::new(binary);
        command.args(["/D", "/S", "/C", "start", "", url]);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("/usr/bin/open");
        command.arg(url);
        command
    } else {
        let binary = ["/usr/bin/xdg-open", "/usr/local/bin/xdg-open"]
            .into_iter()
            .find(|candidate| trusted_regular_executable(Path::new(candidate)))
            .ok_or_else(|| "xdg-open was not found in a trusted system location".to_string())?;
        let mut command = Command::new(binary);
        command.arg(url);
        command
    };
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("Failed to run the browser launcher: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Browser launcher exited with {status}"))
    }
}

fn trusted_regular_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_schematic_editor::Schematic;
    use tempfile::TempDir;

    fn blank_schematic(directory: &Path, name: &str) -> PathBuf {
        let schematic = directory.join(name);
        let template = "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n";
        std::fs::write(&schematic, template).unwrap();
        Schematic::load(&schematic).unwrap().overwrite().unwrap();
        schematic
    }

    #[cfg(unix)]
    fn fake_cli(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join("fake-kicad-cli");
        std::fs::write(
            &path,
            "#!/bin/sh\nset -eu\nout=''; last=''; while [ $# -gt 0 ]; do if [ \"$1\" = --output ]; then shift; out=$1; fi; last=$1; shift; done\nstem=$(basename \"$last\" .kicad_sch)\nprintf '<svg viewBox=\"0 0 100 100\"></svg>' > \"$out/$stem.svg\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn schematic_autosaves_and_locks_do_not_trigger_rendering() {
        assert!(!is_relevant_schematic_change(Path::new(
            "/project/_autosave-root.kicad_sch"
        )));
        assert!(!is_relevant_schematic_change(Path::new(
            "/project/~root.kicad_sch"
        )));
        assert!(is_relevant_schematic_change(Path::new(
            "/project/root.kicad_sch"
        )));
    }

    #[test]
    fn argument_parser_rejects_unknown_or_ambiguous_input() {
        assert!(parse_args(["--unknown".to_string()].into_iter()).is_err());
        assert!(
            parse_args(["a.kicad_sch".to_string(), "b.kicad_sch".to_string()].into_iter()).is_err()
        );
        assert!(parse_args(["--kicad-cli".to_string()].into_iter()).is_err());
    }

    #[test]
    fn lock_path_matches_kicad_convention() {
        assert_eq!(
            kicad_lock_path(Path::new("/project/design.kicad_pro")),
            PathBuf::from("/project/~design.kicad_pro.lck")
        );
    }

    #[test]
    fn installed_kicad_cli_renders_an_isolated_snapshot_when_available() {
        let Ok(cli) = resolve_kicad_cli(Some("kicad-cli".to_string())) else {
            return;
        };
        assert!(Path::new(&cli).is_absolute());
        assert!(trusted_regular_executable(Path::new(&cli)));
        let temporary = TempDir::new().unwrap();
        let schematic = blank_schematic(temporary.path(), "runtime.kicad_sch");

        let state = ViewerState::new(&schematic, cli).unwrap();
        let svg = state.svg(0).unwrap();
        assert!(svg.contains("<svg"));
        assert_eq!(state.public_state().source, "isolated_snapshot");
    }

    #[cfg(unix)]
    #[test]
    fn auto_refresh_watcher_observes_a_real_save_and_renders_again() {
        let temporary = TempDir::new().unwrap();
        let schematic = blank_schematic(temporary.path(), "watch.kicad_sch");
        let cli = fake_cli(temporary.path());
        let state = ViewerState::new(&schematic, cli.to_string_lossy().to_string()).unwrap();
        let initial_revision = state.public_state().revision;

        let mut bytes = std::fs::read(&schematic).unwrap();
        bytes.extend_from_slice(b"\n");
        std::fs::write(&schematic, bytes).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let current = state.public_state();
            if current.revision >= initial_revision + 2 && current.status == "ready" {
                assert!(state.svg(0).is_ok());
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "watcher did not complete an auto-refresh: {:?}",
            state.public_state()
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_refresh_removes_stale_svg_and_reports_the_error() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = TempDir::new().unwrap();
        let schematic = blank_schematic(temporary.path(), "failure.kicad_sch");
        let cli = fake_cli(temporary.path());
        let state = ViewerState::new(&schematic, cli.to_string_lossy().to_string()).unwrap();
        assert!(state.svg(0).is_ok());

        std::fs::write(&cli, "#!/bin/sh\necho deliberate-failure >&2\nexit 9\n").unwrap();
        let mut permissions = std::fs::metadata(&cli).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&cli, permissions).unwrap();

        let error = state.refresh_sheet(0).unwrap_err();
        assert!(error.contains("deliberate-failure"), "{error}");
        assert!(
            state.svg(0).is_err(),
            "stale SVG must not survive a failed refresh"
        );
        let public = state.public_state();
        assert_eq!(public.status, "error");
        assert!(!public.errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn bare_kicad_cli_resolves_exactly_from_absolute_path_entries() {
        let temporary = TempDir::new().unwrap();
        let bin = temporary.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let executable = fake_cli(&bin);
        let path =
            std::env::join_paths([PathBuf::new(), PathBuf::from("relative-bin"), bin.clone()])
                .unwrap();

        let resolved =
            resolve_kicad_cli_with_environment(Some("fake-kicad-cli"), Some(path.as_os_str()), &[])
                .unwrap();
        assert_eq!(resolved, executable.canonicalize().unwrap());
        assert!(resolved.is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn bare_name_ignores_empty_and_relative_path_entries() {
        let relative_root = tempfile::Builder::new()
            .prefix("viewer-relative-path-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        fake_cli(relative_root.path());
        let relative = relative_root.path().file_name().unwrap().to_os_string();
        let path = std::env::join_paths([PathBuf::new(), PathBuf::from(relative)]).unwrap();

        let error =
            resolve_kicad_cli_with_environment(Some("fake-kicad-cli"), Some(path.as_os_str()), &[])
                .unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn official_candidate_is_used_when_safe_path_lookup_misses() {
        let temporary = TempDir::new().unwrap();
        let executable = fake_cli(temporary.path());
        let resolved = resolve_kicad_cli_with_environment(
            Some("kicad-cli"),
            Some(std::ffi::OsStr::new("")),
            std::slice::from_ref(&executable),
        )
        .unwrap();
        assert_eq!(resolved, executable.canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_relative_path_keeps_its_meaning_and_never_falls_back_to_path() {
        let current = std::env::current_dir().unwrap();
        let explicit_root = tempfile::Builder::new()
            .prefix("viewer-explicit-path-")
            .tempdir_in(&current)
            .unwrap();
        let explicit = fake_cli(explicit_root.path());
        let relative = explicit.strip_prefix(&current).unwrap();

        let path_only_root = TempDir::new().unwrap();
        fake_cli(path_only_root.path());
        let path = std::env::join_paths([path_only_root.path()]).unwrap();
        let resolved = resolve_kicad_cli_with_environment(
            Some(relative.to_str().unwrap()),
            Some(path.as_os_str()),
            &[],
        )
        .unwrap();
        assert_eq!(resolved, explicit.canonicalize().unwrap());

        let missing = explicit_root.path().join("missing-kicad-cli");
        let missing = missing.strip_prefix(&current).unwrap().to_str().unwrap();
        assert!(
            resolve_kicad_cli_with_environment(Some(missing), Some(path.as_os_str()), &[]).is_err()
        );
    }
}

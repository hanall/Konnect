use crate::hierarchy::SheetEntry;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROJECT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SVG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const RENDER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PARALLEL_RENDERS: usize = 8;

pub struct SnapshotTree {
    _directory: TempDir,
    pub sources: HashMap<String, PathBuf>,
    pub output_root: PathBuf,
}

#[derive(Debug)]
struct RenderedSvg {
    svg: String,
    warning: Option<String>,
}

#[derive(Debug, Default)]
pub struct RenderReport {
    pub cache: HashMap<String, String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn snapshot_tree(root: &Path, entries: &[SheetEntry]) -> Result<SnapshotTree, String> {
    let root = root.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve root schematic {}: {error}",
            root.display()
        )
    })?;
    let base = root
        .parent()
        .ok_or_else(|| "Root schematic has no parent directory".to_string())?;
    let directory = tempfile::Builder::new()
        .prefix("konnect-viewer-")
        .tempdir()
        .map_err(|error| format!("Failed to create secure render directory: {error}"))?;
    let source_root = directory.path().join("source");
    std::fs::create_dir(&source_root)
        .map_err(|error| format!("Failed to create snapshot source directory: {error}"))?;
    let output_root = directory.path().join("output");
    std::fs::create_dir(&output_root)
        .map_err(|error| format!("Failed to create snapshot output directory: {error}"))?;

    let mut sources = HashMap::new();
    let mut seen = HashSet::new();
    for entry in entries {
        if !seen.insert(entry.file.as_str()) {
            continue;
        }
        let live = Path::new(&entry.file);
        let canonical = live.canonicalize().map_err(|error| {
            format!(
                "Failed to resolve schematic sheet {}: {error}",
                live.display()
            )
        })?;
        if canonical != live {
            return Err(format!(
                "Schematic sheet changed identity before snapshot: {}",
                live.display()
            ));
        }
        let relative = canonical.strip_prefix(base).map_err(|_| {
            format!(
                "Refusing live-file fallback for sheet outside project directory: {}",
                canonical.display()
            )
        })?;
        let destination = source_root.join(relative);
        copy_regular_file(
            &canonical,
            &destination,
            MAX_SOURCE_BYTES,
            "schematic sheet",
        )?;
        sources.insert(entry.file.clone(), destination);
    }

    let project = root.with_extension("kicad_pro");
    if project.exists() {
        let metadata = std::fs::symlink_metadata(&project).map_err(|error| {
            format!(
                "Failed to inspect project file {}: {error}",
                project.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "Project file must be a regular non-symlink file: {}",
                project.display()
            ));
        }
        let name = project
            .file_name()
            .ok_or_else(|| "Project file has no filename".to_string())?;
        copy_regular_file(
            &project,
            &source_root.join(name),
            MAX_PROJECT_BYTES,
            "project file",
        )?;
    }

    Ok(SnapshotTree {
        _directory: directory,
        sources,
        output_root,
    })
}

fn copy_regular_file(
    source: &Path,
    destination: &Path,
    max_bytes: u64,
    subject: &str,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("Failed to inspect {subject} {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{subject} must be a regular non-symlink file: {}",
            source.display()
        ));
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{subject} exceeds the {max_bytes}-byte snapshot limit: {}",
            source.display()
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "Snapshot destination has no parent: {}",
            destination.display()
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Failed to create snapshot directory {}: {error}",
            parent.display()
        )
    })?;
    let mut input = File::open(source)
        .map_err(|error| format!("Failed to read {subject} {}: {error}", source.display()))?;
    let open_metadata = input
        .metadata()
        .map_err(|error| format!("Failed to inspect open {subject}: {error}"))?;
    if !open_metadata.is_file() {
        return Err(format!("Open {subject} is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.dev() != open_metadata.dev() || metadata.ino() != open_metadata.ino() {
            return Err(format!(
                "{subject} changed identity before snapshot read: {}",
                source.display()
            ));
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut input)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Failed to read {subject} {}: {error}", source.display()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{subject} grew beyond the snapshot size limit"));
    }
    let canonical_after = source.canonicalize().map_err(|error| {
        format!(
            "Failed to re-resolve {subject} {}: {error}",
            source.display()
        )
    })?;
    if canonical_after != source {
        return Err(format!(
            "{subject} changed identity during snapshot: {}",
            source.display()
        ));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| {
            format!(
                "Failed to create isolated snapshot {}: {error}",
                destination.display()
            )
        })?;
    output.write_all(&bytes).map_err(|error| {
        format!(
            "Failed to write isolated snapshot {}: {error}",
            destination.display()
        )
    })?;
    output
        .sync_all()
        .map_err(|error| format!("Failed to flush isolated snapshot: {error}"))?;
    Ok(())
}

fn render_to_svg_in(cli: &str, schematic: &Path, output_dir: &Path) -> Result<RenderedSvg, String> {
    render_to_svg_in_with_timeout(cli, schematic, output_dir, RENDER_TIMEOUT)
}

fn render_to_svg_in_with_timeout(
    cli: &str,
    schematic: &Path,
    output_dir: &Path,
    timeout: Duration,
) -> Result<RenderedSvg, String> {
    std::fs::create_dir(output_dir)
        .map_err(|error| format!("Failed to create render output directory: {error}"))?;
    let stderr_path = output_dir.join("kicad-cli.stderr");
    let stderr_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stderr_path)
        .map_err(|error| format!("Failed to create render diagnostic file: {error}"))?;

    let mut command = Command::new(cli);
    command
        .args(["sch", "export", "svg", "--output"])
        .arg(output_dir)
        .arg(schematic)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to run kicad-cli ({cli}): {error}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "kicad-cli render exceeded the {}-second timeout",
                    timeout.as_secs_f64()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Failed to inspect kicad-cli process: {error}"));
            }
        }
    };

    let stderr = read_diagnostic_tail(&stderr_path);
    let stem = schematic
        .file_stem()
        .ok_or_else(|| format!("Schematic has no filename stem: {}", schematic.display()))?
        .to_string_lossy();
    let svg_path = output_dir.join(format!("{stem}.svg"));
    let svg_metadata = std::fs::metadata(&svg_path);
    let produced = svg_metadata
        .as_ref()
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false);
    if !status.success() && !produced {
        return Err(format!(
            "kicad-cli failed with {status}: {}",
            if stderr.is_empty() {
                "no stderr output"
            } else {
                stderr.as_str()
            }
        ));
    }
    let metadata = svg_metadata.map_err(|error| {
        format!(
            "Failed to inspect rendered SVG {}: {error}",
            svg_path.display()
        )
    })?;
    if metadata.len() > MAX_SVG_BYTES {
        return Err(format!(
            "Rendered SVG exceeds the {MAX_SVG_BYTES}-byte limit: {}",
            svg_path.display()
        ));
    }
    let svg = std::fs::read_to_string(&svg_path).map_err(|error| {
        format!(
            "Failed to read rendered SVG {}: {error}",
            svg_path.display()
        )
    })?;
    validate_svg(&svg)?;
    let warning = (!status.success()).then(|| {
        format!(
            "kicad-cli exited with {status} but produced a non-empty validated SVG{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        )
    });
    Ok(RenderedSvg { svg, warning })
}

fn read_diagnostic_tail(path: &Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(MAX_STDERR_BYTES);
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}

fn validate_svg(svg: &str) -> Result<(), String> {
    let prefix = svg.trim_start_matches(|character: char| character.is_whitespace());
    if !(prefix.starts_with("<svg") || (prefix.starts_with("<?xml") && prefix.contains("<svg"))) {
        return Err("kicad-cli output is not an SVG document".to_string());
    }
    if !svg.contains("</svg>") {
        return Err("kicad-cli output is a truncated SVG document".to_string());
    }
    Ok(())
}

pub fn render_all(cli: &str, root: &Path, entries: &[SheetEntry]) -> RenderReport {
    render_some(cli, root, entries, None)
}

pub fn render_some(
    cli: &str,
    root: &Path,
    entries: &[SheetEntry],
    subset: Option<&HashSet<String>>,
) -> RenderReport {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        if seen.insert(entry.file.as_str())
            && subset
                .map(|subset| subset.contains(entry.file.as_str()))
                .unwrap_or(true)
        {
            unique.push((entry.file.clone(), entry.name.clone()));
        }
    }
    if unique.is_empty() {
        return RenderReport::default();
    }
    let snapshot = match snapshot_tree(root, entries) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return RenderReport {
                errors: unique
                    .iter()
                    .map(|(_, name)| format!("{name}: isolated snapshot failed: {error}"))
                    .collect(),
                ..RenderReport::default()
            };
        }
    };

    let mut report = RenderReport::default();
    for (chunk_index, chunk) in unique.chunks(MAX_PARALLEL_RENDERS).enumerate() {
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(index, (file, name))| {
                    let source = snapshot.sources.get(file).cloned();
                    let output = snapshot.output_root.join(format!("{chunk_index}-{index}"));
                    scope.spawn(move || {
                        let result = source
                            .ok_or_else(|| {
                                "Isolated snapshot does not contain this sheet".to_string()
                            })
                            .and_then(|source| render_to_svg_in(cli, &source, &output));
                        (file.clone(), name.clone(), result)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join())
                .collect::<Vec<_>>()
        });
        for result in results {
            match result {
                Ok((file, name, Ok(rendered))) => {
                    if let Some(warning) = rendered.warning {
                        report.warnings.push(format!("{name}: {warning}"));
                    }
                    report.cache.insert(file, rendered.svg);
                }
                Ok((_, name, Err(error))) => report.errors.push(format!("{name}: {error}")),
                Err(_) => report
                    .errors
                    .push("Renderer worker panicked before producing evidence".to_string()),
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_schematic_editor::Schematic;

    fn blank_schematic(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        let template = "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n";
        std::fs::write(&path, template).unwrap();
        Schematic::load(&path).unwrap().overwrite().unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn snapshot_is_unique_private_and_never_uses_live_sheet_paths() {
        let project = TempDir::new().unwrap();
        let root = blank_schematic(project.path(), "root.kicad_sch");
        std::fs::write(project.path().join("root.kicad_pro"), "{}").unwrap();
        let entry = SheetEntry {
            name: "root".to_string(),
            file: root.to_str().unwrap().to_string(),
            depth: 0,
        };
        let first = snapshot_tree(&root, std::slice::from_ref(&entry)).unwrap();
        let second = snapshot_tree(&root, std::slice::from_ref(&entry)).unwrap();
        let first_source = first.sources.get(&entry.file).unwrap();
        assert_ne!(first_source, &root);
        assert_ne!(first_source, second.sources.get(&entry.file).unwrap());
        assert_eq!(
            std::fs::read(first_source).unwrap(),
            std::fs::read(&root).unwrap()
        );
        assert!(first_source
            .parent()
            .unwrap()
            .join("root.kicad_pro")
            .exists());
    }

    #[test]
    fn snapshot_refuses_an_entry_outside_the_root_directory() {
        let project = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let root = blank_schematic(project.path(), "root.kicad_sch");
        let external = blank_schematic(outside.path(), "external.kicad_sch");
        let entries = vec![
            SheetEntry {
                name: "root".to_string(),
                file: root.to_str().unwrap().to_string(),
                depth: 0,
            },
            SheetEntry {
                name: "external".to_string(),
                file: external.to_str().unwrap().to_string(),
                depth: 1,
            },
        ];
        let error = snapshot_tree(&root, &entries).err().unwrap();
        assert!(error.contains("Refusing live-file fallback"), "{error}");
    }

    #[cfg(unix)]
    fn shell_script(directory: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join("fake-kicad-cli");
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn renderer_reports_nonzero_exit_while_preserving_valid_artifact() {
        let temporary = TempDir::new().unwrap();
        let schematic = blank_schematic(temporary.path(), "root.kicad_sch");
        let cli = shell_script(
            temporary.path(),
            "out=''; last=''; while [ $# -gt 0 ]; do if [ \"$1\" = --output ]; then shift; out=$1; fi; last=$1; shift; done; stem=$(basename \"$last\" .kicad_sch); printf '<svg></svg>' > \"$out/$stem.svg\"; echo diagnostic >&2; exit 7",
        );
        let output = temporary.path().join("output");
        let rendered = render_to_svg_in_with_timeout(
            cli.to_str().unwrap(),
            &schematic,
            &output,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(rendered.svg, "<svg></svg>");
        assert!(rendered.warning.unwrap().contains("exit status: 7"));
    }

    #[cfg(unix)]
    #[test]
    fn renderer_kills_a_timed_out_child() {
        let temporary = TempDir::new().unwrap();
        let schematic = blank_schematic(temporary.path(), "root.kicad_sch");
        let cli = shell_script(temporary.path(), "sleep 5");
        let error = render_to_svg_in_with_timeout(
            cli.to_str().unwrap(),
            &schematic,
            &temporary.path().join("output"),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.contains("timeout"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn renderer_can_only_mutate_its_disposable_snapshot() {
        let temporary = TempDir::new().unwrap();
        let schematic = blank_schematic(temporary.path(), "root.kicad_sch");
        let before = std::fs::read(&schematic).unwrap();
        let cli = shell_script(
            temporary.path(),
            "out=''; last=''; while [ $# -gt 0 ]; do if [ \"$1\" = --output ]; then shift; out=$1; fi; last=$1; shift; done; printf '\\n(snapshot-only-mutation)\\n' >> \"$last\"; stem=$(basename \"$last\" .kicad_sch); printf '<svg></svg>' > \"$out/$stem.svg\"",
        );
        let entry = SheetEntry {
            name: "root".to_string(),
            file: schematic.to_string_lossy().to_string(),
            depth: 0,
        };
        let report = render_all(cli.to_str().unwrap(), &schematic, &[entry]);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(std::fs::read(&schematic).unwrap(), before);
    }
}

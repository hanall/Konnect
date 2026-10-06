use konnect_schematic_editor::Schematic;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

pub const MAX_SHEET_DEPTH: usize = 20;
pub const MAX_SHEETS: usize = 256;

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct SheetEntry {
    pub name: String,
    pub file: String,
    pub depth: usize,
}

pub fn discover_sheet_tree(root: &Path) -> Result<Vec<SheetEntry>, String> {
    reject_parent_components(root, "root schematic")?;
    reject_symlink_components(root, "root schematic")?;
    let root = canonical_regular_file(root, "root schematic")?;
    if root.extension().and_then(|value| value.to_str()) != Some("kicad_sch") {
        return Err(format!(
            "Root schematic must end in .kicad_sch: {}",
            root.display()
        ));
    }
    let base = root
        .parent()
        .ok_or_else(|| "Root schematic has no parent directory".to_string())?
        .to_path_buf();
    let root_name = root
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "Root schematic filename is not valid UTF-8".to_string())?;
    let mut entries = Vec::new();
    let mut ancestors = HashSet::new();
    walk_sheet_tree_inner(&root, root_name, 0, &base, &mut ancestors, &mut entries)?;
    Ok(entries)
}

fn walk_sheet_tree_inner(
    path: &Path,
    display_name: &str,
    depth: usize,
    base: &Path,
    ancestors: &mut HashSet<PathBuf>,
    entries: &mut Vec<SheetEntry>,
) -> Result<(), String> {
    if depth > MAX_SHEET_DEPTH {
        return Err(format!(
            "Schematic hierarchy exceeds the maximum depth of {MAX_SHEET_DEPTH}: {}",
            path.display()
        ));
    }
    if entries.len() >= MAX_SHEETS {
        return Err(format!(
            "Schematic hierarchy exceeds the maximum sheet count of {MAX_SHEETS}"
        ));
    }
    let canonical = canonical_regular_file(path, "schematic sheet")?;
    if !canonical.starts_with(base) {
        return Err(format!(
            "Schematic sheet escapes the root project directory: {}",
            canonical.display()
        ));
    }
    if !ancestors.insert(canonical.clone()) {
        return Err(format!(
            "Schematic hierarchy contains a reference cycle at {}",
            canonical.display()
        ));
    }
    let file = canonical
        .to_str()
        .ok_or_else(|| format!("Schematic path is not valid UTF-8: {}", canonical.display()))?
        .to_string();
    entries.push(SheetEntry {
        name: display_name.to_string(),
        file,
        depth,
    });

    let schematic = Schematic::load(&canonical)
        .map_err(|error| format!("Failed to parse {}: {error}", canonical.display()))?;
    let directory = canonical
        .parent()
        .ok_or_else(|| format!("Schematic has no parent directory: {}", canonical.display()))?;
    for sheet in &schematic.sheets {
        let reference = Path::new(sheet.file());
        reject_parent_components(reference, "child-sheet reference")?;
        if reference.is_absolute() {
            return Err(format!(
                "Absolute child-sheet reference is not allowed: {}",
                reference.display()
            ));
        }
        let child = directory.join(reference);
        reject_symlink_components(&child, "child-sheet reference")?;
        let child = canonical_regular_file(&child, "child schematic")?;
        if !child.starts_with(base) {
            return Err(format!(
                "Child-sheet reference escapes the root project directory: {}",
                child.display()
            ));
        }
        walk_sheet_tree_inner(&child, sheet.name(), depth + 1, base, ancestors, entries)?;
    }
    ancestors.remove(&canonical);
    Ok(())
}

fn reject_parent_components(path: &Path, subject: &str) -> Result<(), String> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Parent-directory traversal is not allowed in {subject}: {}",
            path.display()
        ));
    }
    Ok(())
}

fn reject_symlink_components(path: &Path, subject: &str) -> Result<(), String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Failed to resolve current directory: {error}"))?
            .join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "Symbolic links are not allowed in {subject}: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("{subject} not found: {}", current.display()));
            }
            Err(error) => {
                return Err(format!(
                    "Failed to inspect {subject} {}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

fn canonical_regular_file(path: &Path, subject: &str) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Failed to resolve {subject} {}: {error}", path.display()))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| format!("Failed to inspect {subject} {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{subject} is not a regular file: {}",
            path.display()
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_schematic_editor::{Schematic, Sheet};
    use tempfile::TempDir;

    fn blank_schematic(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        let template = "(kicad_sch\n\t(version 20250610)\n\t(generator \"konnect\")\n\t(generator_version \"10.0\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t)\n)\n";
        std::fs::write(&path, template).unwrap();
        Schematic::load(&path).unwrap().overwrite().unwrap();
        path
    }

    fn add_sheet(parent: &Path, name: &str, file: &str) {
        let mut schematic = Schematic::load(parent).unwrap();
        schematic.add_sheet(Sheet::new(name, file, 50.0, 50.0, 80.0, 50.0));
        schematic.overwrite().unwrap();
    }

    #[test]
    fn discovers_nested_and_multi_instance_sheets_in_depth_first_order() {
        let temporary = TempDir::new().unwrap();
        let root = blank_schematic(temporary.path(), "root.kicad_sch");
        let child = blank_schematic(temporary.path(), "child.kicad_sch");
        add_sheet(&root, "First", "child.kicad_sch");
        add_sheet(&root, "Second", "child.kicad_sch");
        let entries = discover_sheet_tree(&root).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "root");
        assert_eq!(entries[1].name, "First");
        assert_eq!(entries[2].name, "Second");
        assert_eq!(
            entries[1].file,
            child.canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn missing_child_is_an_explicit_failure() {
        let temporary = TempDir::new().unwrap();
        let root = blank_schematic(temporary.path(), "root.kicad_sch");
        add_sheet(&root, "Missing", "missing.kicad_sch");
        let error = discover_sheet_tree(&root).unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }

    #[test]
    fn parent_traversal_is_refused_even_when_target_exists() {
        let project_parent = TempDir::new().unwrap();
        let project = project_parent.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let root = blank_schematic(&project, "root.kicad_sch");
        blank_schematic(project_parent.path(), "outside.kicad_sch");
        add_sheet(&root, "Outside", "../outside.kicad_sch");
        let error = discover_sheet_tree(&root).unwrap_err();
        assert!(error.contains("Parent-directory traversal"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_sheet_is_refused() {
        use std::os::unix::fs::symlink;
        let temporary = TempDir::new().unwrap();
        let root = blank_schematic(temporary.path(), "root.kicad_sch");
        let real = blank_schematic(temporary.path(), "real.kicad_sch");
        symlink(real, temporary.path().join("linked.kicad_sch")).unwrap();
        add_sheet(&root, "Linked", "linked.kicad_sch");
        let error = discover_sheet_tree(&root).unwrap_err();
        assert!(error.contains("Symbolic links"), "{error}");
    }

    #[test]
    fn genuine_cycle_is_an_explicit_failure() {
        let temporary = TempDir::new().unwrap();
        let root = blank_schematic(temporary.path(), "root.kicad_sch");
        let child = blank_schematic(temporary.path(), "child.kicad_sch");
        add_sheet(&root, "Child", "child.kicad_sch");
        add_sheet(&child, "Root", "root.kicad_sch");
        let error = discover_sheet_tree(&root).unwrap_err();
        assert!(error.contains("reference cycle"), "{error}");
    }
}

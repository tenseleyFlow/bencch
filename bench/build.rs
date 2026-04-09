use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR for build.rs"),
    );
    let manifest_path = manifest_dir.join("Cargo.toml");

    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let manifest = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("cannot read {}: {}", manifest_path.display(), err));

    if let Some(path) = armfortas_dependency_path(&manifest) {
        let resolved = normalize_dependency_path(&manifest_dir, &path);
        println!(
            "cargo:rustc-env=BENCCH_LINKED_ARMFORTAS_ROOT={}",
            resolved.display()
        );
        println!(
            "cargo:rustc-env=BENCCH_LINKED_ARMFORTAS_MANIFEST={}",
            resolved.join("Cargo.toml").display()
        );
    }
}

fn armfortas_dependency_path(manifest: &str) -> Option<String> {
    let mut in_dependencies = false;

    for raw_line in manifest.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            in_dependencies = line == "[dependencies]";
            continue;
        }

        if !in_dependencies {
            continue;
        }

        if !(line.starts_with("armfortas =") || line.starts_with("armfortas=")) {
            continue;
        }

        if let Some(path) = extract_inline_path(line) {
            return Some(path);
        }
    }

    None
}

fn extract_inline_path(line: &str) -> Option<String> {
    let path_idx = line.find("path")?;
    let path_fragment = &line[path_idx + "path".len()..];
    let eq_idx = path_fragment.find('=')?;
    let after_eq = path_fragment[eq_idx + 1..].trim_start();
    let quote = after_eq.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &after_eq[quote.len_utf8()..];
    let end_idx = rest.find(quote)?;
    Some(rest[..end_idx].to_string())
}

fn normalize_dependency_path(manifest_dir: &Path, configured: &str) -> PathBuf {
    let configured_path = Path::new(configured);
    if configured_path.is_absolute() {
        configured_path.to_path_buf()
    } else {
        manifest_dir.join(configured_path)
    }
}

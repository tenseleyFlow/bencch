use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::{sanitize_component, ArmfortasCliAdapter, ToolchainConfig};

const CATALOG_EXTENSION: &str = "afproj";

#[derive(Debug, Clone)]
pub(crate) enum ProjectCommand {
    List {
        catalog_filter: Option<String>,
        include_deprioritized: bool,
    },
    Run(ProjectRunConfig),
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectRunConfig {
    pub(crate) catalog_filter: Option<String>,
    pub(crate) project_filter: Option<String>,
    pub(crate) keep_workdir: bool,
    pub(crate) tools: ToolchainConfig,
    pub(crate) reference: ProjectCompiler,
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectRunOutcome {
    pub(crate) kept_workdirs: Vec<PathBuf>,
    pub(crate) summary_lines: Vec<String>,
    pub(crate) success: bool,
}

#[derive(Debug, Clone)]
struct ProjectCatalog {
    name: String,
    path: PathBuf,
    projects: Vec<ProjectSpec>,
}

#[derive(Debug, Clone)]
struct ProjectSpec {
    name: String,
    source: PathBuf,
    native_build: String,
    priority: usize,
    status: ProjectStatus,
    coverage: Vec<String>,
    library_seed: Vec<String>,
    build_command: Option<String>,
    test_command: Option<String>,
    smoke_command: Option<String>,
    notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectStatus {
    Active,
    Later,
    Deprioritized,
}

impl ProjectStatus {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "active" => Ok(Self::Active),
            "later" => Ok(Self::Later),
            "deprioritized" => Ok(Self::Deprioritized),
            other => Err(format!("unknown project status '{}'", other)),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Later => "later",
            Self::Deprioritized => "deprioritized",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectCompiler {
    Armfortas,
    FlangNew,
    Gfortran,
}

impl ProjectCompiler {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Armfortas => "armfortas",
            Self::FlangNew => "flang-new",
            Self::Gfortran => "gfortran",
        }
    }
}

/// The reference compiler the campaign diffs armfortas against. Chosen
/// per host, not per catalog: flang-new on macOS, gfortran on the ELF
/// targets (Linux, FreeBSD), where flang-new is not the ecosystem
/// reference. Overridable with `--reference`.
fn default_reference_compiler() -> ProjectCompiler {
    if cfg!(target_os = "macos") {
        ProjectCompiler::FlangNew
    } else {
        ProjectCompiler::Gfortran
    }
}

#[derive(Debug, Clone)]
struct ProjectExecution {
    compiler: ProjectCompiler,
    compiler_bin: String,
    cc_bin: String,
    workdir: PathBuf,
    build: StepExecution,
    test: Option<StepExecution>,
    smoke: Option<StepExecution>,
}

#[derive(Debug, Clone)]
struct StepExecution {
    label: &'static str,
    command: String,
    duration_ms: u128,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl StepExecution {
    fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

#[derive(Debug, Clone)]
struct DifferentialFinding {
    step: &'static str,
    detail: String,
}

pub(crate) fn parse_project_cli(
    args: &[String],
    tools: ToolchainConfig,
) -> Result<ProjectCommand, String> {
    if args.is_empty() {
        return Err("projects requires a subcommand (list or run)".into());
    }

    match args[0].as_str() {
        "list" => {
            let mut catalog_filter = None;
            let mut include_deprioritized = false;
            let mut queue: std::collections::VecDeque<&String> = args[1..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                match arg.as_str() {
                    "--catalog" => {
                        let value = queue.pop_front().ok_or("--catalog requires a value")?;
                        catalog_filter = Some(value.clone());
                    }
                    "--all" => include_deprioritized = true,
                    other => return Err(format!("unknown projects list option: {}", other)),
                }
            }
            Ok(ProjectCommand::List {
                catalog_filter,
                include_deprioritized,
            })
        }
        "run" => {
            let mut config = ProjectRunConfig {
                catalog_filter: None,
                project_filter: None,
                keep_workdir: false,
                tools,
                reference: default_reference_compiler(),
            };
            let mut queue: std::collections::VecDeque<&String> = args[1..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                match arg.as_str() {
                    "--catalog" => {
                        let value = queue.pop_front().ok_or("--catalog requires a value")?;
                        config.catalog_filter = Some(value.clone());
                    }
                    "--project" => {
                        let value = queue.pop_front().ok_or("--project requires a value")?;
                        config.project_filter = Some(value.clone());
                    }
                    "--keep-workdir" => config.keep_workdir = true,
                    "--armfortas-bin" => {
                        let value = queue
                            .pop_front()
                            .ok_or("--armfortas-bin requires a value")?;
                        config.tools.armfortas = ArmfortasCliAdapter::External(value.clone());
                    }
                    "--flang-bin" => {
                        let value = queue.pop_front().ok_or("--flang-bin requires a value")?;
                        config.tools.flang_new = value.clone();
                    }
                    "--gfortran-bin" => {
                        let value = queue.pop_front().ok_or("--gfortran-bin requires a value")?;
                        config.tools.gfortran = value.clone();
                    }
                    "--cc-bin" => {
                        let value = queue.pop_front().ok_or("--cc-bin requires a value")?;
                        config.tools.cc = value.clone();
                    }
                    "--reference" => {
                        let value = queue.pop_front().ok_or("--reference requires a value")?;
                        config.reference = match value.as_str() {
                            "gfortran" => ProjectCompiler::Gfortran,
                            "flang-new" | "flang" => ProjectCompiler::FlangNew,
                            other => {
                                return Err(format!(
                                    "unknown --reference '{}' (expected gfortran or flang-new)",
                                    other
                                ))
                            }
                        };
                    }
                    other => return Err(format!("unknown projects run option: {}", other)),
                }
            }
            if config.project_filter.is_none() {
                return Err("projects run requires --project <name>".into());
            }
            Ok(ProjectCommand::Run(config))
        }
        other => Err(format!("unknown projects subcommand: {}", other)),
    }
}

pub(crate) fn print_project_usage() {
    eprintln!("  cargo run -p afs-tests -- projects list [--catalog <filter>] [--all]");
    eprintln!(
        "  cargo run -p afs-tests -- projects run --project <name> [--catalog <filter>] [--keep-workdir] [--armfortas-bin <path>] [--flang-bin <path>] [--gfortran-bin <path>] [--cc-bin <path>] [--reference <gfortran|flang-new>]"
    );
    eprintln!();
    eprintln!("project env overrides:");
    eprintln!("  BENCCH_ARMFORTAS_BIN, BENCCH_FLANG_BIN, BENCCH_CC_BIN");
}

pub(crate) fn handle_project_command(command: ProjectCommand) -> Result<ProjectRunOutcome, String> {
    match command {
        ProjectCommand::List {
            catalog_filter,
            include_deprioritized,
        } => {
            let catalogs = discover_catalogs(default_catalog_root())?;
            print_catalogs(&catalogs, catalog_filter.as_deref(), include_deprioritized);
            Ok(ProjectRunOutcome {
                kept_workdirs: Vec::new(),
                summary_lines: Vec::new(),
                success: true,
            })
        }
        ProjectCommand::Run(config) => run_project(config),
    }
}

fn default_catalog_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("projects")
}

fn default_project_report_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("reports")
        .join("projects")
}

fn default_project_temp_root() -> PathBuf {
    std::env::temp_dir().join("afs_tests_projects")
}

fn discover_catalogs(root: PathBuf) -> Result<Vec<ProjectCatalog>, String> {
    let mut files = Vec::new();
    collect_catalog_files(&root, &mut files)?;
    files.sort();

    let mut catalogs = Vec::new();
    for path in files {
        catalogs.push(parse_catalog_file(&path)?);
    }
    catalogs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(catalogs)
}

fn collect_catalog_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(root).map_err(|e| {
        format!(
            "cannot read project catalog root '{}': {}",
            root.display(),
            e
        )
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("cannot read entry in '{}': {}", root.display(), e))?;
        let path = entry.path();
        if path.is_dir() {
            collect_catalog_files(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some(CATALOG_EXTENSION) {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_catalog_file(path: &Path) -> Result<ProjectCatalog, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read project catalog '{}': {}", path.display(), e))?;

    let mut catalog_name = None;
    let mut projects = Vec::new();
    let mut current = None;

    for (index, raw_line) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("campaign ") {
            if catalog_name.is_some() {
                return Err(format!(
                    "{}:{}: duplicate campaign declaration",
                    path.display(),
                    line_no
                ));
            }
            catalog_name = Some(parse_quoted(rest, path, line_no)?);
            continue;
        }

        if let Some(rest) = line.strip_prefix("project ") {
            if current.is_some() {
                return Err(format!(
                    "{}:{}: nested project without end",
                    path.display(),
                    line_no
                ));
            }
            current = Some(ProjectBuilder::new(parse_quoted(rest, path, line_no)?));
            continue;
        }

        if line == "end" {
            let builder = current.take().ok_or_else(|| {
                format!(
                    "{}:{}: stray end outside of project",
                    path.display(),
                    line_no
                )
            })?;
            projects.push(builder.build(path)?);
            continue;
        }

        let builder = current.as_mut().ok_or_else(|| {
            format!(
                "{}:{}: expected campaign/project declaration first",
                path.display(),
                line_no
            )
        })?;

        if let Some(rest) = line.strip_prefix("source ") {
            builder.source = Some(resolve_catalog_relative_path(rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("native ") {
            builder.native_build = Some(parse_quoted(rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("priority ") {
            builder.priority = Some(parse_usize(rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("status ") {
            builder.status = Some(
                ProjectStatus::parse(rest)
                    .map_err(|err| format!("{}:{}: {}", path.display(), line_no, err))?,
            );
        } else if let Some(rest) = line.strip_prefix("coverage =>") {
            builder.coverage = parse_csv_list(rest);
        } else if let Some(rest) = line.strip_prefix("library_seed =>") {
            builder.library_seed = parse_csv_list(rest);
        } else if let Some(rest) = line.strip_prefix("build =>") {
            builder.build_command = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("test =>") {
            builder.test_command = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("smoke =>") {
            builder.smoke_command = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("note ") {
            builder.notes.push(parse_quoted(rest, path, line_no)?);
        } else {
            return Err(format!(
                "{}:{}: unrecognized project line '{}'",
                path.display(),
                line_no,
                line
            ));
        }
    }

    if current.is_some() {
        return Err(format!("{}: unterminated project block", path.display()));
    }

    let name =
        catalog_name.ok_or_else(|| format!("{}: missing campaign declaration", path.display()))?;
    if projects.is_empty() {
        return Err(format!("{}: campaign has no projects", path.display()));
    }
    projects.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.name.cmp(&b.name)));

    Ok(ProjectCatalog {
        name,
        path: path.to_path_buf(),
        projects,
    })
}

struct ProjectBuilder {
    name: String,
    source: Option<PathBuf>,
    native_build: Option<String>,
    priority: Option<usize>,
    status: Option<ProjectStatus>,
    coverage: Vec<String>,
    library_seed: Vec<String>,
    build_command: Option<String>,
    test_command: Option<String>,
    smoke_command: Option<String>,
    notes: Vec<String>,
}

impl ProjectBuilder {
    fn new(name: String) -> Self {
        Self {
            name,
            source: None,
            native_build: None,
            priority: None,
            status: None,
            coverage: Vec::new(),
            library_seed: Vec::new(),
            build_command: None,
            test_command: None,
            smoke_command: None,
            notes: Vec::new(),
        }
    }

    fn build(self, catalog_path: &Path) -> Result<ProjectSpec, String> {
        Ok(ProjectSpec {
            name: self.name,
            source: self.source.ok_or_else(|| {
                format!("{}: project missing source path", catalog_path.display())
            })?,
            native_build: self.native_build.ok_or_else(|| {
                format!(
                    "{}: project missing native build name",
                    catalog_path.display()
                )
            })?,
            priority: self
                .priority
                .ok_or_else(|| format!("{}: project missing priority", catalog_path.display()))?,
            status: self.status.unwrap_or(ProjectStatus::Active),
            coverage: self.coverage,
            library_seed: self.library_seed,
            // Optional: a ladder rung can be declared (and listed) before
            // its build command is engineered and validated. `run` errors
            // clearly if you try to run such a rung.
            build_command: self.build_command,
            test_command: self.test_command,
            smoke_command: self.smoke_command,
            notes: self.notes,
        })
    }
}

fn parse_quoted(raw: &str, path: &Path, line_no: usize) -> Result<String, String> {
    let trimmed = raw.trim();
    if !(trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2) {
        return Err(format!(
            "{}:{}: expected quoted string, got '{}'",
            path.display(),
            line_no,
            raw.trim()
        ));
    }
    Ok(trimmed[1..trimmed.len() - 1].to_string())
}

fn parse_usize(raw: &str, path: &Path, line_no: usize) -> Result<usize, String> {
    raw.trim().parse::<usize>().map_err(|e| {
        format!(
            "{}:{}: expected positive integer, got '{}': {}",
            path.display(),
            line_no,
            raw.trim(),
            e
        )
    })
}

fn parse_csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn resolve_catalog_relative_path(
    raw: &str,
    path: &Path,
    line_no: usize,
) -> Result<PathBuf, String> {
    let relative = parse_quoted(raw, path, line_no)?;
    let base = path
        .parent()
        .ok_or_else(|| format!("{}:{}: catalog path has no parent", path.display(), line_no))?;
    Ok(base.join(relative))
}

fn print_catalogs(
    catalogs: &[ProjectCatalog],
    catalog_filter: Option<&str>,
    include_deprioritized: bool,
) {
    for catalog in catalogs {
        if !matches_filter(&catalog.name, catalog_filter) {
            continue;
        }
        println!("campaign {} ({})", catalog.name, catalog.path.display());
        for project in &catalog.projects {
            if project.status == ProjectStatus::Deprioritized && !include_deprioritized {
                continue;
            }
            println!(
                "  {:>2}. {} [{}] {}",
                project.priority,
                project.name,
                project.status.as_str(),
                project.native_build
            );
            if !project.coverage.is_empty() {
                println!("      coverage: {}", project.coverage.join(", "));
            }
            if !project.library_seed.is_empty() {
                println!("      library seeds: {}", project.library_seed.join(", "));
            }
        }
    }
}

fn matches_filter(value: &str, filter: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(filter) => value
            .to_ascii_lowercase()
            .contains(&filter.to_ascii_lowercase()),
    }
}

fn run_project(config: ProjectRunConfig) -> Result<ProjectRunOutcome, String> {
    let catalogs = discover_catalogs(default_catalog_root())?;
    let project_name = config.project_filter.as_deref().unwrap_or_default();

    let mut matches = Vec::new();
    for catalog in &catalogs {
        if !matches_filter(&catalog.name, config.catalog_filter.as_deref()) {
            continue;
        }
        for project in &catalog.projects {
            if matches_filter(&project.name, Some(project_name)) {
                matches.push((catalog.clone(), project.clone()));
            }
        }
    }

    if matches.is_empty() {
        return Err(format!("no project matched '{}'", project_name));
    }
    if matches.len() > 1 {
        let joined = matches
            .iter()
            .map(|(catalog, project)| format!("{}::{}", catalog.name, project.name))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "project filter '{}' matched multiple projects: {}",
            project_name, joined
        ));
    }

    let (catalog, project) = matches.pop().unwrap();
    let armfortas_bin = resolve_armfortas_bin(&config.tools)?;
    let reference = config.reference;
    let reference_ref = match reference {
        ProjectCompiler::Gfortran => crate::ReferenceCompiler::Gfortran,
        // Armfortas is never a reference; treat anything else as flang.
        _ => crate::ReferenceCompiler::FlangNew,
    };
    let flang_bin = config.tools.reference_binary(reference_ref).to_string();
    let cc_bin = config.tools.cc_bin().to_string();

    let mut kept_workdirs = Vec::new();
    let arm_run = run_for_compiler(
        &catalog,
        &project,
        ProjectCompiler::Armfortas,
        &armfortas_bin,
        &cc_bin,
    )?;
    let flang_run = run_for_compiler(&catalog, &project, reference, &flang_bin, &cc_bin)?;

    let findings = compare_executions(&arm_run, &flang_run);
    let success = findings.is_empty()
        && arm_run.build.succeeded()
        && flang_run.build.succeeded()
        && arm_run
            .test
            .as_ref()
            .map(|step| step.succeeded())
            .unwrap_or(true)
        && flang_run
            .test
            .as_ref()
            .map(|step| step.succeeded())
            .unwrap_or(true)
        && arm_run
            .smoke
            .as_ref()
            .map(|step| step.succeeded())
            .unwrap_or(true)
        && flang_run
            .smoke
            .as_ref()
            .map(|step| step.succeeded())
            .unwrap_or(true);

    let report_path = write_project_report(&catalog, &project, &arm_run, &flang_run, &findings)?;
    let summary_lines = render_console_summary(
        &catalog,
        &project,
        &arm_run,
        &flang_run,
        &findings,
        &report_path,
    );

    if config.keep_workdir || !success {
        kept_workdirs.push(arm_run.workdir.clone());
        kept_workdirs.push(flang_run.workdir.clone());
    } else {
        cleanup_workdir(&arm_run.workdir);
        cleanup_workdir(&flang_run.workdir);
    }

    Ok(ProjectRunOutcome {
        kept_workdirs,
        summary_lines,
        success,
    })
}

fn resolve_armfortas_bin(tools: &ToolchainConfig) -> Result<String, String> {
    if let Some(path) = tools.armfortas_external_bin() {
        return Ok(path.to_string());
    }

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("..").join("..");
    for candidate in [
        workspace_root.join("target/release/armfortas"),
        workspace_root.join("target/debug/armfortas"),
    ] {
        if candidate.exists() {
            return Ok(candidate.display().to_string());
        }
    }

    Err(
        "projects run needs an armfortas compiler binary; pass --armfortas-bin or build target/release/armfortas"
            .into(),
    )
}

fn run_for_compiler(
    catalog: &ProjectCatalog,
    project: &ProjectSpec,
    compiler: ProjectCompiler,
    compiler_bin: &str,
    cc_bin: &str,
) -> Result<ProjectExecution, String> {
    let build_command = project.build_command.as_deref().ok_or_else(|| {
        format!(
            "project '{}' has no build command yet (deprioritized rung — not validated)",
            project.name
        )
    })?;
    let workdir = prepare_project_workdir(catalog, project, compiler)?;
    let build = run_step("build", build_command, &workdir, compiler_bin, cc_bin)?;
    let test = if build.succeeded() {
        match &project.test_command {
            Some(command) => Some(run_step("test", command, &workdir, compiler_bin, cc_bin)?),
            None => None,
        }
    } else {
        None
    };
    let smoke = if build.succeeded() && test.as_ref().map(|step| step.succeeded()).unwrap_or(true) {
        match &project.smoke_command {
            Some(command) => Some(run_step("smoke", command, &workdir, compiler_bin, cc_bin)?),
            None => None,
        }
    } else {
        None
    };

    Ok(ProjectExecution {
        compiler,
        compiler_bin: compiler_bin.to_string(),
        cc_bin: cc_bin.to_string(),
        workdir,
        build,
        test,
        smoke,
    })
}

fn prepare_project_workdir(
    catalog: &ProjectCatalog,
    project: &ProjectSpec,
    compiler: ProjectCompiler,
) -> Result<PathBuf, String> {
    let root = default_project_temp_root().join(format!(
        "{}_{}_{}_{}",
        sanitize_component(&catalog.name),
        sanitize_component(&project.name),
        sanitize_component(compiler.as_str()),
        next_project_suffix()
    ));
    if root.exists() {
        fs::remove_dir_all(&root)
            .map_err(|e| format!("cannot clear temp project root '{}': {}", root.display(), e))?;
    }
    fs::create_dir_all(&root).map_err(|e| {
        format!(
            "cannot create temp project root '{}': {}",
            root.display(),
            e
        )
    })?;
    let source_root = root.join("src");
    copy_project_tree(&project.source, &source_root).map_err(|e| {
        format!(
            "cannot copy '{}' into '{}': {}",
            project.source.display(),
            source_root.display(),
            e
        )
    })?;
    if let Some(workspace_root) = project.source.parent() {
        localize_fgof_git_dependencies(&source_root, workspace_root, &root)?;
    }
    Ok(source_root)
}

fn next_project_suffix() -> String {
    format!(
        "{}-{:04}",
        std::process::id(),
        crate::REPORT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

fn copy_project_tree(source: &Path, dest: &Path) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir_all(dest)?;
        fs::set_permissions(dest, metadata.permissions())?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            if should_skip_copy(&name) {
                continue;
            }
            let child_source = entry.path();
            let child_dest = dest.join(&name);
            copy_project_tree(&child_source, &child_dest)?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, dest)?;
        fs::set_permissions(dest, metadata.permissions())?;
    }
    Ok(())
}

fn localize_fgof_git_dependencies(
    project_root: &Path,
    workspace_root: &Path,
    sandbox_root: &Path,
) -> Result<(), String> {
    let manifest = project_root.join("fpm.toml");
    if !manifest.exists() {
        return Ok(());
    }

    let original = fs::read_to_string(&manifest)
        .map_err(|e| format!("cannot read '{}': {}", manifest.display(), e))?;
    let mut rewritten = Vec::new();
    let mut changed = false;
    let deps_root = sandbox_root.join("deps");

    for line in original.lines() {
        if let Some((dep_name, repo_name)) = parse_fgof_git_dependency(line) {
            let local_dep = workspace_root.join(&repo_name);
            if local_dep.exists() {
                let vendored_dep = deps_root.join(&repo_name);
                if !vendored_dep.exists() {
                    copy_project_tree(&local_dep, &vendored_dep).map_err(|e| {
                        format!(
                            "cannot vendor dependency '{}' into '{}': {}",
                            local_dep.display(),
                            vendored_dep.display(),
                            e
                        )
                    })?;
                    localize_fgof_git_dependencies(&vendored_dep, workspace_root, sandbox_root)?;
                }
                let relative = relative_path(project_root, &vendored_dep);
                rewritten.push(format!(
                    "{} = {{ path = \"{}\" }}",
                    dep_name,
                    relative.display()
                ));
                changed = true;
                continue;
            }
        }
        rewritten.push(line.to_string());
    }

    if changed {
        let mut content = rewritten.join("\n");
        if original.ends_with('\n') {
            content.push('\n');
        }
        fs::write(&manifest, content)
            .map_err(|e| format!("cannot write '{}': {}", manifest.display(), e))?;
    }

    Ok(())
}

fn parse_fgof_git_dependency(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let git_prefix = "git = \"https://github.com/FortranGoingOnForty/";
    let git_pos = trimmed.find(git_prefix)?;
    let dep_name = trimmed.split('=').next()?.trim();
    let rest = &trimmed[git_pos + git_prefix.len()..];
    let repo_name = rest.split(".git").next()?.trim();
    if dep_name.is_empty() || repo_name.is_empty() {
        return None;
    }
    Some((dep_name.to_string(), repo_name.to_string()))
}

fn relative_path(from_dir: &Path, to_path: &Path) -> PathBuf {
    let from_components: Vec<_> = from_dir.components().collect();
    let to_components: Vec<_> = to_path.components().collect();
    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    let mut relative = PathBuf::new();
    for _ in common..from_components.len() {
        relative.push("..");
    }
    for component in &to_components[common..] {
        relative.push(component.as_os_str());
    }
    relative
}

fn should_skip_copy(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git")
            | Some("build")
            | Some("bin")
            | Some("target")
            | Some(".fpm")
            | Some("test_results")
            | Some("__pycache__")
    )
}

fn run_step(
    label: &'static str,
    template: &str,
    workdir: &Path,
    compiler_bin: &str,
    cc_bin: &str,
) -> Result<StepExecution, String> {
    let command = expand_command_template(template, compiler_bin, cc_bin);
    let start = Instant::now();
    // Shell per host: macOS ships zsh as the login shell; the ELF
    // targets (Linux, FreeBSD) may not have /bin/zsh, but POSIX
    // /bin/sh is always present and handles `&&`/`||` the same way.
    // Both inherit the parent environment, so PATH for make/fpm carries
    // through; the build commands use absolute {fc}/CC anyway.
    let (shell, shell_flag) = if cfg!(target_os = "macos") {
        ("/bin/zsh", "-lc")
    } else {
        ("/bin/sh", "-c")
    };
    let output = Command::new(shell)
        .arg(shell_flag)
        .arg(&command)
        .current_dir(workdir)
        .env("FC", compiler_bin)
        .env("CC", cc_bin)
        .env("ARMFORTAS", compiler_bin)
        .output()
        .map_err(|e| {
            format!(
                "cannot run {} command '{}' in '{}': {}",
                label,
                command,
                workdir.display(),
                e
            )
        })?;
    let duration_ms = start.elapsed().as_millis();
    let exit_code = output.status.code().unwrap_or(-1);

    Ok(StepExecution {
        label,
        command,
        duration_ms,
        exit_code,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn expand_command_template(template: &str, compiler_bin: &str, cc_bin: &str) -> String {
    template
        .replace("{fc}", compiler_bin)
        .replace("{cc}", cc_bin)
}

fn compare_executions(
    armfortas: &ProjectExecution,
    flang: &ProjectExecution,
) -> Vec<DifferentialFinding> {
    let mut findings = Vec::new();
    compare_step_outcome(
        "build",
        &armfortas.build,
        &flang.build,
        false,
        &mut findings,
    );

    match (&armfortas.test, &flang.test) {
        (Some(left), Some(right)) => {
            compare_step_outcome("test", left, right, false, &mut findings)
        }
        (None, Some(_)) | (Some(_), None) => findings.push(DifferentialFinding {
            step: "test",
            detail: "test step only ran for one compiler".into(),
        }),
        (None, None) => {}
    }

    match (&armfortas.smoke, &flang.smoke) {
        (Some(left), Some(right)) => {
            compare_step_outcome("smoke", left, right, true, &mut findings)
        }
        (None, Some(_)) | (Some(_), None) => findings.push(DifferentialFinding {
            step: "smoke",
            detail: "smoke step only ran for one compiler".into(),
        }),
        (None, None) => {}
    }

    findings
}

fn compare_step_outcome(
    step: &'static str,
    left: &StepExecution,
    right: &StepExecution,
    compare_output: bool,
    findings: &mut Vec<DifferentialFinding>,
) {
    if left.succeeded() != right.succeeded() {
        findings.push(DifferentialFinding {
            step,
            detail: format!(
                "{} success diverged: armfortas={} reference={}",
                step,
                left.succeeded(),
                right.succeeded()
            ),
        });
        return;
    }

    if compare_output && left.succeeded() {
        if left.exit_code != right.exit_code {
            findings.push(DifferentialFinding {
                step,
                detail: format!(
                    "{} exit code diverged: armfortas={} reference={}",
                    step, left.exit_code, right.exit_code
                ),
            });
        }
        if left.stdout != right.stdout {
            findings.push(DifferentialFinding {
                step,
                detail: format!(
                    "{} stdout diverged (armfortas {} bytes vs reference {} bytes)",
                    step,
                    left.stdout.len(),
                    right.stdout.len()
                ),
            });
        }
        if left.stderr != right.stderr {
            findings.push(DifferentialFinding {
                step,
                detail: format!(
                    "{} stderr diverged (armfortas {} bytes vs reference {} bytes)",
                    step,
                    left.stderr.len(),
                    right.stderr.len()
                ),
            });
        }
    }
}

fn write_project_report(
    catalog: &ProjectCatalog,
    project: &ProjectSpec,
    armfortas: &ProjectExecution,
    flang: &ProjectExecution,
    findings: &[DifferentialFinding],
) -> Result<PathBuf, String> {
    let report_root = default_project_report_root()
        .join(sanitize_component(&catalog.name))
        .join(sanitize_component(&project.name));
    fs::create_dir_all(&report_root).map_err(|e| {
        format!(
            "cannot create project report root '{}': {}",
            report_root.display(),
            e
        )
    })?;

    let report_path = report_root.join(format!("{}.md", next_project_suffix()));
    let mut report = String::new();
    writeln!(&mut report, "# Differential Project Report").unwrap();
    writeln!(&mut report).unwrap();
    writeln!(&mut report, "- Campaign: `{}`", catalog.name).unwrap();
    writeln!(&mut report, "- Project: `{}`", project.name).unwrap();
    writeln!(
        &mut report,
        "- Native build system: `{}`",
        project.native_build
    )
    .unwrap();
    writeln!(&mut report, "- Status: `{}`", project.status.as_str()).unwrap();
    writeln!(&mut report, "- Source: `{}`", project.source.display()).unwrap();
    if !project.coverage.is_empty() {
        writeln!(
            &mut report,
            "- Coverage buckets: `{}`",
            project.coverage.join("`, `")
        )
        .unwrap();
    }
    if !project.library_seed.is_empty() {
        writeln!(
            &mut report,
            "- Library seeds: `{}`",
            project.library_seed.join("`, `")
        )
        .unwrap();
    }
    writeln!(&mut report).unwrap();

    if !project.notes.is_empty() {
        writeln!(&mut report, "## Notes").unwrap();
        writeln!(&mut report).unwrap();
        for note in &project.notes {
            writeln!(&mut report, "- {}", note).unwrap();
        }
        writeln!(&mut report).unwrap();
    }

    writeln!(&mut report, "## Differential Summary").unwrap();
    writeln!(&mut report).unwrap();
    if findings.is_empty() {
        writeln!(
            &mut report,
            "- No armfortas-vs-{} step outcome differences were observed.",
            flang.compiler.as_str()
        )
        .unwrap();
    } else {
        for finding in findings {
            writeln!(&mut report, "- `{}`: {}", finding.step, finding.detail).unwrap();
        }
    }
    writeln!(&mut report).unwrap();

    append_execution_report(&mut report, armfortas);
    append_execution_report(&mut report, flang);

    fs::write(&report_path, report).map_err(|e| {
        format!(
            "cannot write project report '{}': {}",
            report_path.display(),
            e
        )
    })?;
    Ok(report_path)
}

fn append_execution_report(report: &mut String, execution: &ProjectExecution) {
    writeln!(report, "## {}", execution.compiler.as_str()).unwrap();
    writeln!(report).unwrap();
    writeln!(report, "- Compiler binary: `{}`", execution.compiler_bin).unwrap();
    writeln!(report, "- C compiler: `{}`", execution.cc_bin).unwrap();
    writeln!(report, "- Workdir: `{}`", execution.workdir.display()).unwrap();
    writeln!(report).unwrap();

    append_step_report(report, &execution.build);
    if let Some(step) = &execution.test {
        append_step_report(report, step);
    }
    if let Some(step) = &execution.smoke {
        append_step_report(report, step);
    }
}

fn append_step_report(report: &mut String, step: &StepExecution) {
    writeln!(report, "### {}", step.label.to_ascii_uppercase()).unwrap();
    writeln!(report).unwrap();
    writeln!(report, "- Command: `{}`", step.command).unwrap();
    writeln!(report, "- Duration: `{}` ms", step.duration_ms).unwrap();
    writeln!(report, "- Exit code: `{}`", step.exit_code).unwrap();
    writeln!(report).unwrap();
    writeln!(report, "```text").unwrap();
    if !step.stdout.is_empty() {
        write!(report, "stdout:\n{}", step.stdout).unwrap();
        if !step.stdout.ends_with('\n') {
            writeln!(report).unwrap();
        }
    } else {
        writeln!(report, "stdout: <empty>").unwrap();
    }
    if !step.stderr.is_empty() {
        write!(report, "stderr:\n{}", step.stderr).unwrap();
        if !step.stderr.ends_with('\n') {
            writeln!(report).unwrap();
        }
    } else {
        writeln!(report, "stderr: <empty>").unwrap();
    }
    writeln!(report, "```").unwrap();
    writeln!(report).unwrap();
}

fn render_console_summary(
    catalog: &ProjectCatalog,
    project: &ProjectSpec,
    armfortas: &ProjectExecution,
    flang: &ProjectExecution,
    findings: &[DifferentialFinding],
    report_path: &Path,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "PROJECT {}::{} ({})",
        catalog.name, project.name, project.native_build
    ));
    lines.push(step_console_line(
        "armfortas",
        &armfortas.build,
        armfortas.test.as_ref(),
        armfortas.smoke.as_ref(),
    ));
    lines.push(step_console_line(
        flang.compiler.as_str(),
        &flang.build,
        flang.test.as_ref(),
        flang.smoke.as_ref(),
    ));
    if findings.is_empty() {
        lines.push("differential: no step outcome differences observed".into());
    } else {
        lines.push(format!("differential: {} finding(s)", findings.len()));
        for finding in findings {
            lines.push(format!("  - {}: {}", finding.step, finding.detail));
        }
    }
    lines.push(format!("report: {}", report_path.display()));
    lines
}

fn step_console_line(
    compiler: &str,
    build: &StepExecution,
    test: Option<&StepExecution>,
    smoke: Option<&StepExecution>,
) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "build={}({}ms)",
        status_word(build.succeeded()),
        build.duration_ms
    ));
    if let Some(step) = test {
        parts.push(format!(
            "test={}({}ms)",
            status_word(step.succeeded()),
            step.duration_ms
        ));
    }
    if let Some(step) = smoke {
        parts.push(format!(
            "smoke={}({}ms)",
            status_word(step.succeeded()),
            step.duration_ms
        ));
    }
    format!("{} {}", compiler, parts.join(" "))
}

fn status_word(ok: bool) -> &'static str {
    if ok {
        "PASS"
    } else {
        "FAIL"
    }
}

fn cleanup_workdir(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_project_catalog() {
        let root = std::env::temp_dir().join("afs_tests_project_catalog");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("projects")).unwrap();
        fs::write(
            root.join("projects").join("demo.afproj"),
            r#"campaign "demo"

project "fortbite"
source "../fortbite"
native "make"
priority 1
status active
coverage => pure_semantics, performance
library_seed => fortargs, fortds
build => make clean && make FC="{fc}"
test => make test FC="{fc}"
smoke => printf '2 + 3\nquit\n' | ./build/bin/fortbite
note "Pure Fortran calculator target."
end
"#,
        )
        .unwrap();

        let catalog = parse_catalog_file(&root.join("projects").join("demo.afproj")).unwrap();
        assert_eq!(catalog.name, "demo");
        assert_eq!(catalog.projects.len(), 1);
        let project = &catalog.projects[0];
        assert_eq!(project.name, "fortbite");
        assert_eq!(project.native_build, "make");
        assert_eq!(project.priority, 1);
        assert_eq!(project.status, ProjectStatus::Active);
        assert_eq!(project.coverage, vec!["pure_semantics", "performance"]);
        assert_eq!(project.library_seed, vec!["fortargs", "fortds"]);
        assert_eq!(
            project.build_command.as_deref(),
            Some("make clean && make FC=\"{fc}\"")
        );
        assert_eq!(project.notes, vec!["Pure Fortran calculator target."]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn expands_project_command_template() {
        let expanded = expand_command_template(
            "make FC=\"{fc}\" CC=\"{cc}\"",
            "/tmp/armfortas",
            "/usr/bin/clang",
        );
        assert_eq!(expanded, "make FC=\"/tmp/armfortas\" CC=\"/usr/bin/clang\"");
    }
}

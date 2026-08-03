mod compiler;
mod project_campaign;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use armfortas::testing::managed_process::{run as run_managed, CommandClass};

use crate::compiler::{
    capture_from_path, capture_graph, compile_graph_output, compile_output, CaptureFailure,
    CaptureRequest, CaptureResult, CapturedStage, EmitMode, FailureStage, OptLevel, RunCapture,
    Stage,
};
use crate::project_campaign::{
    handle_project_command, parse_project_cli, print_project_usage, ProjectCommand,
};

const SUITE_EXTENSION: &str = "afs";

static REPORT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct SuiteSpec {
    name: String,
    path: PathBuf,
    cases: Vec<CaseSpec>,
}

#[derive(Debug, Clone)]
struct CaseSpec {
    name: String,
    source: PathBuf,
    graph_files: Vec<PathBuf>,
    requested: BTreeSet<Stage>,
    opt_levels: Vec<OptLevel>,
    repeat_count: usize,
    reference_compilers: Vec<ReferenceCompiler>,
    consistency_checks: Vec<ConsistencyCheck>,
    expectations: Vec<Expectation>,
    status_rules: Vec<StatusRule>,
}

impl CaseSpec {
    fn is_graph(&self) -> bool {
        !self.graph_files.is_empty()
    }

    fn source_label(&self) -> String {
        if self.is_graph() {
            format!(
                "graph entry {} ({} files)",
                self.source.display(),
                self.graph_files.len()
            )
        } else {
            self.source.display().to_string()
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedInput {
    compiler_source: PathBuf,
    graph_sources: Vec<PathBuf>,
    temp_root: Option<PathBuf>,
}

impl PreparedInput {
    fn is_graph(&self) -> bool {
        !self.graph_sources.is_empty()
    }

    fn compiler_sources(&self) -> &[PathBuf] {
        if self.is_graph() {
            &self.graph_sources
        } else {
            std::slice::from_ref(&self.compiler_source)
        }
    }
}

#[derive(Debug, Clone)]
struct StatusRule {
    kind: StatusKind,
    selector: OptSelector,
    reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Xfail,
    Future,
}

#[derive(Debug, Clone)]
enum OptSelector {
    All,
    Only(Vec<OptLevel>),
}

impl OptSelector {
    fn matches(&self, opt_level: OptLevel) -> bool {
        match self {
            Self::All => true,
            Self::Only(levels) => levels.contains(&opt_level),
        }
    }
}

#[derive(Debug, Clone)]
enum EffectiveStatus {
    Normal,
    Xfail(String),
    Future(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ConsistencyCheck {
    CliObjVsSystemAs,
    CliAsmReproducible,
    CliObjReproducible,
    CliRunReproducible,
    CaptureAsmVsCliAsm,
    CaptureObjVsCliObj,
    CaptureRunVsCliRun,
    CaptureAsmReproducible,
    CaptureObjReproducible,
    CaptureRunReproducible,
}

impl ConsistencyCheck {
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "cli_obj_vs_system_as" | "cli-obj-vs-system-as" => Some(Self::CliObjVsSystemAs),
            "cli_asm_reproducible" | "cli-asm-reproducible" => Some(Self::CliAsmReproducible),
            "cli_obj_reproducible" | "cli-obj-reproducible" => Some(Self::CliObjReproducible),
            "cli_run_reproducible" | "cli-run-reproducible" => Some(Self::CliRunReproducible),
            "capture_asm_vs_cli_asm" | "capture-asm-vs-cli-asm" => Some(Self::CaptureAsmVsCliAsm),
            "capture_obj_vs_cli_obj" | "capture-obj-vs-cli-obj" => Some(Self::CaptureObjVsCliObj),
            "capture_run_vs_cli_run" | "capture-run-vs-cli-run" => Some(Self::CaptureRunVsCliRun),
            "capture_asm_reproducible" | "capture-asm-reproducible" => {
                Some(Self::CaptureAsmReproducible)
            }
            "capture_obj_reproducible" | "capture-obj-reproducible" => {
                Some(Self::CaptureObjReproducible)
            }
            "capture_run_reproducible" | "capture-run-reproducible" => {
                Some(Self::CaptureRunReproducible)
            }
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::CliObjVsSystemAs => "cli_obj_vs_system_as",
            Self::CliAsmReproducible => "cli_asm_reproducible",
            Self::CliObjReproducible => "cli_obj_reproducible",
            Self::CliRunReproducible => "cli_run_reproducible",
            Self::CaptureAsmVsCliAsm => "capture_asm_vs_cli_asm",
            Self::CaptureObjVsCliObj => "capture_obj_vs_cli_obj",
            Self::CaptureRunVsCliRun => "capture_run_vs_cli_run",
            Self::CaptureAsmReproducible => "capture_asm_reproducible",
            Self::CaptureObjReproducible => "capture_obj_reproducible",
            Self::CaptureRunReproducible => "capture_run_reproducible",
        }
    }

    fn required_stage(&self) -> Option<Stage> {
        match self {
            Self::CliObjVsSystemAs
            | Self::CliAsmReproducible
            | Self::CliObjReproducible
            | Self::CliRunReproducible => None,
            Self::CaptureAsmVsCliAsm | Self::CaptureAsmReproducible => Some(Stage::Asm),
            Self::CaptureObjVsCliObj | Self::CaptureObjReproducible => Some(Stage::Obj),
            Self::CaptureRunVsCliRun | Self::CaptureRunReproducible => Some(Stage::Run),
        }
    }
}

#[derive(Debug, Clone)]
enum Expectation {
    CheckComments(Target),
    Contains { target: Target, needle: String },
    NotContains { target: Target, needle: String },
    Equals { target: Target, value: String },
    IntEquals { target: Target, value: i32 },
    FailContains { stage: FailureStage, needle: String },
    FailEquals { stage: FailureStage, value: String },
}

#[derive(Debug, Clone, Copy)]
enum Target {
    Stage(Stage),
    RunStdout,
    RunStderr,
    RunExitCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReferenceCompiler {
    Gfortran,
    FlangNew,
}

impl ReferenceCompiler {
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "gfortran" => Some(Self::Gfortran),
            "flang-new" | "flang_new" | "flang" => Some(Self::FlangNew),
            _ => None,
        }
    }

    fn binary_name(&self) -> &'static str {
        match self {
            Self::Gfortran => "gfortran",
            Self::FlangNew => "flang-new",
        }
    }

    fn as_str(&self) -> &'static str {
        self.binary_name()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArmfortasCliAdapter {
    Linked,
    External(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolchainConfig {
    armfortas: ArmfortasCliAdapter,
    gfortran: String,
    flang_new: String,
    cc: String,
    system_as: String,
    otool: String,
    nm: String,
    objdump: String,
    readelf: String,
}

impl ToolchainConfig {
    fn from_env() -> Self {
        Self {
            armfortas: match std::env::var("BENCCH_ARMFORTAS_BIN") {
                Ok(value) if !value.trim().is_empty() => ArmfortasCliAdapter::External(value),
                _ => ArmfortasCliAdapter::Linked,
            },
            gfortran: tool_override("BENCCH_GFORTRAN_BIN", "gfortran"),
            flang_new: tool_override("BENCCH_FLANG_BIN", "flang-new"),
            cc: tool_override("BENCCH_CC_BIN", "cc"),
            system_as: tool_override("BENCCH_AS_BIN", "as"),
            otool: tool_override("BENCCH_OTOOL_BIN", "otool"),
            nm: tool_override("BENCCH_NM_BIN", "nm"),
            objdump: tool_override("BENCCH_OBJDUMP_BIN", "objdump"),
            readelf: tool_override("BENCCH_READELF_BIN", "readelf"),
        }
    }

    fn armfortas_command_name(&self) -> &str {
        match &self.armfortas {
            ArmfortasCliAdapter::Linked => "armfortas (linked)",
            ArmfortasCliAdapter::External(binary) => binary,
        }
    }

    fn armfortas_external_bin(&self) -> Option<&str> {
        match &self.armfortas {
            ArmfortasCliAdapter::Linked => None,
            ArmfortasCliAdapter::External(binary) => Some(binary),
        }
    }

    fn reference_binary(&self, compiler: ReferenceCompiler) -> &str {
        match compiler {
            ReferenceCompiler::Gfortran => &self.gfortran,
            ReferenceCompiler::FlangNew => &self.flang_new,
        }
    }

    fn system_as_bin(&self) -> &str {
        &self.system_as
    }

    fn cc_bin(&self) -> &str {
        &self.cc
    }

    fn otool_bin(&self) -> &str {
        &self.otool
    }

    fn nm_bin(&self) -> &str {
        &self.nm
    }

    fn objdump_bin(&self) -> &str {
        &self.objdump
    }

    fn readelf_bin(&self) -> &str {
        &self.readelf
    }
}

fn tool_override(var: &str, default: &str) -> String {
    match std::env::var(var) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => default.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeKind {
    Pass,
    Fail,
    Xfail,
    Xpass,
    Future,
}

#[derive(Debug, Clone)]
struct Outcome {
    suite: String,
    case: String,
    opt_level: OptLevel,
    kind: OutcomeKind,
    detail: String,
    bundle: Option<PathBuf>,
    consistency_observations: Vec<ConsistencyObservation>,
}

#[derive(Debug, Default)]
struct Summary {
    passed: usize,
    failed: usize,
    xfailed: usize,
    xpassed: usize,
    future: usize,
    consistency: BTreeMap<ConsistencyCheck, ConsistencyRollup>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ConsistencyRollup {
    cells: usize,
    repeat_counts: BTreeSet<usize>,
    unique_variant_counts: BTreeSet<usize>,
    varying_components: BTreeSet<String>,
    stable_components: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsistencyObservation {
    check: ConsistencyCheck,
    summary: String,
    repeat_count: Option<usize>,
    unique_variant_count: Option<usize>,
    varying_components: Vec<String>,
    stable_components: Vec<String>,
}

#[derive(Debug, Clone)]
struct RunConfig {
    suite_filter: Option<String>,
    case_filter: Option<String>,
    opt_filter: Option<BTreeSet<OptLevel>>,
    verbose: bool,
    fail_fast: bool,
    include_future: bool,
    all_stages: bool,
    tools: ToolchainConfig,
}

#[derive(Debug, Clone)]
struct ExecutionArtifacts {
    requested: BTreeSet<Stage>,
    armfortas: Option<CaptureResult>,
    armfortas_failure: Option<CaptureFailure>,
    references: Vec<ReferenceResult>,
    consistency_issues: Vec<ConsistencyIssue>,
}

#[derive(Debug, Clone)]
struct ReferenceResult {
    compiler: ReferenceCompiler,
    compile_command: String,
    compile_exit_code: i32,
    compile_stdout: String,
    compile_stderr: String,
    run: Option<RunCapture>,
    run_error: Option<String>,
}

#[derive(Debug, Clone)]
struct ConsistencyIssue {
    check: ConsistencyCheck,
    summary: String,
    repeat_count: Option<usize>,
    unique_variant_count: Option<usize>,
    varying_components: Vec<String>,
    stable_components: Vec<String>,
    detail: String,
    temp_root: PathBuf,
}

impl Summary {
    fn record_outcome(&mut self, outcome: &Outcome) {
        match outcome.kind {
            OutcomeKind::Pass => self.passed += 1,
            OutcomeKind::Fail => self.failed += 1,
            OutcomeKind::Xfail => self.xfailed += 1,
            OutcomeKind::Xpass => self.xpassed += 1,
            OutcomeKind::Future => self.future += 1,
        }
        self.record_consistency(&outcome.consistency_observations);
    }

    fn record_consistency(&mut self, observations: &[ConsistencyObservation]) {
        for observation in observations {
            self.consistency
                .entry(observation.check)
                .or_default()
                .record(observation);
        }
    }
}

impl ConsistencyRollup {
    fn record(&mut self, observation: &ConsistencyObservation) {
        self.cells += 1;
        if let Some(repeat_count) = observation.repeat_count {
            self.repeat_counts.insert(repeat_count);
        }
        if let Some(unique_variant_count) = observation.unique_variant_count {
            self.unique_variant_counts.insert(unique_variant_count);
        }
        self.varying_components
            .extend(observation.varying_components.iter().cloned());
        self.stable_components
            .extend(observation.stable_components.iter().cloned());
    }
}

impl ConsistencyIssue {
    fn observation(&self) -> ConsistencyObservation {
        ConsistencyObservation {
            check: self.check,
            summary: self.summary.clone(),
            repeat_count: self.repeat_count,
            unique_variant_count: self.unique_variant_count,
            varying_components: self.varying_components.clone(),
            stable_components: self.stable_components.clone(),
        }
    }
}

impl ReferenceResult {
    fn infrastructure_error(compiler: ReferenceCompiler, command: String, message: String) -> Self {
        Self {
            compiler,
            compile_command: command,
            compile_exit_code: -1,
            compile_stdout: String::new(),
            compile_stderr: message,
            run: None,
            run_error: None,
        }
    }

    fn run_signature(&self) -> Option<RunSignature> {
        self.run.as_ref().map(normalize_run_signature)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RunSignature {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    files: BTreeMap<String, Vec<u8>>,
}

pub fn run_cli(args: &[String]) -> i32 {
    match parse_cli(args) {
        Ok(CommandKind::List { suite_filter }) => match discover_suites(default_suite_root()) {
            Ok(suites) => {
                print_suites(&filter_suites(&suites, suite_filter.as_deref()));
                0
            }
            Err(err) => {
                eprintln!("afs-tests: {}", err);
                1
            }
        },
        Ok(CommandKind::Run(config)) => match run_suites(&config) {
            Ok(summary) => {
                print_summary(&summary);
                if summary.failed == 0 && summary.xpassed == 0 {
                    0
                } else {
                    1
                }
            }
            Err(err) => {
                eprintln!("afs-tests: {}", err);
                1
            }
        },
        Ok(CommandKind::Projects(command)) => match handle_project_command(*command) {
            Ok(outcome) => {
                for line in &outcome.summary_lines {
                    println!("{}", line);
                }
                for workdir in &outcome.kept_workdirs {
                    println!("kept workdir: {}", workdir.display());
                }
                if outcome.success {
                    0
                } else {
                    1
                }
            }
            Err(err) => {
                eprintln!("afs-tests: {}", err);
                1
            }
        },
        Ok(CommandKind::Help) => {
            print_usage();
            0
        }
        Err(err) => {
            eprintln!("afs-tests: {}", err);
            print_usage();
            2
        }
    }
}

enum CommandKind {
    List { suite_filter: Option<String> },
    Run(Box<RunConfig>),
    Projects(Box<ProjectCommand>),
    Help,
}

fn parse_cli(args: &[String]) -> Result<CommandKind, String> {
    if args.is_empty() {
        return Ok(CommandKind::Help);
    }

    match args[0].as_str() {
        "list" => {
            let mut suite_filter = None;
            let mut queue: VecDeque<&String> = args[1..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                match arg.as_str() {
                    "--suite" => {
                        let value = queue.pop_front().ok_or("--suite requires a value")?;
                        suite_filter = Some(value.clone());
                    }
                    "--help" | "-h" => return Ok(CommandKind::Help),
                    other => return Err(format!("unknown list option: {}", other)),
                }
            }
            Ok(CommandKind::List { suite_filter })
        }
        "run" => {
            let mut config = RunConfig {
                suite_filter: None,
                case_filter: None,
                opt_filter: None,
                verbose: false,
                fail_fast: false,
                include_future: false,
                all_stages: false,
                tools: ToolchainConfig::from_env(),
            };
            let mut queue: VecDeque<&String> = args[1..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                match arg.as_str() {
                    "--suite" => {
                        let value = queue.pop_front().ok_or("--suite requires a value")?;
                        config.suite_filter = Some(value.clone());
                    }
                    "--case" => {
                        let value = queue.pop_front().ok_or("--case requires a value")?;
                        config.case_filter = Some(value.clone());
                    }
                    "--opt" => {
                        let value = queue.pop_front().ok_or("--opt requires a value")?;
                        let parsed = parse_opt_level_list(value)?;
                        let filter = config.opt_filter.get_or_insert_with(BTreeSet::new);
                        filter.extend(parsed);
                    }
                    "--verbose" | "-v" => config.verbose = true,
                    "--fail-fast" => config.fail_fast = true,
                    "--include-future" => config.include_future = true,
                    "--all" => config.all_stages = true,
                    "--armfortas-bin" => {
                        let value = queue
                            .pop_front()
                            .ok_or("--armfortas-bin requires a value")?;
                        config.tools.armfortas = ArmfortasCliAdapter::External(value.clone());
                    }
                    "--gfortran-bin" => {
                        let value = queue.pop_front().ok_or("--gfortran-bin requires a value")?;
                        config.tools.gfortran = value.clone();
                    }
                    "--flang-bin" => {
                        let value = queue.pop_front().ok_or("--flang-bin requires a value")?;
                        config.tools.flang_new = value.clone();
                    }
                    "--cc-bin" => {
                        let value = queue.pop_front().ok_or("--cc-bin requires a value")?;
                        config.tools.cc = value.clone();
                    }
                    "--as-bin" => {
                        let value = queue.pop_front().ok_or("--as-bin requires a value")?;
                        config.tools.system_as = value.clone();
                    }
                    "--otool-bin" => {
                        let value = queue.pop_front().ok_or("--otool-bin requires a value")?;
                        config.tools.otool = value.clone();
                    }
                    "--nm-bin" => {
                        let value = queue.pop_front().ok_or("--nm-bin requires a value")?;
                        config.tools.nm = value.clone();
                    }
                    "--objdump-bin" => {
                        let value = queue.pop_front().ok_or("--objdump-bin requires a value")?;
                        config.tools.objdump = value.clone();
                    }
                    "--readelf-bin" => {
                        let value = queue.pop_front().ok_or("--readelf-bin requires a value")?;
                        config.tools.readelf = value.clone();
                    }
                    "--help" | "-h" => return Ok(CommandKind::Help),
                    other => return Err(format!("unknown run option: {}", other)),
                }
            }
            Ok(CommandKind::Run(Box::new(config)))
        }
        "projects" => Ok(CommandKind::Projects(Box::new(parse_project_cli(
            &args[1..],
            ToolchainConfig::from_env(),
        )?))),
        "--help" | "-h" | "help" => Ok(CommandKind::Help),
        other => Err(format!("unknown command: {}", other)),
    }
}

fn print_usage() {
    eprintln!("afs-tests — structured ARMFORTAS bench runner");
    eprintln!();
    eprintln!("usage:");
    eprintln!("  cargo run -p afs-tests -- list [--suite <filter>]");
    eprintln!(
        "  cargo run -p afs-tests -- run [--suite <filter>] [--case <filter>] [--opt <O0,O1,...>] [--verbose] [--fail-fast] [--include-future] [--all] [--armfortas-bin <path>] [--gfortran-bin <path>] [--flang-bin <path>] [--cc-bin <path>] [--as-bin <path>] [--otool-bin <path>] [--nm-bin <path>] [--objdump-bin <path>] [--readelf-bin <path>]"
    );
    print_project_usage();
    eprintln!();
    eprintln!("env overrides:");
    eprintln!("  BENCCH_ARMFORTAS_BIN, BENCCH_GFORTRAN_BIN, BENCCH_FLANG_BIN");
    eprintln!("  BENCCH_CC_BIN, BENCCH_AS_BIN, BENCCH_OTOOL_BIN, BENCCH_NM_BIN");
}

fn default_suite_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("suites")
}

fn default_report_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("reports")
}

fn discover_suites(root: PathBuf) -> Result<Vec<SuiteSpec>, String> {
    let mut files = Vec::new();
    collect_suite_files(&root, &mut files)?;
    files.sort();

    let mut suites = Vec::new();
    for file in files {
        suites.push(parse_suite_file(&file)?);
    }
    suites.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(suites)
}

fn collect_suite_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(root)
        .map_err(|e| format!("cannot read suite root '{}': {}", root.display(), e))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("cannot read entry in '{}': {}", root.display(), e))?;
        let path = entry.path();
        if path.is_dir() {
            collect_suite_files(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some(SUITE_EXTENSION) {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_suite_file(path: &Path) -> Result<SuiteSpec, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read suite '{}': {}", path.display(), e))?;

    let mut suite_name = None;
    let mut cases = Vec::new();
    let mut current = None;

    for (index, raw_line) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("suite ") {
            if suite_name.is_some() {
                return Err(format!(
                    "{}:{}: duplicate suite declaration",
                    path.display(),
                    line_no
                ));
            }
            suite_name = Some(parse_quoted(rest, path, line_no)?);
            continue;
        }

        if let Some(rest) = line.strip_prefix("case ") {
            if current.is_some() {
                return Err(format!(
                    "{}:{}: nested case without end",
                    path.display(),
                    line_no
                ));
            }
            current = Some(CaseBuilder::new(parse_quoted(rest, path, line_no)?));
            continue;
        }

        if line == "end" {
            let builder = current.take().ok_or_else(|| {
                format!("{}:{}: stray end outside of case", path.display(), line_no)
            })?;
            cases.push(builder.build(path)?);
            continue;
        }

        let builder = current.as_mut().ok_or_else(|| {
            format!(
                "{}:{}: expected suite/case declaration first",
                path.display(),
                line_no
            )
        })?;

        if let Some(rest) = line.strip_prefix("source ") {
            builder.source = Some(resolve_suite_relative_path(rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("entry ") {
            builder.graph_entry = Some(resolve_suite_relative_path(rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("file ") {
            builder
                .graph_files
                .push(resolve_suite_relative_path(rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("armfortas =>") {
            builder.requested = parse_stage_list(rest, path, line_no)?;
        } else if let Some(rest) = line.strip_prefix("repeat =>") {
            builder.repeat_count = parse_repeat_count(rest, path, line_no)?;
        } else if let Some(rest) = line.strip_prefix("opts =>") {
            builder.opt_levels = parse_opt_levels(rest, path, line_no)?;
        } else if let Some(rest) = line.strip_prefix("differential =>") {
            builder.reference_compilers = parse_reference_compilers(rest, path, line_no)?;
        } else if let Some(rest) = line.strip_prefix("consistency =>") {
            builder.consistency_checks = parse_consistency_checks(rest, path, line_no)?;
        } else if let Some(rest) = line.strip_prefix("expect-fail ") {
            builder
                .expectations
                .push(parse_failure_expectation(rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("expect ") {
            builder
                .expectations
                .push(parse_expectation(rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("xfail ") {
            builder
                .status_rules
                .push(parse_status_rule(StatusKind::Xfail, rest, path, line_no)?);
        } else if let Some(rest) = line.strip_prefix("future ") {
            builder
                .status_rules
                .push(parse_status_rule(StatusKind::Future, rest, path, line_no)?);
        } else {
            return Err(format!(
                "{}:{}: unrecognized line '{}'",
                path.display(),
                line_no,
                line
            ));
        }
    }

    if current.is_some() {
        return Err(format!("{}: unterminated case block", path.display()));
    }

    let suite_name =
        suite_name.ok_or_else(|| format!("{}: missing suite declaration", path.display()))?;
    if cases.is_empty() {
        return Err(format!("{}: suite has no cases", path.display()));
    }

    Ok(SuiteSpec {
        name: suite_name,
        path: path.to_path_buf(),
        cases,
    })
}

struct CaseBuilder {
    name: String,
    source: Option<PathBuf>,
    graph_entry: Option<PathBuf>,
    graph_files: Vec<PathBuf>,
    requested: BTreeSet<Stage>,
    opt_levels: Vec<OptLevel>,
    repeat_count: usize,
    reference_compilers: Vec<ReferenceCompiler>,
    consistency_checks: Vec<ConsistencyCheck>,
    expectations: Vec<Expectation>,
    status_rules: Vec<StatusRule>,
}

impl CaseBuilder {
    fn new(name: String) -> Self {
        Self {
            name,
            source: None,
            graph_entry: None,
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            opt_levels: Vec::new(),
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        }
    }

    fn build(self, suite_path: &Path) -> Result<CaseSpec, String> {
        if self.source.is_some() && (self.graph_entry.is_some() || !self.graph_files.is_empty()) {
            return Err(format!(
                "{}: case '{}' mixes source with graph entry/file declarations",
                suite_path.display(),
                self.name
            ));
        }

        if self.graph_entry.is_some() && self.graph_files.is_empty() {
            return Err(format!(
                "{}: case '{}' declares an entry without any file members",
                suite_path.display(),
                self.name
            ));
        }

        if self.graph_entry.is_none() && !self.graph_files.is_empty() {
            return Err(format!(
                "{}: case '{}' declares file members without an entry",
                suite_path.display(),
                self.name
            ));
        }

        let (source, graph_files) = if let Some(source) = self.source {
            (source, Vec::new())
        } else if let Some(entry) = self.graph_entry {
            if !self.graph_files.iter().any(|file| file == &entry) {
                return Err(format!(
                    "{}: case '{}' entry '{}' is not listed in file declarations",
                    suite_path.display(),
                    self.name,
                    entry.display()
                ));
            }
            (entry, self.graph_files)
        } else {
            return Err(format!(
                "{}: case '{}' is missing a source path or graph entry",
                suite_path.display(),
                self.name
            ));
        };

        let mut requested = self.requested;
        if requested.is_empty() {
            requested.insert(Stage::Run);
        }

        let opt_levels = if self.opt_levels.is_empty() {
            vec![OptLevel::O0]
        } else {
            self.opt_levels
        };

        Ok(CaseSpec {
            name: self.name,
            source,
            graph_files,
            requested,
            opt_levels,
            repeat_count: self.repeat_count,
            reference_compilers: self.reference_compilers,
            consistency_checks: self.consistency_checks,
            expectations: self.expectations,
            status_rules: self.status_rules,
        })
    }
}

fn resolve_suite_relative_path(rest: &str, path: &Path, line_no: usize) -> Result<PathBuf, String> {
    let relative = parse_quoted(rest, path, line_no)?;
    Ok(path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(relative))
}

fn parse_stage_list(rest: &str, path: &Path, line_no: usize) -> Result<BTreeSet<Stage>, String> {
    let mut stages = BTreeSet::new();
    for raw in rest.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let stage = Stage::parse(name)
            .ok_or_else(|| format!("{}:{}: unknown stage '{}'", path.display(), line_no, name))?;
        stages.insert(stage);
    }
    if stages.is_empty() {
        return Err(format!(
            "{}:{}: armfortas stage list is empty",
            path.display(),
            line_no
        ));
    }
    Ok(stages)
}

fn parse_opt_levels(rest: &str, path: &Path, line_no: usize) -> Result<Vec<OptLevel>, String> {
    let mut levels = BTreeSet::new();
    for raw in rest.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case("all") {
            levels.extend(all_opt_levels());
            continue;
        }
        let level = parse_opt_level_token(name).ok_or_else(|| {
            format!(
                "{}:{}: unknown opt level '{}'",
                path.display(),
                line_no,
                name
            )
        })?;
        levels.insert(level);
    }
    if levels.is_empty() {
        return Err(format!(
            "{}:{}: opt level list is empty",
            path.display(),
            line_no
        ));
    }
    Ok(levels.into_iter().collect())
}

fn parse_reference_compilers(
    rest: &str,
    path: &Path,
    line_no: usize,
) -> Result<Vec<ReferenceCompiler>, String> {
    let mut compilers = BTreeSet::new();
    for raw in rest.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let compiler = ReferenceCompiler::parse(name).ok_or_else(|| {
            format!(
                "{}:{}: unknown reference compiler '{}'",
                path.display(),
                line_no,
                name
            )
        })?;
        compilers.insert(compiler);
    }
    if compilers.is_empty() {
        return Err(format!(
            "{}:{}: differential compiler list is empty",
            path.display(),
            line_no
        ));
    }
    Ok(compilers.into_iter().collect())
}

fn parse_repeat_count(rest: &str, path: &Path, line_no: usize) -> Result<usize, String> {
    let count = rest.trim().parse::<usize>().map_err(|_| {
        format!(
            "{}:{}: repeat count must be an integer >= 2",
            path.display(),
            line_no
        )
    })?;
    if count < 2 {
        return Err(format!(
            "{}:{}: repeat count must be >= 2",
            path.display(),
            line_no
        ));
    }
    Ok(count)
}

fn parse_consistency_checks(
    rest: &str,
    path: &Path,
    line_no: usize,
) -> Result<Vec<ConsistencyCheck>, String> {
    let mut checks = Vec::new();
    for raw in rest.split(',') {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let check = ConsistencyCheck::parse(name).ok_or_else(|| {
            format!(
                "{}:{}: unknown consistency check '{}'",
                path.display(),
                line_no,
                name
            )
        })?;
        if !checks.contains(&check) {
            checks.push(check);
        }
    }
    if checks.is_empty() {
        return Err(format!(
            "{}:{}: consistency check list is empty",
            path.display(),
            line_no
        ));
    }
    Ok(checks)
}

fn parse_expectation(rest: &str, path: &Path, line_no: usize) -> Result<Expectation, String> {
    if let Some(prefix) = rest.strip_suffix(" check-comments") {
        return Ok(Expectation::CheckComments(parse_target(
            prefix.trim(),
            path,
            line_no,
        )?));
    }

    if let Some((target, value)) = rest.split_once(" not-contains ") {
        return Ok(Expectation::NotContains {
            target: parse_target(target.trim(), path, line_no)?,
            needle: parse_quoted(value.trim(), path, line_no)?,
        });
    }

    if let Some((target, value)) = rest.split_once(" contains ") {
        return Ok(Expectation::Contains {
            target: parse_target(target.trim(), path, line_no)?,
            needle: parse_quoted(value.trim(), path, line_no)?,
        });
    }

    if let Some((target, value)) = rest.split_once(" equals ") {
        let target = parse_target(target.trim(), path, line_no)?;
        if matches!(target, Target::RunExitCode) {
            let value = parse_integer(value.trim(), path, line_no)?;
            return Ok(Expectation::IntEquals { target, value });
        }
        return Ok(Expectation::Equals {
            target,
            value: parse_quoted(value.trim(), path, line_no)?,
        });
    }

    Err(format!(
        "{}:{}: unsupported expectation '{}'",
        path.display(),
        line_no,
        rest
    ))
}

fn parse_failure_expectation(
    rest: &str,
    path: &Path,
    line_no: usize,
) -> Result<Expectation, String> {
    if let Some((target, value)) = rest.split_once(" contains ") {
        return Ok(Expectation::FailContains {
            stage: parse_failure_stage(target.trim(), path, line_no)?,
            needle: parse_quoted(value.trim(), path, line_no)?,
        });
    }

    if let Some((target, value)) = rest.split_once(" equals ") {
        return Ok(Expectation::FailEquals {
            stage: parse_failure_stage(target.trim(), path, line_no)?,
            value: parse_quoted(value.trim(), path, line_no)?,
        });
    }

    Err(format!(
        "{}:{}: unsupported failure expectation '{}'",
        path.display(),
        line_no,
        rest
    ))
}

fn parse_status_rule(
    kind: StatusKind,
    rest: &str,
    path: &Path,
    line_no: usize,
) -> Result<StatusRule, String> {
    let rest = rest.trim();
    if rest.starts_with('"') {
        return Ok(StatusRule {
            kind,
            selector: OptSelector::All,
            reason: parse_quoted(rest, path, line_no)?,
        });
    }

    let conditional = rest.strip_prefix("when ").ok_or_else(|| {
        format!(
            "{}:{}: expected quoted reason or 'when <opts> because \"...\"'",
            path.display(),
            line_no
        )
    })?;
    let (selector, reason) = conditional.split_once(" because ").ok_or_else(|| {
        format!(
            "{}:{}: conditional status must use 'when <opts> because \"...\"'",
            path.display(),
            line_no
        )
    })?;

    Ok(StatusRule {
        kind,
        selector: parse_opt_selector(selector.trim(), path, line_no)?,
        reason: parse_quoted(reason.trim(), path, line_no)?,
    })
}

fn parse_opt_selector(raw: &str, path: &Path, line_no: usize) -> Result<OptSelector, String> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("all") {
        return Ok(OptSelector::All);
    }
    if let Some(rest) = raw.strip_prefix("opts =>") {
        return Ok(OptSelector::Only(parse_opt_levels(rest, path, line_no)?));
    }
    Ok(OptSelector::Only(parse_opt_levels(raw, path, line_no)?))
}

fn parse_target(raw: &str, path: &Path, line_no: usize) -> Result<Target, String> {
    match raw {
        "run.stdout" => Ok(Target::RunStdout),
        "run.stderr" => Ok(Target::RunStderr),
        "run.exit_code" => Ok(Target::RunExitCode),
        _ => {
            let stage = Stage::parse(raw).ok_or_else(|| {
                format!(
                    "{}:{}: unsupported expectation target '{}'",
                    path.display(),
                    line_no,
                    raw
                )
            })?;
            Ok(Target::Stage(stage))
        }
    }
}

fn parse_failure_stage(raw: &str, path: &Path, line_no: usize) -> Result<FailureStage, String> {
    FailureStage::parse(raw).ok_or_else(|| {
        format!(
            "{}:{}: unsupported failure stage '{}'",
            path.display(),
            line_no,
            raw
        )
    })
}

fn parse_quoted(raw: &str, path: &Path, line_no: usize) -> Result<String, String> {
    let raw = raw.trim();
    if !(raw.starts_with('"') && raw.ends_with('"')) {
        return Err(format!(
            "{}:{}: expected quoted string, got '{}'",
            path.display(),
            line_no,
            raw
        ));
    }
    let body = &raw[1..raw.len() - 1];
    Ok(body.replace("\\\"", "\"").replace("\\n", "\n"))
}

fn parse_integer(raw: &str, path: &Path, line_no: usize) -> Result<i32, String> {
    let value = if raw.starts_with('"') {
        parse_quoted(raw, path, line_no)?
    } else {
        raw.trim().to_string()
    };
    value.parse::<i32>().map_err(|_| {
        format!(
            "{}:{}: expected integer literal, got '{}'",
            path.display(),
            line_no,
            raw
        )
    })
}

fn parse_opt_level_token(raw: &str) -> Option<OptLevel> {
    let raw = raw.trim();
    let raw = raw.strip_prefix('-').unwrap_or(raw);
    OptLevel::parse_flag(raw)
}

fn parse_opt_level_list(raw: &str) -> Result<Vec<OptLevel>, String> {
    let mut levels = BTreeSet::new();
    for value in raw.split(',') {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if value.eq_ignore_ascii_case("all") {
            levels.extend(all_opt_levels());
            continue;
        }
        let level =
            parse_opt_level_token(value).ok_or_else(|| format!("unknown opt level '{}'", value))?;
        levels.insert(level);
    }
    if levels.is_empty() {
        return Err("opt filter is empty".into());
    }
    Ok(levels.into_iter().collect())
}

fn all_opt_levels() -> [OptLevel; 6] {
    [
        OptLevel::O0,
        OptLevel::O1,
        OptLevel::O2,
        OptLevel::O3,
        OptLevel::Os,
        OptLevel::Ofast,
    ]
}

fn filter_suites<'a>(suites: &'a [SuiteSpec], suite_filter: Option<&str>) -> Vec<&'a SuiteSpec> {
    let filter = suite_filter.map(|value| value.to_ascii_lowercase());
    suites
        .iter()
        .filter(|suite| {
            if let Some(filter) = &filter {
                suite.name.to_ascii_lowercase().contains(filter)
            } else {
                true
            }
        })
        .collect()
}

fn print_suites(suites: &[&SuiteSpec]) {
    for suite in suites {
        println!("{} ({})", suite.name, suite.cases.len());
        println!("  {}", suite.path.display());
    }
}

fn run_suites(config: &RunConfig) -> Result<Summary, String> {
    let suites = discover_suites(default_suite_root())?;
    let suites = filter_suites(&suites, config.suite_filter.as_deref());
    if suites.is_empty() {
        return Err("no suites matched the requested filter".into());
    }

    let case_filter = config
        .case_filter
        .as_ref()
        .map(|value| value.to_ascii_lowercase());
    let mut summary = Summary::default();
    let mut matched_cells = 0usize;

    for suite in suites {
        println!("=== {} ===", suite.name);
        for case in &suite.cases {
            if let Some(filter) = &case_filter {
                if !case.name.to_ascii_lowercase().contains(filter) {
                    continue;
                }
            }

            let opt_levels = selected_opt_levels(case, config);
            for opt_level in opt_levels {
                matched_cells += 1;
                let outcome = execute_case_cell(suite, case, opt_level, config)?;
                print_outcome(&outcome);
                summary.record_outcome(&outcome);

                if config.fail_fast
                    && matches!(outcome.kind, OutcomeKind::Fail | OutcomeKind::Xpass)
                {
                    return Ok(summary);
                }
            }
        }
    }

    if matched_cells == 0 {
        return Err("no cases matched the requested filters".into());
    }

    Ok(summary)
}

fn selected_opt_levels(case: &CaseSpec, config: &RunConfig) -> Vec<OptLevel> {
    case.opt_levels
        .iter()
        .copied()
        .filter(|level| {
            config
                .opt_filter
                .as_ref()
                .map(|filter| filter.contains(level))
                .unwrap_or(true)
        })
        .collect()
}

fn execute_case_cell(
    suite: &SuiteSpec,
    case: &CaseSpec,
    opt_level: OptLevel,
    config: &RunConfig,
) -> Result<Outcome, String> {
    let effective_status = status_for_opt(case, opt_level);
    if let EffectiveStatus::Future(reason) = &effective_status {
        if !config.include_future {
            return Ok(Outcome {
                suite: suite.name.clone(),
                case: case.name.clone(),
                opt_level,
                kind: OutcomeKind::Future,
                detail: reason.clone(),
                bundle: None,
                consistency_observations: Vec::new(),
            });
        }
    }

    let mut requested = case.requested.clone();
    if config.all_stages {
        requested.extend(Stage::ALL);
    }
    for expectation in &case.expectations {
        ensure_target_stage(expectation, &mut requested);
    }
    if !case.reference_compilers.is_empty() {
        requested.insert(Stage::Run);
    }
    for check in &case.consistency_checks {
        ensure_consistency_stage(*check, &mut requested);
    }

    let prepared = prepare_case_input(case, suite, opt_level)?;

    if config.verbose {
        let stage_list = requested
            .iter()
            .map(Stage::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let refs = if case.reference_compilers.is_empty() {
            "none".to_string()
        } else {
            case.reference_compilers
                .iter()
                .map(ReferenceCompiler::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("  source: {}", case.source_label());
        if case.is_graph() {
            for file in &case.graph_files {
                println!("  file: {}", file.display());
            }
            println!("  compiled_as: separate translation units");
        }
        println!("  opt: {}", opt_level.as_str());
        println!("  stages: {}", stage_list);
        println!("  refs: {}", refs);
        if !case.consistency_checks.is_empty() {
            println!("  repeat: {}", case.repeat_count);
        }
    }

    let references = run_reference_compilers(&prepared, case, opt_level, &config.tools);
    let mut artifacts = ExecutionArtifacts {
        requested: requested.clone(),
        armfortas: None,
        armfortas_failure: None,
        references,
        consistency_issues: Vec::new(),
    };

    match capture_prepared_input(&prepared, &requested, opt_level) {
        Ok(result) => artifacts.armfortas = Some(result),
        Err(failure) => artifacts.armfortas_failure = Some(failure),
    }

    let execution = match (&artifacts.armfortas, &artifacts.armfortas_failure) {
        (Some(result), None) => {
            if has_failure_expectation(case) {
                Err(format!(
                    "expected armfortas to fail ({}) but compilation succeeded",
                    expected_failure_description(case)
                ))
            } else {
                let mut execution = evaluate_positive_expectations(case, result);
                if execution.is_ok() && !artifacts.references.is_empty() {
                    execution = compare_differential(result, &artifacts.references);
                }
                if execution.is_ok() && !case.consistency_checks.is_empty() {
                    artifacts.consistency_issues =
                        run_consistency_checks(case, &prepared, opt_level, result, &config.tools);
                    if !artifacts.consistency_issues.is_empty() {
                        execution = Err(format_consistency_issues(&artifacts.consistency_issues));
                    }
                }
                execution
            }
        }
        (None, Some(failure)) => {
            let partial = failure.partial_result();
            let mut execution = evaluate_positive_expectations(case, &partial);
            if execution.is_ok() {
                if has_failure_expectation(case) {
                    execution = evaluate_failure_expectations(case, failure);
                } else {
                    execution = Err(compose_armfortas_failure_detail(&artifacts));
                }
            }
            if execution.is_ok() && !artifacts.references.is_empty() {
                execution =
                    Err("differential comparison requires a successful armfortas run".to_string());
            }
            execution
        }
        (Some(_), Some(_)) => Err("armfortas produced both a result and a failure".into()),
        (None, None) => Err("armfortas produced neither a result nor a failure".into()),
    };

    let consistency_observations = artifacts
        .consistency_issues
        .iter()
        .map(ConsistencyIssue::observation)
        .collect::<Vec<_>>();

    let mut outcome = match (effective_status, execution) {
        (EffectiveStatus::Normal, Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Pass,
            detail: String::new(),
            bundle: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Normal, Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Fail,
            detail,
            bundle: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Xfail(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xpass,
            detail: reason,
            bundle: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Xfail(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xfail,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Future(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Pass,
            detail: reason,
            bundle: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Future(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Fail,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
            consistency_observations,
        },
    };

    let should_bundle = matches!(outcome.kind, OutcomeKind::Fail | OutcomeKind::Xpass)
        || (matches!(outcome.kind, OutcomeKind::Xfail) && !artifacts.consistency_issues.is_empty());

    if should_bundle {
        match write_failure_bundle(suite, case, &prepared, &outcome, &artifacts) {
            Ok(bundle) => outcome.bundle = Some(bundle),
            Err(err) => {
                if outcome.detail.is_empty() {
                    outcome.detail = format!("failed to write failure bundle: {}", err);
                } else {
                    outcome.detail.push_str(&format!(
                        "\n\nwarning: failed to write failure bundle: {}",
                        err
                    ));
                }
            }
        }
    }

    cleanup_prepared_input(&prepared);
    cleanup_consistency_issues(&artifacts.consistency_issues);

    Ok(outcome)
}

fn capture_prepared_input(
    prepared: &PreparedInput,
    requested: &BTreeSet<Stage>,
    opt_level: OptLevel,
) -> Result<CaptureResult, CaptureFailure> {
    if prepared.is_graph() {
        let work_root = prepared
            .temp_root
            .as_deref()
            .expect("prepared graph input must own a work directory");
        capture_graph(
            &prepared.compiler_source,
            &prepared.graph_sources,
            requested,
            opt_level,
            work_root,
        )
    } else {
        let request = CaptureRequest {
            input: prepared.compiler_source.clone(),
            requested: requested.clone(),
            opt_level,
        };
        capture_from_path(&request)
    }
}

fn prepare_case_input(
    case: &CaseSpec,
    suite: &SuiteSpec,
    opt_level: OptLevel,
) -> Result<PreparedInput, String> {
    if case.graph_files.is_empty() {
        return Ok(PreparedInput {
            compiler_source: case.source.clone(),
            graph_sources: Vec::new(),
            temp_root: None,
        });
    }

    let temp_root = default_report_root().join(".tmp").join(format!(
        "graph_{}_{}_{}",
        sanitize_component(&suite.name),
        sanitize_component(&case.name),
        next_report_suffix(opt_level)
    ));
    fs::create_dir_all(&temp_root).map_err(|e| {
        format!(
            "cannot create graph temp dir '{}': {}",
            temp_root.display(),
            e
        )
    })?;

    for file in &case.graph_files {
        fs::metadata(file)
            .map_err(|e| format!("cannot inspect graph file '{}': {}", file.display(), e))?;
    }

    Ok(PreparedInput {
        compiler_source: case.source.clone(),
        graph_sources: case.graph_files.clone(),
        temp_root: Some(temp_root),
    })
}

fn cleanup_prepared_input(prepared: &PreparedInput) {
    if let Some(temp_root) = &prepared.temp_root {
        let _ = fs::remove_dir_all(temp_root);
    }
}

fn status_for_opt(case: &CaseSpec, opt_level: OptLevel) -> EffectiveStatus {
    let mut status = EffectiveStatus::Normal;
    for rule in &case.status_rules {
        if rule.selector.matches(opt_level) {
            status = match rule.kind {
                StatusKind::Xfail => EffectiveStatus::Xfail(rule.reason.clone()),
                StatusKind::Future => EffectiveStatus::Future(rule.reason.clone()),
            };
        }
    }
    status
}

fn ensure_target_stage(expectation: &Expectation, requested: &mut BTreeSet<Stage>) {
    match expectation {
        Expectation::CheckComments(target)
        | Expectation::Contains { target, .. }
        | Expectation::NotContains { target, .. }
        | Expectation::Equals { target, .. }
        | Expectation::IntEquals { target, .. } => match target {
            Target::Stage(stage) => {
                requested.insert(*stage);
            }
            Target::RunStdout | Target::RunStderr | Target::RunExitCode => {
                requested.insert(Stage::Run);
            }
        },
        Expectation::FailContains { .. } | Expectation::FailEquals { .. } => {}
    }
}

fn ensure_consistency_stage(check: ConsistencyCheck, requested: &mut BTreeSet<Stage>) {
    if let Some(stage) = check.required_stage() {
        requested.insert(stage);
    }
}

fn evaluate_positive_expectations(case: &CaseSpec, result: &CaptureResult) -> Result<(), String> {
    for expectation in &case.expectations {
        match expectation {
            Expectation::CheckComments(target) => {
                let text = target_text(result, target)?;
                let source = fs::read_to_string(&case.source)
                    .map_err(|e| format!("cannot read '{}': {}", case.source.display(), e))?;
                let checks = extract_checks(&source);
                let file_checks = extract_file_checks(&source, &case.source)?;
                if checks.is_empty() && file_checks.is_empty() {
                    return Err(format!(
                        "case '{}' requested check-comments but '{}' has no supported \
                         ! CHECK:, ! FILE_CHECK:, or ! FILE_NOT: lines",
                        case.name,
                        case.source.display()
                    ));
                }
                if !checks.is_empty() {
                    match_checks(&checks, text, &case.name)?;
                }
                if !file_checks.is_empty() {
                    if !matches!(*target, Target::RunStdout) {
                        return Err(format!(
                            "case '{}' uses FILE_CHECK/FILE_NOT but applies check-comments to {}; \
                             file directives require run.stdout check-comments",
                            case.name,
                            target_name(*target)
                        ));
                    }
                    let run = result
                        .get(Stage::Run)
                        .and_then(CapturedStage::as_run)
                        .ok_or_else(|| {
                            format!(
                                "case '{}' uses FILE_CHECK/FILE_NOT but has no captured run stage",
                                case.name
                            )
                        })?;
                    match_file_checks(&file_checks, &run.files, &case.source)?;
                }
            }
            Expectation::Contains { target, needle } => {
                let text = target_text(result, target)?;
                if !text.contains(needle) {
                    return Err(format!(
                        "expected {} to contain {:?}\nactual:\n{}",
                        target_name(*target),
                        needle,
                        text
                    ));
                }
            }
            Expectation::NotContains { target, needle } => {
                let text = target_text(result, target)?;
                if text.contains(needle) {
                    return Err(format!(
                        "expected {} to not contain {:?}\nactual:\n{}",
                        target_name(*target),
                        needle,
                        text
                    ));
                }
            }
            Expectation::Equals { target, value } => {
                let text = target_text(result, target)?;
                if text.trim_end() != value {
                    return Err(format!(
                        "expected {} to equal {:?}\nactual:\n{}",
                        target_name(*target),
                        value,
                        text
                    ));
                }
            }
            Expectation::IntEquals { target, value } => {
                let actual = target_int(result, target)?;
                if actual != *value {
                    return Err(format!(
                        "expected {} to equal {}\nactual: {}",
                        target_name(*target),
                        value,
                        actual
                    ));
                }
            }
            Expectation::FailContains { .. } | Expectation::FailEquals { .. } => {}
        }
    }
    Ok(())
}

fn evaluate_failure_expectations(case: &CaseSpec, failure: &CaptureFailure) -> Result<(), String> {
    let mut saw_failure_expectation = false;
    for expectation in &case.expectations {
        match expectation {
            Expectation::FailContains { stage, needle } => {
                saw_failure_expectation = true;
                if failure.stage != *stage {
                    return Err(format!(
                        "expected failure stage {} but armfortas failed in {}\n{}",
                        stage.as_str(),
                        failure.stage.as_str(),
                        failure.detail
                    ));
                }
                if !failure.detail.contains(needle) {
                    return Err(format!(
                        "expected failure detail at {} to contain {:?}\nactual:\n{}",
                        stage.as_str(),
                        needle,
                        failure.detail
                    ));
                }
            }
            Expectation::FailEquals { stage, value } => {
                saw_failure_expectation = true;
                if failure.stage != *stage {
                    return Err(format!(
                        "expected failure stage {} but armfortas failed in {}\n{}",
                        stage.as_str(),
                        failure.stage.as_str(),
                        failure.detail
                    ));
                }
                if failure.detail.trim_end() != value {
                    return Err(format!(
                        "expected failure detail at {} to equal {:?}\nactual:\n{}",
                        stage.as_str(),
                        value,
                        failure.detail
                    ));
                }
            }
            Expectation::CheckComments(_)
            | Expectation::Contains { .. }
            | Expectation::NotContains { .. }
            | Expectation::Equals { .. }
            | Expectation::IntEquals { .. } => {}
        }
    }

    if !saw_failure_expectation {
        return Err(format!(
            "armfortas failed in {} but the case did not declare an expect-fail rule\n{}",
            failure.stage.as_str(),
            failure.detail
        ));
    }

    Ok(())
}

fn has_failure_expectation(case: &CaseSpec) -> bool {
    case.expectations.iter().any(|expectation| {
        matches!(
            expectation,
            Expectation::FailContains { .. } | Expectation::FailEquals { .. }
        )
    })
}

fn expected_failure_description(case: &CaseSpec) -> String {
    let mut items = Vec::new();
    for expectation in &case.expectations {
        match expectation {
            Expectation::FailContains { stage, needle } => {
                items.push(format!("{} contains {:?}", stage.as_str(), needle));
            }
            Expectation::FailEquals { stage, value } => {
                items.push(format!("{} equals {:?}", stage.as_str(), value));
            }
            _ => {}
        }
    }
    if items.is_empty() {
        "declared failure".to_string()
    } else {
        items.join(", ")
    }
}

fn target_text<'a>(result: &'a CaptureResult, target: &Target) -> Result<&'a str, String> {
    match target {
        Target::Stage(stage) => match result.get(*stage) {
            Some(CapturedStage::Text(text)) => Ok(text),
            Some(CapturedStage::Run(_)) => {
                Err(format!("stage '{}' is not textual", stage.as_str()))
            }
            None => Err(format!("missing captured stage '{}'", stage.as_str())),
        },
        Target::RunStdout => match result.get(Stage::Run).and_then(CapturedStage::as_run) {
            Some(run) => captured_output_text(&run.stdout, "run.stdout"),
            None => Err("missing captured run stage".into()),
        },
        Target::RunStderr => match result.get(Stage::Run).and_then(CapturedStage::as_run) {
            Some(run) => captured_output_text(&run.stderr, "run.stderr"),
            None => Err("missing captured run stage".into()),
        },
        Target::RunExitCode => {
            Err("run.exit_code is numeric; use 'expect run.exit_code equals <int>'".into())
        }
    }
}

fn captured_output_text<'a>(bytes: &'a [u8], target: &str) -> Result<&'a str, String> {
    std::str::from_utf8(bytes).map_err(|error| {
        format!(
            "{target} is not valid UTF-8 (first invalid byte at offset {})",
            error.valid_up_to()
        )
    })
}

fn target_int(result: &CaptureResult, target: &Target) -> Result<i32, String> {
    match target {
        Target::RunExitCode => match result.get(Stage::Run).and_then(CapturedStage::as_run) {
            Some(run) => Ok(run.exit_code),
            None => Err("missing captured run stage".into()),
        },
        _ => Err(format!(
            "{} is textual; use a string matcher instead",
            target_name(*target)
        )),
    }
}

fn target_name(target: Target) -> &'static str {
    match target {
        Target::Stage(stage) => stage.as_str(),
        Target::RunStdout => "run.stdout",
        Target::RunStderr => "run.stderr",
        Target::RunExitCode => "run.exit_code",
    }
}

fn compare_differential(
    result: &CaptureResult,
    references: &[ReferenceResult],
) -> Result<(), String> {
    let arm_run = result
        .get(Stage::Run)
        .and_then(CapturedStage::as_run)
        .ok_or("differential comparison requires the run stage")?;
    let arm_sig = normalize_run_signature(arm_run);

    let mut reference_sigs = BTreeSet::new();
    let mut matching_refs = 0usize;
    let mut detail = Vec::new();

    for reference in references {
        if reference.compile_exit_code != 0 {
            return Err(format!(
                "reference compiler '{}' failed to compile\n{}",
                reference.compiler.as_str(),
                format_reference_result(reference)
            ));
        }

        if let Some(run_error) = &reference.run_error {
            return Err(format!(
                "reference compiler '{}' built but could not run: {}\n{}",
                reference.compiler.as_str(),
                run_error,
                format_reference_result(reference)
            ));
        }

        let signature = reference.run_signature().ok_or_else(|| {
            format!(
                "reference compiler '{}' did not produce a run result",
                reference.compiler.as_str()
            )
        })?;

        if signature == arm_sig {
            matching_refs += 1;
        } else {
            detail.push(format_reference_result(reference));
        }
        reference_sigs.insert(signature);
    }

    if matching_refs == references.len() {
        return Ok(());
    }

    let classification = if matching_refs == 0 && reference_sigs.len() == 1 {
        "classification: armfortas-only divergence"
    } else if reference_sigs.len() > 1 {
        "classification: reference disagreement"
    } else {
        "classification: partial disagreement"
    };

    Err(format!(
        "behavior mismatch against reference compilers\n{}\n\narmfortas\n{}\n\n{}",
        classification,
        format_run_capture(arm_run),
        detail.join("\n\n")
    ))
}

fn compose_armfortas_failure_detail(artifacts: &ExecutionArtifacts) -> String {
    let mut detail = String::new();
    if let Some(failure) = &artifacts.armfortas_failure {
        detail.push_str(&format!(
            "armfortas failed in {}\n{}",
            failure.stage.as_str(),
            failure.detail
        ));
    } else {
        detail.push_str("armfortas failed without an error message");
    }

    if !artifacts.references.is_empty() {
        detail.push_str("\n\nreference compilers\n");
        detail.push_str(&format_reference_summary(&artifacts.references));
    }

    detail
}

fn run_consistency_checks(
    case: &CaseSpec,
    prepared: &PreparedInput,
    opt_level: OptLevel,
    capture_result: &CaptureResult,
    tools: &ToolchainConfig,
) -> Vec<ConsistencyIssue> {
    let mut failures = Vec::new();
    for check in &case.consistency_checks {
        let issue = if prepared.is_graph()
            && !matches!(
                check,
                ConsistencyCheck::CliRunReproducible
                    | ConsistencyCheck::CaptureRunVsCliRun
                    | ConsistencyCheck::CaptureRunReproducible
            ) {
            unsupported_graph_consistency_issue(*check, opt_level)
        } else {
            match check {
                ConsistencyCheck::CliObjVsSystemAs => {
                    run_cli_obj_vs_system_as(&prepared.compiler_source, opt_level, tools)
                }
                ConsistencyCheck::CliAsmReproducible => run_cli_asm_reproducible(
                    &prepared.compiler_source,
                    opt_level,
                    case.repeat_count,
                    tools,
                ),
                ConsistencyCheck::CliObjReproducible => run_cli_obj_reproducible(
                    &prepared.compiler_source,
                    opt_level,
                    case.repeat_count,
                    tools,
                ),
                ConsistencyCheck::CliRunReproducible => {
                    run_cli_run_reproducible(prepared, opt_level, case.repeat_count, tools)
                }
                ConsistencyCheck::CaptureAsmVsCliAsm => run_capture_asm_vs_cli_asm(
                    &prepared.compiler_source,
                    opt_level,
                    case.repeat_count,
                    capture_result,
                    tools,
                ),
                ConsistencyCheck::CaptureObjVsCliObj => run_capture_obj_vs_cli_obj(
                    &prepared.compiler_source,
                    opt_level,
                    case.repeat_count,
                    capture_result,
                    tools,
                ),
                ConsistencyCheck::CaptureRunVsCliRun => run_capture_run_vs_cli_run(
                    prepared,
                    opt_level,
                    case.repeat_count,
                    capture_result,
                    tools,
                ),
                ConsistencyCheck::CaptureAsmReproducible => run_capture_asm_reproducible(
                    &prepared.compiler_source,
                    opt_level,
                    case.repeat_count,
                    capture_result,
                    tools,
                ),
                ConsistencyCheck::CaptureObjReproducible => run_capture_obj_reproducible(
                    &prepared.compiler_source,
                    opt_level,
                    case.repeat_count,
                    capture_result,
                    tools,
                ),
                ConsistencyCheck::CaptureRunReproducible => run_capture_run_reproducible(
                    prepared,
                    opt_level,
                    case.repeat_count,
                    capture_result,
                    tools,
                ),
            }
        };
        if let Some(issue) = issue {
            failures.push(issue);
        }
    }
    failures
}

fn unsupported_graph_consistency_issue(
    check: ConsistencyCheck,
    opt_level: OptLevel,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    Some(ConsistencyIssue {
        check,
        summary: "consistency check has no graph-aware artifact contract".into(),
        repeat_count: None,
        unique_variant_count: None,
        varying_components: Vec::new(),
        stable_components: Vec::new(),
        detail: format!(
            "consistency check '{}' cannot be applied to a graph as if its entry source were the whole program",
            check.as_str()
        ),
        temp_root,
    })
}

fn format_consistency_issues(issues: &[ConsistencyIssue]) -> String {
    issues
        .iter()
        .map(|issue| {
            format!(
                "consistency check '{}' failed\n{}",
                issue.check.as_str(),
                issue.detail
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn cleanup_consistency_issues(issues: &[ConsistencyIssue]) {
    for issue in issues {
        let _ = fs::remove_dir_all(&issue.temp_root);
    }
}

fn run_cli_obj_vs_system_as(
    source: &Path,
    opt_level: OptLevel,
    tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliObjVsSystemAs,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let asm_path = temp_root.join("from_cli.s");
    let asm_obj_path = temp_root.join("from_cli_asm.o");
    let obj_path = temp_root.join("from_cli_obj.o");

    let asm_command =
        match compile_with_driver(source, opt_level, DriverEmitMode::Asm, &asm_path, tools) {
            Ok(command) => command,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliObjVsSystemAs,
                    summary: "armfortas -S failed during consistency check".into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };

    let as_args = vec![
        "-o".to_string(),
        asm_obj_path.display().to_string(),
        asm_path.display().to_string(),
    ];
    let as_command = render_command(tools.system_as_bin(), &as_args);
    let as_output = match run_managed(
        Command::new(tools.system_as_bin()).args([
            "-o",
            asm_obj_path.to_str().unwrap(),
            asm_path.to_str().unwrap(),
        ]),
        CommandClass::Tool,
    ) {
        Ok(output) => output,
        Err(err) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CliObjVsSystemAs,
                summary: "system assembler invocation failed".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("{}\ncannot run assembler: {}", as_command, err),
                temp_root,
            })
        }
    };
    if !as_output.status.success() {
        let stderr = String::from_utf8_lossy(&as_output.stderr);
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliObjVsSystemAs,
            summary: "system assembler rejected armfortas -S output".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!("{}\nassembler failed:\n{}", as_command, stderr),
            temp_root,
        });
    }

    let obj_command =
        match compile_with_driver(source, opt_level, DriverEmitMode::Obj, &obj_path, tools) {
            Ok(command) => command,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliObjVsSystemAs,
                    summary: "armfortas -c failed during consistency check".into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };

    let asm_snapshot = match object_snapshot(&asm_obj_path, tools) {
        Ok(snapshot) => snapshot,
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CliObjVsSystemAs,
                summary: "could not snapshot object assembled from -S output".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("{}\n{}", as_command, detail),
                temp_root,
            })
        }
    };
    let obj_snapshot = match object_snapshot(&obj_path, tools) {
        Ok(snapshot) => snapshot,
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CliObjVsSystemAs,
                summary: "could not snapshot object from armfortas -c".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("{}\n{}", obj_command, detail),
                temp_root,
            })
        }
    };

    if asm_snapshot != obj_snapshot {
        let snapshots = [&asm_snapshot, &obj_snapshot];
        let varying = varying_object_components(&snapshots)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let stable = stable_object_components(&snapshots)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliObjVsSystemAs,
            summary: format!(
                "varying_components={} stable_components={}",
                join_or_none_from_strings(&varying),
                join_or_none_from_strings(&stable)
            ),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: varying,
            stable_components: stable,
            detail: format!(
                "object snapshot mismatch between armfortas -S | as and armfortas -c\n{}\n{}\n{}\n{}",
                asm_command,
                as_command,
                obj_command,
                describe_object_difference(&asm_snapshot, &obj_snapshot, "-S | as", "-c")
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_cli_asm_reproducible(
    source: &Path,
    opt_level: OptLevel,
    repeat_count: usize,
    tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliAsmReproducible,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let mut runs = Vec::new();
    for index in 0..repeat_count {
        let asm_path = temp_root.join(format!("run_{:02}.s", index));
        let command =
            match compile_with_driver(source, opt_level, DriverEmitMode::Asm, &asm_path, tools) {
                Ok(command) => command,
                Err(detail) => {
                    return Some(ConsistencyIssue {
                        check: ConsistencyCheck::CliAsmReproducible,
                        summary: "armfortas -S failed during reproducibility check".into(),
                        repeat_count: None,
                        unique_variant_count: None,
                        varying_components: Vec::new(),
                        stable_components: Vec::new(),
                        detail,
                        temp_root,
                    })
                }
            };
        let text = match read_text_artifact(&asm_path) {
            Ok(text) => text,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliAsmReproducible,
                    summary: "could not read emitted assembly during reproducibility check".into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        runs.push(TextRun {
            label: format!("run {} (-S)", index + 1),
            command,
            normalized: normalize_text_artifact(&text),
        });
    }

    let unique_variants = count_unique_strings(runs.iter().map(|run| run.normalized.as_str()));
    if unique_variants > 1 {
        let (left, right) =
            first_distinct_text_pair(&runs).expect("unique variants > 1 implies a distinct pair");
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliAsmReproducible,
            summary: format!("repeat_count={} unique_variants={}", repeat_count, unique_variants),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_variants),
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "assembly output is not reproducible across repeated armfortas -S runs\nrepeat count: {}\nunique variants: {}\n{}\n{}\n{}",
                repeat_count,
                unique_variants,
                left.command,
                right.command,
                describe_text_difference(&left.normalized, &right.normalized, &left.label, &right.label)
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_cli_obj_reproducible(
    source: &Path,
    opt_level: OptLevel,
    repeat_count: usize,
    tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliObjReproducible,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let mut runs = Vec::new();
    for index in 0..repeat_count {
        let obj_path = temp_root.join(format!("run_{:02}.o", index));
        let command =
            match compile_with_driver(source, opt_level, DriverEmitMode::Obj, &obj_path, tools) {
                Ok(command) => command,
                Err(detail) => {
                    return Some(ConsistencyIssue {
                        check: ConsistencyCheck::CliObjReproducible,
                        summary: "armfortas -c failed during reproducibility check".into(),
                        repeat_count: None,
                        unique_variant_count: None,
                        varying_components: Vec::new(),
                        stable_components: Vec::new(),
                        detail,
                        temp_root,
                    })
                }
            };
        let snapshot = match object_snapshot(&obj_path, tools) {
            Ok(snapshot) => snapshot,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliObjReproducible,
                    summary: "could not snapshot object during reproducibility check".into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail: format!("{}\n{}", command, detail),
                    temp_root,
                })
            }
        };
        runs.push(ObjectRun {
            label: format!("run {} (-c)", index + 1),
            command,
            snapshot,
        });
    }

    let rendered = runs
        .iter()
        .map(|run| render_object_snapshot(&run.snapshot))
        .collect::<Vec<_>>();
    let unique_variants = count_unique_strings(rendered.iter().map(String::as_str));
    if unique_variants > 1 {
        let snapshots = runs.iter().map(|run| &run.snapshot).collect::<Vec<_>>();
        let (left, right) =
            first_distinct_object_pair(&runs).expect("unique variants > 1 implies a distinct pair");
        let varying = join_or_none(&varying_object_components(&snapshots));
        let stable = join_or_none(&stable_object_components(&snapshots));
        let varying_components = varying_object_components(&snapshots)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let stable_components = stable_object_components(&snapshots)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliObjReproducible,
            summary: format!(
                "repeat_count={} unique_variants={} varying_components={} stable_components={}",
                repeat_count, unique_variants, varying, stable
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_variants),
            varying_components,
            stable_components,
            detail: format!(
                "object output is not reproducible across repeated armfortas -c runs\nrepeat count: {}\nunique variants: {}\nvarying components across repeats: {}\nstable components across repeats: {}\n{}\n{}\n{}",
                repeat_count,
                unique_variants,
                varying,
                stable,
                left.command,
                right.command,
                describe_object_difference(&left.snapshot, &right.snapshot, &left.label, &right.label)
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_cli_run_reproducible(
    prepared: &PreparedInput,
    opt_level: OptLevel,
    repeat_count: usize,
    tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliRunReproducible,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let mut runs = Vec::new();
    for index in 0..repeat_count {
        let binary_path = temp_root.join(format!("cli_run_{:02}.out", index));
        let build_command = match compile_prepared_with_driver(
            prepared,
            opt_level,
            DriverEmitMode::Binary,
            &binary_path,
            tools,
        ) {
            Ok(command) => command,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliRunReproducible,
                    summary: "armfortas binary build failed during runtime reproducibility check"
                        .into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let run_command = render_binary_run_command(&binary_path);
        let run_sandbox = temp_root.join(format!("cli_run_{:02}.sandbox", index));
        let run = match run_binary_capture(&binary_path, &run_sandbox, &run_command) {
            Ok(run) => run,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliRunReproducible,
                    summary: "armfortas binary could not run during runtime reproducibility check"
                        .into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let command = format!("build: {}\nrun: {}", build_command, run_command);
        if let Err(err) = write_behavior_run_artifacts(
            &temp_root,
            &format!("cli_run_{:02}", index),
            &command,
            &run,
        ) {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CliRunReproducible,
                summary: "could not write cli runtime artifact".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("cannot write cli runtime artifact: {}", err),
                temp_root,
            });
        }
        runs.push(BehaviorRun {
            label: format!("cli run {}", index + 1),
            command,
            signature: normalize_run_signature(&run),
            run,
        });
    }

    let unique_variants = count_unique_run_signatures(runs.iter().map(|run| &run.signature));
    if unique_variants > 1 {
        let signatures = runs.iter().map(|run| &run.signature).collect::<Vec<_>>();
        let varying = varying_run_components(&signatures);
        let stable = stable_run_components(&signatures);
        let (left, right) = first_distinct_behavior_pair(&runs)
            .expect("unique variants > 1 implies a distinct pair");
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliRunReproducible,
            summary: format!(
                "repeat_count={} unique_variants={} varying_components={} stable_components={}",
                repeat_count,
                unique_variants,
                join_or_none(&varying),
                join_or_none(&stable)
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_variants),
            varying_components: varying.iter().map(|value| (*value).to_string()).collect(),
            stable_components: stable.iter().map(|value| (*value).to_string()).collect(),
            detail: format!(
                "armfortas runtime behavior is not reproducible across repeated full CLI builds\nrepeat count: {}\nunique variants: {}\nvarying components across repeats: {}\nstable components across repeats: {}\n{}\n{}\n{}",
                repeat_count,
                unique_variants,
                join_or_none(&varying),
                join_or_none(&stable),
                left.command,
                right.command,
                describe_run_difference(&left.run, &right.run, &left.label, &right.label)
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_capture_asm_vs_cli_asm(
    source: &Path,
    opt_level: OptLevel,
    repeat_count: usize,
    capture_result: &CaptureResult,
    tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureAsmVsCliAsm,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let capture_command = render_capture_command(source, opt_level, Stage::Asm);
    let capture_text = match capture_text_stage(capture_result, Stage::Asm) {
        Ok(text) => text,
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureAsmVsCliAsm,
                summary: "capture result did not include assembly text".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail,
                temp_root,
            })
        }
    };
    if let Err(err) = fs::write(temp_root.join("from_capture.s"), capture_text) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureAsmVsCliAsm,
            summary: "could not write captured assembly artifact".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!("cannot write captured assembly artifact: {}", err),
            temp_root,
        });
    }
    let capture_normalized = normalize_text_artifact(capture_text);

    let mut cli_runs = Vec::new();
    let mut mismatch_indices = Vec::new();
    for index in 0..repeat_count {
        let asm_path = temp_root.join(format!("cli_run_{:02}.s", index));
        let command =
            match compile_with_driver(source, opt_level, DriverEmitMode::Asm, &asm_path, tools) {
                Ok(command) => command,
                Err(detail) => {
                    return Some(ConsistencyIssue {
                        check: ConsistencyCheck::CaptureAsmVsCliAsm,
                        summary: "armfortas -S failed during capture-vs-cli consistency check"
                            .into(),
                        repeat_count: None,
                        unique_variant_count: None,
                        varying_components: Vec::new(),
                        stable_components: Vec::new(),
                        detail,
                        temp_root,
                    })
                }
            };
        let text = match read_text_artifact(&asm_path) {
            Ok(text) => text,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CaptureAsmVsCliAsm,
                    summary: "could not read cli assembly artifact".into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let normalized = normalize_text_artifact(&text);
        if normalized != capture_normalized {
            mismatch_indices.push(index);
        }
        cli_runs.push(TextRun {
            label: format!("cli run {} (-S)", index + 1),
            command,
            normalized,
        });
    }

    if !mismatch_indices.is_empty() {
        let matching_runs = repeat_count.saturating_sub(mismatch_indices.len());
        let unique_cli_variants =
            count_unique_strings(cli_runs.iter().map(|run| run.normalized.as_str()));
        let first_mismatch = &cli_runs[mismatch_indices[0]];
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureAsmVsCliAsm,
            summary: format!(
                "repeat_count={} matching_runs={} mismatching_runs={} unique_cli_variants={}",
                repeat_count,
                matching_runs,
                mismatch_indices.len(),
                unique_cli_variants
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_cli_variants),
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "captured assembly does not match repeated armfortas -S runs\nrepeat count: {}\nmatching runs: {}\nmismatching runs: {}\nunique cli variants: {}\n{}\n{}\n{}",
                repeat_count,
                matching_runs,
                mismatch_indices.len(),
                unique_cli_variants,
                capture_command,
                first_mismatch.command,
                describe_text_difference(
                    &capture_normalized,
                    &first_mismatch.normalized,
                    "capture asm",
                    &first_mismatch.label
                )
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_capture_obj_vs_cli_obj(
    source: &Path,
    opt_level: OptLevel,
    repeat_count: usize,
    capture_result: &CaptureResult,
    tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureObjVsCliObj,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let capture_command = render_capture_command(source, opt_level, Stage::Obj);
    let capture_text = match capture_text_stage(capture_result, Stage::Obj) {
        Ok(text) => text,
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureObjVsCliObj,
                summary: "capture result did not include object snapshot text".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail,
                temp_root,
            })
        }
    };
    if let Err(err) = fs::write(temp_root.join("from_capture.obj.txt"), capture_text) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureObjVsCliObj,
            summary: "could not write captured object snapshot artifact".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!("cannot write captured object snapshot artifact: {}", err),
            temp_root,
        });
    }
    let capture_snapshot = match parse_object_snapshot_text(capture_text) {
        Ok(snapshot) => snapshot,
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureObjVsCliObj,
                summary: "captured object snapshot had an unexpected format".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail,
                temp_root,
            })
        }
    };

    let mut cli_runs = Vec::new();
    let mut mismatch_indices = Vec::new();
    for index in 0..repeat_count {
        let obj_path = temp_root.join(format!("cli_run_{:02}.o", index));
        let command =
            match compile_with_driver(source, opt_level, DriverEmitMode::Obj, &obj_path, tools) {
                Ok(command) => command,
                Err(detail) => {
                    return Some(ConsistencyIssue {
                        check: ConsistencyCheck::CaptureObjVsCliObj,
                        summary: "armfortas -c failed during capture-vs-cli consistency check"
                            .into(),
                        repeat_count: None,
                        unique_variant_count: None,
                        varying_components: Vec::new(),
                        stable_components: Vec::new(),
                        detail,
                        temp_root,
                    })
                }
            };
        let snapshot = match object_snapshot(&obj_path, tools) {
            Ok(snapshot) => snapshot,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CaptureObjVsCliObj,
                    summary: "could not snapshot cli object artifact".into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail: format!("{}\n{}", command, detail),
                    temp_root,
                })
            }
        };
        if let Err(err) = fs::write(
            temp_root.join(format!("cli_run_{:02}.obj.txt", index)),
            render_object_snapshot(&snapshot),
        ) {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureObjVsCliObj,
                summary: "could not write cli object snapshot artifact".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("cannot write cli object snapshot artifact: {}", err),
                temp_root,
            });
        }
        if snapshot != capture_snapshot {
            mismatch_indices.push(index);
        }
        cli_runs.push(ObjectRun {
            label: format!("cli run {} (-c)", index + 1),
            command,
            snapshot,
        });
    }

    if !mismatch_indices.is_empty() {
        let matching_runs = repeat_count.saturating_sub(mismatch_indices.len());
        let rendered = cli_runs
            .iter()
            .map(|run| render_object_snapshot(&run.snapshot))
            .collect::<Vec<_>>();
        let unique_cli_variants = count_unique_strings(rendered.iter().map(String::as_str));
        let mismatch_snapshots = mismatch_indices
            .iter()
            .map(|index| &cli_runs[*index].snapshot)
            .collect::<Vec<_>>();
        let mut summary_snapshots = vec![&capture_snapshot];
        summary_snapshots.extend(mismatch_snapshots.iter().copied());
        let varying = varying_object_components(&summary_snapshots)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let stable = stable_object_components(&summary_snapshots)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let first_mismatch = &cli_runs[mismatch_indices[0]];
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureObjVsCliObj,
            summary: format!(
                "repeat_count={} matching_runs={} mismatching_runs={} unique_cli_variants={} varying_components={} stable_components={}",
                repeat_count,
                matching_runs,
                mismatch_indices.len(),
                unique_cli_variants,
                join_or_none_from_strings(&varying),
                join_or_none_from_strings(&stable)
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_cli_variants),
            varying_components: varying,
            stable_components: stable,
            detail: format!(
                "captured object snapshot does not match repeated armfortas -c runs\nrepeat count: {}\nmatching runs: {}\nmismatching runs: {}\nunique cli variants: {}\n{}\n{}\n{}",
                repeat_count,
                matching_runs,
                mismatch_indices.len(),
                unique_cli_variants,
                capture_command,
                first_mismatch.command,
                describe_object_difference(
                    &capture_snapshot,
                    &first_mismatch.snapshot,
                    "capture obj",
                    &first_mismatch.label
                )
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_capture_run_vs_cli_run(
    prepared: &PreparedInput,
    opt_level: OptLevel,
    repeat_count: usize,
    capture_result: &CaptureResult,
    tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureRunVsCliRun,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let capture_command = render_prepared_capture_command(prepared, opt_level, Stage::Run);
    let capture_run = match capture_run_stage(capture_result) {
        Ok(run) => run.clone(),
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureRunVsCliRun,
                summary: "capture result did not include runtime behavior".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail,
                temp_root,
            })
        }
    };
    if let Err(err) =
        write_behavior_run_artifacts(&temp_root, "from_capture", &capture_command, &capture_run)
    {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureRunVsCliRun,
            summary: "could not write captured runtime artifact".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!("cannot write captured runtime artifact: {}", err),
            temp_root,
        });
    }
    let capture_signature = normalize_run_signature(&capture_run);

    let mut cli_runs = Vec::new();
    let mut mismatch_indices = Vec::new();
    for index in 0..repeat_count {
        let binary_path = temp_root.join(format!("cli_run_{:02}.out", index));
        let build_command = match compile_prepared_with_driver(
            prepared,
            opt_level,
            DriverEmitMode::Binary,
            &binary_path,
            tools,
        ) {
            Ok(command) => command,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CaptureRunVsCliRun,
                    summary: "armfortas binary build failed during capture-vs-cli runtime check"
                        .into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let run_command = render_binary_run_command(&binary_path);
        let run_sandbox = temp_root.join(format!("cli_run_{:02}.sandbox", index));
        let run = match run_binary_capture(&binary_path, &run_sandbox, &run_command) {
            Ok(run) => run,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CaptureRunVsCliRun,
                    summary: "armfortas binary could not run during capture-vs-cli runtime check"
                        .into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let command = format!("build: {}\nrun: {}", build_command, run_command);
        if let Err(err) = write_behavior_run_artifacts(
            &temp_root,
            &format!("cli_run_{:02}", index),
            &command,
            &run,
        ) {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureRunVsCliRun,
                summary: "could not write cli runtime artifact".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("cannot write cli runtime artifact: {}", err),
                temp_root,
            });
        }
        if normalize_run_signature(&run) != capture_signature {
            mismatch_indices.push(index);
        }
        cli_runs.push(BehaviorRun {
            label: format!("cli run {}", index + 1),
            command,
            signature: normalize_run_signature(&run),
            run,
        });
    }

    if !mismatch_indices.is_empty() {
        let matching_runs = repeat_count.saturating_sub(mismatch_indices.len());
        let unique_cli_variants =
            count_unique_run_signatures(cli_runs.iter().map(|run| &run.signature));
        let mismatch_signatures = mismatch_indices
            .iter()
            .map(|index| &cli_runs[*index].signature)
            .collect::<Vec<_>>();
        let mut summary_signatures = vec![&capture_signature];
        summary_signatures.extend(mismatch_signatures.iter().copied());
        let varying = varying_run_components(&summary_signatures)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let stable = stable_run_components(&summary_signatures)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let first_mismatch = &cli_runs[mismatch_indices[0]];
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureRunVsCliRun,
            summary: format!(
                "repeat_count={} matching_runs={} mismatching_runs={} unique_cli_variants={} varying_components={} stable_components={}",
                repeat_count,
                matching_runs,
                mismatch_indices.len(),
                unique_cli_variants,
                join_or_none_from_strings(&varying),
                join_or_none_from_strings(&stable)
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_cli_variants),
            varying_components: varying,
            stable_components: stable,
            detail: format!(
                "captured runtime behavior does not match repeated full CLI builds\nrepeat count: {}\nmatching runs: {}\nmismatching runs: {}\nunique cli variants: {}\n{}\n{}\n{}",
                repeat_count,
                matching_runs,
                mismatch_indices.len(),
                unique_cli_variants,
                capture_command,
                first_mismatch.command,
                describe_run_difference(
                    &capture_run,
                    &first_mismatch.run,
                    "capture run",
                    &first_mismatch.label
                )
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_capture_asm_reproducible(
    source: &Path,
    opt_level: OptLevel,
    repeat_count: usize,
    capture_result: &CaptureResult,
    _tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureAsmReproducible,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let mut runs = Vec::new();
    let command = render_capture_command(source, opt_level, Stage::Asm);
    let initial_text = match capture_text_stage(capture_result, Stage::Asm) {
        Ok(text) => text,
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureAsmReproducible,
                summary: "initial capture result did not include assembly text".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail,
                temp_root,
            })
        }
    };
    if let Err(err) = fs::write(temp_root.join("capture_run_00.s"), initial_text) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureAsmReproducible,
            summary: "could not write captured assembly artifact".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!("cannot write captured assembly artifact: {}", err),
            temp_root,
        });
    }
    runs.push(TextRun {
        label: "capture run 1".into(),
        command: command.clone(),
        normalized: normalize_text_artifact(initial_text),
    });

    for index in 1..repeat_count {
        let text = match capture_text_from_testing(source, opt_level, Stage::Asm) {
            Ok(text) => text,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CaptureAsmReproducible,
                    summary: "armfortas::testing capture failed during asm reproducibility check"
                        .into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        if let Err(err) = fs::write(temp_root.join(format!("capture_run_{:02}.s", index)), &text) {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureAsmReproducible,
                summary: "could not write captured assembly artifact".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("cannot write captured assembly artifact: {}", err),
                temp_root,
            });
        }
        runs.push(TextRun {
            label: format!("capture run {}", index + 1),
            command: command.clone(),
            normalized: normalize_text_artifact(&text),
        });
    }

    let unique_variants = count_unique_strings(runs.iter().map(|run| run.normalized.as_str()));
    if unique_variants > 1 {
        let (left, right) =
            first_distinct_text_pair(&runs).expect("unique variants > 1 implies a distinct pair");
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureAsmReproducible,
            summary: format!("repeat_count={} unique_variants={}", repeat_count, unique_variants),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_variants),
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "captured assembly is not reproducible across repeated armfortas::testing runs\nrepeat count: {}\nunique variants: {}\n{}\n{}\n{}",
                repeat_count,
                unique_variants,
                left.command,
                right.command,
                describe_text_difference(&left.normalized, &right.normalized, &left.label, &right.label)
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_capture_obj_reproducible(
    source: &Path,
    opt_level: OptLevel,
    repeat_count: usize,
    capture_result: &CaptureResult,
    _tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureObjReproducible,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let command = render_capture_command(source, opt_level, Stage::Obj);
    let initial_text = match capture_text_stage(capture_result, Stage::Obj) {
        Ok(text) => text,
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureObjReproducible,
                summary: "initial capture result did not include object snapshot text".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail,
                temp_root,
            })
        }
    };
    let initial_snapshot = match parse_object_snapshot_text(initial_text) {
        Ok(snapshot) => snapshot,
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureObjReproducible,
                summary: "captured object snapshot had an unexpected format".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail,
                temp_root,
            })
        }
    };
    if let Err(err) = fs::write(temp_root.join("capture_run_00.obj.txt"), initial_text) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureObjReproducible,
            summary: "could not write captured object snapshot artifact".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!("cannot write captured object snapshot artifact: {}", err),
            temp_root,
        });
    }

    let mut runs = vec![ObjectRun {
        label: "capture run 1".into(),
        command: command.clone(),
        snapshot: initial_snapshot,
    }];

    for index in 1..repeat_count {
        let text = match capture_text_from_testing(source, opt_level, Stage::Obj) {
            Ok(text) => text,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CaptureObjReproducible,
                    summary: "armfortas::testing capture failed during obj reproducibility check"
                        .into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        if let Err(err) = fs::write(
            temp_root.join(format!("capture_run_{:02}.obj.txt", index)),
            &text,
        ) {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureObjReproducible,
                summary: "could not write captured object snapshot artifact".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("cannot write captured object snapshot artifact: {}", err),
                temp_root,
            });
        }
        let snapshot = match parse_object_snapshot_text(&text) {
            Ok(snapshot) => snapshot,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CaptureObjReproducible,
                    summary: "captured object snapshot had an unexpected format".into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        runs.push(ObjectRun {
            label: format!("capture run {}", index + 1),
            command: command.clone(),
            snapshot,
        });
    }

    let rendered = runs
        .iter()
        .map(|run| render_object_snapshot(&run.snapshot))
        .collect::<Vec<_>>();
    let unique_variants = count_unique_strings(rendered.iter().map(String::as_str));
    if unique_variants > 1 {
        let snapshots = runs.iter().map(|run| &run.snapshot).collect::<Vec<_>>();
        let (left, right) =
            first_distinct_object_pair(&runs).expect("unique variants > 1 implies a distinct pair");
        let varying = varying_object_components(&snapshots);
        let stable = stable_object_components(&snapshots);
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureObjReproducible,
            summary: format!(
                "repeat_count={} unique_variants={} varying_components={} stable_components={}",
                repeat_count,
                unique_variants,
                join_or_none(&varying),
                join_or_none(&stable)
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_variants),
            varying_components: varying.iter().map(|value| (*value).to_string()).collect(),
            stable_components: stable.iter().map(|value| (*value).to_string()).collect(),
            detail: format!(
                "captured object snapshots are not reproducible across repeated armfortas::testing runs\nrepeat count: {}\nunique variants: {}\nvarying components across repeats: {}\nstable components across repeats: {}\n{}\n{}\n{}",
                repeat_count,
                unique_variants,
                join_or_none(&varying),
                join_or_none(&stable),
                left.command,
                right.command,
                describe_object_difference(&left.snapshot, &right.snapshot, &left.label, &right.label)
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_capture_run_reproducible(
    prepared: &PreparedInput,
    opt_level: OptLevel,
    repeat_count: usize,
    capture_result: &CaptureResult,
    _tools: &ToolchainConfig,
) -> Option<ConsistencyIssue> {
    let temp_root = next_consistency_temp_root(opt_level);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureRunReproducible,
            summary: "could not create consistency temp dir".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "cannot create consistency temp dir '{}': {}",
                temp_root.display(),
                err
            ),
            temp_root,
        });
    }

    let command = render_prepared_capture_command(prepared, opt_level, Stage::Run);
    let initial_run = match capture_run_stage(capture_result) {
        Ok(run) => run.clone(),
        Err(detail) => {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureRunReproducible,
                summary: "initial capture result did not include runtime behavior".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail,
                temp_root,
            })
        }
    };
    if let Err(err) =
        write_behavior_run_artifacts(&temp_root, "capture_run_00", &command, &initial_run)
    {
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureRunReproducible,
            summary: "could not write captured runtime artifact".into(),
            repeat_count: None,
            unique_variant_count: None,
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!("cannot write captured runtime artifact: {}", err),
            temp_root,
        });
    }
    let mut runs = vec![BehaviorRun {
        label: "capture run 1".into(),
        command: command.clone(),
        signature: normalize_run_signature(&initial_run),
        run: initial_run,
    }];

    for index in 1..repeat_count {
        let run = match capture_prepared_run(
            prepared,
            opt_level,
            &temp_root.join(format!("capture_graph_{:02}", index)),
        ) {
            Ok(run) => run,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CaptureRunReproducible,
                    summary:
                        "armfortas::testing capture failed during runtime reproducibility check"
                            .into(),
                    repeat_count: None,
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        if let Err(err) = write_behavior_run_artifacts(
            &temp_root,
            &format!("capture_run_{:02}", index),
            &command,
            &run,
        ) {
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CaptureRunReproducible,
                summary: "could not write captured runtime artifact".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!("cannot write captured runtime artifact: {}", err),
                temp_root,
            });
        }
        runs.push(BehaviorRun {
            label: format!("capture run {}", index + 1),
            command: command.clone(),
            signature: normalize_run_signature(&run),
            run,
        });
    }

    let unique_variants = count_unique_run_signatures(runs.iter().map(|run| &run.signature));
    if unique_variants > 1 {
        let signatures = runs.iter().map(|run| &run.signature).collect::<Vec<_>>();
        let varying = varying_run_components(&signatures);
        let stable = stable_run_components(&signatures);
        let (left, right) = first_distinct_behavior_pair(&runs)
            .expect("unique variants > 1 implies a distinct pair");
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CaptureRunReproducible,
            summary: format!(
                "repeat_count={} unique_variants={} varying_components={} stable_components={}",
                repeat_count,
                unique_variants,
                join_or_none(&varying),
                join_or_none(&stable)
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_variants),
            varying_components: varying.iter().map(|value| (*value).to_string()).collect(),
            stable_components: stable.iter().map(|value| (*value).to_string()).collect(),
            detail: format!(
                "captured runtime behavior is not reproducible across repeated armfortas::testing runs\nrepeat count: {}\nunique variants: {}\nvarying components across repeats: {}\nstable components across repeats: {}\n{}\n{}\n{}",
                repeat_count,
                unique_variants,
                join_or_none(&varying),
                join_or_none(&stable),
                left.command,
                right.command,
                describe_run_difference(&left.run, &right.run, &left.label, &right.label)
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_reference_compilers(
    prepared: &PreparedInput,
    case: &CaseSpec,
    opt_level: OptLevel,
    tools: &ToolchainConfig,
) -> Vec<ReferenceResult> {
    case.reference_compilers
        .iter()
        .copied()
        .map(|compiler| {
            if prepared.is_graph() {
                run_reference_graph(&prepared.graph_sources, opt_level, compiler, tools)
            } else {
                run_reference_case(&prepared.compiler_source, opt_level, compiler, tools)
            }
        })
        .collect()
}

fn run_reference_graph(
    sources: &[PathBuf],
    opt_level: OptLevel,
    compiler: ReferenceCompiler,
    tools: &ToolchainConfig,
) -> ReferenceResult {
    let temp_root = next_report_temp_root(compiler, opt_level);
    let binary = temp_root.join("reference.out");
    let compiler_bin = tools.reference_binary(compiler);
    if let Err(err) = fs::create_dir_all(&temp_root) {
        return ReferenceResult::infrastructure_error(
            compiler,
            compiler_bin.to_string(),
            format!("cannot create temp dir '{}': {}", temp_root.display(), err),
        );
    }
    let _temp_cleanup = ReferenceTempCleanup(temp_root.clone());

    let mut commands = Vec::new();
    let mut compile_stdout = String::new();
    let mut compile_stderr = String::new();
    let mut objects = vec![None; sources.len()];
    for index in 0..sources.len() {
        let source = &sources[index];
        let object = temp_root.join(format!("unit_{:04}.o", index));
        let mut args = vec![opt_level.as_flag().to_string(), "-c".to_string()];
        if source_uses_cpp(source) {
            args.push("-cpp".to_string());
        }
        args.push("-I".to_string());
        args.push(temp_root.display().to_string());
        args.push("-J".to_string());
        args.push(temp_root.display().to_string());
        args.push(source.display().to_string());
        args.push("-o".to_string());
        args.push(object.display().to_string());
        let command = render_command(compiler_bin, &args);
        commands.push(command.clone());
        let output = match run_managed(
            Command::new(compiler_bin)
                .current_dir(&temp_root)
                .args(&args),
            CommandClass::Compile,
        ) {
            Ok(output) => output,
            Err(err) => {
                let _ = fs::remove_dir_all(&temp_root);
                return ReferenceResult::infrastructure_error(
                    compiler,
                    commands.join("\n"),
                    format!("cannot run {}: {}", compiler_bin, err),
                );
            }
        };
        append_command_output(&mut compile_stdout, index, source, &output.stdout);
        append_command_output(&mut compile_stderr, index, source, &output.stderr);
        if !output.status.success() {
            let result = ReferenceResult {
                compiler,
                compile_command: commands.join("\n"),
                compile_exit_code: output.status.code().unwrap_or(-1),
                compile_stdout,
                compile_stderr,
                run: None,
                run_error: None,
            };
            let _ = fs::remove_dir_all(&temp_root);
            return result;
        }
        objects[index] = Some(object);
    }

    let mut link_args = Vec::with_capacity(objects.len() + 2);
    for (index, object) in objects.into_iter().enumerate() {
        let Some(object) = object else {
            let _ = fs::remove_dir_all(&temp_root);
            return ReferenceResult::infrastructure_error(
                compiler,
                commands.join("\n"),
                format!(
                    "graph source [{}] '{}' did not produce an object",
                    index,
                    sources[index].display()
                ),
            );
        };
        link_args.push(object.display().to_string());
    }
    link_args.push("-o".to_string());
    link_args.push(binary.display().to_string());
    let link_command = render_command(compiler_bin, &link_args);
    commands.push(link_command);
    let link = match run_managed(
        Command::new(compiler_bin)
            .current_dir(&temp_root)
            .args(&link_args),
        CommandClass::Compile,
    ) {
        Ok(output) => output,
        Err(err) => {
            let _ = fs::remove_dir_all(&temp_root);
            return ReferenceResult::infrastructure_error(
                compiler,
                commands.join("\n"),
                format!("cannot run {}: {}", compiler_bin, err),
            );
        }
    };
    append_command_output(
        &mut compile_stdout,
        sources.len(),
        Path::new("<link>"),
        &link.stdout,
    );
    append_command_output(
        &mut compile_stderr,
        sources.len(),
        Path::new("<link>"),
        &link.stderr,
    );

    let mut result = ReferenceResult {
        compiler,
        compile_command: commands.join("\n"),
        compile_exit_code: link.status.code().unwrap_or(-1),
        compile_stdout,
        compile_stderr,
        run: None,
        run_error: None,
    };
    if link.status.success() {
        let run_command = render_binary_run_command(&binary);
        match run_binary_capture(&binary, &temp_root.join("run_sandbox"), &run_command) {
            Ok(run) => result.run = Some(run),
            Err(err) => {
                result.run_error = Some(err);
            }
        }
    }

    let _ = fs::remove_dir_all(&temp_root);
    result
}

fn append_command_output(destination: &mut String, index: usize, source: &Path, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if !destination.is_empty() {
        destination.push('\n');
    }
    destination.push_str(&format!(
        "===== graph command [{:04}] {} =====\n",
        index,
        source.display()
    ));
    destination.push_str(&String::from_utf8_lossy(bytes));
}

fn run_reference_case(
    source: &Path,
    opt_level: OptLevel,
    compiler: ReferenceCompiler,
    tools: &ToolchainConfig,
) -> ReferenceResult {
    let temp_root = next_report_temp_root(compiler, opt_level);
    let binary = temp_root.join("reference.out");
    let uses_cpp = source_uses_cpp(source);

    let mut args = vec![opt_level.as_flag().to_string()];
    if uses_cpp {
        args.push("-cpp".to_string());
    }
    args.push(source.display().to_string());
    args.push("-o".to_string());
    args.push(binary.display().to_string());

    let compiler_bin = tools.reference_binary(compiler);
    let command_string = render_command(compiler_bin, &args);

    if let Err(err) = fs::create_dir_all(&temp_root) {
        return ReferenceResult::infrastructure_error(
            compiler,
            command_string,
            format!("cannot create temp dir '{}': {}", temp_root.display(), err),
        );
    }
    let _temp_cleanup = ReferenceTempCleanup(temp_root.clone());

    let compile = match run_managed(
        Command::new(compiler_bin)
            .current_dir(&temp_root)
            .args(&args),
        CommandClass::Compile,
    ) {
        Ok(output) => output,
        Err(err) => {
            let _ = fs::remove_dir_all(&temp_root);
            return ReferenceResult::infrastructure_error(
                compiler,
                command_string,
                format!("cannot run {}: {}", compiler_bin, err),
            );
        }
    };

    let mut result = ReferenceResult {
        compiler,
        compile_command: command_string,
        compile_exit_code: compile.status.code().unwrap_or(-1),
        compile_stdout: String::from_utf8_lossy(&compile.stdout).into_owned(),
        compile_stderr: String::from_utf8_lossy(&compile.stderr).into_owned(),
        run: None,
        run_error: None,
    };

    if compile.status.success() {
        let run_command = render_binary_run_command(&binary);
        match run_binary_capture(&binary, &temp_root.join("run_sandbox"), &run_command) {
            Ok(run) => result.run = Some(run),
            Err(err) => {
                result.run_error = Some(err);
            }
        }
    }

    let _ = fs::remove_dir_all(&temp_root);
    result
}

fn source_uses_cpp(source: &Path) -> bool {
    fs::read_to_string(source)
        .map(|text| text.lines().any(|line| line.trim_start().starts_with('#')))
        .unwrap_or(false)
}

struct ReferenceTempCleanup(PathBuf);

impl Drop for ReferenceTempCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverEmitMode {
    Asm,
    Obj,
    Binary,
}

fn compile_with_driver(
    source: &Path,
    opt_level: OptLevel,
    mode: DriverEmitMode,
    output: &Path,
    tools: &ToolchainConfig,
) -> Result<String, String> {
    let command = render_armfortas_command(source, opt_level, mode, output, tools);
    if let Some(binary) = tools.armfortas_external_bin() {
        let mut args = vec![opt_level.as_flag().to_string()];
        match mode {
            DriverEmitMode::Asm => args.push("-S".to_string()),
            DriverEmitMode::Obj => args.push("-c".to_string()),
            DriverEmitMode::Binary => {}
        }
        args.push(source.display().to_string());
        args.push("-o".to_string());
        args.push(output.display().to_string());

        let compile = run_managed(Command::new(binary).args(&args), CommandClass::Compile)
            .map_err(|err| format!("{} failed:\ncannot run '{}': {}", command, binary, err))?;
        if !compile.status.success() {
            let stderr = String::from_utf8_lossy(&compile.stderr);
            return Err(format!("{} failed:\n{}", command, stderr.trim_end()));
        }
    } else {
        let emit_mode = match mode {
            DriverEmitMode::Asm => EmitMode::Asm,
            DriverEmitMode::Obj => EmitMode::Obj,
            DriverEmitMode::Binary => EmitMode::Binary,
        };
        compile_output(source, opt_level, emit_mode, output)
            .map_err(|detail| format!("{} failed:\n{}", command, detail))?;
    }
    Ok(command)
}

fn compile_prepared_with_driver(
    prepared: &PreparedInput,
    opt_level: OptLevel,
    mode: DriverEmitMode,
    output: &Path,
    tools: &ToolchainConfig,
) -> Result<String, String> {
    if !prepared.is_graph() {
        return compile_with_driver(&prepared.compiler_source, opt_level, mode, output, tools);
    }
    if mode != DriverEmitMode::Binary {
        return Err(format!(
            "graph CLI adapter does not define a single '{}' artifact",
            match mode {
                DriverEmitMode::Asm => "assembly",
                DriverEmitMode::Obj => "object",
                DriverEmitMode::Binary => unreachable!(),
            }
        ));
    }

    let module_output_dir = graph_module_output_dir(output);
    let command = render_prepared_armfortas_command(prepared, opt_level, mode, output, tools);
    if let Some(binary) = tools.armfortas_external_bin() {
        let mut args = vec![opt_level.as_flag().to_string()];
        args.push("-J".to_string());
        args.push(module_output_dir.display().to_string());
        args.extend(
            prepared
                .compiler_sources()
                .iter()
                .map(|source| source.display().to_string()),
        );
        args.push("-o".to_string());
        args.push(output.display().to_string());
        let compile = run_managed(Command::new(binary).args(&args), CommandClass::Compile)
            .map_err(|err| format!("{} failed:\ncannot run '{}': {}", command, binary, err))?;
        if !compile.status.success() {
            return Err(format!(
                "{} failed:\n{}",
                command,
                String::from_utf8_lossy(&compile.stderr).trim_end()
            ));
        }
    } else {
        compile_graph_output(
            prepared.compiler_sources(),
            opt_level,
            output,
            module_output_dir,
        )
        .map_err(|detail| format!("{} failed:\n{}", command, detail))?;
    }
    Ok(command)
}

fn graph_module_output_dir(output: &Path) -> &Path {
    output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn render_armfortas_command(
    source: &Path,
    opt_level: OptLevel,
    mode: DriverEmitMode,
    output: &Path,
    tools: &ToolchainConfig,
) -> String {
    let mut args = vec![opt_level.as_flag().to_string()];
    match mode {
        DriverEmitMode::Asm => args.push("-S".to_string()),
        DriverEmitMode::Obj => args.push("-c".to_string()),
        DriverEmitMode::Binary => {}
    }
    args.push(source.display().to_string());
    args.push("-o".to_string());
    args.push(output.display().to_string());
    render_command(tools.armfortas_command_name(), &args)
}

fn render_prepared_armfortas_command(
    prepared: &PreparedInput,
    opt_level: OptLevel,
    mode: DriverEmitMode,
    output: &Path,
    tools: &ToolchainConfig,
) -> String {
    if !prepared.is_graph() {
        return render_armfortas_command(&prepared.compiler_source, opt_level, mode, output, tools);
    }
    let mut args = vec![opt_level.as_flag().to_string()];
    match mode {
        DriverEmitMode::Asm => args.push("-S".to_string()),
        DriverEmitMode::Obj => args.push("-c".to_string()),
        DriverEmitMode::Binary => {}
    }
    args.push("-J".to_string());
    args.push(graph_module_output_dir(output).display().to_string());
    args.extend(
        prepared
            .compiler_sources()
            .iter()
            .map(|source| source.display().to_string()),
    );
    args.push("-o".to_string());
    args.push(output.display().to_string());
    render_command(tools.armfortas_command_name(), &args)
}

fn render_binary_run_command(binary: &Path) -> String {
    render_command(&binary.display().to_string(), &[])
}

fn render_capture_command(source: &Path, opt_level: OptLevel, stage: Stage) -> String {
    format!(
        "armfortas::testing capture {} --stage {} {}",
        opt_level.as_flag(),
        stage.as_str(),
        quote_arg(&source.display().to_string())
    )
}

fn render_prepared_capture_command(
    prepared: &PreparedInput,
    opt_level: OptLevel,
    stage: Stage,
) -> String {
    if !prepared.is_graph() {
        return render_capture_command(&prepared.compiler_source, opt_level, stage);
    }
    format!(
        "armfortas::testing graph capture {} --stage {} {}",
        opt_level.as_flag(),
        stage.as_str(),
        prepared
            .compiler_sources()
            .iter()
            .map(|source| quote_arg(&source.display().to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn capture_text_from_testing(
    source: &Path,
    opt_level: OptLevel,
    stage: Stage,
) -> Result<String, String> {
    let command = render_capture_command(source, opt_level, stage);
    let request = CaptureRequest {
        input: source.to_path_buf(),
        requested: BTreeSet::from([stage]),
        opt_level,
    };
    let result = capture_from_path(&request)
        .map_err(|failure| format!("{} failed:\n{}", command, failure))?;
    capture_text_stage(&result, stage).map(str::to_string)
}

fn capture_run_from_testing(source: &Path, opt_level: OptLevel) -> Result<RunCapture, String> {
    let command = render_capture_command(source, opt_level, Stage::Run);
    let request = CaptureRequest {
        input: source.to_path_buf(),
        requested: BTreeSet::from([Stage::Run]),
        opt_level,
    };
    let result = capture_from_path(&request)
        .map_err(|failure| format!("{} failed:\n{}", command, failure))?;
    capture_run_stage(&result).cloned()
}

fn capture_prepared_run(
    prepared: &PreparedInput,
    opt_level: OptLevel,
    graph_work_root: &Path,
) -> Result<RunCapture, String> {
    if !prepared.is_graph() {
        return capture_run_from_testing(&prepared.compiler_source, opt_level);
    }
    let result = capture_graph(
        &prepared.compiler_source,
        &prepared.graph_sources,
        &BTreeSet::from([Stage::Run]),
        opt_level,
        graph_work_root,
    )
    .map_err(|failure| failure.to_string())?;
    capture_run_stage(&result).cloned()
}

fn capture_text_stage(result: &CaptureResult, stage: Stage) -> Result<&str, String> {
    match result.get(stage) {
        Some(CapturedStage::Text(text)) => Ok(text),
        Some(CapturedStage::Run(_)) => Err(format!(
            "capture result contained non-text data for stage '{}'",
            stage.as_str()
        )),
        None => Err(format!(
            "capture result was missing requested stage '{}'",
            stage.as_str()
        )),
    }
}

fn capture_run_stage(result: &CaptureResult) -> Result<&RunCapture, String> {
    match result.get(Stage::Run) {
        Some(CapturedStage::Run(run)) => Ok(run),
        Some(CapturedStage::Text(_)) => {
            Err("capture result contained text data for the run stage".into())
        }
        None => Err("capture result was missing requested stage 'run'".into()),
    }
}

fn run_binary_capture(binary: &Path, sandbox: &Path, command: &str) -> Result<RunCapture, String> {
    fs::create_dir(sandbox).map_err(|error| {
        format!(
            "{} failed:\ncannot create isolated run sandbox '{}': {}",
            command,
            sandbox.display(),
            error
        )
    })?;
    let output = run_managed(Command::new(binary).current_dir(sandbox), CommandClass::Run)
        .map_err(|err| {
            format!(
                "{} failed:\ncannot run '{}': {}",
                command,
                binary.display(),
                err
            )
        })?;
    let files = armfortas::testing::snapshot_sandbox_files(sandbox)
        .map_err(|detail| format!("{} failed:\n{}", command, detail))?;
    Ok(RunCapture {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
        files,
    })
}

fn normalize_run_signature(run: &RunCapture) -> RunSignature {
    RunSignature {
        exit_code: run.exit_code,
        stdout: normalize_behavior_bytes(&run.stdout),
        stderr: normalize_behavior_bytes(&run.stderr),
        files: run.files.clone(),
    }
}

fn normalize_behavior_bytes(bytes: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(text) => normalize_behavior_text(text).into_bytes(),
        Err(_) => bytes.to_vec(),
    }
}

fn normalize_behavior_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(normalize_behavior_line)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn normalize_behavior_line(line: &str) -> String {
    line.split_whitespace()
        .map(normalize_behavior_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_behavior_token(token: &str) -> String {
    if let Some(number) = parse_numeric_token(token) {
        format!("num:{:.6e}", number)
    } else {
        token.to_string()
    }
}

fn parse_numeric_token(token: &str) -> Option<f64> {
    if token.is_empty() {
        return None;
    }

    let normalized = token
        .trim()
        .trim_end_matches(',')
        .trim_end_matches(';')
        .replace('D', "E")
        .replace('d', "e");

    normalized.parse::<f64>().ok()
}

fn format_reference_summary(references: &[ReferenceResult]) -> String {
    references
        .iter()
        .map(format_reference_result)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_reference_result(reference: &ReferenceResult) -> String {
    let mut lines = Vec::new();
    lines.push(reference.compiler.as_str().to_string());
    lines.push(format!("command: {}", reference.compile_command));
    lines.push(format!("compile exit: {}", reference.compile_exit_code));
    if !reference.compile_stdout.trim().is_empty() {
        lines.push(format!(
            "compile stdout:\n{}",
            reference.compile_stdout.trim_end()
        ));
    }
    if !reference.compile_stderr.trim().is_empty() {
        lines.push(format!(
            "compile stderr:\n{}",
            reference.compile_stderr.trim_end()
        ));
    }
    match (&reference.run, &reference.run_error) {
        (Some(run), _) => {
            lines.push(format!("run\n{}", format_run_capture(run)));
        }
        (None, Some(err)) => {
            lines.push(format!("run error: {}", err));
        }
        (None, None) => {}
    }
    lines.join("\n")
}

fn format_run_capture(run: &RunCapture) -> String {
    let stdout = format_captured_output(&run.stdout);
    let stderr = format_captured_output(&run.stderr);
    let files = format_file_snapshot(&run.files);
    format!(
        "exit: {}\nstdout:\n{}\nstderr:\n{}\nfiles:\n{}",
        run.exit_code, stdout, stderr, files
    )
}

fn format_run_signature(signature: &RunSignature) -> String {
    let stdout = format_captured_output(&signature.stdout);
    let stderr = format_captured_output(&signature.stderr);
    let files = format_file_snapshot(&signature.files);
    format!(
        "exit: {}\nstdout:\n{}\nstderr:\n{}\nfiles:\n{}",
        signature.exit_code, stdout, stderr, files
    )
}

fn format_file_snapshot(files: &BTreeMap<String, Vec<u8>>) -> String {
    if files.is_empty() {
        return "<empty>".to_string();
    }

    files
        .iter()
        .map(|(path, bytes)| {
            format!(
                "{} ({} bytes):\n{}",
                path,
                bytes.len(),
                format_file_preview(bytes)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_file_preview(bytes: &[u8]) -> String {
    const PREVIEW_LIMIT: usize = 256;

    if bytes.len() <= PREVIEW_LIMIT {
        return format_captured_output(bytes);
    }

    let mut preview_len = PREVIEW_LIMIT;
    if let Ok(text) = std::str::from_utf8(bytes) {
        while !text.is_char_boundary(preview_len) {
            preview_len -= 1;
        }
    }

    format!(
        "{}\n... <{} more bytes>",
        format_captured_output(&bytes[..preview_len]),
        bytes.len() - preview_len
    )
}

fn format_captured_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "<empty>".to_string();
    }

    match std::str::from_utf8(bytes) {
        Ok(text) => text.trim_end().to_string(),
        Err(_) => {
            const DISPLAY_LIMIT: usize = 256;
            let mut rendered = format!("<non-UTF-8 output: {} bytes> ", bytes.len());
            for &byte in bytes.iter().take(DISPLAY_LIMIT) {
                match byte {
                    b'\\' => rendered.push_str("\\\\"),
                    b'\n' => rendered.push_str("\\n"),
                    b'\r' => rendered.push_str("\\r"),
                    b'\t' => rendered.push_str("\\t"),
                    0x20..=0x7e => rendered.push(char::from(byte)),
                    _ => write!(rendered, "\\x{byte:02x}").expect("writing to String cannot fail"),
                }
            }
            if bytes.len() > DISPLAY_LIMIT {
                write!(rendered, "... <{} more bytes>", bytes.len() - DISPLAY_LIMIT)
                    .expect("writing to String cannot fail");
            }
            rendered
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObjectSnapshot {
    text: String,
    load_commands: String,
    relocations: String,
    symbols: String,
}

#[derive(Debug, Clone)]
struct TextRun {
    label: String,
    command: String,
    normalized: String,
}

#[derive(Debug, Clone)]
struct BehaviorRun {
    label: String,
    command: String,
    signature: RunSignature,
    run: RunCapture,
}

#[derive(Debug, Clone)]
struct ObjectRun {
    label: String,
    command: String,
    snapshot: ObjectSnapshot,
}

fn object_snapshot(path: &Path, tools: &ToolchainConfig) -> Result<ObjectSnapshot, String> {
    // Dispatch on the host's object format (sprint x01): otool/nm -m on
    // Mach-O; objdump/readelf/plain nm on ELF (`-m` is Apple-nm-only).
    // Snapshots are compared within a single host run, never across
    // tools, so cross-tool formatting differences are harmless.
    let p = path.to_str().unwrap();
    let (text, load_commands, relocations, symbols) =
        match armfortas::target::TargetSpec::host().object_format() {
            armfortas::target::ObjectFormat::MachO => (
                tool_output(tools.otool_bin(), &["-t", p])?,
                tool_output(tools.otool_bin(), &["-l", p])?,
                tool_output(tools.otool_bin(), &["-rv", p])?,
                tool_output(tools.nm_bin(), &["-m", p])?,
            ),
            armfortas::target::ObjectFormat::Elf => (
                tool_output(tools.objdump_bin(), &["-d", p])?,
                tool_output(tools.readelf_bin(), &["-lSW", p])?,
                tool_output(tools.objdump_bin(), &["-r", p])?,
                tool_output(tools.nm_bin(), &[p])?,
            ),
        };

    Ok(ObjectSnapshot {
        text: normalize_tool_output(&text),
        load_commands: normalize_tool_output(&load_commands),
        relocations: normalize_tool_output(&relocations),
        symbols: normalize_tool_output(&symbols),
    })
}

fn tool_output(tool: &str, args: &[&str]) -> Result<String, String> {
    let output = run_managed(Command::new(tool).args(args), CommandClass::Tool)
        .map_err(|e| format!("cannot run {}: {}", tool, e))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{} failed:\n{}",
            tool,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn normalize_tool_output(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_end().ends_with(".o:"))
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_text_artifact(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path.display(), e))
}

fn normalize_text_artifact(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn count_unique_strings<'a>(values: impl IntoIterator<Item = &'a str>) -> usize {
    values.into_iter().collect::<BTreeSet<_>>().len()
}

fn first_distinct_text_pair(runs: &[TextRun]) -> Option<(&TextRun, &TextRun)> {
    for left_index in 0..runs.len() {
        for right_index in (left_index + 1)..runs.len() {
            if runs[left_index].normalized != runs[right_index].normalized {
                return Some((&runs[left_index], &runs[right_index]));
            }
        }
    }
    None
}

fn count_unique_run_signatures<'a>(values: impl IntoIterator<Item = &'a RunSignature>) -> usize {
    values.into_iter().collect::<BTreeSet<_>>().len()
}

fn first_distinct_behavior_pair(runs: &[BehaviorRun]) -> Option<(&BehaviorRun, &BehaviorRun)> {
    for left_index in 0..runs.len() {
        for right_index in (left_index + 1)..runs.len() {
            if runs[left_index].signature != runs[right_index].signature {
                return Some((&runs[left_index], &runs[right_index]));
            }
        }
    }
    None
}

fn first_distinct_object_pair(runs: &[ObjectRun]) -> Option<(&ObjectRun, &ObjectRun)> {
    for left_index in 0..runs.len() {
        for right_index in (left_index + 1)..runs.len() {
            if runs[left_index].snapshot != runs[right_index].snapshot {
                return Some((&runs[left_index], &runs[right_index]));
            }
        }
    }
    None
}

fn render_object_snapshot(snapshot: &ObjectSnapshot) -> String {
    format!(
        "== text ==\n{}\n\n== load_commands ==\n{}\n\n== relocations ==\n{}\n\n== symbols ==\n{}",
        snapshot.text, snapshot.load_commands, snapshot.relocations, snapshot.symbols
    )
}

fn parse_object_snapshot_text(text: &str) -> Result<ObjectSnapshot, String> {
    let text = text
        .strip_prefix("== text ==\n")
        .ok_or_else(|| "object snapshot was missing the '== text ==' header".to_string())?;
    let (text, rest) = text
        .split_once("\n\n== load_commands ==\n")
        .ok_or_else(|| {
            "object snapshot was missing the '== load_commands ==' section".to_string()
        })?;
    let (load_commands, rest) = rest
        .split_once("\n\n== relocations ==\n")
        .ok_or_else(|| "object snapshot was missing the '== relocations ==' section".to_string())?;
    let (relocations, symbols) = rest
        .split_once("\n\n== symbols ==\n")
        .ok_or_else(|| "object snapshot was missing the '== symbols ==' section".to_string())?;

    Ok(ObjectSnapshot {
        text: text.to_string(),
        load_commands: load_commands.to_string(),
        relocations: relocations.to_string(),
        symbols: symbols.to_string(),
    })
}

fn describe_text_difference(
    expected: &str,
    actual: &str,
    left_label: &str,
    right_label: &str,
) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let shared = expected_lines.len().min(actual_lines.len());

    for index in 0..shared {
        if expected_lines[index] != actual_lines[index] {
            return format!(
                "first differing line: {}\n{}: {}\n{}: {}",
                index + 1,
                left_label,
                expected_lines[index],
                right_label,
                actual_lines[index]
            );
        }
    }

    format!(
        "snapshot length differs\n{} lines: {}\n{} lines: {}",
        left_label,
        expected_lines.len(),
        right_label,
        actual_lines.len()
    )
}

fn describe_object_difference(
    expected: &ObjectSnapshot,
    actual: &ObjectSnapshot,
    left_label: &str,
    right_label: &str,
) -> String {
    let mut differing = Vec::new();
    if expected.text != actual.text {
        differing.push(("text", &expected.text, &actual.text));
    }
    if expected.load_commands != actual.load_commands {
        differing.push((
            "load_commands",
            &expected.load_commands,
            &actual.load_commands,
        ));
    }
    if expected.relocations != actual.relocations {
        differing.push(("relocations", &expected.relocations, &actual.relocations));
    }
    if expected.symbols != actual.symbols {
        differing.push(("symbols", &expected.symbols, &actual.symbols));
    }

    if differing.is_empty() {
        return "object snapshots matched".to_string();
    }

    let component_list = differing
        .iter()
        .map(|(name, _, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let (first_name, first_expected, first_actual) = differing[0];

    format!(
        "differing object components: {}\nfirst differing component: {}\n{}",
        component_list,
        first_name,
        describe_text_difference(first_expected, first_actual, left_label, right_label)
    )
}

fn describe_run_difference(
    expected: &RunCapture,
    actual: &RunCapture,
    left_label: &str,
    right_label: &str,
) -> String {
    let expected = normalize_run_signature(expected);
    let actual = normalize_run_signature(actual);
    let mut differing = Vec::new();
    if expected.exit_code != actual.exit_code {
        differing.push("exit_code");
    }
    if expected.stdout != actual.stdout {
        differing.push("stdout");
    }
    if expected.stderr != actual.stderr {
        differing.push("stderr");
    }
    if expected.files != actual.files {
        differing.push("files");
    }

    if differing.is_empty() {
        return "runtime behavior matched".to_string();
    }

    let component_list = differing.join(", ");
    match differing[0] {
        "exit_code" => format!(
            "differing runtime components: {}\nfirst differing component: exit_code\n{}: {}\n{}: {}",
            component_list,
            left_label,
            expected.exit_code,
            right_label,
            actual.exit_code
        ),
        "stdout" => format!(
            "differing runtime components: {}\nfirst differing component: stdout\n{}",
            component_list,
            describe_output_difference(&expected.stdout, &actual.stdout, left_label, right_label)
        ),
        "stderr" => format!(
            "differing runtime components: {}\nfirst differing component: stderr\n{}",
            component_list,
            describe_output_difference(&expected.stderr, &actual.stderr, left_label, right_label)
        ),
        "files" => format!(
            "differing runtime components: {}\nfirst differing component: files\n{}",
            component_list,
            describe_file_snapshot_difference(
                &expected.files,
                &actual.files,
                left_label,
                right_label
            )
        ),
        _ => unreachable!("only known runtime components are compared"),
    }
}

fn describe_file_snapshot_difference(
    expected: &BTreeMap<String, Vec<u8>>,
    actual: &BTreeMap<String, Vec<u8>>,
    left_label: &str,
    right_label: &str,
) -> String {
    let paths = expected
        .keys()
        .chain(actual.keys())
        .collect::<BTreeSet<_>>();
    let path = paths
        .into_iter()
        .find(|path| expected.get(*path) != actual.get(*path))
        .expect("different file snapshots have a differing path");

    match (expected.get(path), actual.get(path)) {
        (Some(expected), Some(actual)) => format!(
            "file '{}' differs\n{}",
            path,
            describe_byte_difference(expected, actual, left_label, right_label)
        ),
        (Some(expected), None) => format!(
            "file '{}' is present only in {} ({} bytes)",
            path,
            left_label,
            expected.len()
        ),
        (None, Some(actual)) => format!(
            "file '{}' is present only in {} ({} bytes)",
            path,
            right_label,
            actual.len()
        ),
        (None, None) => unreachable!("path came from at least one snapshot"),
    }
}

fn describe_output_difference(
    expected: &[u8],
    actual: &[u8],
    left_label: &str,
    right_label: &str,
) -> String {
    if let (Ok(expected), Ok(actual)) = (std::str::from_utf8(expected), std::str::from_utf8(actual))
    {
        return describe_text_difference(expected, actual, left_label, right_label);
    }

    describe_byte_difference(expected, actual, left_label, right_label)
}

fn describe_byte_difference(
    expected: &[u8],
    actual: &[u8],
    left_label: &str,
    right_label: &str,
) -> String {
    let differing_offset = expected
        .iter()
        .zip(actual)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| expected.len().min(actual.len()));
    let byte_label = |bytes: &[u8]| match bytes.get(differing_offset) {
        Some(byte) => format!("0x{byte:02x}"),
        None => "<end-of-output>".to_string(),
    };

    format!(
        "first differing byte offset: {differing_offset}\n{left_label}: {} ({} bytes)\n{right_label}: {} ({} bytes)",
        byte_label(expected),
        expected.len(),
        byte_label(actual),
        actual.len()
    )
}

fn varying_object_components(snapshots: &[&ObjectSnapshot]) -> Vec<&'static str> {
    object_components_by_variation(snapshots, true)
}

fn stable_object_components(snapshots: &[&ObjectSnapshot]) -> Vec<&'static str> {
    object_components_by_variation(snapshots, false)
}

fn varying_run_components(signatures: &[&RunSignature]) -> Vec<&'static str> {
    run_components_by_variation(signatures, true)
}

fn stable_run_components(signatures: &[&RunSignature]) -> Vec<&'static str> {
    run_components_by_variation(signatures, false)
}

fn object_components_by_variation(
    snapshots: &[&ObjectSnapshot],
    want_varying: bool,
) -> Vec<&'static str> {
    let components = [
        (
            "text",
            snapshots
                .iter()
                .map(|snapshot| snapshot.text.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "load_commands",
            snapshots
                .iter()
                .map(|snapshot| snapshot.load_commands.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "relocations",
            snapshots
                .iter()
                .map(|snapshot| snapshot.relocations.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "symbols",
            snapshots
                .iter()
                .map(|snapshot| snapshot.symbols.as_str())
                .collect::<Vec<_>>(),
        ),
    ];

    components
        .into_iter()
        .filter_map(|(name, values)| {
            let varies = count_unique_strings(values) > 1;
            if varies == want_varying {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

fn run_components_by_variation(
    signatures: &[&RunSignature],
    want_varying: bool,
) -> Vec<&'static str> {
    let components = [
        (
            "exit_code",
            signatures
                .iter()
                .map(|signature| signature.exit_code)
                .collect::<BTreeSet<_>>()
                .len()
                > 1,
        ),
        (
            "stdout",
            signatures
                .iter()
                .map(|signature| signature.stdout.as_slice())
                .collect::<BTreeSet<_>>()
                .len()
                > 1,
        ),
        (
            "stderr",
            signatures
                .iter()
                .map(|signature| signature.stderr.as_slice())
                .collect::<BTreeSet<_>>()
                .len()
                > 1,
        ),
        (
            "files",
            signatures
                .iter()
                .map(|signature| &signature.files)
                .collect::<BTreeSet<_>>()
                .len()
                > 1,
        ),
    ];

    components
        .into_iter()
        .filter_map(|(name, varies)| {
            if varies == want_varying {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}

fn join_or_none(values: &[&str]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

fn join_or_none_from_strings(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

fn join_usize_set(values: &BTreeSet<usize>) -> String {
    if values.is_empty() {
        "n/a".to_string()
    } else {
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn join_string_set(values: &BTreeSet<String>) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

fn render_consistency_rollup(rollup: &ConsistencyRollup) -> String {
    let mut parts = vec![format!("{} cells", rollup.cells)];
    if !rollup.repeat_counts.is_empty() {
        parts.push(format!(
            "repeat_count={}",
            join_usize_set(&rollup.repeat_counts)
        ));
    }
    if !rollup.unique_variant_counts.is_empty() {
        parts.push(format!(
            "unique_variants={}",
            join_usize_set(&rollup.unique_variant_counts)
        ));
    }
    if !rollup.varying_components.is_empty() {
        parts.push(format!(
            "varying={}",
            join_string_set(&rollup.varying_components)
        ));
    }
    if !rollup.stable_components.is_empty() {
        parts.push(format!(
            "stable={}",
            join_string_set(&rollup.stable_components)
        ));
    }
    parts.join("; ")
}

fn write_behavior_run_artifacts(
    root: &Path,
    prefix: &str,
    command: &str,
    run: &RunCapture,
) -> Result<(), std::io::Error> {
    let signature = normalize_run_signature(run);
    fs::write(root.join(format!("{}.command.txt", prefix)), command)?;
    fs::write(root.join(format!("{}.stdout.txt", prefix)), &run.stdout)?;
    fs::write(root.join(format!("{}.stderr.txt", prefix)), &run.stderr)?;
    fs::write(
        root.join(format!("{}.exit_code.txt", prefix)),
        format!("{}\n", run.exit_code),
    )?;
    fs::write(
        root.join(format!("{}.normalized.txt", prefix)),
        format_run_signature(&signature),
    )?;
    write_file_snapshot(&root.join(format!("{}.files", prefix)), &run.files)?;
    Ok(())
}

fn write_file_snapshot(
    root: &Path,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(), std::io::Error> {
    fs::create_dir_all(root)?;
    for (relative, bytes) in files {
        let relative = Path::new(relative);
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "sandbox snapshot contains unsafe path '{}'",
                    relative.display()
                ),
            ));
        }
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, bytes)?;
    }
    Ok(())
}

fn render_summary(summary: &Summary) -> String {
    let mut lines = vec![
        "Summary".to_string(),
        format!("  passed: {}", summary.passed),
        format!("  failed: {}", summary.failed),
        format!("  xfailed: {}", summary.xfailed),
        format!("  xpassed: {}", summary.xpassed),
        format!("  future: {}", summary.future),
    ];

    if !summary.consistency.is_empty() {
        lines.push(String::new());
        lines.push("Consistency".to_string());
        lines.push(format!("  affected_checks: {}", summary.consistency.len()));
        lines.push(format!(
            "  cells_with_issues: {}",
            summary
                .consistency
                .values()
                .map(|rollup| rollup.cells)
                .sum::<usize>()
        ));
        for (check, rollup) in &summary.consistency {
            lines.push(format!(
                "  {}: {}",
                check.as_str(),
                render_consistency_rollup(rollup)
            ));
        }
    }

    lines.join("\n")
}

fn write_failure_bundle(
    suite: &SuiteSpec,
    case: &CaseSpec,
    prepared: &PreparedInput,
    outcome: &Outcome,
    artifacts: &ExecutionArtifacts,
) -> Result<PathBuf, String> {
    let bundle_root = default_report_root()
        .join(sanitize_component(&suite.name))
        .join(sanitize_component(&case.name))
        .join(next_report_suffix(outcome.opt_level));
    fs::create_dir_all(&bundle_root).map_err(|e| {
        format!(
            "cannot create report bundle '{}': {}",
            bundle_root.display(),
            e
        )
    })?;

    let stage_list = artifacts
        .requested
        .iter()
        .map(Stage::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let refs = if case.reference_compilers.is_empty() {
        "none".to_string()
    } else {
        case.reference_compilers
            .iter()
            .map(ReferenceCompiler::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let consistency = if case.consistency_checks.is_empty() {
        "none".to_string()
    } else {
        case.consistency_checks
            .iter()
            .map(ConsistencyCheck::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let metadata = format!(
        "suite: {}\ncase: {}\noutcome: {:?}\nopt: {}\nsource: {}\nrequested_stages: {}\nrepeat_count: {}\nreference_compilers: {}\nconsistency_checks: {}\n",
        suite.name,
        case.name,
        outcome.kind,
        outcome.opt_level.as_str(),
        case.source_label(),
        stage_list,
        case.repeat_count,
        refs,
        consistency
    );
    fs::write(bundle_root.join("metadata.txt"), metadata)
        .map_err(|e| format!("cannot write bundle metadata: {}", e))?;
    fs::write(bundle_root.join("detail.txt"), &outcome.detail)
        .map_err(|e| format!("cannot write bundle detail: {}", e))?;

    write_case_sources_bundle(&bundle_root, case, prepared)?;

    let armfortas_root = bundle_root.join("armfortas");
    fs::create_dir_all(&armfortas_root)
        .map_err(|e| format!("cannot create armfortas bundle dir: {}", e))?;
    if let Some(result) = &artifacts.armfortas {
        write_capture_result(&armfortas_root, result)?;
    }
    if let Some(failure) = &artifacts.armfortas_failure {
        write_capture_result(&armfortas_root, &failure.partial_result())?;
        fs::write(
            armfortas_root.join("error.txt"),
            format!("stage: {}\n{}\n", failure.stage.as_str(), failure.detail),
        )
        .map_err(|e| format!("cannot write armfortas error bundle: {}", e))?;
    }

    if !artifacts.references.is_empty() {
        let refs_root = bundle_root.join("references");
        fs::create_dir_all(&refs_root)
            .map_err(|e| format!("cannot create references bundle dir: {}", e))?;
        for reference in &artifacts.references {
            write_reference_bundle(&refs_root, reference)?;
        }
    }

    if !artifacts.consistency_issues.is_empty() {
        write_consistency_bundle(&bundle_root, &artifacts.consistency_issues)?;
    }

    Ok(bundle_root)
}

fn write_case_sources_bundle(
    bundle_root: &Path,
    case: &CaseSpec,
    prepared: &PreparedInput,
) -> Result<(), String> {
    if case.graph_files.is_empty() {
        let source_text = fs::read_to_string(&case.source)
            .map_err(|e| format!("cannot read case source '{}': {}", case.source.display(), e))?;
        fs::write(bundle_root.join("source.f90"), source_text)
            .map_err(|e| format!("cannot write bundle source copy: {}", e))?;
        return Ok(());
    }

    let entry_text = fs::read_to_string(&prepared.compiler_source).map_err(|e| {
        format!(
            "cannot read graph entry source '{}': {}",
            prepared.compiler_source.display(),
            e
        )
    })?;
    fs::write(bundle_root.join("source.f90"), entry_text)
        .map_err(|e| format!("cannot write graph entry bundle source copy: {}", e))?;

    let sources_root = bundle_root.join("sources");
    fs::create_dir_all(&sources_root)
        .map_err(|e| format!("cannot create bundle sources dir: {}", e))?;
    for (index, file) in case.graph_files.iter().enumerate() {
        let text = fs::read_to_string(file)
            .map_err(|e| format!("cannot read graph source '{}': {}", file.display(), e))?;
        let file_name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("source.f90");
        let target = sources_root.join(format!("{:02}_{}", index, file_name));
        fs::write(target, text).map_err(|e| format!("cannot write bundle graph source: {}", e))?;
    }

    Ok(())
}

fn write_capture_result(root: &Path, result: &CaptureResult) -> Result<(), String> {
    for (stage, captured) in &result.stages {
        match captured {
            CapturedStage::Text(text) => {
                fs::write(root.join(format!("{}.txt", stage.as_str())), text).map_err(|e| {
                    format!("cannot write '{}' stage bundle: {}", stage.as_str(), e)
                })?;
            }
            CapturedStage::Run(run) => {
                fs::write(root.join("run.stdout.txt"), &run.stdout)
                    .map_err(|e| format!("cannot write run stdout bundle: {}", e))?;
                fs::write(root.join("run.stderr.txt"), &run.stderr)
                    .map_err(|e| format!("cannot write run stderr bundle: {}", e))?;
                fs::write(
                    root.join("run.exit_code.txt"),
                    format!("{}\n", run.exit_code),
                )
                .map_err(|e| format!("cannot write run exit-code bundle: {}", e))?;
                write_file_snapshot(&root.join("run.files"), &run.files)
                    .map_err(|e| format!("cannot write run file snapshot bundle: {}", e))?;
            }
        }
    }
    Ok(())
}

fn write_reference_bundle(root: &Path, reference: &ReferenceResult) -> Result<(), String> {
    let ref_root = root.join(sanitize_component(reference.compiler.as_str()));
    fs::create_dir_all(&ref_root)
        .map_err(|e| format!("cannot create reference bundle dir: {}", e))?;
    fs::write(ref_root.join("command.txt"), &reference.compile_command)
        .map_err(|e| format!("cannot write reference command bundle: {}", e))?;
    fs::write(
        ref_root.join("compile.exit_code.txt"),
        format!("{}\n", reference.compile_exit_code),
    )
    .map_err(|e| format!("cannot write reference compile exit-code bundle: {}", e))?;
    fs::write(
        ref_root.join("compile.stdout.txt"),
        &reference.compile_stdout,
    )
    .map_err(|e| format!("cannot write reference compile stdout bundle: {}", e))?;
    fs::write(
        ref_root.join("compile.stderr.txt"),
        &reference.compile_stderr,
    )
    .map_err(|e| format!("cannot write reference compile stderr bundle: {}", e))?;
    if let Some(run) = &reference.run {
        fs::write(ref_root.join("run.stdout.txt"), &run.stdout)
            .map_err(|e| format!("cannot write reference run stdout bundle: {}", e))?;
        fs::write(ref_root.join("run.stderr.txt"), &run.stderr)
            .map_err(|e| format!("cannot write reference run stderr bundle: {}", e))?;
        fs::write(
            ref_root.join("run.exit_code.txt"),
            format!("{}\n", run.exit_code),
        )
        .map_err(|e| format!("cannot write reference run exit-code bundle: {}", e))?;
        write_file_snapshot(&ref_root.join("run.files"), &run.files)
            .map_err(|e| format!("cannot write reference run file snapshot bundle: {}", e))?;
    }
    if let Some(err) = &reference.run_error {
        fs::write(ref_root.join("run.error.txt"), err)
            .map_err(|e| format!("cannot write reference run error bundle: {}", e))?;
    }
    Ok(())
}

fn render_consistency_bundle_summary(issues: &[ConsistencyIssue]) -> String {
    let mut rollups = BTreeMap::new();
    for issue in issues {
        rollups
            .entry(issue.check)
            .or_insert_with(ConsistencyRollup::default)
            .record(&issue.observation());
    }

    let mut aggregate = ConsistencyRollup::default();
    for issue in issues {
        aggregate.record(&issue.observation());
    }

    let checks = if rollups.is_empty() {
        "none".to_string()
    } else {
        rollups
            .keys()
            .map(ConsistencyCheck::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut lines = vec![
        format!("issue_count: {}", issues.len()),
        format!("checks: {}", checks),
    ];

    if !aggregate.repeat_counts.is_empty() {
        lines.push(format!(
            "repeat_counts: {}",
            join_usize_set(&aggregate.repeat_counts)
        ));
    }
    if !aggregate.unique_variant_counts.is_empty() {
        lines.push(format!(
            "unique_variants: {}",
            join_usize_set(&aggregate.unique_variant_counts)
        ));
    }
    if !aggregate.varying_components.is_empty() {
        lines.push(format!(
            "varying_components: {}",
            join_string_set(&aggregate.varying_components)
        ));
    }
    if !aggregate.stable_components.is_empty() {
        lines.push(format!(
            "stable_components: {}",
            join_string_set(&aggregate.stable_components)
        ));
    }

    if !rollups.is_empty() {
        lines.push(String::new());
        lines.push("per_check:".to_string());
        for (check, rollup) in rollups {
            lines.push(format!(
                "  {}: {}",
                check.as_str(),
                render_consistency_rollup(&rollup)
            ));
        }
    }

    lines.push(String::new());
    for issue in issues {
        lines.push(format!("check: {}", issue.check.as_str()));
        lines.push(format!("summary: {}", issue.summary));
        if let Some(repeat_count) = issue.repeat_count {
            lines.push(format!("repeat_count: {}", repeat_count));
        }
        if let Some(unique_variant_count) = issue.unique_variant_count {
            lines.push(format!("unique_variants: {}", unique_variant_count));
        }
        if !issue.varying_components.is_empty() {
            lines.push(format!(
                "varying_components: {}",
                join_or_none_from_strings(&issue.varying_components)
            ));
        }
        if !issue.stable_components.is_empty() {
            lines.push(format!(
                "stable_components: {}",
                join_or_none_from_strings(&issue.stable_components)
            ));
        }
        lines.push(format!(
            "artifacts: {}",
            sanitize_component(issue.check.as_str())
        ));
        lines.push(String::new());
    }

    lines.join("\n")
}

fn write_consistency_bundle(root: &Path, issues: &[ConsistencyIssue]) -> Result<(), String> {
    let consistency_root = root.join("consistency");
    fs::create_dir_all(&consistency_root)
        .map_err(|e| format!("cannot create consistency bundle dir: {}", e))?;

    let summary = render_consistency_bundle_summary(issues);
    fs::write(consistency_root.join("summary.txt"), summary)
        .map_err(|e| format!("cannot write consistency summary bundle: {}", e))?;

    for issue in issues {
        let issue_root = consistency_root.join(sanitize_component(issue.check.as_str()));
        fs::create_dir_all(&issue_root)
            .map_err(|e| format!("cannot create consistency issue bundle dir: {}", e))?;
        fs::write(issue_root.join("summary.txt"), &issue.summary)
            .map_err(|e| format!("cannot write consistency issue summary bundle: {}", e))?;
        fs::write(issue_root.join("detail.txt"), &issue.detail)
            .map_err(|e| format!("cannot write consistency issue detail bundle: {}", e))?;
        let runs_root = issue_root.join("artifacts");
        copy_directory_recursive(&issue.temp_root, &runs_root)?;
    }

    Ok(())
}

fn copy_directory_recursive(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| {
        format!(
            "cannot create copied artifact dir '{}': {}",
            destination.display(),
            e
        )
    })?;

    for entry in fs::read_dir(source)
        .map_err(|e| format!("cannot read artifact dir '{}': {}", source.display(), e))?
    {
        let entry = entry
            .map_err(|e| format!("cannot read artifact entry '{}': {}", source.display(), e))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(|e| {
            format!(
                "cannot read artifact type '{}': {}",
                source_path.display(),
                e
            )
        })?;
        if file_type.is_dir() {
            copy_directory_recursive(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path).map_err(|e| {
                format!(
                    "cannot copy artifact '{}' to '{}': {}",
                    source_path.display(),
                    destination_path.display(),
                    e
                )
            })?;
        }
    }

    Ok(())
}

fn render_command(binary: &str, args: &[String]) -> String {
    let mut rendered = vec![quote_arg(binary)];
    rendered.extend(args.iter().map(|arg| quote_arg(arg)));
    rendered.join(" ")
}

fn quote_arg(arg: &str) -> String {
    if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./".contains(ch))
    {
        arg.to_string()
    } else {
        format!("{:?}", arg)
    }
}

fn sanitize_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

fn next_report_temp_root(compiler: ReferenceCompiler, opt_level: OptLevel) -> PathBuf {
    default_report_root().join(".tmp").join(format!(
        "{}_{}_{}",
        sanitize_component(compiler.as_str()),
        opt_level.as_str().to_ascii_lowercase(),
        next_report_suffix(opt_level)
    ))
}

fn next_consistency_temp_root(opt_level: OptLevel) -> PathBuf {
    default_report_root().join(".tmp").join(format!(
        "consistency_{}_{}",
        opt_level.as_str().to_ascii_lowercase(),
        next_report_suffix(opt_level)
    ))
}

fn next_report_suffix(opt_level: OptLevel) -> String {
    format!(
        "{}-{}-{:04}",
        opt_level.as_str().to_ascii_lowercase(),
        std::process::id(),
        REPORT_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn print_outcome(outcome: &Outcome) {
    let label = format!(
        "{}::{}[{}]",
        outcome.suite,
        outcome.case,
        outcome.opt_level.as_str()
    );
    match outcome.kind {
        OutcomeKind::Pass => println!("PASS   {}", label),
        OutcomeKind::Fail => {
            println!("FAIL   {}", label);
            if !outcome.detail.is_empty() {
                println!("{}", outcome.detail);
            }
        }
        OutcomeKind::Xfail => {
            println!("XFAIL  {}", label);
            if !outcome.detail.is_empty() {
                println!("{}", outcome.detail);
            }
        }
        OutcomeKind::Xpass => {
            println!("XPASS  {}", label);
            if !outcome.detail.is_empty() {
                println!("{}", outcome.detail);
            }
        }
        OutcomeKind::Future => {
            println!("FUTURE {}", label);
            if !outcome.detail.is_empty() {
                println!("{}", outcome.detail);
            }
        }
    }
    if let Some(bundle) = &outcome.bundle {
        println!("bundle: {}", bundle.display());
    }
}

fn print_summary(summary: &Summary) {
    println!();
    println!("{}", render_summary(summary));
}

#[derive(Debug, Clone)]
struct Check {
    line_num: usize,
    pattern: String,
}

#[derive(Debug, Clone)]
struct FileCheck {
    line_num: usize,
    relative_path: String,
    pattern: String,
    negative: bool,
}

fn extract_checks(source: &str) -> Vec<Check> {
    source
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let trimmed = line.trim();
            trimmed.strip_prefix("! CHECK:").map(|rest| Check {
                line_num: i + 1,
                pattern: rest.trim().to_string(),
            })
        })
        .collect()
}

fn extract_file_checks(source: &str, source_path: &Path) -> Result<Vec<FileCheck>, String> {
    let mut checks = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        let (rest, negative) = if let Some(rest) = trimmed.strip_prefix("! FILE_CHECK:") {
            (rest.trim(), false)
        } else if let Some(rest) = trimmed.strip_prefix("! FILE_NOT:") {
            (rest.trim(), true)
        } else if trimmed.starts_with("! FILE_") {
            let directive = trimmed
                .split_once(':')
                .map(|(name, _)| name)
                .unwrap_or(trimmed);
            return Err(format!(
                "{}:{}: unsupported {} directive in bencch check-comments; \
                     supported file directives are FILE_CHECK and FILE_NOT",
                source_path.display(),
                index + 1,
                directive
            ));
        } else {
            continue;
        };

        let Some((raw_path, raw_pattern)) = rest.split_once("=>") else {
            return Err(format!(
                "{}:{}: {} must be written as <relative-path> => <substring>",
                source_path.display(),
                index + 1,
                if negative { "FILE_NOT" } else { "FILE_CHECK" }
            ));
        };
        let relative_path = raw_path.trim();
        if relative_path.is_empty() {
            return Err(format!(
                "{}:{}: FILE_CHECK/FILE_NOT path cannot be empty",
                source_path.display(),
                index + 1
            ));
        }
        if Path::new(relative_path).is_absolute() {
            return Err(format!(
                "{}:{}: FILE_CHECK/FILE_NOT path must be relative, got '{}'",
                source_path.display(),
                index + 1,
                relative_path
            ));
        }

        checks.push(FileCheck {
            line_num: index + 1,
            relative_path: relative_path.to_string(),
            pattern: raw_pattern.trim().to_string(),
            negative,
        });
    }
    Ok(checks)
}

fn match_checks(checks: &[Check], output: &str, case_name: &str) -> Result<(), String> {
    let output_lines: Vec<&str> = output.lines().collect();
    let mut output_idx = 0;

    for check in checks {
        let mut found = false;
        while output_idx < output_lines.len() {
            if output_lines[output_idx].trim().contains(&check.pattern) {
                found = true;
                output_idx += 1;
                break;
            }
            output_idx += 1;
        }
        if !found {
            return Err(format!(
                "{}:{}: CHECK failed: expected '{}' not found in remaining output\nfull output:\n{}",
                case_name, check.line_num, check.pattern, output
            ));
        }
    }

    Ok(())
}

fn match_file_checks(
    checks: &[FileCheck],
    files: &BTreeMap<String, Vec<u8>>,
    source_path: &Path,
) -> Result<(), String> {
    let mut search_offsets: BTreeMap<&str, usize> = BTreeMap::new();
    for check in checks {
        let Some(bytes) = files.get(&check.relative_path) else {
            return Err(format!(
                "{}:{}: FILE_CHECK/FILE_NOT expected sandbox file '{}' to exist",
                source_path.display(),
                check.line_num,
                check.relative_path
            ));
        };
        let text = String::from_utf8_lossy(bytes);
        if check.negative {
            if text.contains(&check.pattern) {
                return Err(format!(
                    "{}:{}: FILE_CHECK/FILE_NOT failed: substring '{}' appears in sandbox file '{}'\n\
                     Full file contents:\n{}",
                    source_path.display(),
                    check.line_num,
                    check.pattern,
                    check.relative_path,
                    text
                ));
            }
            continue;
        }

        let search_offset = search_offsets
            .entry(check.relative_path.as_str())
            .or_insert(0);
        if let Some(relative_offset) = text[*search_offset..].find(&check.pattern) {
            *search_offset += relative_offset + check.pattern.len();
        } else {
            return Err(format!(
                "{}:{}: FILE_CHECK/FILE_NOT failed: substring '{}' not found in sandbox file '{}' from offset {}\n\
                 Full file contents:\n{}",
                source_path.display(),
                check.line_num,
                check.pattern,
                check.relative_path,
                *search_offset,
                text
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::test_support::{
        verify_module, BlockParam, FloatWidth, Function, Inst, InstKind, IntWidth, IrType, Module,
        Position, Span, Terminator, ValueId,
    };

    fn dummy_span() -> Span {
        Span {
            file_id: 0,
            start: Position { line: 1, col: 1 },
            end: Position { line: 1, col: 1 },
        }
    }

    #[test]
    fn parses_suite_and_case() {
        let root = std::env::temp_dir().join("afs_tests_parser_spec.afs");
        fs::write(
            &root,
            r#"suite "runtime/smoke"

case "hello"
source "../../../test_programs/hello.f90"
armfortas => run, ir
expect run.stdout check-comments
expect ir contains "module main"
expect asm not-contains "x18"
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        assert_eq!(suite.name, "runtime/smoke");
        assert_eq!(suite.cases.len(), 1);
        assert!(suite.cases[0].requested.contains(&Stage::Run));
        assert!(suite.cases[0].requested.contains(&Stage::Ir));
        assert!(matches!(
            suite.cases[0].expectations[2],
            Expectation::NotContains {
                target: Target::Stage(Stage::Asm),
                ..
            }
        ));
        assert_eq!(suite.cases[0].opt_levels, vec![OptLevel::O0]);
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn parses_matrix_status_and_differential() {
        let root = std::env::temp_dir().join("afs_tests_matrix_spec.afs");
        fs::write(
            &root,
            r#"suite "runtime/matrix"

case "hello"
source "../../../test_programs/hello.f90"
opts => O0, O1, O2
armfortas => run
differential => gfortran, flang-new
expect run.exit_code equals 0
xfail when O1, O2 because "known issue"
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        let case = &suite.cases[0];
        assert_eq!(
            case.opt_levels,
            vec![OptLevel::O0, OptLevel::O1, OptLevel::O2]
        );
        assert_eq!(
            case.reference_compilers,
            vec![ReferenceCompiler::Gfortran, ReferenceCompiler::FlangNew]
        );
        assert!(matches!(
            status_for_opt(case, OptLevel::O0),
            EffectiveStatus::Normal
        ));
        assert!(matches!(
            status_for_opt(case, OptLevel::O1),
            EffectiveStatus::Xfail(_)
        ));
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn parses_all_opt_levels_including_os() {
        assert_eq!(
            parse_opt_level_list("all").unwrap(),
            vec![
                OptLevel::O0,
                OptLevel::O1,
                OptLevel::O2,
                OptLevel::O3,
                OptLevel::Os,
                OptLevel::Ofast,
            ]
        );
    }

    #[test]
    fn parses_consistency_checks() {
        let root = std::env::temp_dir().join("afs_tests_consistency_spec.afs");
        fs::write(
            &root,
            r#"suite "consistency/object"

case "driver_paths"
source "../../fixtures/backend/runtime_calls.f90"
armfortas => asm, obj
repeat => 5
consistency => cli_obj_vs_system_as, cli-obj-vs-system-as, cli_asm_reproducible, cli-obj-reproducible, cli_run_reproducible, capture_asm_vs_cli_asm, capture-obj-vs-cli-obj, capture_run_vs_cli_run, capture_asm_reproducible, capture-obj-reproducible, capture_run_reproducible
expect obj contains "_main"
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        let case = &suite.cases[0];
        assert_eq!(
            case.consistency_checks,
            vec![
                ConsistencyCheck::CliObjVsSystemAs,
                ConsistencyCheck::CliAsmReproducible,
                ConsistencyCheck::CliObjReproducible,
                ConsistencyCheck::CliRunReproducible,
                ConsistencyCheck::CaptureAsmVsCliAsm,
                ConsistencyCheck::CaptureObjVsCliObj,
                ConsistencyCheck::CaptureRunVsCliRun,
                ConsistencyCheck::CaptureAsmReproducible,
                ConsistencyCheck::CaptureObjReproducible,
                ConsistencyCheck::CaptureRunReproducible,
            ]
        );
        assert_eq!(case.repeat_count, 5);
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn parses_graph_case() {
        let root = std::env::temp_dir().join("afs_tests_graph_spec");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("math_values.f90"),
            "module math_values\nend module\n",
        )
        .unwrap();
        fs::write(root.join("main.f90"), "program main\nend program\n").unwrap();
        fs::write(
            root.join("graph.afs"),
            r#"suite "modules/graph"

case "basic_use"
entry "main.f90"
file "math_values.f90"
file "main.f90"
armfortas => run
expect run.exit_code equals 0
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root.join("graph.afs")).unwrap();
        let case = &suite.cases[0];
        assert_eq!(case.source, root.join("main.f90"));
        assert_eq!(
            case.graph_files,
            vec![root.join("math_values.f90"), root.join("main.f90")]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cli_collects_tool_overrides() {
        let args = vec![
            "run".to_string(),
            "--suite".to_string(),
            "consistency/runtime".to_string(),
            "--armfortas-bin".to_string(),
            "/tmp/armfortas".to_string(),
            "--gfortran-bin".to_string(),
            "/tmp/gfortran".to_string(),
            "--flang-bin".to_string(),
            "/tmp/flang-new".to_string(),
            "--cc-bin".to_string(),
            "/tmp/clang".to_string(),
            "--as-bin".to_string(),
            "/tmp/as".to_string(),
            "--otool-bin".to_string(),
            "/tmp/otool".to_string(),
            "--nm-bin".to_string(),
            "/tmp/nm".to_string(),
            "--objdump-bin".to_string(),
            "/tmp/objdump".to_string(),
            "--readelf-bin".to_string(),
            "/tmp/readelf".to_string(),
        ];

        let command = parse_cli(&args).unwrap();
        let config = match command {
            CommandKind::Run(config) => config,
            other => panic!(
                "expected run command, got {:?}",
                std::mem::discriminant(&other)
            ),
        };

        assert_eq!(config.suite_filter.as_deref(), Some("consistency/runtime"));
        assert_eq!(
            config.tools.armfortas,
            ArmfortasCliAdapter::External("/tmp/armfortas".into())
        );
        assert_eq!(config.tools.gfortran, "/tmp/gfortran");
        assert_eq!(config.tools.flang_new, "/tmp/flang-new");
        assert_eq!(config.tools.cc, "/tmp/clang");
        assert_eq!(config.tools.system_as, "/tmp/as");
        assert_eq!(config.tools.otool, "/tmp/otool");
        assert_eq!(config.tools.nm, "/tmp/nm");
        assert_eq!(config.tools.objdump, "/tmp/objdump");
        assert_eq!(config.tools.readelf, "/tmp/readelf");
    }

    #[test]
    fn parses_failure_expectation() {
        let root = std::env::temp_dir().join("afs_tests_failure_spec.afs");
        fs::write(
            &root,
            r#"suite "frontend/parser"

case "missing_then"
source "../../fixtures/frontend/parser/missing_then.f90"
armfortas => tokens
expect tokens contains "if"
expect-fail parser contains "expected"
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        assert_eq!(suite.cases.len(), 1);
        assert!(has_failure_expectation(&suite.cases[0]));
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn check_matching_preserves_order() {
        let checks = vec![
            Check {
                line_num: 1,
                pattern: "alpha".into(),
            },
            Check {
                line_num: 2,
                pattern: "omega".into(),
            },
        ];
        assert!(match_checks(&checks, "alpha\nmiddle\nomega\n", "demo").is_ok());
        assert!(match_checks(&checks, "omega\nalpha\n", "demo").is_err());
    }

    #[test]
    fn file_check_comments_reject_wrong_file_contents() {
        let root = next_report_temp_root(ReferenceCompiler::Gfortran, OptLevel::O0);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("file_contract.f90");
        fs::write(
            &source,
            r#"program file_contract
  implicit none
  open(unit=10, file='artifact.txt', status='replace', action='write')
  write(10, '(I0)') 99
  close(10)
  print '(I0)', 42
end program file_contract
! CHECK: 42
! FILE_CHECK: artifact.txt => 42
! FILE_NOT: artifact.txt => 99
"#,
        )
        .unwrap();

        let case = CaseSpec {
            name: "file_contract".into(),
            source: source.clone(),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Run]),
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::CheckComments(Target::RunStdout)],
            status_rules: Vec::new(),
        };
        let request = CaptureRequest {
            input: source,
            requested: BTreeSet::from([Stage::Run]),
            opt_level: OptLevel::O0,
        };
        let result = capture_from_path(&request).unwrap();
        let evaluation = evaluate_positive_expectations(&case, &result);

        let _ = fs::remove_dir_all(&root);
        let error = evaluation.expect_err("FILE_CHECK mismatch must fail the case");
        assert!(error.contains("FILE_CHECK/FILE_NOT failed"), "{error}");
        assert!(error.contains("artifact.txt"), "{error}");
    }

    #[test]
    fn file_checks_preserve_order_and_enforce_negative_patterns() {
        let source_path = Path::new("ordered_file_checks.f90");
        let checks = extract_file_checks(
            "! FILE_CHECK: artifact.txt => alpha\n\
             ! FILE_CHECK: artifact.txt => omega\n\
             ! FILE_NOT: artifact.txt => forbidden\n",
            source_path,
        )
        .unwrap();
        let files = BTreeMap::from([(
            "artifact.txt".to_string(),
            b"alpha\nmiddle\nomega\n".to_vec(),
        )]);
        assert!(match_file_checks(&checks, &files, source_path).is_ok());

        let out_of_order = BTreeMap::from([(
            "artifact.txt".to_string(),
            b"omega\nmiddle\nalpha\n".to_vec(),
        )]);
        assert!(match_file_checks(&checks, &out_of_order, source_path).is_err());

        let forbidden = BTreeMap::from([(
            "artifact.txt".to_string(),
            b"alpha\nomega\nforbidden\n".to_vec(),
        )]);
        let error = match_file_checks(&checks, &forbidden, source_path).unwrap_err();
        assert!(error.contains("substring 'forbidden' appears"), "{error}");
    }

    #[test]
    fn malformed_and_unsupported_file_directives_fail_closed() {
        let source_path = Path::new("invalid_file_checks.f90");
        let malformed =
            extract_file_checks("! FILE_CHECK: artifact.txt\n", source_path).unwrap_err();
        assert!(
            malformed.contains("<relative-path> => <substring>"),
            "{malformed}"
        );

        let unsupported =
            extract_file_checks("! FILE_EXISTS: artifact.txt\n", source_path).unwrap_err();
        assert!(
            unsupported.contains("unsupported ! FILE_EXISTS"),
            "{unsupported}"
        );
        assert!(
            unsupported.contains("FILE_CHECK and FILE_NOT"),
            "{unsupported}"
        );
    }

    fn run_only_result(stdout: &str, stderr: &str, exit_code: i32) -> CaptureResult {
        CaptureResult {
            input: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            stages: std::collections::BTreeMap::from([(
                Stage::Run,
                CapturedStage::Run(RunCapture {
                    exit_code,
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: stderr.as_bytes().to_vec(),
                    files: BTreeMap::new(),
                }),
            )]),
        }
    }

    fn reference_run(
        compiler: ReferenceCompiler,
        stdout: &str,
        stderr: &str,
        exit_code: i32,
    ) -> ReferenceResult {
        ReferenceResult {
            compiler,
            compile_command: format!("{} demo.f90 -o demo", compiler.as_str()),
            compile_exit_code: 0,
            compile_stdout: String::new(),
            compile_stderr: String::new(),
            run: Some(RunCapture {
                exit_code,
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
                files: BTreeMap::new(),
            }),
            run_error: None,
        }
    }

    #[test]
    fn not_contains_expectation_checks_text_absence() {
        let case = CaseSpec {
            name: "no_reserved_register".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Asm]),
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::NotContains {
                target: Target::Stage(Stage::Asm),
                needle: "x18".into(),
            }],
            status_rules: Vec::new(),
        };
        let result = CaptureResult {
            input: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            stages: std::collections::BTreeMap::from([(
                Stage::Asm,
                CapturedStage::Text("mov x19, x0\nret\n".into()),
            )]),
        };
        assert!(evaluate_positive_expectations(&case, &result).is_ok());

        let bad = CaptureResult {
            input: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            stages: std::collections::BTreeMap::from([(
                Stage::Asm,
                CapturedStage::Text("mov x18, x0\nret\n".into()),
            )]),
        };
        let err = evaluate_positive_expectations(&case, &bad).unwrap_err();
        assert!(err.contains("expected asm to not contain"));
    }

    #[test]
    fn writes_failure_bundle_with_artifacts() {
        let source = std::env::temp_dir().join("afs_tests_bundle_source.f90");
        fs::write(&source, "program hello\nprint *, 'hello'\nend program\n").unwrap();

        let suite = SuiteSpec {
            name: "runtime/bundles".into(),
            path: PathBuf::from("/tmp/runtime/bundles.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "hello_bundle".into(),
            source: source.clone(),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Ir, Stage::Run]),
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: vec![ReferenceCompiler::Gfortran],
            consistency_checks: vec![ConsistencyCheck::CliObjVsSystemAs],
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };
        let mut stages = std::collections::BTreeMap::new();
        stages.insert(Stage::Ir, CapturedStage::Text("module main".into()));
        stages.insert(
            Stage::Run,
            CapturedStage::Run(RunCapture {
                exit_code: 1,
                stdout: b"oops\n".to_vec(),
                stderr: b"broken\n".to_vec(),
                files: BTreeMap::from([(
                    "nested/armfortas.bin".to_string(),
                    vec![0x00, 0xff, 0x7f],
                )]),
            }),
        );
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Ir, Stage::Run]),
            armfortas: None,
            armfortas_failure: Some(CaptureFailure {
                input: source.clone(),
                opt_level: OptLevel::O0,
                stage: FailureStage::Sema,
                detail: "compiler failed".into(),
                stages,
            }),
            references: vec![ReferenceResult {
                compiler: ReferenceCompiler::Gfortran,
                compile_command: "gfortran hello.f90 -o hello".into(),
                compile_exit_code: 0,
                compile_stdout: String::new(),
                compile_stderr: String::new(),
                run: Some(RunCapture {
                    exit_code: 0,
                    stdout: b"hello\n".to_vec(),
                    stderr: Vec::new(),
                    files: BTreeMap::from([(
                        "reference.txt".to_string(),
                        b"reference bytes\n".to_vec(),
                    )]),
                }),
                run_error: None,
            }],
            consistency_issues: {
                let asm_temp_root =
                    std::env::temp_dir().join("afs_tests_consistency_bundle_issue_asm");
                fs::create_dir_all(&asm_temp_root).unwrap();
                fs::write(asm_temp_root.join("run_00.s"), "mov x19, x0\n").unwrap();

                let obj_temp_root =
                    std::env::temp_dir().join("afs_tests_consistency_bundle_issue_obj");
                fs::create_dir_all(&obj_temp_root).unwrap();
                fs::write(obj_temp_root.join("run_00.o"), "fake object bytes\n").unwrap();

                vec![
                    ConsistencyIssue {
                        check: ConsistencyCheck::CliAsmReproducible,
                        summary: "repeat_count=3 unique_variants=3".into(),
                        repeat_count: Some(3),
                        unique_variant_count: Some(3),
                        varying_components: Vec::new(),
                        stable_components: Vec::new(),
                        detail: "assembly output is not reproducible".into(),
                        temp_root: asm_temp_root,
                    },
                    ConsistencyIssue {
                        check: ConsistencyCheck::CliObjReproducible,
                        summary: "repeat_count=3 unique_variants=2 varying_components=text stable_components=load_commands, relocations, symbols".into(),
                        repeat_count: Some(3),
                        unique_variant_count: Some(2),
                        varying_components: vec!["text".into()],
                        stable_components: vec![
                            "load_commands".into(),
                            "relocations".into(),
                            "symbols".into(),
                        ],
                        detail: "object output is not reproducible".into(),
                        temp_root: obj_temp_root,
                    },
                ]
            },
        };
        let outcome = Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level: OptLevel::O0,
            kind: OutcomeKind::Fail,
            detail: "boom".into(),
            bundle: None,
            consistency_observations: Vec::new(),
        };
        let prepared = PreparedInput {
            compiler_source: source.clone(),
            graph_sources: Vec::new(),
            temp_root: None,
        };

        let bundle = write_failure_bundle(&suite, &case, &prepared, &outcome, &artifacts).unwrap();
        assert!(bundle.join("metadata.txt").exists());
        assert!(bundle.join("detail.txt").exists());
        assert!(bundle.join("source.f90").exists());
        assert!(bundle.join("armfortas").join("ir.txt").exists());
        assert!(bundle.join("armfortas").join("run.stdout.txt").exists());
        assert_eq!(
            fs::read(
                bundle
                    .join("armfortas")
                    .join("run.files")
                    .join("nested")
                    .join("armfortas.bin")
            )
            .unwrap(),
            vec![0x00, 0xff, 0x7f]
        );
        assert!(bundle.join("armfortas").join("error.txt").exists());
        assert!(bundle
            .join("references")
            .join("gfortran")
            .join("run.stdout.txt")
            .exists());
        assert_eq!(
            fs::read(
                bundle
                    .join("references")
                    .join("gfortran")
                    .join("run.files")
                    .join("reference.txt")
            )
            .unwrap(),
            b"reference bytes\n"
        );
        assert!(bundle.join("consistency").join("summary.txt").exists());
        let consistency_summary =
            fs::read_to_string(bundle.join("consistency").join("summary.txt")).unwrap();
        assert!(consistency_summary.contains("issue_count: 2"));
        assert!(consistency_summary.contains("checks: cli_asm_reproducible, cli_obj_reproducible"));
        assert!(consistency_summary.contains("repeat_counts: 3"));
        assert!(consistency_summary.contains("unique_variants: 2, 3"));
        assert!(consistency_summary.contains("varying_components: text"));
        assert!(
            consistency_summary.contains("stable_components: load_commands, relocations, symbols")
        );
        assert!(bundle
            .join("consistency")
            .join("cli_asm_reproducible")
            .join("summary.txt")
            .exists());
        assert!(bundle
            .join("consistency")
            .join("cli_asm_reproducible")
            .join("detail.txt")
            .exists());
        assert!(bundle
            .join("consistency")
            .join("cli_asm_reproducible")
            .join("artifacts")
            .join("run_00.s")
            .exists());
        assert!(bundle
            .join("consistency")
            .join("cli_obj_reproducible")
            .join("artifacts")
            .join("run_00.o")
            .exists());

        let _ = fs::remove_dir_all(bundle);
        let _ =
            fs::remove_dir_all(std::env::temp_dir().join("afs_tests_consistency_bundle_issue_asm"));
        let _ =
            fs::remove_dir_all(std::env::temp_dir().join("afs_tests_consistency_bundle_issue_obj"));
        let _ = fs::remove_file(source);
    }

    #[test]
    fn file_snapshot_artifact_writer_rejects_parent_traversal() {
        let root = std::env::temp_dir().join(format!(
            "afs_tests_snapshot_path_{}",
            next_report_suffix(OptLevel::O0)
        ));
        let snapshot_root = root.join("snapshot");
        let escaped = root.join("escaped.bin");
        fs::create_dir_all(&root).unwrap();

        let error = write_file_snapshot(
            &snapshot_root,
            &BTreeMap::from([("../escaped.bin".to_string(), b"escape".to_vec())]),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!escaped.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn preserves_graph_translation_units_as_authored_inputs() {
        let root = std::env::temp_dir().join("afs_tests_graph_materialize");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let module = root.join("math_values.f90");
        let main = root.join("main.f90");
        fs::write(&module, "module math_values\ncontains\nend module\n").unwrap();
        fs::write(&main, "program main\nuse math_values\nend program\n").unwrap();

        let suite = SuiteSpec {
            name: "modules/graph".into(),
            path: root.join("graph.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "basic_use".into(),
            source: main.clone(),
            graph_files: vec![module.clone(), main.clone()],
            requested: BTreeSet::from([Stage::Run]),
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };

        let prepared = prepare_case_input(&case, &suite, OptLevel::O0).unwrap();
        assert_eq!(prepared.compiler_source, main);
        assert_eq!(prepared.graph_sources, vec![module, main]);
        assert!(prepared.temp_root.is_some());

        cleanup_prepared_input(&prepared);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn graph_capture_compiles_modules_separately_and_links_all_objects() {
        if armfortas::testing::native_e2e_level_support("-O0").is_err() {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "afs_tests_graph_multitu_{}",
            next_report_suffix(OptLevel::O0)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let module = root.join("answer_values.f90");
        let main = root.join("main.f90");
        fs::write(
            &module,
            "module answer_values\n  implicit none\ncontains\n  integer function answer() result(value)\n    value = 42\n  end function answer\nend module answer_values\n",
        )
        .unwrap();
        fs::write(
            &main,
            "program main\n  use answer_values, only : answer\n  implicit none\n  print *, answer()\nend program main\n",
        )
        .unwrap();

        let suite = SuiteSpec {
            name: "modules/separate-graph".into(),
            path: root.join("graph.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "module_use".into(),
            source: main.clone(),
            // Deliberately put the consumer first. The graph adapter must use
            // the real dependency scan, not declaration order or concatenation.
            graph_files: vec![main.clone(), module.clone()],
            requested: BTreeSet::from([Stage::Ast, Stage::Sema, Stage::Run]),
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };

        let prepared = prepare_case_input(&case, &suite, OptLevel::O0).unwrap();
        let result = capture_prepared_input(&prepared, &case.requested, OptLevel::O0).unwrap();
        let run = capture_run_stage(&result).unwrap();
        assert_eq!(run.exit_code, 0);
        let stdout = run.stdout_text().expect("graph stdout should be UTF-8");
        assert!(
            stdout.split_whitespace().any(|field| field == "42"),
            "graph binary did not execute the separately compiled module: {stdout:?}"
        );

        let ast = capture_text_stage(&result, Stage::Ast).unwrap();
        assert!(ast.contains(&format!("[0000] {}", main.display())));
        assert!(ast.contains(&format!("[0001] {}", module.display())));

        let work_root = prepared.temp_root.as_ref().unwrap();
        assert!(work_root.join("answer_values.amod").is_file());
        assert!(work_root.join("answer_values.mod").is_file());
        assert!(work_root.join("unit_0000.o").is_file());
        assert!(work_root.join("unit_0001.o").is_file());

        cleanup_prepared_input(&prepared);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn graph_cli_build_keeps_module_artifacts_out_of_the_process_cwd() {
        if armfortas::testing::native_e2e_level_support("-O0").is_err() {
            return;
        }
        let suffix = next_report_suffix(OptLevel::O0).replace('-', "_");
        let module_name = format!("bencch_hc004_{}", suffix);
        let root = std::env::temp_dir().join(format!("afs_tests_graph_cli_{}", suffix));
        let source_root = root.join("src");
        let output_root = root.join("out");
        let module = source_root.join("provider.f90");
        let main = source_root.join("main.f90");
        let binary = output_root.join("graph.out");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&output_root).unwrap();
        fs::write(
            &module,
            format!(
                "module {module_name}\n  implicit none\ncontains\n  integer function answer() result(value)\n    value = 42\n  end function answer\nend module {module_name}\n"
            ),
        )
        .unwrap();
        fs::write(
            &main,
            format!(
                "program main\n  use {module_name}, only : answer\n  implicit none\n  print *, answer()\nend program main\n"
            ),
        )
        .unwrap();

        let current_dir = std::env::current_dir().unwrap();
        let leaked_amod = current_dir.join(format!("{module_name}.amod"));
        let leaked_mod = current_dir.join(format!("{module_name}.mod"));
        assert!(!leaked_amod.exists());
        assert!(!leaked_mod.exists());

        compile_graph_output(&[main, module], OptLevel::O0, &binary, &output_root).unwrap();

        assert!(binary.is_file());
        assert!(output_root.join(format!("{module_name}.amod")).is_file());
        assert!(output_root.join(format!("{module_name}.mod")).is_file());
        assert!(!leaked_amod.exists());
        assert!(!leaked_mod.exists());

        let run = run_managed(&mut Command::new(&binary), CommandClass::Run).unwrap();
        assert!(run.status.success());
        assert!(String::from_utf8_lossy(&run.stdout)
            .split_whitespace()
            .any(|field| field == "42"));

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn reference_graph_compiles_each_source_then_links_distinct_objects() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "afs_tests_reference_graph_{}",
            next_report_suffix(OptLevel::O0)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let module = root.join("provider.f90");
        let main = root.join("main.f90");
        let log = root.join("commands.log");
        let fake_compiler = root.join("fake-gfortran");
        fs::write(&module, "module provider\nend module provider\n").unwrap();
        fs::write(&main, "program main\nuse provider\nend program main\n").unwrap();
        fs::write(
            &fake_compiler,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nout=''\ncompile=0\nwant_output=0\nfor arg in \"$@\"; do\n  if [ \"$want_output\" -eq 1 ]; then out=\"$arg\"; want_output=0; continue; fi\n  case \"$arg\" in\n    -c) compile=1 ;;\n    -o) want_output=1 ;;\n  esac\ndone\nif [ -z \"$out\" ]; then exit 64; fi\nif [ \"$compile\" -eq 1 ]; then\n  : > \"$out\"\nelse\n  printf '#!/bin/sh\\nprintf \"42\\\\n\"\\n' > \"$out\"\n  chmod +x \"$out\"\nfi\n",
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&fake_compiler).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake_compiler, permissions).unwrap();

        let mut tools = ToolchainConfig::from_env();
        tools.gfortran = fake_compiler.display().to_string();
        let result = run_reference_graph(
            &[module.clone(), main.clone()],
            OptLevel::O0,
            ReferenceCompiler::Gfortran,
            &tools,
        );
        assert_eq!(result.compile_exit_code, 0, "{}", result.compile_stderr);
        assert_eq!(result.run.unwrap().stdout, b"42\n");

        let commands = fs::read_to_string(&log).unwrap();
        let commands = commands.lines().collect::<Vec<_>>();
        assert_eq!(commands.len(), 3, "{commands:#?}");
        assert!(commands[0].contains(&module.display().to_string()));
        assert!(!commands[0].contains(&main.display().to_string()));
        assert!(commands[1].contains(&main.display().to_string()));
        assert!(!commands[1].contains(&module.display().to_string()));
        assert!(commands[2].contains("unit_0000.o"));
        assert!(commands[2].contains("unit_0001.o"));
        assert!(!commands[2].contains(".f90"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn graph_failure_bundle_writes_authored_sources() {
        let root = std::env::temp_dir().join("afs_tests_graph_bundle");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let module = root.join("math_values.f90");
        let main = root.join("main.f90");
        fs::write(
            &module,
            "module math_values\n integer :: answer = 42\nend module\n",
        )
        .unwrap();
        fs::write(
            &main,
            "program main\n use math_values\n print *, answer\nend program\n",
        )
        .unwrap();

        let suite = SuiteSpec {
            name: "modules/bundles".into(),
            path: root.join("bundle.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "graph_bundle".into(),
            source: main.clone(),
            graph_files: vec![module.clone(), main.clone()],
            requested: BTreeSet::from([Stage::Run]),
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };
        let outcome = Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level: OptLevel::O0,
            kind: OutcomeKind::Fail,
            detail: "boom".into(),
            bundle: None,
            consistency_observations: Vec::new(),
        };
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Run]),
            armfortas: Some(run_only_result("42\n", "", 0)),
            armfortas_failure: None,
            references: Vec::new(),
            consistency_issues: Vec::new(),
        };
        let prepared = PreparedInput {
            compiler_source: main.clone(),
            graph_sources: vec![module.clone(), main.clone()],
            temp_root: None,
        };

        let bundle = write_failure_bundle(&suite, &case, &prepared, &outcome, &artifacts).unwrap();
        assert!(bundle.join("source.f90").exists());
        assert_eq!(
            fs::read_to_string(bundle.join("source.f90")).unwrap(),
            fs::read_to_string(&main).unwrap()
        );
        assert!(bundle.join("sources").join("00_math_values.f90").exists());
        assert!(bundle.join("sources").join("01_main.f90").exists());

        let _ = fs::remove_dir_all(bundle);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn render_summary_includes_consistency_rollups() {
        let mut summary = Summary::default();
        summary.record_consistency(&[
            ConsistencyObservation {
                check: ConsistencyCheck::CliAsmReproducible,
                summary: "repeat_count=3 unique_variants=3".into(),
                repeat_count: Some(3),
                unique_variant_count: Some(3),
                varying_components: Vec::new(),
                stable_components: Vec::new(),
            },
            ConsistencyObservation {
                check: ConsistencyCheck::CliObjReproducible,
                summary:
                    "repeat_count=3 unique_variants=2 varying_components=text stable_components=load_commands, relocations, symbols"
                        .into(),
                repeat_count: Some(3),
                unique_variant_count: Some(2),
                varying_components: vec!["text".into()],
                stable_components: vec![
                    "load_commands".into(),
                    "relocations".into(),
                    "symbols".into(),
                ],
            },
        ]);

        let rendered = render_summary(&summary);
        assert!(rendered.contains("Consistency"));
        assert!(rendered.contains("affected_checks: 2"));
        assert!(rendered.contains("cells_with_issues: 2"));
        assert!(
            rendered.contains("cli_asm_reproducible: 1 cells; repeat_count=3; unique_variants=3")
        );
        assert!(rendered.contains(
            "cli_obj_reproducible: 1 cells; repeat_count=3; unique_variants=2; varying=text; stable=load_commands, relocations, symbols"
        ));
    }

    #[test]
    fn differential_reports_armfortas_only_divergence() {
        let result = run_only_result("0\n", "", 0);
        let refs = vec![
            reference_run(ReferenceCompiler::Gfortran, "42\n", "", 0),
            reference_run(ReferenceCompiler::FlangNew, "42\n", "", 0),
        ];

        let err = compare_differential(&result, &refs).unwrap_err();
        assert!(err.contains("classification: armfortas-only divergence"));
    }

    #[test]
    fn differential_reports_reference_disagreement() {
        let result = run_only_result("42\n", "", 0);
        let refs = vec![
            reference_run(ReferenceCompiler::Gfortran, "42\n", "", 0),
            reference_run(ReferenceCompiler::FlangNew, "99\n", "", 0),
        ];

        let err = compare_differential(&result, &refs).unwrap_err();
        assert!(err.contains("classification: reference disagreement"));
    }

    #[test]
    fn differential_tolerates_numeric_formatting_differences() {
        let result = run_only_result("     5.5000000E0\n", "", 0);
        let refs = vec![
            reference_run(ReferenceCompiler::Gfortran, "   5.50000000\n", "", 0),
            reference_run(ReferenceCompiler::FlangNew, " 5.5\n", "", 0),
        ];

        assert!(compare_differential(&result, &refs).is_ok());
    }

    #[test]
    fn differential_rejects_filesystem_side_effect_mismatch() {
        let mut result = run_only_result("same output\n", "", 0);
        let arm_files =
            BTreeMap::from([("artifact.txt".to_string(), b"armfortas bytes\n".to_vec())]);
        result
            .stages
            .get_mut(&Stage::Run)
            .and_then(|stage| match stage {
                CapturedStage::Run(run) => Some(run),
                CapturedStage::Text(_) => None,
            })
            .unwrap()
            .files = arm_files.clone();

        let mut references = vec![reference_run(
            ReferenceCompiler::Gfortran,
            "same output\n",
            "",
            0,
        )];
        references[0].run.as_mut().unwrap().files =
            BTreeMap::from([("artifact.txt".to_string(), b"gfortran bytes\n".to_vec())]);

        let error = compare_differential(&result, &references).unwrap_err();
        assert!(error.contains("files"), "{error}");
        assert!(error.contains("artifact.txt"), "{error}");

        references[0].run.as_mut().unwrap().files = arm_files;
        assert!(compare_differential(&result, &references).is_ok());

        references[0].run.as_mut().unwrap().files.clear();
        let missing_error = compare_differential(&result, &references).unwrap_err();
        assert!(missing_error.contains("artifact.txt"), "{missing_error}");
    }

    #[test]
    fn consistency_diff_reports_first_mismatch() {
        let detail = describe_text_difference("alpha\nbeta\n", "alpha\ngamma\n", "left", "right");
        assert!(detail.contains("first differing line: 2"));
        assert!(detail.contains("left: beta"));
        assert!(detail.contains("right: gamma"));
    }

    #[test]
    fn object_diff_reports_changed_components() {
        let expected = ObjectSnapshot {
            text: "alpha\nbeta\n".into(),
            load_commands: "same".into(),
            relocations: "same".into(),
            symbols: "same".into(),
        };
        let actual = ObjectSnapshot {
            text: "alpha\ngamma\n".into(),
            load_commands: "same".into(),
            relocations: "same".into(),
            symbols: "same".into(),
        };

        let detail = describe_object_difference(&expected, &actual, "first -c", "second -c");
        assert!(detail.contains("differing object components: text"));
        assert!(detail.contains("first differing component: text"));
        assert!(detail.contains("first -c: beta"));
        assert!(detail.contains("second -c: gamma"));
    }

    #[test]
    fn object_component_variation_classifies_text_only_instability() {
        let first = ObjectSnapshot {
            text: "alpha".into(),
            load_commands: "load".into(),
            relocations: "reloc".into(),
            symbols: "symbols".into(),
        };
        let second = ObjectSnapshot {
            text: "beta".into(),
            load_commands: "load".into(),
            relocations: "reloc".into(),
            symbols: "symbols".into(),
        };
        let snapshots = vec![&first, &second];

        assert_eq!(varying_object_components(&snapshots), vec!["text"]);
        assert_eq!(
            stable_object_components(&snapshots),
            vec!["load_commands", "relocations", "symbols"]
        );
    }

    #[test]
    fn run_diff_reports_changed_components() {
        let left = RunCapture {
            exit_code: 0,
            stdout: b"alpha\nbeta\n".to_vec(),
            stderr: Vec::new(),
            files: BTreeMap::new(),
        };
        let right = RunCapture {
            exit_code: 0,
            stdout: b"alpha\ngamma\n".to_vec(),
            stderr: Vec::new(),
            files: BTreeMap::new(),
        };

        let detail = describe_run_difference(&left, &right, "capture run", "cli run 2");
        assert!(detail.contains("differing runtime components: stdout"));
        assert!(detail.contains("first differing component: stdout"));
        assert!(detail.contains("capture run: beta"));
        assert!(detail.contains("cli run 2: gamma"));
    }

    #[test]
    fn run_diff_reports_exact_file_content_and_presence_changes() {
        let left = RunCapture {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            files: BTreeMap::from([("artifact.bin".to_string(), vec![0x00, 0xff])]),
        };
        let right = RunCapture {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            files: BTreeMap::from([("artifact.bin".to_string(), vec![0x00, 0xfe])]),
        };

        let detail = describe_run_difference(&left, &right, "capture run", "cli run");
        assert!(
            detail.contains("first differing component: files"),
            "{detail}"
        );
        assert!(detail.contains("file 'artifact.bin' differs"), "{detail}");
        assert!(
            detail.contains("first differing byte offset: 1"),
            "{detail}"
        );
        assert!(detail.contains("capture run: 0xff"), "{detail}");
        assert!(detail.contains("cli run: 0xfe"), "{detail}");

        let mut missing = right;
        missing.files.clear();
        let presence = describe_run_difference(&left, &missing, "capture run", "cli run");
        assert!(
            presence.contains("file 'artifact.bin' is present only in capture run"),
            "{presence}"
        );
    }

    #[test]
    fn file_snapshot_preview_is_bounded_without_splitting_utf8() {
        let mut bytes = vec![b'a'; 255];
        bytes.extend_from_slice("é".as_bytes());
        bytes.extend_from_slice(b"tail");

        let preview = format_file_preview(&bytes);
        assert!(!preview.contains("<non-UTF-8 output"), "{preview}");
        assert!(preview.contains("<6 more bytes>"), "{preview}");
        assert!(!preview.contains("tail"), "{preview}");
    }

    #[cfg(unix)]
    #[test]
    fn run_capture_distinguishes_invalid_utf8_bytes() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "afs_tests_invalid_utf8_{}",
            next_report_suffix(OptLevel::O0)
        ));
        fs::create_dir_all(&root).unwrap();
        let ff = root.join("emit_ff");
        let fe = root.join("emit_fe");
        fs::write(&ff, "#!/bin/sh\nprintf '\\377'\n").unwrap();
        fs::write(&fe, "#!/bin/sh\nprintf '\\376'\n").unwrap();
        fs::set_permissions(&ff, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&fe, fs::Permissions::from_mode(0o700)).unwrap();

        let ff_run = run_binary_capture(&ff, &root.join("ff-run"), "emit ff").unwrap();
        let fe_run = run_binary_capture(&fe, &root.join("fe-run"), "emit fe").unwrap();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(ff_run.exit_code, 0);
        assert_eq!(fe_run.exit_code, 0);
        assert_eq!(ff_run.stdout, vec![0xff]);
        assert_eq!(fe_run.stdout, vec![0xfe]);
        assert_ne!(
            normalize_run_signature(&ff_run),
            normalize_run_signature(&fe_run),
            "distinct invalid UTF-8 byte streams must not compare equal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_capture_snapshots_nested_binary_side_effects() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "afs_tests_file_snapshot_{}",
            next_report_suffix(OptLevel::O0)
        ));
        fs::create_dir_all(&root).unwrap();
        let writer = root.join("write_file");
        let staged_writer = root.join("write_file.staged");
        fs::write(
            &staged_writer,
            "#!/bin/sh\nmkdir nested\nprintf '\\000\\377\\177' > nested/artifact.bin\n",
        )
        .unwrap();
        fs::set_permissions(&staged_writer, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(staged_writer, &writer).unwrap();

        let run =
            run_binary_capture(&writer, &root.join("run-sandbox"), "write binary file").unwrap();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(run.exit_code, 0);
        assert_eq!(
            run.files,
            BTreeMap::from([("nested/artifact.bin".to_string(), vec![0x00, 0xff, 0x7f])])
        );
    }

    #[test]
    fn invalid_utf8_run_output_fails_text_checks_and_renders_exact_bytes() {
        let result = CaptureResult {
            input: PathBuf::from("invalid-output.f90"),
            opt_level: OptLevel::O0,
            stages: BTreeMap::from([(
                Stage::Run,
                CapturedStage::Run(RunCapture {
                    exit_code: 0,
                    stdout: vec![0xff],
                    stderr: Vec::new(),
                    files: BTreeMap::new(),
                }),
            )]),
        };

        let error = target_text(&result, &Target::RunStdout).unwrap_err();
        assert!(error.contains("run.stdout is not valid UTF-8"), "{error}");
        assert!(error.contains("offset 0"), "{error}");

        let left = result
            .get(Stage::Run)
            .and_then(CapturedStage::as_run)
            .unwrap();
        let right = RunCapture {
            exit_code: 0,
            stdout: vec![0xfe],
            stderr: Vec::new(),
            files: BTreeMap::new(),
        };
        assert!(format_run_capture(left).contains("\\xff"));
        let detail = describe_run_difference(left, &right, "left", "right");
        assert!(
            detail.contains("first differing byte offset: 0"),
            "{detail}"
        );
        assert!(detail.contains("left: 0xff"), "{detail}");
        assert!(detail.contains("right: 0xfe"), "{detail}");
        assert!(!detail.contains('\u{fffd}'), "{detail}");
    }

    #[test]
    fn run_component_variation_classifies_stdout_only_instability() {
        let first = RunSignature {
            exit_code: 0,
            stdout: b"alpha".to_vec(),
            stderr: Vec::new(),
            files: BTreeMap::new(),
        };
        let second = RunSignature {
            exit_code: 0,
            stdout: b"beta".to_vec(),
            stderr: Vec::new(),
            files: BTreeMap::new(),
        };
        let signatures = vec![&first, &second];

        assert_eq!(varying_run_components(&signatures), vec!["stdout"]);
        assert_eq!(
            stable_run_components(&signatures),
            vec!["exit_code", "stderr", "files"]
        );
    }

    #[test]
    fn run_component_variation_classifies_file_only_instability() {
        let first = RunSignature {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            files: BTreeMap::from([("artifact.bin".to_string(), vec![0x00])]),
        };
        let second = RunSignature {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            files: BTreeMap::from([("artifact.bin".to_string(), vec![0x01])]),
        };
        let signatures = vec![&first, &second];

        assert_eq!(varying_run_components(&signatures), vec!["files"]);
        assert_eq!(
            stable_run_components(&signatures),
            vec!["exit_code", "stdout", "stderr"]
        );
    }

    #[test]
    fn parse_object_snapshot_text_round_trips_rendered_snapshot() {
        let snapshot = ObjectSnapshot {
            text: "text bytes".into(),
            load_commands: "load commands".into(),
            relocations: "relocations".into(),
            symbols: "symbols".into(),
        };

        let rendered = render_object_snapshot(&snapshot);
        let parsed = parse_object_snapshot_text(&rendered).unwrap();
        assert_eq!(parsed, snapshot);
    }

    #[test]
    fn verifier_regression_detects_integer_op_on_float_values() {
        let mut module = Module::new("verify".into(), armfortas::target::TargetLayout::LP64);
        let mut func = Function::new("broken".into(), vec![], IrType::Void);
        func.blocks[0].insts.push(Inst {
            id: ValueId(0),
            kind: InstKind::ConstFloat(1.0, FloatWidth::F32),
            ty: IrType::Float(FloatWidth::F32),
            span: dummy_span(),
        });
        func.blocks[0].insts.push(Inst {
            id: ValueId(1),
            kind: InstKind::ConstFloat(2.0, FloatWidth::F32),
            ty: IrType::Float(FloatWidth::F32),
            span: dummy_span(),
        });
        func.blocks[0].insts.push(Inst {
            id: ValueId(2),
            kind: InstKind::IAdd(ValueId(0), ValueId(1)),
            ty: IrType::Int(IntWidth::I32),
            span: dummy_span(),
        });
        func.blocks[0].terminator = Some(Terminator::Return(None));
        module.add_function(func);

        let errors = verify_module(&module);
        assert!(
            errors
                .iter()
                .any(|error| error.msg.contains("non-integer operand")),
            "expected verifier error, got: {:?}",
            errors
        );
    }

    #[test]
    fn verifier_regression_detects_branch_argument_mismatch() {
        let mut module = Module::new("verify".into(), armfortas::target::TargetLayout::LP64);
        let mut func = Function::new("broken_branch".into(), vec![], IrType::Void);
        let target = func.create_block("target");
        func.block_mut(target).params.push(BlockParam {
            id: ValueId(0),
            ty: IrType::Int(IntWidth::I32),
        });
        func.blocks[0].terminator = Some(Terminator::Branch(target, vec![]));
        func.block_mut(target).terminator = Some(Terminator::Return(None));
        module.add_function(func);

        let errors = verify_module(&module);
        assert!(
            errors
                .iter()
                .any(|error| error.msg.contains("expected 1 args, got 0")),
            "expected verifier error, got: {:?}",
            errors
        );
    }

    #[test]
    fn verifier_regression_detects_store_to_non_pointer() {
        let mut module = Module::new("verify".into(), armfortas::target::TargetLayout::LP64);
        let mut func = Function::new("broken_store".into(), vec![], IrType::Void);
        func.blocks[0].insts.push(Inst {
            id: ValueId(0),
            kind: InstKind::ConstInt(42, IntWidth::I32),
            ty: IrType::Int(IntWidth::I32),
            span: dummy_span(),
        });
        func.blocks[0].insts.push(Inst {
            id: ValueId(1),
            kind: InstKind::ConstInt(0, IntWidth::I32),
            ty: IrType::Int(IntWidth::I32),
            span: dummy_span(),
        });
        func.blocks[0].insts.push(Inst {
            id: ValueId(2),
            kind: InstKind::Store(ValueId(0), ValueId(1)),
            ty: IrType::Void,
            span: dummy_span(),
        });
        func.blocks[0].terminator = Some(Terminator::Return(None));
        module.add_function(func);

        let errors = verify_module(&module);
        assert!(
            errors.iter().any(|error| error.msg.contains("non-pointer")),
            "expected verifier error, got: {:?}",
            errors
        );
    }
}

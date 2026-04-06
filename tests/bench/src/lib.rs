use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use armfortas::driver::OptLevel;
use armfortas::testing::{
    capture_from_path, CaptureRequest, CaptureResult, CapturedStage, RunCapture, Stage,
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
    requested: BTreeSet<Stage>,
    opt_levels: Vec<OptLevel>,
    reference_compilers: Vec<ReferenceCompiler>,
    expectations: Vec<Expectation>,
    status_rules: Vec<StatusRule>,
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

#[derive(Debug, Clone)]
enum Expectation {
    CheckComments(Target),
    Contains { target: Target, needle: String },
    Equals { target: Target, value: String },
    IntEquals { target: Target, value: i32 },
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
}

#[derive(Debug, Default)]
struct Summary {
    passed: usize,
    failed: usize,
    xfailed: usize,
    xpassed: usize,
    future: usize,
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
}

#[derive(Debug, Clone)]
struct ExecutionArtifacts {
    requested: BTreeSet<Stage>,
    armfortas: Option<CaptureResult>,
    armfortas_error: Option<String>,
    references: Vec<ReferenceResult>,
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
    stdout: String,
    stderr: String,
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
    Run(RunConfig),
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
                    "--help" | "-h" => return Ok(CommandKind::Help),
                    other => return Err(format!("unknown run option: {}", other)),
                }
            }
            Ok(CommandKind::Run(config))
        }
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
        "  cargo run -p afs-tests -- run [--suite <filter>] [--case <filter>] [--opt <O0,O1,...>] [--verbose] [--fail-fast] [--include-future] [--all]"
    );
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
            let relative = parse_quoted(rest, path, line_no)?;
            let source = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(relative);
            builder.source = Some(source);
        } else if let Some(rest) = line.strip_prefix("armfortas =>") {
            builder.requested = parse_stage_list(rest, path, line_no)?;
        } else if let Some(rest) = line.strip_prefix("opts =>") {
            builder.opt_levels = parse_opt_levels(rest, path, line_no)?;
        } else if let Some(rest) = line.strip_prefix("differential =>") {
            builder.reference_compilers = parse_reference_compilers(rest, path, line_no)?;
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
    requested: BTreeSet<Stage>,
    opt_levels: Vec<OptLevel>,
    reference_compilers: Vec<ReferenceCompiler>,
    expectations: Vec<Expectation>,
    status_rules: Vec<StatusRule>,
}

impl CaseBuilder {
    fn new(name: String) -> Self {
        Self {
            name,
            source: None,
            requested: BTreeSet::new(),
            opt_levels: Vec::new(),
            reference_compilers: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        }
    }

    fn build(self, suite_path: &Path) -> Result<CaseSpec, String> {
        let source = self.source.ok_or_else(|| {
            format!(
                "{}: case '{}' is missing a source path",
                suite_path.display(),
                self.name
            )
        })?;

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
            requested,
            opt_levels,
            reference_compilers: self.reference_compilers,
            expectations: self.expectations,
            status_rules: self.status_rules,
        })
    }
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

fn parse_expectation(rest: &str, path: &Path, line_no: usize) -> Result<Expectation, String> {
    if let Some(prefix) = rest.strip_suffix(" check-comments") {
        return Ok(Expectation::CheckComments(parse_target(
            prefix.trim(),
            path,
            line_no,
        )?));
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

fn all_opt_levels() -> [OptLevel; 5] {
    [
        OptLevel::O0,
        OptLevel::O1,
        OptLevel::O2,
        OptLevel::O3,
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
                match outcome.kind {
                    OutcomeKind::Pass => summary.passed += 1,
                    OutcomeKind::Fail => summary.failed += 1,
                    OutcomeKind::Xfail => summary.xfailed += 1,
                    OutcomeKind::Xpass => summary.xpassed += 1,
                    OutcomeKind::Future => summary.future += 1,
                }

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
        println!("  source: {}", case.source.display());
        println!("  opt: {}", opt_level.as_str());
        println!("  stages: {}", stage_list);
        println!("  refs: {}", refs);
    }

    let request = CaptureRequest {
        input: case.source.clone(),
        requested: requested.clone(),
        opt_level,
    };

    let references = run_reference_compilers(case, opt_level);
    let mut artifacts = ExecutionArtifacts {
        requested,
        armfortas: None,
        armfortas_error: None,
        references,
    };

    match capture_from_path(&request) {
        Ok(result) => artifacts.armfortas = Some(result),
        Err(err) => artifacts.armfortas_error = Some(err),
    }

    let execution = match &artifacts.armfortas {
        Some(result) => {
            let mut execution = evaluate_expectations(case, result);
            if execution.is_ok() && !artifacts.references.is_empty() {
                execution = compare_differential(result, &artifacts.references);
            }
            execution
        }
        None => Err(compose_armfortas_failure_detail(&artifacts)),
    };

    let mut outcome = match (effective_status, execution) {
        (EffectiveStatus::Normal, Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Pass,
            detail: String::new(),
            bundle: None,
        },
        (EffectiveStatus::Normal, Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Fail,
            detail,
            bundle: None,
        },
        (EffectiveStatus::Xfail(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xpass,
            detail: reason,
            bundle: None,
        },
        (EffectiveStatus::Xfail(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xfail,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
        },
        (EffectiveStatus::Future(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Pass,
            detail: reason,
            bundle: None,
        },
        (EffectiveStatus::Future(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Fail,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
        },
    };

    if matches!(outcome.kind, OutcomeKind::Fail | OutcomeKind::Xpass) {
        match write_failure_bundle(suite, case, &outcome, &artifacts) {
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

    Ok(outcome)
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
        | Expectation::Equals { target, .. }
        | Expectation::IntEquals { target, .. } => match target {
            Target::Stage(stage) => {
                requested.insert(*stage);
            }
            Target::RunStdout | Target::RunStderr | Target::RunExitCode => {
                requested.insert(Stage::Run);
            }
        },
    }
}

fn evaluate_expectations(case: &CaseSpec, result: &CaptureResult) -> Result<(), String> {
    for expectation in &case.expectations {
        match expectation {
            Expectation::CheckComments(target) => {
                let text = target_text(result, target)?;
                let source = fs::read_to_string(&case.source)
                    .map_err(|e| format!("cannot read '{}': {}", case.source.display(), e))?;
                let checks = extract_checks(&source);
                if checks.is_empty() {
                    return Err(format!(
                        "case '{}' requested check-comments but '{}' has no ! CHECK: lines",
                        case.name,
                        case.source.display()
                    ));
                }
                match_checks(&checks, text, &case.name)?;
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
        }
    }
    Ok(())
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
            Some(run) => Ok(&run.stdout),
            None => Err("missing captured run stage".into()),
        },
        Target::RunStderr => match result.get(Stage::Run).and_then(CapturedStage::as_run) {
            Some(run) => Ok(&run.stderr),
            None => Err("missing captured run stage".into()),
        },
        Target::RunExitCode => {
            Err("run.exit_code is numeric; use 'expect run.exit_code equals <int>'".into())
        }
    }
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
    if let Some(err) = &artifacts.armfortas_error {
        detail.push_str(err);
    } else {
        detail.push_str("armfortas failed without an error message");
    }

    if !artifacts.references.is_empty() {
        detail.push_str("\n\nreference compilers\n");
        detail.push_str(&format_reference_summary(&artifacts.references));
    }

    detail
}

fn run_reference_compilers(case: &CaseSpec, opt_level: OptLevel) -> Vec<ReferenceResult> {
    case.reference_compilers
        .iter()
        .copied()
        .map(|compiler| run_reference_case(&case.source, opt_level, compiler))
        .collect()
}

fn run_reference_case(
    source: &Path,
    opt_level: OptLevel,
    compiler: ReferenceCompiler,
) -> ReferenceResult {
    let temp_root = default_report_root().join(".tmp").join(format!(
        "{}_{}_{}",
        sanitize_component(compiler.as_str()),
        opt_level.as_str().to_ascii_lowercase(),
        REPORT_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let binary = temp_root.join("reference.out");
    let uses_cpp = source_uses_cpp(source);

    let mut args = vec![opt_level.as_flag().to_string()];
    if uses_cpp {
        args.push("-cpp".to_string());
    }
    args.push(source.display().to_string());
    args.push("-o".to_string());
    args.push(binary.display().to_string());

    let command_string = render_command(compiler.binary_name(), &args);

    if let Err(err) = fs::create_dir_all(&temp_root) {
        return ReferenceResult::infrastructure_error(
            compiler,
            command_string,
            format!("cannot create temp dir '{}': {}", temp_root.display(), err),
        );
    }

    let compile = match Command::new(compiler.binary_name())
        .current_dir(&temp_root)
        .args(&args)
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            return ReferenceResult::infrastructure_error(
                compiler,
                command_string,
                format!("cannot run {}: {}", compiler.binary_name(), err),
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
        match Command::new(&binary).current_dir(&temp_root).output() {
            Ok(output) => {
                result.run = Some(RunCapture {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            Err(err) => {
                result.run_error = Some(format!("cannot run '{}': {}", binary.display(), err));
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

fn normalize_run_signature(run: &RunCapture) -> RunSignature {
    RunSignature {
        exit_code: run.exit_code,
        stdout: normalize_behavior_text(&run.stdout),
        stderr: normalize_behavior_text(&run.stderr),
    }
}

fn normalize_behavior_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
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
    let stdout = if run.stdout.is_empty() {
        "<empty>".to_string()
    } else {
        run.stdout.trim_end().to_string()
    };
    let stderr = if run.stderr.is_empty() {
        "<empty>".to_string()
    } else {
        run.stderr.trim_end().to_string()
    };
    format!(
        "exit: {}\nstdout:\n{}\nstderr:\n{}",
        run.exit_code, stdout, stderr
    )
}

fn write_failure_bundle(
    suite: &SuiteSpec,
    case: &CaseSpec,
    outcome: &Outcome,
    artifacts: &ExecutionArtifacts,
) -> Result<PathBuf, String> {
    let bundle_root = default_report_root()
        .join(sanitize_component(&suite.name))
        .join(sanitize_component(&case.name))
        .join(format!(
            "{}-{:04}",
            outcome.opt_level.as_str().to_ascii_lowercase(),
            REPORT_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
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
    let metadata = format!(
        "suite: {}\ncase: {}\noutcome: {:?}\nopt: {}\nsource: {}\nrequested_stages: {}\nreference_compilers: {}\n",
        suite.name,
        case.name,
        outcome.kind,
        outcome.opt_level.as_str(),
        case.source.display(),
        stage_list,
        refs
    );
    fs::write(bundle_root.join("metadata.txt"), metadata)
        .map_err(|e| format!("cannot write bundle metadata: {}", e))?;
    fs::write(bundle_root.join("detail.txt"), &outcome.detail)
        .map_err(|e| format!("cannot write bundle detail: {}", e))?;

    let source_text = fs::read_to_string(&case.source)
        .map_err(|e| format!("cannot read case source '{}': {}", case.source.display(), e))?;
    fs::write(bundle_root.join("source.f90"), source_text)
        .map_err(|e| format!("cannot write bundle source copy: {}", e))?;

    let armfortas_root = bundle_root.join("armfortas");
    fs::create_dir_all(&armfortas_root)
        .map_err(|e| format!("cannot create armfortas bundle dir: {}", e))?;
    if let Some(result) = &artifacts.armfortas {
        write_capture_result(&armfortas_root, result)?;
    }
    if let Some(err) = &artifacts.armfortas_error {
        fs::write(armfortas_root.join("error.txt"), err)
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

    Ok(bundle_root)
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
    }
    if let Some(err) = &reference.run_error {
        fs::write(ref_root.join("run.error.txt"), err)
            .map_err(|e| format!("cannot write reference run error bundle: {}", e))?;
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
    println!("Summary");
    println!("  passed: {}", summary.passed);
    println!("  failed: {}", summary.failed);
    println!("  xfailed: {}", summary.xfailed);
    println!("  xpassed: {}", summary.xpassed);
    println!("  future: {}", summary.future);
}

#[derive(Debug, Clone)]
struct Check {
    line_num: usize,
    pattern: String,
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

#[cfg(test)]
mod tests {
    use super::*;

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
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        assert_eq!(suite.name, "runtime/smoke");
        assert_eq!(suite.cases.len(), 1);
        assert!(suite.cases[0].requested.contains(&Stage::Run));
        assert!(suite.cases[0].requested.contains(&Stage::Ir));
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
            requested: BTreeSet::from([Stage::Ir, Stage::Run]),
            opt_levels: vec![OptLevel::O0],
            reference_compilers: vec![ReferenceCompiler::Gfortran],
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };
        let mut stages = std::collections::BTreeMap::new();
        stages.insert(Stage::Ir, CapturedStage::Text("module main".into()));
        stages.insert(
            Stage::Run,
            CapturedStage::Run(RunCapture {
                exit_code: 1,
                stdout: "oops\n".into(),
                stderr: "broken\n".into(),
            }),
        );
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Ir, Stage::Run]),
            armfortas: Some(CaptureResult {
                input: source.clone(),
                opt_level: OptLevel::O0,
                stages,
            }),
            armfortas_error: Some("compiler failed".into()),
            references: vec![ReferenceResult {
                compiler: ReferenceCompiler::Gfortran,
                compile_command: "gfortran hello.f90 -o hello".into(),
                compile_exit_code: 0,
                compile_stdout: String::new(),
                compile_stderr: String::new(),
                run: Some(RunCapture {
                    exit_code: 0,
                    stdout: "hello\n".into(),
                    stderr: String::new(),
                }),
                run_error: None,
            }],
        };
        let outcome = Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level: OptLevel::O0,
            kind: OutcomeKind::Fail,
            detail: "boom".into(),
            bundle: None,
        };

        let bundle = write_failure_bundle(&suite, &case, &outcome, &artifacts).unwrap();
        assert!(bundle.join("metadata.txt").exists());
        assert!(bundle.join("detail.txt").exists());
        assert!(bundle.join("source.f90").exists());
        assert!(bundle.join("armfortas").join("ir.txt").exists());
        assert!(bundle.join("armfortas").join("run.stdout.txt").exists());
        assert!(bundle
            .join("references")
            .join("gfortran")
            .join("run.stdout.txt")
            .exists());

        let _ = fs::remove_dir_all(bundle);
        let _ = fs::remove_file(source);
    }
}

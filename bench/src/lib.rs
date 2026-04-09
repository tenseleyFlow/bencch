mod compiler;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::compiler::{
    linked_capture_available, object_snapshot_text, ArmfortasAdapters, ArmfortasCliAdapter,
    CaptureBackend, CaptureFailure, CaptureRequest, CaptureResult, CapturedStage,
    CliObservableCaptureBackend, EmitMode, FailureStage, OptLevel, RunCapture, Stage,
};
use bencch_core::{
    ArtifactDifference, ArtifactKey, ArtifactValue, ComparisonResult, CompilerCapabilities,
    CompilerObservation, CompilerSpec, NamedCompiler, ObservationProvenance,
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
    generic_introspect: Option<GenericIntrospectCase>,
    generic_compare: Option<GenericCompareCase>,
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

    fn is_generic_introspect(&self) -> bool {
        self.generic_introspect.is_some()
    }

    fn is_generic_compare(&self) -> bool {
        self.generic_compare.is_some()
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
struct GenericIntrospectCase {
    compiler: CompilerSpec,
    artifacts: BTreeSet<ArtifactKey>,
}

#[derive(Debug, Clone)]
struct GenericCompareCase {
    left: CompilerSpec,
    right: CompilerSpec,
    artifacts: BTreeSet<ArtifactKey>,
}

#[derive(Debug, Clone)]
struct PreparedInput {
    compiler_source: PathBuf,
    generated_source: Option<PathBuf>,
    temp_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct StatusRule {
    kind: StatusKind,
    selector: OptSelector,
    reason: String,
}

#[derive(Debug, Clone)]
enum PendingStatusRule {
    Explicit(StatusRule),
    XfailSourceComments,
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

    fn requires_capture_result(&self) -> bool {
        matches!(
            self,
            Self::CaptureAsmVsCliAsm
                | Self::CaptureObjVsCliObj
                | Self::CaptureRunVsCliRun
                | Self::CaptureAsmReproducible
                | Self::CaptureObjReproducible
                | Self::CaptureRunReproducible
        )
    }

    fn supports_generic_introspect(&self) -> bool {
        matches!(
            self,
            Self::CliAsmReproducible | Self::CliObjReproducible | Self::CliRunReproducible
        )
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
    FailSourceComments,
    FailCommentPatterns(Vec<String>),
}

#[derive(Debug, Clone)]
enum Target {
    Stage(Stage),
    Artifact(ArtifactKey),
    CompareStatus,
    CompareClassification,
    CompareChangedArtifacts,
    CompareDifferenceCount,
    CompareBasis,
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
struct ToolchainConfig {
    armfortas: ArmfortasCliAdapter,
    gfortran: String,
    flang_new: String,
    system_as: String,
    otool: String,
    nm: String,
}

impl ToolchainConfig {
    fn from_env() -> Self {
        Self {
            armfortas: match std::env::var("BENCCH_ARMFORTAS_BIN") {
                Ok(value) if !value.trim().is_empty() => ArmfortasCliAdapter::External(value),
                _ if linked_capture_available() => ArmfortasCliAdapter::Linked,
                _ => ArmfortasCliAdapter::External("armfortas".into()),
            },
            gfortran: tool_override("BENCCH_GFORTRAN_BIN", "gfortran"),
            flang_new: tool_override("BENCCH_FLANG_BIN", "flang-new"),
            system_as: tool_override("BENCCH_AS_BIN", "as"),
            otool: tool_override("BENCCH_OTOOL_BIN", "otool"),
            nm: tool_override("BENCCH_NM_BIN", "nm"),
        }
    }

    fn armfortas_adapters(&self) -> ArmfortasAdapters {
        ArmfortasAdapters::new(self.armfortas.clone())
    }

    fn cli_observable_capture_backend(&self, work_root: PathBuf) -> CliObservableCaptureBackend {
        CliObservableCaptureBackend::new(
            self.armfortas.clone(),
            work_root,
            self.otool.clone(),
            self.nm.clone(),
        )
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

    fn otool_bin(&self) -> &str {
        &self.otool
    }

    fn nm_bin(&self) -> &str {
        &self.nm
    }

    fn named_compiler_binary(&self, compiler: NamedCompiler) -> Option<String> {
        match compiler {
            NamedCompiler::Armfortas => match &self.armfortas {
                ArmfortasCliAdapter::Linked => None,
                ArmfortasCliAdapter::External(binary) => Some(binary.clone()),
            },
            NamedCompiler::Gfortran => Some(self.gfortran.clone()),
            NamedCompiler::FlangNew => Some(self.flang_new.clone()),
        }
    }
}

fn generic_external_capabilities(spec: CompilerSpec) -> CompilerCapabilities {
    CompilerCapabilities::new(spec).support_all([
        ArtifactKey::Diagnostics,
        ArtifactKey::ExitCode,
        ArtifactKey::Stdout,
        ArtifactKey::Stderr,
        ArtifactKey::Asm,
        ArtifactKey::Obj,
        ArtifactKey::Executable,
        ArtifactKey::Runtime,
    ])
}

fn armfortas_capabilities(tools: &ToolchainConfig) -> CompilerCapabilities {
    let mut capabilities =
        generic_external_capabilities(CompilerSpec::Named(NamedCompiler::Armfortas));
    let linked_reason = "linked armfortas capture is unavailable in this build; use scripts/bootstrap-linked-armfortas.sh or request only asm/obj/run from an external armfortas binary".to_string();
    let capture_available = tools.armfortas_adapters().capture_mode_name() != "unavailable";
    for stage in Stage::ALL {
        if matches!(stage, Stage::Asm | Stage::Obj | Stage::Run) {
            continue;
        }
        let artifact = ArtifactKey::Extra(format!("armfortas.{}", stage.as_str()));
        capabilities = if capture_available {
            capabilities.support(artifact)
        } else {
            capabilities.mark_unavailable(artifact, linked_reason.clone())
        };
    }
    capabilities
}

fn compiler_capabilities(spec: &CompilerSpec, tools: &ToolchainConfig) -> CompilerCapabilities {
    match spec {
        CompilerSpec::Named(NamedCompiler::Armfortas) => armfortas_capabilities(tools),
        CompilerSpec::Named(named) => generic_external_capabilities(CompilerSpec::Named(*named)),
        CompilerSpec::Binary(path) => {
            generic_external_capabilities(CompilerSpec::Binary(path.clone()))
        }
    }
}

fn capability_extra_summary(extras: &BTreeMap<String, Vec<String>>) -> String {
    if extras.is_empty() {
        return "none".to_string();
    }

    extras
        .iter()
        .map(|(namespace, names)| format!("{}({})", namespace, names.join(", ")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn capability_unavailable_summary(capabilities: &CompilerCapabilities) -> String {
    if capabilities.unavailable_artifacts.is_empty() {
        return "none".to_string();
    }

    let mut grouped = BTreeMap::<String, Vec<String>>::new();
    for artifact in capabilities.unavailable_artifacts.keys() {
        if let Some((namespace, local_name)) = artifact.extra_parts() {
            grouped
                .entry(namespace.to_string())
                .or_insert_with(Vec::new)
                .push(local_name.to_string());
        } else {
            grouped
                .entry("generic".to_string())
                .or_insert_with(Vec::new)
                .push(artifact.as_str().to_string());
        }
    }

    grouped
        .iter()
        .map(|(namespace, names)| format!("{}({})", namespace, names.join(", ")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn compiler_capability_backend(spec: &CompilerSpec, tools: &ToolchainConfig) -> (String, String) {
    match spec {
        CompilerSpec::Named(NamedCompiler::Armfortas) => {
            let adapters = tools.armfortas_adapters();
            (
                adapters.capture_mode_name().to_string(),
                adapters.capture_description().to_string(),
            )
        }
        CompilerSpec::Named(named) => {
            let binary = tools
                .named_compiler_binary(*named)
                .unwrap_or_else(|| named.as_str().to_string());
            (
                "external-driver".to_string(),
                format!("generic external driver adapter using {}", binary),
            )
        }
        CompilerSpec::Binary(path) => (
            "external-driver".to_string(),
            format!("generic external driver adapter using {}", path.display()),
        ),
    }
}

fn observation_from_capability_mismatch(
    spec: &CompilerSpec,
    program: &Path,
    opt_level: OptLevel,
    requested: BTreeSet<ArtifactKey>,
    backend_mode: String,
    backend_detail: String,
    detail: String,
) -> ObservedProgram {
    ObservedProgram {
        observation: CompilerObservation {
            compiler: spec.clone(),
            program: program.to_path_buf(),
            opt_level,
            compile_exit_code: 1,
            artifacts: BTreeMap::from([(ArtifactKey::Diagnostics, ArtifactValue::Text(detail))]),
            provenance: ObservationProvenance {
                compiler_identity: spec.display_name(),
                adapter_kind: match spec {
                    CompilerSpec::Named(_) => "named".into(),
                    CompilerSpec::Binary(_) => "explicit-path".into(),
                },
                backend_mode,
                backend_detail,
                artifacts_captured: vec!["diagnostics".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        },
        requested_artifacts: requested,
    }
}

fn preflight_introspection_request(
    spec: &CompilerSpec,
    program: &Path,
    opt_level: OptLevel,
    requested: &BTreeSet<ArtifactKey>,
    tools: &ToolchainConfig,
) -> Option<ObservedProgram> {
    let capabilities = compiler_capabilities(spec, tools);
    let (backend_mode, backend_detail) = compiler_capability_backend(spec, tools);

    let unavailable = capabilities.unavailable_requests(requested);
    if !unavailable.is_empty() {
        let detail = unavailable
            .into_iter()
            .map(|(artifact, reason)| format!("requested {}: {}", artifact, reason))
            .collect::<Vec<_>>()
            .join("\n");
        return Some(observation_from_capability_mismatch(
            spec,
            program,
            opt_level,
            requested.clone(),
            backend_mode,
            backend_detail,
            detail,
        ));
    }

    let unsupported = capabilities.unsupported_requests(requested);
    if !unsupported.is_empty() {
        let detail = format!(
            "{} does not support requested artifacts in this adapter: {}",
            spec.display_name(),
            unsupported.join(", ")
        );
        return Some(observation_from_capability_mismatch(
            spec,
            program,
            opt_level,
            requested.clone(),
            backend_mode,
            backend_detail,
            detail,
        ));
    }

    None
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
    primary_backend: Option<PrimaryBackendReport>,
    consistency_observations: Vec<ConsistencyObservation>,
}

#[derive(Debug, Default)]
struct Summary {
    passed: usize,
    failed: usize,
    xfailed: usize,
    xpassed: usize,
    future: usize,
    outcomes: Vec<Outcome>,
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
struct ListConfig {
    suite_filter: Option<String>,
    verbose: bool,
    tools: ToolchainConfig,
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
    json_report: Option<PathBuf>,
    markdown_report: Option<PathBuf>,
    tools: ToolchainConfig,
}

#[derive(Debug, Clone)]
struct CompareConfig {
    left: CompilerSpec,
    right: CompilerSpec,
    program: PathBuf,
    opt_level: OptLevel,
    artifacts: BTreeSet<ArtifactKey>,
    json_report: Option<PathBuf>,
    markdown_report: Option<PathBuf>,
    tools: ToolchainConfig,
}

#[derive(Debug, Clone)]
struct IntrospectConfig {
    compiler: CompilerSpec,
    program: PathBuf,
    opt_level: OptLevel,
    artifacts: BTreeSet<ArtifactKey>,
    json_report: Option<PathBuf>,
    markdown_report: Option<PathBuf>,
    all_artifacts: bool,
    summary_only: bool,
    max_artifact_lines: Option<usize>,
    tools: ToolchainConfig,
}

#[derive(Debug, Clone)]
struct ExecutionArtifacts {
    requested: BTreeSet<Stage>,
    armfortas: Option<CaptureResult>,
    armfortas_failure: Option<CaptureFailure>,
    armfortas_observation: Option<ObservedProgram>,
    references: Vec<ReferenceResult>,
    reference_observations: Vec<ObservedProgram>,
    consistency_issues: Vec<ConsistencyIssue>,
}

#[derive(Debug, Clone)]
struct ObservedProgram {
    observation: CompilerObservation,
    requested_artifacts: BTreeSet<ArtifactKey>,
}

#[derive(Debug, Clone, Copy)]
struct IntrospectionRenderConfig {
    summary_only: bool,
    max_artifact_lines: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimaryCaptureBackendKind {
    Full,
    Observable,
}

impl PrimaryCaptureBackendKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Observable => "observable",
        }
    }
}

struct SelectedPrimaryBackend {
    kind: PrimaryCaptureBackendKind,
    backend: Box<dyn CaptureBackend>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrimaryBackendReport {
    kind: String,
    mode: String,
    detail: String,
}

impl PrimaryBackendReport {
    fn from_selected(selected: &SelectedPrimaryBackend) -> Self {
        Self {
            kind: selected.kind.as_str().to_string(),
            mode: selected.backend.mode_name().to_string(),
            detail: selected.backend.description().to_string(),
        }
    }
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
        self.outcomes.push(outcome.clone());
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
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RunSignature {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

pub fn run_cli(args: &[String]) -> i32 {
    run_cli_named("afs-tests", args)
}

pub fn run_cli_named(program_name: &str, args: &[String]) -> i32 {
    match parse_cli(args) {
        Ok(CommandKind::List(config)) => match discover_suites(default_suite_root()) {
            Ok(suites) => {
                print_suites(
                    &filter_suites(&suites, config.suite_filter.as_deref()),
                    &config,
                );
                0
            }
            Err(err) => {
                eprintln!("{}: {}", program_name, err);
                1
            }
        },
        Ok(CommandKind::Run(config)) => match run_suites(&config) {
            Ok(summary) => {
                print_summary(&summary);
                if let Err(err) = write_requested_reports(&config, &summary) {
                    eprintln!("afs-tests: {}", err);
                    return 1;
                }
                if summary.failed == 0 && summary.xpassed == 0 {
                    0
                } else {
                    1
                }
            }
            Err(err) => {
                eprintln!("{}: {}", program_name, err);
                1
            }
        },
        Ok(CommandKind::Compare(config)) => match run_compare(&config) {
            Ok(result) => {
                print_compare_result(&result);
                if let Err(err) = write_compare_reports(&config, &result) {
                    eprintln!("{}: {}", program_name, err);
                    return 1;
                }
                if result.differences.is_empty() {
                    0
                } else {
                    1
                }
            }
            Err(err) => {
                eprintln!("{}: {}", program_name, err);
                1
            }
        },
        Ok(CommandKind::Introspect(config)) => match run_introspect(&config) {
            Ok(observation) => {
                print_introspection(&config, &observation);
                if let Err(err) = write_introspection_reports(&config, &observation) {
                    eprintln!("{}: {}", program_name, err);
                    return 1;
                }
                if observation.observation.compile_exit_code == 0 {
                    0
                } else {
                    1
                }
            }
            Err(err) => {
                eprintln!("{}: {}", program_name, err);
                1
            }
        },
        Ok(CommandKind::Doctor(config)) => {
            println!("{}", render_doctor_report(&config));
            if let Err(err) = write_doctor_reports(&config) {
                eprintln!("{}: {}", program_name, err);
                return 1;
            }
            0
        }
        Ok(CommandKind::Help) => {
            print_usage(program_name);
            0
        }
        Err(err) => {
            eprintln!("{}: {}", program_name, err);
            print_usage(program_name);
            2
        }
    }
}

enum CommandKind {
    List(ListConfig),
    Run(RunConfig),
    Compare(CompareConfig),
    Introspect(IntrospectConfig),
    Doctor(DoctorConfig),
    Help,
}

#[derive(Debug, Clone)]
struct DoctorConfig {
    tools: ToolchainConfig,
    json_report: Option<PathBuf>,
    markdown_report: Option<PathBuf>,
}

fn parse_tool_override_arg(
    arg: &str,
    queue: &mut VecDeque<&String>,
    tools: &mut ToolchainConfig,
) -> Result<bool, String> {
    match arg {
        "--armfortas-bin" => {
            let value = queue
                .pop_front()
                .ok_or("--armfortas-bin requires a value")?;
            tools.armfortas = ArmfortasCliAdapter::External(value.clone());
            Ok(true)
        }
        "--gfortran-bin" => {
            let value = queue.pop_front().ok_or("--gfortran-bin requires a value")?;
            tools.gfortran = value.clone();
            Ok(true)
        }
        "--flang-bin" => {
            let value = queue.pop_front().ok_or("--flang-bin requires a value")?;
            tools.flang_new = value.clone();
            Ok(true)
        }
        "--as-bin" => {
            let value = queue.pop_front().ok_or("--as-bin requires a value")?;
            tools.system_as = value.clone();
            Ok(true)
        }
        "--otool-bin" => {
            let value = queue.pop_front().ok_or("--otool-bin requires a value")?;
            tools.otool = value.clone();
            Ok(true)
        }
        "--nm-bin" => {
            let value = queue.pop_front().ok_or("--nm-bin requires a value")?;
            tools.nm = value.clone();
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn parse_cli(args: &[String]) -> Result<CommandKind, String> {
    if args.is_empty() {
        return Ok(CommandKind::Help);
    }

    match args[0].as_str() {
        "list" => {
            let mut config = ListConfig {
                suite_filter: None,
                verbose: false,
                tools: ToolchainConfig::from_env(),
            };
            let mut queue: VecDeque<&String> = args[1..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                if parse_tool_override_arg(arg, &mut queue, &mut config.tools)? {
                    continue;
                }
                match arg.as_str() {
                    "--suite" => {
                        let value = queue.pop_front().ok_or("--suite requires a value")?;
                        config.suite_filter = Some(value.clone());
                    }
                    "--verbose" => config.verbose = true,
                    "--help" | "-h" => return Ok(CommandKind::Help),
                    other => return Err(format!("unknown list option: {}", other)),
                }
            }
            Ok(CommandKind::List(config))
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
                json_report: None,
                markdown_report: None,
                tools: ToolchainConfig::from_env(),
            };
            let mut queue: VecDeque<&String> = args[1..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                if parse_tool_override_arg(arg, &mut queue, &mut config.tools)? {
                    continue;
                }
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
                    "--json-report" => {
                        let value = queue.pop_front().ok_or("--json-report requires a value")?;
                        config.json_report = Some(PathBuf::from(value));
                    }
                    "--markdown-report" => {
                        let value = queue
                            .pop_front()
                            .ok_or("--markdown-report requires a value")?;
                        config.markdown_report = Some(PathBuf::from(value));
                    }
                    "--help" | "-h" => return Ok(CommandKind::Help),
                    other => return Err(format!("unknown run option: {}", other)),
                }
            }
            Ok(CommandKind::Run(config))
        }
        "compare" => {
            if args.len() < 3 {
                return Err(
                    "compare requires <compiler-a> <compiler-b> and --program <path>".to_string(),
                );
            }
            let left = CompilerSpec::parse(&args[1]);
            let right = CompilerSpec::parse(&args[2]);
            let mut config = CompareConfig {
                left,
                right,
                program: PathBuf::new(),
                opt_level: OptLevel::O0,
                artifacts: BTreeSet::new(),
                json_report: None,
                markdown_report: None,
                tools: ToolchainConfig::from_env(),
            };
            let mut queue: VecDeque<&String> = args[3..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                if parse_tool_override_arg(arg, &mut queue, &mut config.tools)? {
                    continue;
                }
                match arg.as_str() {
                    "--program" => {
                        let value = queue.pop_front().ok_or("--program requires a value")?;
                        config.program = PathBuf::from(value);
                    }
                    "--opt" => {
                        let value = queue.pop_front().ok_or("--opt requires a value")?;
                        let parsed = parse_opt_level_list(value)?;
                        let opt = parsed
                            .into_iter()
                            .next()
                            .ok_or("--opt requires at least one optimization level")?;
                        config.opt_level = opt;
                    }
                    "--artifact" => {
                        let value = queue.pop_front().ok_or("--artifact requires a value")?;
                        config.artifacts.extend(ArtifactKey::parse_list(value)?);
                    }
                    "--json-report" => {
                        let value = queue.pop_front().ok_or("--json-report requires a value")?;
                        config.json_report = Some(PathBuf::from(value));
                    }
                    "--markdown-report" => {
                        let value = queue
                            .pop_front()
                            .ok_or("--markdown-report requires a value")?;
                        config.markdown_report = Some(PathBuf::from(value));
                    }
                    "--help" | "-h" => return Ok(CommandKind::Help),
                    other => return Err(format!("unknown compare option: {}", other)),
                }
            }
            if config.program.as_os_str().is_empty() {
                return Err("compare requires --program <path>".to_string());
            }
            Ok(CommandKind::Compare(config))
        }
        "introspect" => {
            if args.len() < 3 {
                return Err("introspect requires <compiler> <program>".to_string());
            }
            let compiler = CompilerSpec::parse(&args[1]);
            let mut config = IntrospectConfig {
                compiler,
                program: PathBuf::from(&args[2]),
                opt_level: OptLevel::O0,
                artifacts: BTreeSet::new(),
                json_report: None,
                markdown_report: None,
                all_artifacts: false,
                summary_only: false,
                max_artifact_lines: None,
                tools: ToolchainConfig::from_env(),
            };
            let mut queue: VecDeque<&String> = args[3..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                if parse_tool_override_arg(arg, &mut queue, &mut config.tools)? {
                    continue;
                }
                match arg.as_str() {
                    "--program" => {
                        let value = queue.pop_front().ok_or("--program requires a value")?;
                        config.program = PathBuf::from(value);
                    }
                    "--opt" => {
                        let value = queue.pop_front().ok_or("--opt requires a value")?;
                        let parsed = parse_opt_level_list(value)?;
                        let opt = parsed
                            .into_iter()
                            .next()
                            .ok_or("--opt requires at least one optimization level")?;
                        config.opt_level = opt;
                    }
                    "--artifact" => {
                        let value = queue.pop_front().ok_or("--artifact requires a value")?;
                        config.artifacts.extend(ArtifactKey::parse_list(value)?);
                    }
                    "--all" => config.all_artifacts = true,
                    "--summary-only" => config.summary_only = true,
                    "--max-artifact-lines" => {
                        let value = queue
                            .pop_front()
                            .ok_or("--max-artifact-lines requires a value")?;
                        let parsed = value.parse::<usize>().map_err(|_| {
                            format!("invalid --max-artifact-lines value '{}'", value)
                        })?;
                        if parsed == 0 {
                            return Err("--max-artifact-lines must be greater than 0".to_string());
                        }
                        config.max_artifact_lines = Some(parsed);
                    }
                    "--json-report" => {
                        let value = queue.pop_front().ok_or("--json-report requires a value")?;
                        config.json_report = Some(PathBuf::from(value));
                    }
                    "--markdown-report" => {
                        let value = queue
                            .pop_front()
                            .ok_or("--markdown-report requires a value")?;
                        config.markdown_report = Some(PathBuf::from(value));
                    }
                    "--help" | "-h" => return Ok(CommandKind::Help),
                    other => return Err(format!("unknown introspect option: {}", other)),
                }
            }
            Ok(CommandKind::Introspect(config))
        }
        "doctor" => {
            let mut config = DoctorConfig {
                tools: ToolchainConfig::from_env(),
                json_report: None,
                markdown_report: None,
            };
            let mut queue: VecDeque<&String> = args[1..].iter().collect();
            while let Some(arg) = queue.pop_front() {
                if parse_tool_override_arg(arg, &mut queue, &mut config.tools)? {
                    continue;
                }
                match arg.as_str() {
                    "--json-report" => {
                        let value = queue.pop_front().ok_or("--json-report requires a value")?;
                        config.json_report = Some(PathBuf::from(value));
                    }
                    "--markdown-report" => {
                        let value = queue
                            .pop_front()
                            .ok_or("--markdown-report requires a value")?;
                        config.markdown_report = Some(PathBuf::from(value));
                    }
                    "--help" | "-h" => return Ok(CommandKind::Help),
                    other => return Err(format!("unknown doctor option: {}", other)),
                }
            }
            Ok(CommandKind::Doctor(config))
        }
        "--help" | "-h" | "help" => Ok(CommandKind::Help),
        other => Err(format!("unknown command: {}", other)),
    }
}

fn print_usage(program_name: &str) {
    eprintln!(
        "{} — generic compiler bench runner (afs-tests compatibility preserved)",
        program_name
    );
    eprintln!();
    eprintln!("usage:");
    eprintln!(
        "  {} list [--suite <filter>] [--verbose] [tool overrides]",
        program_name
    );
    eprintln!(
        "  {} run [--suite <filter>] [--case <filter>] [--opt <O0,O1,...>] [--verbose] [--fail-fast] [--include-future] [--all] [--json-report <path>] [--markdown-report <path>] [--armfortas-bin <path>] [--gfortran-bin <path>] [--flang-bin <path>] [--as-bin <path>] [--otool-bin <path>] [--nm-bin <path>]",
        program_name
    );
    eprintln!(
        "  {} compare <compiler-a> <compiler-b> --program <path> [--opt <O0>] [--artifact <asm,obj,stdout,stderr,exit-code,executable>] [--json-report <path>] [--markdown-report <path>] [tool overrides]",
        program_name
    );
    eprintln!(
        "  {} introspect <compiler> <program> [--opt <O0>] [--artifact <list>] [--all] [--summary-only] [--max-artifact-lines <n>] [--json-report <path>] [--markdown-report <path>] [tool overrides]",
        program_name
    );
    eprintln!(
        "  {} doctor [--json-report <path>] [--markdown-report <path>] [--armfortas-bin <path>] [--gfortran-bin <path>] [--flang-bin <path>] [--as-bin <path>] [--otool-bin <path>] [--nm-bin <path>]",
        program_name
    );
    eprintln!();
    eprintln!("env overrides:");
    eprintln!("  BENCCH_ARMFORTAS_BIN, BENCCH_GFORTRAN_BIN, BENCCH_FLANG_BIN");
    eprintln!("  BENCCH_AS_BIN, BENCCH_OTOOL_BIN, BENCCH_NM_BIN");
    eprintln!();
    if linked_capture_available() {
        eprintln!("mode:");
        eprintln!("  linked armfortas capture is available in this build");
    } else {
        eprintln!("mode:");
        eprintln!("  linked armfortas capture is unavailable in this build");
        eprintln!("  compare, introspect, and generic/observable suite runs still work");
        eprintln!("  use scripts/bootstrap-linked-armfortas.sh for rich armfortas stages and legacy frontend/module suites");
    }
}

fn default_compare_artifacts(extra: &BTreeSet<ArtifactKey>) -> BTreeSet<ArtifactKey> {
    let mut requested = BTreeSet::from([ArtifactKey::Diagnostics, ArtifactKey::Runtime]);
    requested.extend(extra.iter().cloned());
    requested
}

fn default_differential_artifacts() -> BTreeSet<ArtifactKey> {
    BTreeSet::from([ArtifactKey::Diagnostics, ArtifactKey::Runtime])
}

fn default_introspection_artifacts(
    compiler: &CompilerSpec,
    all_artifacts: bool,
) -> BTreeSet<ArtifactKey> {
    let mut requested = BTreeSet::from([
        ArtifactKey::Diagnostics,
        ArtifactKey::Runtime,
        ArtifactKey::Asm,
        ArtifactKey::Obj,
    ]);
    if matches!(compiler, CompilerSpec::Named(NamedCompiler::Armfortas)) {
        requested.insert(ArtifactKey::Extra("armfortas.ir".into()));
        if all_artifacts {
            for name in [
                "armfortas.preprocess",
                "armfortas.tokens",
                "armfortas.ast",
                "armfortas.sema",
                "armfortas.ir",
                "armfortas.optir",
                "armfortas.mir",
                "armfortas.regalloc",
            ] {
                requested.insert(ArtifactKey::Extra(name.to_string()));
            }
        }
    }
    requested
}

fn run_compare(config: &CompareConfig) -> Result<ComparisonResult, String> {
    let requested = default_compare_artifacts(&config.artifacts);
    preflight_compare_request(config, &requested)?;
    let left = observe_compiler(
        &config.left,
        &config.program,
        config.opt_level,
        &requested,
        &config.tools,
    )?;
    let right = observe_compiler(
        &config.right,
        &config.program,
        config.opt_level,
        &requested,
        &config.tools,
    )?;
    Ok(compare_observations(left, right, &requested))
}

fn capability_request_issue(
    spec: &CompilerSpec,
    requested: &BTreeSet<ArtifactKey>,
    tools: &ToolchainConfig,
) -> Option<String> {
    let capabilities = compiler_capabilities(spec, tools);
    let unavailable = capabilities.unavailable_requests(requested);
    let unsupported = capabilities.unsupported_requests(requested);
    if unavailable.is_empty() && unsupported.is_empty() {
        return None;
    }

    let mut lines = vec![format!("{}:", spec.display_name())];
    for (artifact, reason) in unavailable {
        lines.push(format!("  unavailable {}: {}", artifact, reason));
    }
    if !unsupported.is_empty() {
        lines.push(format!(
            "  unsupported in this adapter: {}",
            unsupported.join(", ")
        ));
    }
    Some(lines.join("\n"))
}

fn preflight_compare_request(
    config: &CompareConfig,
    requested: &BTreeSet<ArtifactKey>,
) -> Result<(), String> {
    let mut issues = Vec::new();
    if let Some(issue) = capability_request_issue(&config.left, requested, &config.tools) {
        issues.push(format!("left {}\n{}", config.left.display_name(), issue));
    }
    if let Some(issue) = capability_request_issue(&config.right, requested, &config.tools) {
        issues.push(format!("right {}\n{}", config.right.display_name(), issue));
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "compare request is not supported for the selected compiler surfaces\n{}",
            issues.join("\n")
        ))
    }
}

fn run_introspect(config: &IntrospectConfig) -> Result<ObservedProgram, String> {
    let requested = if config.artifacts.is_empty() {
        default_introspection_artifacts(&config.compiler, config.all_artifacts)
    } else {
        let mut requested = config.artifacts.clone();
        if config.all_artifacts
            && matches!(
                config.compiler,
                CompilerSpec::Named(NamedCompiler::Armfortas)
            )
        {
            requested.extend(default_introspection_artifacts(&config.compiler, true));
        }
        requested
    };
    if let Some(observed) = preflight_introspection_request(
        &config.compiler,
        &config.program,
        config.opt_level,
        &requested,
        &config.tools,
    ) {
        return Ok(observed);
    }
    Ok(ObservedProgram {
        observation: observe_compiler(
            &config.compiler,
            &config.program,
            config.opt_level,
            &requested,
            &config.tools,
        )?,
        requested_artifacts: requested,
    })
}

fn requested_linked_armfortas_artifacts(requested: &BTreeSet<ArtifactKey>) -> Vec<String> {
    requested
        .iter()
        .filter_map(|artifact| match artifact {
            ArtifactKey::Extra(name) if name.starts_with("armfortas.") => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn observe_compiler(
    spec: &CompilerSpec,
    program: &Path,
    opt_level: OptLevel,
    requested: &BTreeSet<ArtifactKey>,
    tools: &ToolchainConfig,
) -> Result<CompilerObservation, String> {
    match spec {
        CompilerSpec::Named(NamedCompiler::Armfortas) => {
            observe_armfortas(program, opt_level, requested, tools)
        }
        CompilerSpec::Named(named) => {
            let binary = tools.named_compiler_binary(*named).ok_or_else(|| {
                format!("named compiler '{}' has no resolved binary", named.as_str())
            })?;
            observe_external_driver(
                spec,
                &binary,
                program,
                opt_level,
                requested,
                matches!(named, NamedCompiler::Gfortran | NamedCompiler::FlangNew)
                    && source_uses_cpp(program),
                "named".to_string(),
                tools.otool_bin(),
                tools.nm_bin(),
            )
        }
        CompilerSpec::Binary(path) => observe_external_driver(
            spec,
            &path.display().to_string(),
            program,
            opt_level,
            requested,
            false,
            "explicit-path".to_string(),
            tools.otool_bin(),
            tools.nm_bin(),
        ),
    }
}

fn observe_armfortas(
    program: &Path,
    opt_level: OptLevel,
    requested: &BTreeSet<ArtifactKey>,
    tools: &ToolchainConfig,
) -> Result<CompilerObservation, String> {
    let stages = armfortas_requested_stages(requested)?;
    let linked_only_artifacts = requested_linked_armfortas_artifacts(requested);
    let linked_backend = tools.armfortas_adapters();
    if !linked_only_artifacts.is_empty() && linked_backend.capture_mode_name() == "unavailable" {
        let detail = format!(
            "linked armfortas capture is unavailable in this build; requested {}; use scripts/bootstrap-linked-armfortas.sh or request only asm/obj/run from an external armfortas binary",
            linked_only_artifacts.join(", ")
        );
        return Ok(CompilerObservation {
            compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
            program: program.to_path_buf(),
            opt_level,
            compile_exit_code: 1,
            artifacts: BTreeMap::from([(
                ArtifactKey::Diagnostics,
                ArtifactValue::Text(detail.clone()),
            )]),
            provenance: ObservationProvenance {
                compiler_identity: "armfortas".into(),
                adapter_kind: "named".into(),
                backend_mode: linked_backend.capture_mode_name().into(),
                backend_detail: linked_backend.capture_description().into(),
                artifacts_captured: vec!["diagnostics".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        });
    }
    let cli_observable_only = requested.iter().all(|artifact| {
        matches!(
            artifact,
            ArtifactKey::Diagnostics
                | ArtifactKey::Runtime
                | ArtifactKey::Stdout
                | ArtifactKey::Stderr
                | ArtifactKey::ExitCode
                | ArtifactKey::Asm
                | ArtifactKey::Obj
                | ArtifactKey::Executable
        )
    });
    let (backend_mode, backend_detail, capture) = if cli_observable_only
        && matches!(tools.armfortas, ArmfortasCliAdapter::External(_))
    {
        let backend = tools.cli_observable_capture_backend(next_primary_cli_temp_root(opt_level));
        let detail = backend.description().to_string();
        let mode = backend.mode_name().to_string();
        let request = CaptureRequest {
            input: program.to_path_buf(),
            requested: stages.clone(),
            opt_level,
        };
        (mode, detail, backend.capture(&request))
    } else {
        let backend = linked_backend;
        let detail = backend.capture_description().to_string();
        let mode = backend.capture_mode_name().to_string();
        let request = CaptureRequest {
            input: program.to_path_buf(),
            requested: stages.clone(),
            opt_level,
        };
        (mode, detail, backend.capture(&request))
    };

    let mut artifacts = BTreeMap::new();
    let mut compile_exit_code = 0;
    let mut failure_stage = None;
    match capture {
        Ok(result) => {
            for (stage, captured) in &result.stages {
                match (stage, captured) {
                    (Stage::Asm, CapturedStage::Text(text))
                        if requested.contains(&ArtifactKey::Asm) =>
                    {
                        artifacts.insert(ArtifactKey::Asm, ArtifactValue::Text(text.clone()));
                    }
                    (Stage::Obj, CapturedStage::Text(text))
                        if requested.contains(&ArtifactKey::Obj) =>
                    {
                        artifacts.insert(ArtifactKey::Obj, ArtifactValue::Text(text.clone()));
                    }
                    (Stage::Run, CapturedStage::Run(run)) => {
                        insert_run_artifacts(requested, run, &mut artifacts);
                    }
                    (stage, CapturedStage::Text(text)) => {
                        let key = ArtifactKey::Extra(format!("armfortas.{}", stage.as_str()));
                        if requested.contains(&key) {
                            artifacts.insert(key, ArtifactValue::Text(text.clone()));
                        }
                    }
                    _ => {}
                }
            }
        }
        Err(failure) => {
            compile_exit_code = 1;
            failure_stage = Some(failure.stage.as_str().to_string());
            artifacts.insert(
                ArtifactKey::Diagnostics,
                ArtifactValue::Text(failure.detail.clone()),
            );
            for (stage, captured) in &failure.stages {
                match (stage, captured) {
                    (Stage::Asm, CapturedStage::Text(text))
                        if requested.contains(&ArtifactKey::Asm) =>
                    {
                        artifacts.insert(ArtifactKey::Asm, ArtifactValue::Text(text.clone()));
                    }
                    (Stage::Obj, CapturedStage::Text(text))
                        if requested.contains(&ArtifactKey::Obj) =>
                    {
                        artifacts.insert(ArtifactKey::Obj, ArtifactValue::Text(text.clone()));
                    }
                    (Stage::Run, CapturedStage::Run(run)) => {
                        insert_run_artifacts(requested, run, &mut artifacts);
                    }
                    (stage, CapturedStage::Text(text)) => {
                        let key = ArtifactKey::Extra(format!("armfortas.{}", stage.as_str()));
                        if requested.contains(&key) {
                            artifacts.insert(key, ArtifactValue::Text(text.clone()));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if requested.contains(&ArtifactKey::Executable) && compile_exit_code == 0 {
        let temp_root = next_observation_temp_root("armfortas", opt_level);
        fs::create_dir_all(&temp_root).map_err(|e| {
            format!(
                "cannot create introspection temp dir '{}': {}",
                temp_root.display(),
                e
            )
        })?;
        let binary = temp_root.join("introspect.out");
        tools
            .armfortas_adapters()
            .compile_output(program, opt_level, EmitMode::Binary, &binary)
            .map_err(|detail| {
                format!("failed to build armfortas executable artifact:\n{}", detail)
            })?;
        artifacts.insert(ArtifactKey::Executable, ArtifactValue::Path(binary));
    }

    let artifacts_captured = artifacts
        .keys()
        .map(|artifact| artifact.as_str().to_string())
        .collect::<Vec<_>>();

    Ok(CompilerObservation {
        compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
        program: program.to_path_buf(),
        opt_level,
        compile_exit_code,
        artifacts,
        provenance: ObservationProvenance {
            compiler_identity: "armfortas".into(),
            adapter_kind: "named".into(),
            backend_mode,
            backend_detail,
            artifacts_captured,
            comparison_basis: None,
            failure_stage,
        },
    })
}

#[derive(Debug, Clone)]
struct DriverCompileResult {
    command: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
    output: PathBuf,
}

fn observe_external_driver(
    spec: &CompilerSpec,
    binary: &str,
    program: &Path,
    opt_level: OptLevel,
    requested: &BTreeSet<ArtifactKey>,
    uses_cpp: bool,
    adapter_kind: String,
    otool: &str,
    nm: &str,
) -> Result<CompilerObservation, String> {
    let temp_root = next_observation_temp_root(&spec.display_name(), opt_level);
    fs::create_dir_all(&temp_root).map_err(|e| {
        format!(
            "cannot create observation temp dir '{}': {}",
            temp_root.display(),
            e
        )
    })?;

    let needs_runtime = requested.contains(&ArtifactKey::Runtime)
        || requested.contains(&ArtifactKey::Stdout)
        || requested.contains(&ArtifactKey::Stderr)
        || requested.contains(&ArtifactKey::ExitCode)
        || requested.contains(&ArtifactKey::Executable);
    let primary_mode = if needs_runtime {
        DriverEmitMode::Binary
    } else if requested.contains(&ArtifactKey::Asm) {
        DriverEmitMode::Asm
    } else if requested.contains(&ArtifactKey::Obj) {
        DriverEmitMode::Obj
    } else {
        DriverEmitMode::Binary
    };
    let primary_name = match primary_mode {
        DriverEmitMode::Binary => "observe.out",
        DriverEmitMode::Asm => "observe.s",
        DriverEmitMode::Obj => "observe.o",
    };
    let primary = compile_with_external_driver(
        binary,
        program,
        opt_level,
        primary_mode,
        &temp_root.join(primary_name),
        uses_cpp,
    )?;

    let mut artifacts = BTreeMap::new();
    if !primary.stdout.trim().is_empty()
        || !primary.stderr.trim().is_empty()
        || primary.exit_code != 0
    {
        let diagnostics = [primary.stdout.trim_end(), primary.stderr.trim_end()]
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        artifacts.insert(ArtifactKey::Diagnostics, ArtifactValue::Text(diagnostics));
    }

    let mut compile_exit_code = primary.exit_code;
    if primary.exit_code == 0 {
        match primary_mode {
            DriverEmitMode::Binary => {
                if requested.contains(&ArtifactKey::Executable) {
                    artifacts.insert(
                        ArtifactKey::Executable,
                        ArtifactValue::Path(primary.output.clone()),
                    );
                }
                if needs_runtime {
                    let run_command = render_binary_run_command(&primary.output);
                    let run = run_binary_capture(&primary.output, &temp_root, &run_command)
                        .map_err(|detail| format!("build: {}\n{}", primary.command, detail))?;
                    insert_run_artifacts(requested, &run, &mut artifacts);
                }
            }
            DriverEmitMode::Asm if requested.contains(&ArtifactKey::Asm) => {
                artifacts.insert(
                    ArtifactKey::Asm,
                    ArtifactValue::Text(fs::read_to_string(&primary.output).map_err(|e| {
                        format!(
                            "cannot read asm artifact '{}': {}",
                            primary.output.display(),
                            e
                        )
                    })?),
                );
            }
            DriverEmitMode::Obj if requested.contains(&ArtifactKey::Obj) => {
                artifacts.insert(
                    ArtifactKey::Obj,
                    ArtifactValue::Text(
                        object_snapshot_text(&primary.output, otool, nm)
                            .unwrap_or_else(|_| "object snapshot unavailable".into()),
                    ),
                );
            }
            _ => {}
        }

        if requested.contains(&ArtifactKey::Asm) && primary_mode != DriverEmitMode::Asm {
            let asm = compile_with_external_driver(
                binary,
                program,
                opt_level,
                DriverEmitMode::Asm,
                &temp_root.join("observe-extra.s"),
                uses_cpp,
            )?;
            if asm.exit_code != 0 {
                compile_exit_code = asm.exit_code;
                artifacts.insert(
                    ArtifactKey::Diagnostics,
                    ArtifactValue::Text(asm.stderr.trim_end().to_string()),
                );
            } else {
                artifacts.insert(
                    ArtifactKey::Asm,
                    ArtifactValue::Text(fs::read_to_string(&asm.output).map_err(|e| {
                        format!("cannot read asm artifact '{}': {}", asm.output.display(), e)
                    })?),
                );
            }
        }

        if requested.contains(&ArtifactKey::Obj) && primary_mode != DriverEmitMode::Obj {
            let obj = compile_with_external_driver(
                binary,
                program,
                opt_level,
                DriverEmitMode::Obj,
                &temp_root.join("observe-extra.o"),
                uses_cpp,
            )?;
            if obj.exit_code != 0 {
                compile_exit_code = obj.exit_code;
                artifacts.insert(
                    ArtifactKey::Diagnostics,
                    ArtifactValue::Text(obj.stderr.trim_end().to_string()),
                );
            } else {
                artifacts.insert(
                    ArtifactKey::Obj,
                    ArtifactValue::Text(
                        object_snapshot_text(&obj.output, otool, nm)
                            .unwrap_or_else(|_| "object snapshot unavailable".into()),
                    ),
                );
            }
        }
    }

    let artifacts_captured = artifacts
        .keys()
        .map(|artifact| artifact.as_str().to_string())
        .collect::<Vec<_>>();

    Ok(CompilerObservation {
        compiler: spec.clone(),
        program: program.to_path_buf(),
        opt_level,
        compile_exit_code,
        artifacts,
        provenance: ObservationProvenance {
            compiler_identity: spec.display_name(),
            adapter_kind,
            backend_mode: "external-driver".into(),
            backend_detail: format!("generic external driver adapter using {}", binary),
            artifacts_captured,
            comparison_basis: None,
            failure_stage: None,
        },
    })
}

fn compile_with_external_driver(
    binary: &str,
    source: &Path,
    opt_level: OptLevel,
    mode: DriverEmitMode,
    output: &Path,
    uses_cpp: bool,
) -> Result<DriverCompileResult, String> {
    let mut args = vec![opt_level.as_flag().to_string()];
    if uses_cpp {
        args.push("-cpp".to_string());
    }
    match mode {
        DriverEmitMode::Asm => args.push("-S".to_string()),
        DriverEmitMode::Obj => args.push("-c".to_string()),
        DriverEmitMode::Binary => {}
    }
    args.push(source.display().to_string());
    args.push("-o".to_string());
    args.push(output.display().to_string());
    let command = render_command(binary, &args);
    let output_result = Command::new(binary)
        .args(&args)
        .output()
        .map_err(|e| format!("cannot run '{}': {}", binary, e))?;
    Ok(DriverCompileResult {
        command,
        exit_code: output_result.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output_result.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output_result.stderr).into_owned(),
        output: output.to_path_buf(),
    })
}

fn next_observation_temp_root(label: &str, opt_level: OptLevel) -> PathBuf {
    default_report_root().join(".tmp").join(format!(
        "observe_{}_{}",
        sanitize_component(label),
        next_report_suffix(opt_level)
    ))
}

fn armfortas_requested_stages(
    requested: &BTreeSet<ArtifactKey>,
) -> Result<BTreeSet<Stage>, String> {
    let mut stages = BTreeSet::new();
    for artifact in requested {
        match artifact {
            ArtifactKey::Asm => {
                stages.insert(Stage::Asm);
            }
            ArtifactKey::Obj => {
                stages.insert(Stage::Obj);
            }
            ArtifactKey::Runtime
            | ArtifactKey::Stdout
            | ArtifactKey::Stderr
            | ArtifactKey::ExitCode => {
                stages.insert(Stage::Run);
            }
            ArtifactKey::Diagnostics | ArtifactKey::Executable => {}
            ArtifactKey::Extra(name) => {
                let suffix = name
                    .strip_prefix("armfortas.")
                    .ok_or_else(|| format!("unsupported adapter-specific artifact '{}'", name))?;
                let stage = Stage::parse(suffix)
                    .ok_or_else(|| format!("unknown armfortas artifact '{}'", name))?;
                stages.insert(stage);
            }
        }
    }
    if stages.is_empty() {
        stages.insert(Stage::Run);
    }
    Ok(stages)
}

fn insert_run_artifacts(
    requested: &BTreeSet<ArtifactKey>,
    run: &RunCapture,
    artifacts: &mut BTreeMap<ArtifactKey, ArtifactValue>,
) {
    if requested.contains(&ArtifactKey::Runtime) {
        artifacts.insert(ArtifactKey::Runtime, ArtifactValue::Run(run.clone()));
    }
    if requested.contains(&ArtifactKey::Stdout) {
        artifacts.insert(ArtifactKey::Stdout, ArtifactValue::Text(run.stdout.clone()));
    }
    if requested.contains(&ArtifactKey::Stderr) {
        artifacts.insert(ArtifactKey::Stderr, ArtifactValue::Text(run.stderr.clone()));
    }
    if requested.contains(&ArtifactKey::ExitCode) {
        artifacts.insert(ArtifactKey::ExitCode, ArtifactValue::Int(run.exit_code));
    }
}

fn compare_observations(
    mut left: CompilerObservation,
    mut right: CompilerObservation,
    requested: &BTreeSet<ArtifactKey>,
) -> ComparisonResult {
    let basis = format!(
        "compile-status, diagnostics, runtime{}",
        if requested.is_empty() {
            String::new()
        } else {
            let extras = requested
                .iter()
                .filter(|artifact| {
                    !matches!(artifact, ArtifactKey::Diagnostics | ArtifactKey::Runtime)
                })
                .map(|artifact| artifact.as_str().to_string())
                .collect::<Vec<_>>();
            if extras.is_empty() {
                String::new()
            } else {
                format!(", {}", extras.join(", "))
            }
        }
    );
    left.provenance.comparison_basis = Some(basis.clone());
    right.provenance.comparison_basis = Some(basis.clone());

    let mut differences = Vec::new();
    if left.compile_exit_code != right.compile_exit_code {
        differences.push(ArtifactDifference {
            artifact: "compile-exit-code".into(),
            detail: format!(
                "{}: {}\n{}: {}",
                left.compiler.display_name(),
                left.compile_exit_code,
                right.compiler.display_name(),
                right.compile_exit_code
            ),
        });
    }

    compare_artifact_text(
        &left,
        &right,
        &ArtifactKey::Diagnostics,
        "diagnostics",
        &mut differences,
    );

    if left.compile_exit_code == 0 && right.compile_exit_code == 0 {
        if requested.contains(&ArtifactKey::Runtime) {
            compare_artifact_runtime(&left, &right, &mut differences);
        }
        for artifact in requested {
            match artifact {
                ArtifactKey::Diagnostics | ArtifactKey::Runtime => {}
                ArtifactKey::Stdout | ArtifactKey::Stderr | ArtifactKey::Asm | ArtifactKey::Obj => {
                    compare_artifact_text(
                        &left,
                        &right,
                        artifact,
                        artifact.as_str(),
                        &mut differences,
                    );
                }
                ArtifactKey::ExitCode => {
                    compare_artifact_int(&left, &right, artifact, &mut differences)
                }
                ArtifactKey::Executable => {
                    compare_artifact_path(&left, &right, artifact, &mut differences)
                }
                ArtifactKey::Extra(name) => {
                    compare_artifact_text(&left, &right, artifact, name, &mut differences)
                }
            }
        }
    }

    ComparisonResult {
        left,
        right,
        basis,
        differences,
    }
}

fn compare_artifact_text(
    left: &CompilerObservation,
    right: &CompilerObservation,
    artifact: &ArtifactKey,
    label: &str,
    differences: &mut Vec<ArtifactDifference>,
) {
    let left_text = match left.artifacts.get(artifact) {
        Some(ArtifactValue::Text(text)) => text.as_str(),
        _ => "",
    };
    let right_text = match right.artifacts.get(artifact) {
        Some(ArtifactValue::Text(text)) => text.as_str(),
        _ => "",
    };
    if left_text != right_text {
        differences.push(ArtifactDifference {
            artifact: label.to_string(),
            detail: describe_text_difference(
                left_text,
                right_text,
                &left.compiler.display_name(),
                &right.compiler.display_name(),
            ),
        });
    }
}

fn compare_artifact_runtime(
    left: &CompilerObservation,
    right: &CompilerObservation,
    differences: &mut Vec<ArtifactDifference>,
) {
    let left_run = match left.artifacts.get(&ArtifactKey::Runtime) {
        Some(ArtifactValue::Run(run)) => Some(run),
        _ => None,
    };
    let right_run = match right.artifacts.get(&ArtifactKey::Runtime) {
        Some(ArtifactValue::Run(run)) => Some(run),
        _ => None,
    };
    match (left_run, right_run) {
        (Some(left_run), Some(right_run)) => {
            if normalize_run_signature(left_run) != normalize_run_signature(right_run) {
                differences.push(ArtifactDifference {
                    artifact: "runtime".into(),
                    detail: describe_run_difference(
                        left_run,
                        right_run,
                        &left.compiler.display_name(),
                        &right.compiler.display_name(),
                    ),
                });
            }
        }
        _ => differences.push(ArtifactDifference {
            artifact: "runtime".into(),
            detail: "one side did not produce a runtime result".into(),
        }),
    }
}

fn compare_artifact_int(
    left: &CompilerObservation,
    right: &CompilerObservation,
    artifact: &ArtifactKey,
    differences: &mut Vec<ArtifactDifference>,
) {
    let left_value = match left.artifacts.get(artifact) {
        Some(ArtifactValue::Int(value)) => Some(*value),
        _ => None,
    };
    let right_value = match right.artifacts.get(artifact) {
        Some(ArtifactValue::Int(value)) => Some(*value),
        _ => None,
    };
    if left_value != right_value {
        differences.push(ArtifactDifference {
            artifact: artifact.as_str().to_string(),
            detail: format!(
                "{}: {:?}\n{}: {:?}",
                left.compiler.display_name(),
                left_value,
                right.compiler.display_name(),
                right_value
            ),
        });
    }
}

fn compare_artifact_path(
    left: &CompilerObservation,
    right: &CompilerObservation,
    artifact: &ArtifactKey,
    differences: &mut Vec<ArtifactDifference>,
) {
    let left_path = match left.artifacts.get(artifact) {
        Some(ArtifactValue::Path(path)) => Some(path),
        _ => None,
    };
    let right_path = match right.artifacts.get(artifact) {
        Some(ArtifactValue::Path(path)) => Some(path),
        _ => None,
    };

    match (left_path, right_path) {
        (Some(left_path), Some(right_path)) => {
            let left_bytes = fs::read(left_path);
            let right_bytes = fs::read(right_path);
            match (left_bytes, right_bytes) {
                (Ok(left_bytes), Ok(right_bytes)) => {
                    if left_bytes != right_bytes {
                        differences.push(ArtifactDifference {
                            artifact: artifact.as_str().to_string(),
                            detail: describe_binary_difference(
                                &left_bytes,
                                &right_bytes,
                                &left.compiler.display_name(),
                                &right.compiler.display_name(),
                                left_path,
                                right_path,
                            ),
                        });
                    }
                }
                (Err(left_err), Err(right_err)) => {
                    differences.push(ArtifactDifference {
                        artifact: artifact.as_str().to_string(),
                        detail: format!(
                            "{}: unable to read '{}': {}\n{}: unable to read '{}': {}",
                            left.compiler.display_name(),
                            left_path.display(),
                            left_err,
                            right.compiler.display_name(),
                            right_path.display(),
                            right_err
                        ),
                    });
                }
                (Err(left_err), Ok(_)) => {
                    differences.push(ArtifactDifference {
                        artifact: artifact.as_str().to_string(),
                        detail: format!(
                            "{}: unable to read '{}': {}\n{}: readable '{}'",
                            left.compiler.display_name(),
                            left_path.display(),
                            left_err,
                            right.compiler.display_name(),
                            right_path.display()
                        ),
                    });
                }
                (Ok(_), Err(right_err)) => {
                    differences.push(ArtifactDifference {
                        artifact: artifact.as_str().to_string(),
                        detail: format!(
                            "{}: readable '{}'\n{}: unable to read '{}': {}",
                            left.compiler.display_name(),
                            left_path.display(),
                            right.compiler.display_name(),
                            right_path.display(),
                            right_err
                        ),
                    });
                }
            }
        }
        _ => {
            let left_value = left_path.map(|path| path.display().to_string());
            let right_value = right_path.map(|path| path.display().to_string());
            if left_value != right_value {
                differences.push(ArtifactDifference {
                    artifact: artifact.as_str().to_string(),
                    detail: format!(
                        "{}: {:?}\n{}: {:?}",
                        left.compiler.display_name(),
                        left_value,
                        right.compiler.display_name(),
                        right_value
                    ),
                });
            }
        }
    }
}

fn describe_binary_difference(
    left: &[u8],
    right: &[u8],
    left_label: &str,
    right_label: &str,
    left_path: &Path,
    right_path: &Path,
) -> String {
    let shared = left.len().min(right.len());
    for index in 0..shared {
        if left[index] != right[index] {
            return format!(
                "first differing byte: {}\n{}: {} bytes ({})\n{}: 0x{:02x}\n{}: {} bytes ({})\n{}: 0x{:02x}",
                index,
                left_label,
                left.len(),
                left_path.display(),
                left_label,
                left[index],
                right_label,
                right.len(),
                right_path.display(),
                right_label,
                right[index]
            );
        }
    }

    format!(
        "binary length differs\n{}: {} bytes ({})\n{}: {} bytes ({})",
        left_label,
        left.len(),
        left_path.display(),
        right_label,
        right.len(),
        right_path.display()
    )
}

fn compare_status(result: &ComparisonResult) -> &'static str {
    if result.differences.is_empty() {
        "match"
    } else {
        "diff"
    }
}

fn compare_classification(result: &ComparisonResult) -> &'static str {
    if result.differences.is_empty() {
        return "match";
    }

    let mut has_compile = false;
    let mut has_diagnostics = false;
    let mut has_runtime = false;
    let mut has_artifact = false;

    for difference in &result.differences {
        match difference.artifact.as_str() {
            "compile-exit-code" => has_compile = true,
            "diagnostics" => has_diagnostics = true,
            "runtime" => has_runtime = true,
            _ => has_artifact = true,
        }
    }

    if has_compile {
        if has_runtime || has_artifact {
            "mixed divergence"
        } else {
            "compile divergence"
        }
    } else if has_runtime && !has_diagnostics && !has_artifact {
        "runtime divergence"
    } else if has_artifact && !has_runtime && !has_diagnostics {
        "artifact divergence"
    } else if has_diagnostics && !has_runtime && !has_artifact {
        "diagnostics divergence"
    } else {
        "mixed divergence"
    }
}

fn compare_changed_artifacts(result: &ComparisonResult) -> Vec<String> {
    result
        .differences
        .iter()
        .map(|difference| difference.artifact.clone())
        .collect()
}

fn render_compare_text(result: &ComparisonResult) -> String {
    let changed_artifacts = compare_changed_artifacts(result);
    let mut lines = vec![
        "Compare".to_string(),
        format!("  left: {}", result.left.compiler.display_name()),
        format!("  right: {}", result.right.compiler.display_name()),
        format!("  program: {}", result.left.program.display()),
        format!("  opt: {}", result.left.opt_level.as_str()),
        format!("  status: {}", compare_status(result)),
        format!("  classification: {}", compare_classification(result)),
        format!("  basis: {}", result.basis),
        format!("  difference_count: {}", result.differences.len()),
        format!(
            "  changed_artifacts: {}",
            if changed_artifacts.is_empty() {
                "none".to_string()
            } else {
                changed_artifacts.join(", ")
            }
        ),
        format!(
            "  left_backend: {} ({})",
            result.left.provenance.backend_mode, result.left.provenance.backend_detail
        ),
        format!(
            "  right_backend: {} ({})",
            result.right.provenance.backend_mode, result.right.provenance.backend_detail
        ),
    ];

    if result.differences.is_empty() {
        lines.push(String::new());
        lines.push("No differences detected.".to_string());
    } else {
        for difference in &result.differences {
            lines.push(String::new());
            lines.push(format!("== {} ==", difference.artifact));
            lines.push(difference.detail.clone());
        }
    }

    lines.join("\n")
}

fn print_compare_result(result: &ComparisonResult) {
    println!("{}", render_compare_text(result));
}

fn print_introspection(config: &IntrospectConfig, observed: &ObservedProgram) {
    println!(
        "{}",
        render_introspection_text(
            observed,
            IntrospectionRenderConfig {
                summary_only: config.summary_only,
                max_artifact_lines: config.max_artifact_lines,
            }
        )
    );
}

fn write_compare_reports(config: &CompareConfig, result: &ComparisonResult) -> Result<(), String> {
    if let Some(path) = &config.json_report {
        write_report(path, &render_compare_json(result), "json report")?;
        println!("json report: {}", path.display());
    }
    if let Some(path) = &config.markdown_report {
        write_report(path, &render_compare_markdown(result), "markdown report")?;
        println!("markdown report: {}", path.display());
    }
    Ok(())
}

fn write_introspection_reports(
    config: &IntrospectConfig,
    observed: &ObservedProgram,
) -> Result<(), String> {
    let render_config = IntrospectionRenderConfig {
        summary_only: config.summary_only,
        max_artifact_lines: config.max_artifact_lines,
    };
    if let Some(path) = &config.json_report {
        write_report(path, &render_introspection_json(observed), "json report")?;
        println!("json report: {}", path.display());
    }
    if let Some(path) = &config.markdown_report {
        write_report(
            path,
            &render_introspection_markdown(observed, render_config),
            "markdown report",
        )?;
        println!("markdown report: {}", path.display());
    }
    Ok(())
}

fn introspection_status(observation: &CompilerObservation) -> &'static str {
    if observation.compile_exit_code == 0 {
        "compile ok"
    } else {
        "compile failed"
    }
}

fn diagnostic_excerpt(observation: &CompilerObservation) -> Option<String> {
    match observation.artifacts.get(&ArtifactKey::Diagnostics) {
        Some(ArtifactValue::Text(text)) => text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.to_string()),
        _ => None,
    }
}

fn failure_stage_summary(observation: &CompilerObservation) -> &str {
    observation
        .provenance
        .failure_stage
        .as_deref()
        .unwrap_or("none")
}

fn requested_introspection_artifact_names(observed: &ObservedProgram) -> Vec<String> {
    observed
        .requested_artifacts
        .iter()
        .map(|artifact| artifact.as_str().to_string())
        .collect()
}

fn missing_introspection_artifact_names(observed: &ObservedProgram) -> Vec<String> {
    observed
        .requested_artifacts
        .iter()
        .filter(|artifact| {
            if matches!(artifact, ArtifactKey::Diagnostics)
                && observed.observation.compile_exit_code == 0
                && !observed.observation.artifacts.contains_key(*artifact)
            {
                return false;
            }
            !observed.observation.artifacts.contains_key(*artifact)
        })
        .map(|artifact| artifact.as_str().to_string())
        .collect()
}

fn observation_generic_artifacts<'a>(
    observation: &'a CompilerObservation,
) -> Vec<(String, &'a ArtifactValue)> {
    observation
        .artifacts
        .iter()
        .filter(|(artifact, _)| artifact.is_generic())
        .map(|(artifact, value)| (artifact.as_str().to_string(), value))
        .collect()
}

fn observation_adapter_extras<'a>(
    observation: &'a CompilerObservation,
) -> BTreeMap<String, Vec<(String, &'a ArtifactValue)>> {
    let mut extras = BTreeMap::new();
    for (artifact, value) in &observation.artifacts {
        if let ArtifactKey::Extra(name) = artifact {
            let (namespace, local_name) = artifact
                .extra_parts()
                .map(|(namespace, local_name)| (namespace.to_string(), local_name.to_string()))
                .unwrap_or_else(|| ("extra".to_string(), name.clone()));
            extras
                .entry(namespace)
                .or_insert_with(Vec::new)
                .push((local_name, value));
        }
    }
    extras
}

fn format_artifact_name_list(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

fn format_adapter_extra_summary(
    extras: &BTreeMap<String, Vec<(String, &ArtifactValue)>>,
) -> String {
    if extras.is_empty() {
        return "none".to_string();
    }

    extras
        .iter()
        .map(|(namespace, entries)| {
            format!(
                "{}({})",
                namespace,
                entries
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_named_artifact_map_json(entries: &[(String, &ArtifactValue)]) -> String {
    let mut rendered = String::from("{");
    for (index, (name, value)) in entries.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&format!(
            "\"{}\": {}",
            json_escape(name),
            render_artifact_value_json(value)
        ));
    }
    rendered.push('}');
    rendered
}

fn render_flat_artifacts_json(observation: &CompilerObservation) -> String {
    let mut entries = Vec::new();
    for (artifact, value) in &observation.artifacts {
        entries.push((artifact.as_str().to_string(), value));
    }
    render_named_artifact_map_json(&entries)
}

fn render_artifact_summary_json(value: &ArtifactValue) -> String {
    match value {
        ArtifactValue::Text(text) => format!(
            "{{\"kind\":\"text\",\"summary\":\"{}\",\"line_count\":{},\"char_count\":{}}}",
            json_escape(&artifact_value_summary(value)),
            text_line_count(text),
            text.len()
        ),
        ArtifactValue::Int(number) => format!(
            "{{\"kind\":\"int\",\"summary\":\"{}\",\"value\":{}}}",
            json_escape(&artifact_value_summary(value)),
            number
        ),
        ArtifactValue::Run(run) => format!(
            "{{\"kind\":\"runtime\",\"summary\":\"{}\",\"exit_code\":{},\"stdout_lines\":{},\"stderr_lines\":{}}}",
            json_escape(&artifact_value_summary(value)),
            run.exit_code,
            text_line_count(&run.stdout),
            text_line_count(&run.stderr)
        ),
        ArtifactValue::Path(path) => match fs::metadata(path) {
            Ok(metadata) => format!(
                "{{\"kind\":\"path\",\"summary\":\"{}\",\"byte_count\":{}}}",
                json_escape(&artifact_value_summary(value)),
                metadata.len()
            ),
            Err(_) => format!(
                "{{\"kind\":\"path\",\"summary\":\"{}\",\"byte_count\":null}}",
                json_escape(&artifact_value_summary(value))
            ),
        },
    }
}

fn render_artifact_summaries_json(observation: &CompilerObservation) -> String {
    let mut rendered = String::from("{");
    for (index, (artifact, value)) in observation.artifacts.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&format!(
            "\"{}\": {}",
            json_escape(artifact.as_str()),
            render_artifact_summary_json(value)
        ));
    }
    rendered.push('}');
    rendered
}

fn render_adapter_extra_summary_json(
    extras: &BTreeMap<String, Vec<(String, &ArtifactValue)>>,
) -> String {
    let mut rendered = String::from("{");
    for (index, (namespace, entries)) in extras.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        let names = entries
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        rendered.push_str(&format!(
            "\"{}\": {}",
            json_escape(namespace),
            json_string_array(&names)
        ));
    }
    rendered.push('}');
    rendered
}

fn render_namespaced_artifacts_json(
    extras: &BTreeMap<String, Vec<(String, &ArtifactValue)>>,
) -> String {
    let mut rendered = String::from("{");
    for (index, (namespace, entries)) in extras.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&format!(
            "\"{}\": {}",
            json_escape(namespace),
            render_named_artifact_map_json(entries)
        ));
    }
    rendered.push('}');
    rendered
}

fn text_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

fn render_config_summary(config: IntrospectionRenderConfig) -> String {
    if config.summary_only {
        "summary-only".to_string()
    } else if let Some(limit) = config.max_artifact_lines {
        format!("first {} lines per artifact", limit)
    } else {
        "full artifact bodies".to_string()
    }
}

fn artifact_value_summary(value: &ArtifactValue) -> String {
    match value {
        ArtifactValue::Text(text) => {
            format!(
                "text, {} lines, {} chars",
                text_line_count(text),
                text.len()
            )
        }
        ArtifactValue::Int(value) => format!("int, value {}", value),
        ArtifactValue::Run(run) => format!(
            "runtime, exit {}, stdout {} lines, stderr {} lines",
            run.exit_code,
            text_line_count(&run.stdout),
            text_line_count(&run.stderr)
        ),
        ArtifactValue::Path(path) => match fs::metadata(path) {
            Ok(metadata) => format!("path, {} bytes", metadata.len()),
            Err(_) => "path".to_string(),
        },
    }
}

fn render_artifact_body_lines(
    value: &ArtifactValue,
    config: IntrospectionRenderConfig,
) -> Vec<String> {
    if config.summary_only {
        return vec!["[content omitted by --summary-only]".to_string()];
    }

    let rendered = render_artifact_value_text(value);
    let mut lines = rendered
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return vec!["<empty>".to_string()];
    }

    if let Some(limit) = config.max_artifact_lines {
        if lines.len() > limit {
            let total = lines.len();
            lines.truncate(limit);
            lines.push(format!(
                "... (truncated; showing first {} of {} lines)",
                limit, total
            ));
        }
    }

    lines
}

fn render_introspection_text(
    observed: &ObservedProgram,
    render_config: IntrospectionRenderConfig,
) -> String {
    let observation = &observed.observation;
    let generic_artifacts = observation_generic_artifacts(observation);
    let generic_names = generic_artifacts
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let adapter_extras = observation_adapter_extras(observation);
    let requested_artifacts = requested_introspection_artifact_names(observed);
    let missing_artifacts = missing_introspection_artifact_names(observed);
    let diagnostic_excerpt = diagnostic_excerpt(observation);
    let mut lines = vec![
        "Introspect".to_string(),
        format!("  status: {}", introspection_status(observation)),
        format!("  compiler: {}", observation.compiler.display_name()),
        format!("  program: {}", observation.program.display()),
        format!("  opt: {}", observation.opt_level.as_str()),
        format!("  compile_exit_code: {}", observation.compile_exit_code),
        format!("  adapter_kind: {}", observation.provenance.adapter_kind),
        format!("  backend_mode: {}", observation.provenance.backend_mode),
        format!(
            "  backend_detail: {}",
            observation.provenance.backend_detail
        ),
        format!("  failure_stage: {}", failure_stage_summary(observation)),
        format!(
            "  diagnostic_excerpt: {}",
            diagnostic_excerpt
                .clone()
                .unwrap_or_else(|| "none".to_string())
        ),
        format!("  content_mode: {}", render_config_summary(render_config)),
        format!("  artifact_count: {}", observation.artifacts.len()),
        format!(
            "  requested_artifacts: {}",
            format_artifact_name_list(&requested_artifacts)
        ),
        format!(
            "  missing_artifacts: {}",
            format_artifact_name_list(&missing_artifacts)
        ),
        format!(
            "  generic_artifacts: {}",
            format_artifact_name_list(&generic_names)
        ),
        format!(
            "  adapter_extras: {}",
            format_adapter_extra_summary(&adapter_extras)
        ),
    ];
    if !observation.provenance.artifacts_captured.is_empty() {
        lines.push(format!(
            "  captured_artifacts: {}",
            observation.provenance.artifacts_captured.join(", ")
        ));
    }

    if !generic_artifacts.is_empty() {
        lines.push(String::new());
        lines.push("Generic artifacts".to_string());
        for (artifact, value) in generic_artifacts {
            lines.push(String::new());
            lines.push(format!("== {} ==", artifact));
            lines.push(format!("summary: {}", artifact_value_summary(value)));
            lines.extend(render_artifact_body_lines(value, render_config));
        }
    }

    if !adapter_extras.is_empty() {
        lines.push(String::new());
        lines.push("Adapter extras".to_string());
        for (namespace, entries) in adapter_extras {
            lines.push(String::new());
            lines.push(format!("-- {} --", namespace));
            for (name, value) in entries {
                lines.push(String::new());
                lines.push(format!("== {} ==", name));
                lines.push(format!("summary: {}", artifact_value_summary(value)));
                lines.extend(render_artifact_body_lines(value, render_config));
            }
        }
    }
    lines.join("\n")
}

fn render_compare_json(result: &ComparisonResult) -> String {
    let changed_artifacts = compare_changed_artifacts(result);
    format!(
        "{{\n  \"status\": \"{}\",\n  \"classification\": \"{}\",\n  \"difference_count\": {},\n  \"changed_artifacts\": {},\n  \"basis\": \"{}\",\n  \"left\": {},\n  \"right\": {},\n  \"differences\": {}\n}}\n",
        compare_status(result),
        compare_classification(result),
        result.differences.len(),
        json_string_array(&changed_artifacts),
        json_escape(&result.basis),
        render_observation_json(&result.left),
        render_observation_json(&result.right),
        render_differences_json(&result.differences)
    )
}

fn render_compare_markdown(result: &ComparisonResult) -> String {
    let changed_artifacts = compare_changed_artifacts(result);
    let mut lines = vec![
        "# bencch compare report".to_string(),
        String::new(),
        format!("status: {}", compare_status(result)),
        format!("classification: {}", compare_classification(result)),
        format!(
            "compilers: `{}` vs `{}`",
            result.left.compiler.display_name(),
            result.right.compiler.display_name()
        ),
        format!("basis: {}", result.basis),
        format!("difference_count: {}", result.differences.len()),
        format!(
            "changed_artifacts: {}",
            if changed_artifacts.is_empty() {
                "none".to_string()
            } else {
                changed_artifacts.join(", ")
            }
        ),
        String::new(),
        "## Left".to_string(),
        render_observation_markdown(&result.left),
        String::new(),
        "## Right".to_string(),
        render_observation_markdown(&result.right),
        String::new(),
        "## Differences".to_string(),
    ];
    if result.differences.is_empty() {
        lines.push("none".to_string());
    } else {
        for difference in &result.differences {
            lines.push(String::new());
            lines.push(format!("### `{}`", difference.artifact));
            lines.push("```text".to_string());
            lines.extend(difference.detail.lines().map(|line| line.to_string()));
            lines.push("```".to_string());
        }
    }
    lines.join("\n") + "\n"
}

fn render_introspection_json(observed: &ObservedProgram) -> String {
    let observation = &observed.observation;
    let generic_artifacts = observation_generic_artifacts(observation);
    let generic_names = generic_artifacts
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let adapter_extras = observation_adapter_extras(observation);
    let requested_artifacts = requested_introspection_artifact_names(observed);
    let missing_artifacts = missing_introspection_artifact_names(observed);
    let diagnostic_excerpt = diagnostic_excerpt(observation);
    format!(
        "{{\n  \"status\": \"{}\",\n  \"compiler\": \"{}\",\n  \"program\": \"{}\",\n  \"opt\": \"{}\",\n  \"compile_exit_code\": {},\n  \"failure\": {{\n    \"stage\": {},\n    \"diagnostic_excerpt\": {}\n  }},\n  \"artifact_summary\": {{\n    \"artifact_count\": {},\n    \"requested_artifacts\": {},\n    \"captured_artifacts\": {},\n    \"missing_artifacts\": {},\n    \"generic_artifacts\": {},\n    \"adapter_extras\": {}\n  }},\n  \"provenance\": {{\n    \"compiler_identity\": \"{}\",\n    \"adapter_kind\": \"{}\",\n    \"backend_mode\": \"{}\",\n    \"backend_detail\": \"{}\",\n    \"artifacts_captured\": {},\n    \"comparison_basis\": {},\n    \"failure_stage\": {}\n  }},\n  \"artifact_summaries\": {},\n  \"generic_artifacts\": {},\n  \"adapter_extras\": {},\n  \"artifacts\": {}\n}}\n",
        json_escape(introspection_status(observation)),
        json_escape(&observation.compiler.display_name()),
        json_escape(&observation.program.display().to_string()),
        observation.opt_level.as_str(),
        observation.compile_exit_code,
        match &observation.provenance.failure_stage {
            Some(stage) => format!("\"{}\"", json_escape(stage)),
            None => "null".to_string(),
        },
        match &diagnostic_excerpt {
            Some(line) => format!("\"{}\"", json_escape(line)),
            None => "null".to_string(),
        },
        observation.artifacts.len(),
        json_string_array(&requested_artifacts),
        json_string_array(&observation.provenance.artifacts_captured),
        json_string_array(&missing_artifacts),
        json_string_array(&generic_names),
        render_adapter_extra_summary_json(&adapter_extras),
        json_escape(&observation.provenance.compiler_identity),
        json_escape(&observation.provenance.adapter_kind),
        json_escape(&observation.provenance.backend_mode),
        json_escape(&observation.provenance.backend_detail),
        json_string_array(&observation.provenance.artifacts_captured),
        match &observation.provenance.comparison_basis {
            Some(basis) => format!("\"{}\"", json_escape(basis)),
            None => "null".to_string(),
        },
        match &observation.provenance.failure_stage {
            Some(stage) => format!("\"{}\"", json_escape(stage)),
            None => "null".to_string(),
        },
        render_artifact_summaries_json(observation),
        render_named_artifact_map_json(&generic_artifacts),
        render_namespaced_artifacts_json(&adapter_extras),
        render_flat_artifacts_json(observation)
    )
}

fn render_introspection_markdown(
    observed: &ObservedProgram,
    render_config: IntrospectionRenderConfig,
) -> String {
    let observation = &observed.observation;
    let generic_artifacts = observation_generic_artifacts(observation);
    let generic_names = generic_artifacts
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let adapter_extras = observation_adapter_extras(observation);
    let requested_artifacts = requested_introspection_artifact_names(observed);
    let missing_artifacts = missing_introspection_artifact_names(observed);
    let diagnostic_excerpt = diagnostic_excerpt(observation);
    let mut lines = vec![
        "# bencch introspect report".to_string(),
        String::new(),
        format!("status: {}", introspection_status(observation)),
        format!("compiler: `{}`", observation.compiler.display_name()),
        format!("program: `{}`", observation.program.display()),
        format!("opt: `{}`", observation.opt_level.as_str()),
        format!("compile_exit_code: `{}`", observation.compile_exit_code),
        format!("adapter_kind: `{}`", observation.provenance.adapter_kind),
        format!("backend_mode: `{}`", observation.provenance.backend_mode),
        format!("backend_detail: {}", observation.provenance.backend_detail),
        format!("failure_stage: `{}`", failure_stage_summary(observation)),
        format!(
            "diagnostic_excerpt: {}",
            diagnostic_excerpt
                .map(|line| format!("`{}`", line))
                .unwrap_or_else(|| "none".to_string())
        ),
        format!("content_mode: `{}`", render_config_summary(render_config)),
        format!("artifact_count: {}", observation.artifacts.len()),
        format!(
            "requested_artifacts: {}",
            if requested_artifacts.is_empty() {
                "none".to_string()
            } else {
                format!("`{}`", requested_artifacts.join("`, `"))
            }
        ),
        format!(
            "missing_artifacts: {}",
            if missing_artifacts.is_empty() {
                "none".to_string()
            } else {
                format!("`{}`", missing_artifacts.join("`, `"))
            }
        ),
        format!(
            "generic_artifacts: {}",
            if generic_names.is_empty() {
                "none".to_string()
            } else {
                format!("`{}`", generic_names.join("`, `"))
            }
        ),
        format!(
            "adapter_extras: {}",
            format_adapter_extra_summary(&adapter_extras)
        ),
    ];
    if !observation.provenance.artifacts_captured.is_empty() {
        lines.push(format!(
            "captured_artifacts: `{}`",
            observation.provenance.artifacts_captured.join("`, `")
        ));
    }

    if !generic_artifacts.is_empty() {
        lines.push(String::new());
        lines.push("## Generic artifacts".to_string());
        for (artifact, value) in generic_artifacts {
            lines.push(String::new());
            lines.push(format!("### `{}`", artifact));
            lines.push(format!("summary: {}", artifact_value_summary(value)));
            lines.push("```text".to_string());
            lines.extend(render_artifact_body_lines(value, render_config));
            lines.push("```".to_string());
        }
    }

    if !adapter_extras.is_empty() {
        lines.push(String::new());
        lines.push("## Adapter extras".to_string());
        for (namespace, entries) in adapter_extras {
            lines.push(String::new());
            lines.push(format!("### `{}`", namespace));
            for (name, value) in entries {
                lines.push(String::new());
                lines.push(format!("#### `{}`", name));
                lines.push(format!("summary: {}", artifact_value_summary(value)));
                lines.push("```text".to_string());
                lines.extend(render_artifact_body_lines(value, render_config));
                lines.push("```".to_string());
            }
        }
    }

    lines.join("\n") + "\n"
}

fn render_observation_json(observation: &CompilerObservation) -> String {
    let mut lines = vec![
        "{".to_string(),
        format!(
            "  \"compiler\": \"{}\",",
            json_escape(&observation.compiler.display_name())
        ),
        format!(
            "  \"program\": \"{}\",",
            json_escape(&observation.program.display().to_string())
        ),
        format!("  \"opt\": \"{}\",", observation.opt_level.as_str()),
        format!(
            "  \"compile_exit_code\": {},",
            observation.compile_exit_code
        ),
        "  \"provenance\": {".to_string(),
        format!(
            "    \"compiler_identity\": \"{}\",",
            json_escape(&observation.provenance.compiler_identity)
        ),
        format!(
            "    \"adapter_kind\": \"{}\",",
            json_escape(&observation.provenance.adapter_kind)
        ),
        format!(
            "    \"backend_mode\": \"{}\",",
            json_escape(&observation.provenance.backend_mode)
        ),
        format!(
            "    \"backend_detail\": \"{}\",",
            json_escape(&observation.provenance.backend_detail)
        ),
        format!(
            "    \"artifacts_captured\": {},",
            json_string_array(&observation.provenance.artifacts_captured)
        ),
        match &observation.provenance.failure_stage {
            Some(stage) => format!("    \"failure_stage\": \"{}\",", json_escape(stage)),
            None => "    \"failure_stage\": null,".to_string(),
        },
        match &observation.provenance.comparison_basis {
            Some(basis) => format!("    \"comparison_basis\": \"{}\"", json_escape(basis)),
            None => "    \"comparison_basis\": null".to_string(),
        },
        "  },".to_string(),
        "  \"artifacts\": {".to_string(),
    ];
    for (index, (artifact, value)) in observation.artifacts.iter().enumerate() {
        lines.push(format!(
            "    \"{}\": {}{}",
            json_escape(artifact.as_str()),
            render_artifact_value_json(value),
            if index + 1 == observation.artifacts.len() {
                ""
            } else {
                ","
            }
        ));
    }
    lines.push("  }".to_string());
    lines.push("}".to_string());
    lines.join("\n")
}

fn render_observation_markdown(observation: &CompilerObservation) -> String {
    let mut lines = vec![
        format!("compiler: `{}`", observation.compiler.display_name()),
        format!("program: `{}`", observation.program.display()),
        format!("opt: `{}`", observation.opt_level.as_str()),
        format!("compile_exit_code: `{}`", observation.compile_exit_code),
        format!("adapter_kind: `{}`", observation.provenance.adapter_kind),
        format!("backend_mode: `{}`", observation.provenance.backend_mode),
        format!("backend_detail: {}", observation.provenance.backend_detail),
        format!("failure_stage: `{}`", failure_stage_summary(observation)),
    ];
    if !observation.provenance.artifacts_captured.is_empty() {
        lines.push(format!(
            "artifacts: `{}`",
            observation.provenance.artifacts_captured.join("`, `")
        ));
    }
    for (artifact, value) in &observation.artifacts {
        lines.push(String::new());
        lines.push(format!("## `{}`", artifact.as_str()));
        lines.push("```text".to_string());
        lines.extend(
            render_artifact_value_text(value)
                .lines()
                .map(|line| line.to_string()),
        );
        lines.push("```".to_string());
    }
    lines.join("\n")
}

fn render_differences_json(differences: &[ArtifactDifference]) -> String {
    let mut rendered = String::from("[");
    for (index, difference) in differences.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&format!(
            "{{\"artifact\":\"{}\",\"detail\":\"{}\"}}",
            json_escape(&difference.artifact),
            json_escape(&difference.detail)
        ));
    }
    rendered.push(']');
    rendered
}

fn render_artifact_value_json(value: &ArtifactValue) -> String {
    match value {
        ArtifactValue::Text(text) => {
            format!("{{\"kind\":\"text\",\"value\":\"{}\"}}", json_escape(text))
        }
        ArtifactValue::Int(value) => format!("{{\"kind\":\"int\",\"value\":{}}}", value),
        ArtifactValue::Run(run) => format!(
            "{{\"kind\":\"runtime\",\"exit_code\":{},\"stdout\":\"{}\",\"stderr\":\"{}\"}}",
            run.exit_code,
            json_escape(&run.stdout),
            json_escape(&run.stderr)
        ),
        ArtifactValue::Path(path) => format!(
            "{{\"kind\":\"path\",\"value\":\"{}\"}}",
            json_escape(&path.display().to_string())
        ),
    }
}

fn render_artifact_value_text(value: &ArtifactValue) -> String {
    match value {
        ArtifactValue::Text(text) => text.trim_end().to_string(),
        ArtifactValue::Int(value) => value.to_string(),
        ArtifactValue::Run(run) => format_run_capture(run),
        ArtifactValue::Path(path) => path.display().to_string(),
    }
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

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn doctor_report_fields(config: &DoctorConfig) -> Vec<(String, String)> {
    let workspace_root = workspace_root();
    let suite_root = default_suite_root();
    let report_root = default_report_root();
    let armfortas = config.tools.armfortas_adapters();
    let observable_backend = config
        .tools
        .cli_observable_capture_backend(report_root.join(".tmp").join("doctor"));
    let capture_root = armfortas.capture_root();
    let capture_manifest = capture_root.as_ref().map(|root| root.join("Cargo.toml"));

    let mut fields = vec![
        ("workspace_root".to_string(), display_path(&workspace_root)),
        ("suite_root".to_string(), display_path(&suite_root)),
        ("report_root".to_string(), display_path(&report_root)),
        (
            "armfortas_cli_adapter".to_string(),
            armfortas.cli_description().to_string(),
        ),
        (
            "armfortas_capture_adapter".to_string(),
            armfortas.capture_description().to_string(),
        ),
        (
            "primary_backend_full".to_string(),
            armfortas.capture_description().to_string(),
        ),
        (
            "primary_backend_observable".to_string(),
            observable_backend.description().to_string(),
        ),
        (
            "armfortas_capture_root".to_string(),
            capture_root
                .as_ref()
                .map(|root| display_path(root))
                .unwrap_or_else(|| "unavailable".to_string()),
        ),
        (
            "armfortas_capture_manifest".to_string(),
            capture_manifest
                .as_ref()
                .map(|manifest| {
                    if manifest.exists() {
                        display_path(manifest)
                    } else {
                        "missing".to_string()
                    }
                })
                .unwrap_or_else(|| "unavailable".to_string()),
        ),
        (
            "armfortas_cli_mode".to_string(),
            armfortas.cli_mode_name().to_string(),
        ),
    ];
    match armfortas.cli() {
        ArmfortasCliAdapter::Linked => {
            fields.push((
                "armfortas_cli_status".to_string(),
                capture_root
                    .as_ref()
                    .map(|root| format!("linked via Cargo to {}", display_path(root)))
                    .unwrap_or_else(|| {
                        "linked adapter requested but unavailable in this build".to_string()
                    }),
            ));
        }
        ArmfortasCliAdapter::External(binary) => {
            fields.push((
                "armfortas_cli_status".to_string(),
                tool_probe_status(binary, false),
            ));
        }
    }
    fields.push((
        "armfortas_capture_mode".to_string(),
        armfortas.capture_mode_name().to_string(),
    ));
    fields.push((
        "armfortas_capture_status".to_string(),
        capture_root
            .as_ref()
            .map(|root| format!("linked via Cargo to {}", display_path(root)))
            .unwrap_or_else(|| {
                "unavailable in this build; use scripts/bootstrap-linked-armfortas.sh".to_string()
            }),
    ));
    fields.push((
        "primary_backend_selection".to_string(),
        "observable backend is selected for asm/obj/run-only cells when the armfortas CLI is external and the case does not require expect-fail or capture-consistency semantics; otherwise full backend".to_string(),
    ));
    fields.push((
        "named_compiler.armfortas".to_string(),
        format!(
            "cli={} capture={}",
            armfortas.cli_mode_name(),
            armfortas.capture_mode_name()
        ),
    ));
    let armfortas_capabilities = compiler_capabilities(
        &CompilerSpec::Named(NamedCompiler::Armfortas),
        &config.tools,
    );
    fields.push((
        "named_compiler.armfortas.generic_artifacts".to_string(),
        format_artifact_name_list(&armfortas_capabilities.generic_artifacts()),
    ));
    fields.push((
        "named_compiler.armfortas.adapter_extras".to_string(),
        capability_extra_summary(&armfortas_capabilities.adapter_extras()),
    ));
    fields.push((
        "named_compiler.armfortas.unavailable_artifacts".to_string(),
        capability_unavailable_summary(&armfortas_capabilities),
    ));
    fields.push((
        "named_compiler.gfortran".to_string(),
        tool_probe_status(&config.tools.gfortran, false),
    ));
    let gfortran_capabilities =
        compiler_capabilities(&CompilerSpec::Named(NamedCompiler::Gfortran), &config.tools);
    fields.push((
        "named_compiler.gfortran.generic_artifacts".to_string(),
        format_artifact_name_list(&gfortran_capabilities.generic_artifacts()),
    ));
    fields.push((
        "named_compiler.gfortran.adapter_extras".to_string(),
        capability_extra_summary(&gfortran_capabilities.adapter_extras()),
    ));
    fields.push((
        "named_compiler.flang-new".to_string(),
        tool_probe_status(&config.tools.flang_new, false),
    ));
    let flang_capabilities =
        compiler_capabilities(&CompilerSpec::Named(NamedCompiler::FlangNew), &config.tools);
    fields.push((
        "named_compiler.flang-new.generic_artifacts".to_string(),
        format_artifact_name_list(&flang_capabilities.generic_artifacts()),
    ));
    fields.push((
        "named_compiler.flang-new.adapter_extras".to_string(),
        capability_extra_summary(&flang_capabilities.adapter_extras()),
    ));
    fields.push((
        "explicit_compiler_path".to_string(),
        "any filesystem path passed to compare/introspect uses the generic external-driver adapter"
            .to_string(),
    ));
    let explicit_capabilities = compiler_capabilities(
        &CompilerSpec::Binary(PathBuf::from("/path/to/compiler")),
        &config.tools,
    );
    fields.push((
        "explicit_compiler_path.generic_artifacts".to_string(),
        format_artifact_name_list(&explicit_capabilities.generic_artifacts()),
    ));
    fields.push((
        "explicit_compiler_path.adapter_extras".to_string(),
        capability_extra_summary(&explicit_capabilities.adapter_extras()),
    ));
    fields.push((
        "gfortran".to_string(),
        tool_probe_status(&config.tools.gfortran, false),
    ));
    fields.push((
        "flang-new".to_string(),
        tool_probe_status(&config.tools.flang_new, false),
    ));
    fields.push((
        "as".to_string(),
        tool_probe_status(&config.tools.system_as, false),
    ));
    fields.push((
        "otool".to_string(),
        tool_probe_status(&config.tools.otool, false),
    ));
    fields.push(("nm".to_string(), tool_probe_status(&config.tools.nm, false)));
    fields.push((
        "note".to_string(),
        if capture_root.is_some() {
            "linked capture still depends on the surrounding armfortas checkout".to_string()
        } else {
            "linked capture is unavailable in this build; external compiler compare/introspect surfaces still work".to_string()
        },
    ));
    fields.push((
        if capture_root.is_some() {
            "linked_mode_surface".to_string()
        } else {
            "external_only_surface".to_string()
        },
        if capture_root.is_some() {
            "rich armfortas stages, legacy frontend/module suites, capture consistency".to_string()
        } else {
            "compare, introspect, generic suite-v2, observable-only run cells".to_string()
        },
    ));
    fields.push((
        if capture_root.is_some() {
            "external_only_limits".to_string()
        } else {
            "linked_only_surface".to_string()
        },
        if capture_root.is_some() {
            "none in this build".to_string()
        } else {
            "armfortas.* extras, legacy frontend/module suites, capture consistency".to_string()
        },
    ));

    fields
}

fn render_doctor_report(config: &DoctorConfig) -> String {
    let mut lines = vec!["Doctor".to_string()];
    for (field, value) in doctor_report_fields(config) {
        lines.push(format!("  {}: {}", field, value));
    }
    lines.join("\n")
}

fn render_doctor_capabilities_json(capabilities: &CompilerCapabilities) -> String {
    let unavailable = capabilities
        .unavailable_artifacts
        .iter()
        .map(|(artifact, reason)| {
            format!(
                "{{\"artifact\":\"{}\",\"reason\":\"{}\"}}",
                json_escape(artifact.as_str()),
                json_escape(reason)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{{\"generic_artifacts\":{},\"adapter_extras\":{},\"unavailable_artifacts\":[{}]}}",
        json_string_iter(
            capabilities
                .generic_artifacts()
                .iter()
                .map(|artifact| artifact.as_str())
        ),
        json_string_vec_map(&capabilities.adapter_extras()),
        unavailable
    )
}

fn json_string_vec_map(map: &BTreeMap<String, Vec<String>>) -> String {
    let mut rendered = String::from("{");
    for (index, (key, values)) in map.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push('"');
        rendered.push_str(&json_escape(key));
        rendered.push_str("\": ");
        rendered.push_str(&json_string_iter(values.iter().map(|value| value.as_str())));
    }
    rendered.push('}');
    rendered
}

fn render_doctor_json(config: &DoctorConfig) -> String {
    let fields = doctor_report_fields(config);
    let workspace_root = workspace_root();
    let suite_root = default_suite_root();
    let report_root = default_report_root();
    let armfortas = config.tools.armfortas_adapters();
    let observable_backend = config
        .tools
        .cli_observable_capture_backend(report_root.join(".tmp").join("doctor"));
    let capture_root = armfortas.capture_root();
    let capture_manifest = capture_root.as_ref().map(|root| root.join("Cargo.toml"));
    let armfortas_capabilities = compiler_capabilities(
        &CompilerSpec::Named(NamedCompiler::Armfortas),
        &config.tools,
    );
    let gfortran_capabilities =
        compiler_capabilities(&CompilerSpec::Named(NamedCompiler::Gfortran), &config.tools);
    let flang_capabilities =
        compiler_capabilities(&CompilerSpec::Named(NamedCompiler::FlangNew), &config.tools);
    let explicit_capabilities = compiler_capabilities(
        &CompilerSpec::Binary(PathBuf::from("/path/to/compiler")),
        &config.tools,
    );
    let mut lines = vec![
        "{".to_string(),
        "  \"command\": \"doctor\",".to_string(),
        "  \"workspace\": {".to_string(),
        format!(
            "    \"workspace_root\": \"{}\",",
            json_escape(&display_path(&workspace_root))
        ),
        format!(
            "    \"suite_root\": \"{}\",",
            json_escape(&display_path(&suite_root))
        ),
        format!(
            "    \"report_root\": \"{}\"",
            json_escape(&display_path(&report_root))
        ),
        "  },".to_string(),
        "  \"armfortas\": {".to_string(),
        format!(
            "    \"cli_adapter\": \"{}\",",
            json_escape(armfortas.cli_description())
        ),
        format!(
            "    \"capture_adapter\": \"{}\",",
            json_escape(armfortas.capture_description())
        ),
        format!(
            "    \"cli_mode\": \"{}\",",
            json_escape(armfortas.cli_mode_name())
        ),
        format!(
            "    \"cli_status\": \"{}\",",
            json_escape(
                &match armfortas.cli() {
                    ArmfortasCliAdapter::Linked => capture_root
                        .as_ref()
                        .map(|root| format!("linked via Cargo to {}", display_path(root)))
                        .unwrap_or_else(|| {
                            "linked adapter requested but unavailable in this build".to_string()
                        }),
                    ArmfortasCliAdapter::External(binary) => tool_probe_status(binary, false),
                }
            )
        ),
        format!(
            "    \"capture_mode\": \"{}\",",
            json_escape(armfortas.capture_mode_name())
        ),
        format!(
            "    \"capture_status\": \"{}\",",
            json_escape(
                &capture_root
                    .as_ref()
                    .map(|root| format!("linked via Cargo to {}", display_path(root)))
                    .unwrap_or_else(|| {
                        "unavailable in this build; use scripts/bootstrap-linked-armfortas.sh"
                            .to_string()
                    })
            )
        ),
        format!(
            "    \"capture_root\": {},",
            match capture_root.as_ref() {
                Some(root) => format!("\"{}\"", json_escape(&display_path(root))),
                None => "null".to_string(),
            }
        ),
        format!(
            "    \"capture_manifest\": \"{}\"",
            json_escape(
                &capture_manifest
                    .as_ref()
                    .map(|manifest| {
                        if manifest.exists() {
                            display_path(manifest)
                        } else {
                            "missing".to_string()
                        }
                    })
                    .unwrap_or_else(|| "unavailable".to_string())
            )
        ),
        "  },".to_string(),
        "  \"primary_backends\": {".to_string(),
        format!(
            "    \"full\": \"{}\",",
            json_escape(armfortas.capture_description())
        ),
        format!(
            "    \"observable\": \"{}\",",
            json_escape(observable_backend.description())
        ),
        format!(
            "    \"selection\": \"{}\"",
            json_escape("observable backend is selected for asm/obj/run-only cells when the armfortas CLI is external and the case does not require expect-fail or capture-consistency semantics; otherwise full backend")
        ),
        "  },".to_string(),
        "  \"named_compilers\": {".to_string(),
        format!(
            "    \"armfortas\": {{\"surface\":\"{}\",\"capabilities\":{}}},",
            json_escape(&format!(
                "cli={} capture={}",
                armfortas.cli_mode_name(),
                armfortas.capture_mode_name()
            )),
            render_doctor_capabilities_json(&armfortas_capabilities)
        ),
        format!(
            "    \"gfortran\": {{\"status\":\"{}\",\"capabilities\":{}}},",
            json_escape(&tool_probe_status(&config.tools.gfortran, false)),
            render_doctor_capabilities_json(&gfortran_capabilities)
        ),
        format!(
            "    \"flang-new\": {{\"status\":\"{}\",\"capabilities\":{}}}",
            json_escape(&tool_probe_status(&config.tools.flang_new, false)),
            render_doctor_capabilities_json(&flang_capabilities)
        ),
        "  },".to_string(),
        format!(
            "  \"explicit_compiler_path\": {{\"description\":\"{}\",\"capabilities\":{}}},",
            json_escape(
                "any filesystem path passed to compare/introspect uses the generic external-driver adapter"
            ),
            render_doctor_capabilities_json(&explicit_capabilities)
        ),
        "  \"tools\": {".to_string(),
        format!(
            "    \"gfortran\": \"{}\",",
            json_escape(&tool_probe_status(&config.tools.gfortran, false))
        ),
        format!(
            "    \"flang-new\": \"{}\",",
            json_escape(&tool_probe_status(&config.tools.flang_new, false))
        ),
        format!(
            "    \"as\": \"{}\",",
            json_escape(&tool_probe_status(&config.tools.system_as, false))
        ),
        format!(
            "    \"otool\": \"{}\",",
            json_escape(&tool_probe_status(&config.tools.otool, false))
        ),
        format!(
            "    \"nm\": \"{}\"",
            json_escape(&tool_probe_status(&config.tools.nm, false))
        ),
        "  },".to_string(),
        "  \"mode\": {".to_string(),
        format!(
            "    \"note\": \"{}\",",
            json_escape(
                if capture_root.is_some() {
                    "linked capture still depends on the surrounding armfortas checkout"
                } else {
                    "linked capture is unavailable in this build; external compiler compare/introspect surfaces still work"
                }
            )
        ),
        format!(
            "    \"surface_key\": \"{}\",",
            if capture_root.is_some() {
                "linked_mode_surface"
            } else {
                "external_only_surface"
            }
        ),
        format!(
            "    \"surface_value\": \"{}\",",
            json_escape(
                if capture_root.is_some() {
                    "rich armfortas stages, legacy frontend/module suites, capture consistency"
                } else {
                    "compare, introspect, generic suite-v2, observable-only run cells"
                }
            )
        ),
        format!(
            "    \"limits_key\": \"{}\",",
            if capture_root.is_some() {
                "external_only_limits"
            } else {
                "linked_only_surface"
            }
        ),
        format!(
            "    \"limits_value\": \"{}\"",
            json_escape(
                if capture_root.is_some() {
                    "none in this build"
                } else {
                    "armfortas.* extras, legacy frontend/module suites, capture consistency"
                }
            )
        ),
        "  },".to_string(),
        "  \"fields\": {".to_string(),
    ];
    for (index, (field, value)) in fields.iter().enumerate() {
        lines.push(format!(
            "    \"{}\": \"{}\"{}",
            json_escape(field),
            json_escape(value),
            if index + 1 == fields.len() { "" } else { "," }
        ));
    }
    lines.push("  }".to_string());
    lines.push("}".to_string());
    lines.join("\n") + "\n"
}

fn render_doctor_markdown(config: &DoctorConfig) -> String {
    let mut lines = vec![
        "# bencch doctor report".to_string(),
        String::new(),
        "| field | value |".to_string(),
        "| --- | --- |".to_string(),
    ];
    for (field, value) in doctor_report_fields(config) {
        lines.push(format!(
            "| `{}` | {} |",
            field,
            doctor_markdown_cell(&value)
        ));
    }
    lines.join("\n") + "\n"
}

fn write_doctor_reports(config: &DoctorConfig) -> Result<(), String> {
    if let Some(path) = &config.json_report {
        write_report(path, &render_doctor_json(config), "json report")?;
        println!("json report: {}", path.display());
    }
    if let Some(path) = &config.markdown_report {
        write_report(path, &render_doctor_markdown(config), "markdown report")?;
        println!("markdown report: {}", path.display());
    }
    Ok(())
}

fn doctor_markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', "<br>")
}

fn tool_probe_status(configured: &str, already_resolved_path: bool) -> String {
    let resolved = if already_resolved_path {
        let path = PathBuf::from(configured);
        if path.exists() {
            Some(path)
        } else {
            None
        }
    } else {
        resolve_tool_path(configured)
    };

    match resolved {
        Some(path) => format!("configured={} resolved={}", configured, path.display()),
        None => format!("configured={} resolved=missing", configured),
    }
}

fn resolve_tool_path(configured: &str) -> Option<PathBuf> {
    let configured_path = Path::new(configured);
    if configured.contains('/') || configured.starts_with('.') {
        return configured_path
            .exists()
            .then(|| configured_path.to_path_buf());
    }

    let path_var = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path_var) {
        let candidate = entry.join(configured);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn display_path(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
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
        } else if let Some(rest) = line.strip_prefix("compiler ") {
            let (compiler, artifacts) = parse_compiler_artifact_declaration(rest, path, line_no)?;
            builder.generic_compiler = Some(compiler);
            builder.generic_artifacts = artifacts;
        } else if let Some(rest) = line.strip_prefix("compare ") {
            let (left, right, artifacts) = parse_compare_declaration(rest, path, line_no)?;
            builder.generic_compare = Some((left, right));
            builder.generic_compare_artifacts = artifacts;
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
    generic_compiler: Option<CompilerSpec>,
    generic_artifacts: BTreeSet<ArtifactKey>,
    generic_compare: Option<(CompilerSpec, CompilerSpec)>,
    generic_compare_artifacts: BTreeSet<ArtifactKey>,
    opt_levels: Vec<OptLevel>,
    repeat_count: usize,
    reference_compilers: Vec<ReferenceCompiler>,
    consistency_checks: Vec<ConsistencyCheck>,
    expectations: Vec<Expectation>,
    status_rules: Vec<PendingStatusRule>,
}

impl CaseBuilder {
    fn new(name: String) -> Self {
        Self {
            name,
            source: None,
            graph_entry: None,
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            generic_compiler: None,
            generic_artifacts: BTreeSet::new(),
            generic_compare: None,
            generic_compare_artifacts: BTreeSet::new(),
            opt_levels: Vec::new(),
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        }
    }

    fn build(self, suite_path: &Path) -> Result<CaseSpec, String> {
        let generic_mode_count = usize::from(self.generic_compiler.is_some())
            + usize::from(self.generic_compare.is_some());
        if generic_mode_count > 1 {
            return Err(format!(
                "{}: case '{}' mixes multiple suite-v2 execution forms",
                suite_path.display(),
                self.name
            ));
        }

        if generic_mode_count > 0 && !self.requested.is_empty() {
            return Err(format!(
                "{}: case '{}' mixes generic suite-v2 syntax with legacy 'armfortas => ...' stages",
                suite_path.display(),
                self.name
            ));
        }

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

        if self.generic_compare.is_some()
            && (!self.reference_compilers.is_empty() || !self.consistency_checks.is_empty())
        {
            return Err(format!(
                "{}: case '{}' compare suite-v2 cases do not support differential/consistency rules",
                suite_path.display(),
                self.name
            ));
        }

        if self.generic_compiler.is_some() {
            let unsupported = self
                .consistency_checks
                .iter()
                .copied()
                .filter(|check| !check.supports_generic_introspect())
                .collect::<Vec<_>>();
            if !unsupported.is_empty() {
                return Err(format!(
                    "{}: case '{}' generic compiler cases only support cli_asm_reproducible, cli_obj_reproducible, and cli_run_reproducible today (unsupported: {})",
                    suite_path.display(),
                    self.name,
                    unsupported
                        .iter()
                        .map(ConsistencyCheck::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }

        let needs_source_comment_resolution = self
            .expectations
            .iter()
            .any(|expectation| matches!(expectation, Expectation::FailSourceComments))
            || self
                .status_rules
                .iter()
                .any(|rule| matches!(rule, PendingStatusRule::XfailSourceComments));
        let source_text = if needs_source_comment_resolution {
            Some(fs::read_to_string(&source).map_err(|e| {
                format!(
                    "{}: case '{}': cannot read source '{}' for comment-based directives: {}",
                    suite_path.display(),
                    self.name,
                    source.display(),
                    e
                )
            })?)
        } else {
            None
        };

        let generic_introspect = if let Some(compiler) = self.generic_compiler {
            if self.generic_artifacts.is_empty() {
                return Err(format!(
                    "{}: case '{}' generic compiler artifact list is empty",
                    suite_path.display(),
                    self.name
                ));
            }
            Some(GenericIntrospectCase {
                compiler,
                artifacts: self.generic_artifacts,
            })
        } else {
            None
        };

        let generic_compare = if let Some((left, right)) = self.generic_compare {
            let mut artifacts = self.generic_compare_artifacts;
            if artifacts.is_empty() {
                return Err(format!(
                    "{}: case '{}' compare artifact list is empty",
                    suite_path.display(),
                    self.name
                ));
            }
            artifacts.extend(default_compare_artifacts(&artifacts));
            Some(GenericCompareCase {
                left,
                right,
                artifacts,
            })
        } else {
            None
        };

        let mut requested = self.requested;
        if requested.is_empty() && generic_introspect.is_none() && generic_compare.is_none() {
            requested.insert(Stage::Run);
        }

        let opt_levels = if self.opt_levels.is_empty() {
            vec![OptLevel::O0]
        } else {
            self.opt_levels
        };
        let expectations = resolve_source_comment_expectations(
            self.expectations,
            source_text.as_deref(),
            suite_path,
            &self.name,
            &source,
        )?;
        let status_rules = resolve_source_comment_status_rules(
            self.status_rules,
            source_text.as_deref(),
            suite_path,
            &self.name,
            &source,
        )?;

        Ok(CaseSpec {
            name: self.name,
            source,
            graph_files,
            requested,
            generic_introspect,
            generic_compare,
            opt_levels,
            repeat_count: self.repeat_count,
            reference_compilers: self.reference_compilers,
            consistency_checks: self.consistency_checks,
            expectations,
            status_rules,
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

fn parse_compiler_artifact_declaration(
    rest: &str,
    path: &Path,
    line_no: usize,
) -> Result<(CompilerSpec, BTreeSet<ArtifactKey>), String> {
    let (compiler_raw, artifact_raw) = rest.split_once("=>").ok_or_else(|| {
        format!(
            "{}:{}: compiler declaration must use 'compiler <spec> => <artifacts>'",
            path.display(),
            line_no
        )
    })?;
    let compiler = parse_compiler_spec_token(compiler_raw.trim(), path, line_no)?;
    let artifacts = ArtifactKey::parse_list(artifact_raw.trim())
        .map_err(|err| format!("{}:{}: {}", path.display(), line_no, err))?;
    if artifacts.is_empty() {
        return Err(format!(
            "{}:{}: generic compiler artifact list is empty",
            path.display(),
            line_no
        ));
    }
    Ok((compiler, artifacts))
}

fn parse_compare_declaration(
    rest: &str,
    path: &Path,
    line_no: usize,
) -> Result<(CompilerSpec, CompilerSpec, BTreeSet<ArtifactKey>), String> {
    let (compilers_raw, artifacts_raw) = rest.split_once("=>").ok_or_else(|| {
        format!(
            "{}:{}: compare declaration must use 'compare <left> <right> => <artifacts>'",
            path.display(),
            line_no
        )
    })?;
    let tokens = split_compiler_tokens(compilers_raw.trim(), path, line_no)?;
    if tokens.len() != 2 {
        return Err(format!(
            "{}:{}: compare declaration requires exactly two compiler specs",
            path.display(),
            line_no
        ));
    }
    let left = parse_compiler_spec_token(&tokens[0], path, line_no)?;
    let right = parse_compiler_spec_token(&tokens[1], path, line_no)?;
    let artifacts = ArtifactKey::parse_list(artifacts_raw.trim())
        .map_err(|err| format!("{}:{}: {}", path.display(), line_no, err))?;
    Ok((left, right, artifacts))
}

fn split_compiler_tokens(raw: &str, path: &Path, line_no: usize) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    for ch in raw.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            c if c.is_whitespace() && !quoted => {
                if !current.trim().is_empty() {
                    tokens.push(current.trim().to_string());
                    current.clear();
                }
            }
            other => current.push(other),
        }
    }

    if quoted {
        return Err(format!(
            "{}:{}: unterminated quoted compiler spec '{}'",
            path.display(),
            line_no,
            raw
        ));
    }

    if !current.trim().is_empty() {
        tokens.push(current.trim().to_string());
    }

    Ok(tokens)
}

fn parse_compiler_spec_token(
    raw: &str,
    path: &Path,
    line_no: usize,
) -> Result<CompilerSpec, String> {
    let token = if raw.starts_with('"') {
        parse_quoted(raw, path, line_no)?
    } else {
        raw.trim().to_string()
    };
    if token.is_empty() {
        return Err(format!(
            "{}:{}: compiler declaration is missing a compiler spec",
            path.display(),
            line_no
        ));
    }
    if let Some(named) = NamedCompiler::parse(&token) {
        return Ok(CompilerSpec::Named(named));
    }
    let parsed = PathBuf::from(&token);
    let resolved = if parsed.is_absolute() {
        parsed
    } else {
        path.parent().unwrap_or_else(|| Path::new(".")).join(parsed)
    };
    Ok(CompilerSpec::Binary(resolved))
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
        if matches!(
            target,
            Target::RunExitCode
                | Target::Artifact(ArtifactKey::ExitCode)
                | Target::CompareDifferenceCount
        ) {
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
    if rest.trim().eq_ignore_ascii_case("comments") {
        return Ok(Expectation::FailSourceComments);
    }

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
) -> Result<PendingStatusRule, String> {
    let rest = rest.trim();
    if kind == StatusKind::Xfail && rest.eq_ignore_ascii_case("comments") {
        return Ok(PendingStatusRule::XfailSourceComments);
    }
    if rest.starts_with('"') {
        return Ok(PendingStatusRule::Explicit(StatusRule {
            kind,
            selector: OptSelector::All,
            reason: parse_quoted(rest, path, line_no)?,
        }));
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

    Ok(PendingStatusRule::Explicit(StatusRule {
        kind,
        selector: parse_opt_selector(selector.trim(), path, line_no)?,
        reason: parse_quoted(reason.trim(), path, line_no)?,
    }))
}

fn resolve_source_comment_expectations(
    expectations: Vec<Expectation>,
    source_text: Option<&str>,
    suite_path: &Path,
    case_name: &str,
    source_path: &Path,
) -> Result<Vec<Expectation>, String> {
    let mut resolved = Vec::with_capacity(expectations.len());
    for expectation in expectations {
        match expectation {
            Expectation::FailSourceComments => {
                let source_text = source_text.ok_or_else(|| {
                    format!(
                        "{}: case '{}': source comments were required but '{}' was not loaded",
                        suite_path.display(),
                        case_name,
                        source_path.display()
                    )
                })?;
                let patterns = extract_error_expected_patterns(source_text);
                if patterns.is_empty() {
                    return Err(format!(
                        "{}: case '{}' requests expect-fail comments but '{}' has no ! ERROR_EXPECTED: lines",
                        suite_path.display(),
                        case_name,
                        source_path.display()
                    ));
                }
                resolved.push(Expectation::FailCommentPatterns(patterns));
            }
            other => resolved.push(other),
        }
    }
    Ok(resolved)
}

fn resolve_source_comment_status_rules(
    status_rules: Vec<PendingStatusRule>,
    source_text: Option<&str>,
    suite_path: &Path,
    case_name: &str,
    source_path: &Path,
) -> Result<Vec<StatusRule>, String> {
    let mut resolved = Vec::with_capacity(status_rules.len());
    for rule in status_rules {
        match rule {
            PendingStatusRule::Explicit(rule) => resolved.push(rule),
            PendingStatusRule::XfailSourceComments => {
                let source_text = source_text.ok_or_else(|| {
                    format!(
                        "{}: case '{}': source comments were required but '{}' was not loaded",
                        suite_path.display(),
                        case_name,
                        source_path.display()
                    )
                })?;
                let reason = extract_xfail_reason(source_text).ok_or_else(|| {
                    format!(
                        "{}: case '{}' requests xfail comments but '{}' has no ! XFAIL: lines",
                        suite_path.display(),
                        case_name,
                        source_path.display()
                    )
                })?;
                resolved.push(StatusRule {
                    kind: StatusKind::Xfail,
                    selector: OptSelector::All,
                    reason,
                });
            }
        }
    }
    Ok(resolved)
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
        "compare.status" => Ok(Target::CompareStatus),
        "compare.classification" => Ok(Target::CompareClassification),
        "compare.changed_artifacts" => Ok(Target::CompareChangedArtifacts),
        "compare.difference_count" => Ok(Target::CompareDifferenceCount),
        "compare.basis" => Ok(Target::CompareBasis),
        "run.stdout" => Ok(Target::RunStdout),
        "run.stderr" => Ok(Target::RunStderr),
        "run.exit_code" => Ok(Target::RunExitCode),
        _ => {
            if let Some(artifact) = ArtifactKey::parse(raw) {
                return Ok(Target::Artifact(artifact));
            }
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

fn print_suites(suites: &[&SuiteSpec], config: &ListConfig) {
    for suite in suites {
        println!("{} ({})", suite.name, suite.cases.len());
        println!("  {}", suite.path.display());
        if config.verbose {
            for case in &suite.cases {
                println!("  - {} [{}]", case.name, case_discovery_mode_label(case));
                for line in case_discovery_lines(case, &config.tools) {
                    println!("    {}", line);
                }
            }
        }
    }
}

fn case_discovery_mode_label(case: &CaseSpec) -> &'static str {
    if case.is_generic_compare() {
        "generic-compare"
    } else if case.is_generic_introspect() {
        "generic-introspect"
    } else {
        "legacy"
    }
}

fn format_opt_level_list(levels: &[OptLevel]) -> String {
    levels
        .iter()
        .map(OptLevel::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn case_discovery_lines(case: &CaseSpec, tools: &ToolchainConfig) -> Vec<String> {
    let mut lines = vec![
        format!("source: {}", case.source_label()),
        format!("opts: {}", format_opt_level_list(&case.opt_levels)),
    ];

    if let Some(generic) = &case.generic_introspect {
        lines.push(format!("compiler: {}", generic.compiler.display_name()));
        lines.push(format!(
            "artifacts: {}",
            format_artifact_name_list(
                &generic
                    .artifacts
                    .iter()
                    .map(|artifact| artifact.as_str().to_string())
                    .collect::<Vec<_>>()
            )
        ));
        match capability_request_issue(&generic.compiler, &generic.artifacts, tools) {
            Some(issue) => {
                lines.push("capability_status: blocked".to_string());
                lines.extend(
                    issue
                        .lines()
                        .map(|line| format!("capability_detail: {}", line)),
                );
            }
            None => lines.push("capability_status: ready".to_string()),
        }
        return lines;
    }

    if let Some(generic) = &case.generic_compare {
        lines.push(format!(
            "compare: {} vs {}",
            generic.left.display_name(),
            generic.right.display_name()
        ));
        lines.push(format!(
            "artifacts: {}",
            format_artifact_name_list(
                &generic
                    .artifacts
                    .iter()
                    .map(|artifact| artifact.as_str().to_string())
                    .collect::<Vec<_>>()
            )
        ));
        let mut issues = Vec::new();
        if let Some(issue) = capability_request_issue(&generic.left, &generic.artifacts, tools) {
            issues.push(format!("left {}", issue));
        }
        if let Some(issue) = capability_request_issue(&generic.right, &generic.artifacts, tools) {
            issues.push(format!("right {}", issue));
        }
        if issues.is_empty() {
            lines.push("capability_status: ready".to_string());
        } else {
            lines.push("capability_status: blocked".to_string());
            lines.extend(issues.into_iter().flat_map(|issue| {
                issue
                    .lines()
                    .map(|line| format!("capability_detail: {}", line))
                    .collect::<Vec<_>>()
            }));
        }
        return lines;
    }

    let needs_linked_capture = primary_backend_kind_for_case(case, &case.requested, tools)
        == PrimaryCaptureBackendKind::Full;
    if case.requested.is_empty() {
        lines.push("stages: run".to_string());
    } else {
        let stages = case
            .requested
            .iter()
            .map(Stage::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("stages: {}", stages));
    }
    if !case.reference_compilers.is_empty() {
        lines.push(format!(
            "differential: {}",
            case.reference_compilers
                .iter()
                .map(ReferenceCompiler::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !case.consistency_checks.is_empty() {
        lines.push(format!(
            "consistency: {}",
            case.consistency_checks
                .iter()
                .map(ConsistencyCheck::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if needs_linked_capture {
        lines.push("surface: linked armfortas capture".to_string());
        if linked_capture_available() {
            lines.push("capability_status: ready".to_string());
        } else {
            lines.push("capability_status: blocked".to_string());
            lines.push(
                "capability_detail: linked armfortas capture is unavailable in this build"
                    .to_string(),
            );
        }
    } else {
        lines.push("surface: observable-only legacy path".to_string());
        lines.push("capability_status: ready".to_string());
    }

    lines
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
    if case.is_generic_compare() {
        return execute_generic_compare_case_cell(suite, case, opt_level, config);
    }
    if case.is_generic_introspect() {
        return execute_generic_introspect_case_cell(suite, case, opt_level, config);
    }

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
                primary_backend: None,
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
    let selected_backend =
        select_primary_capture_backend(case, &requested, opt_level, &config.tools);

    if let Some(detail) = legacy_unavailable_backend_detail(case, &selected_backend) {
        cleanup_prepared_input(&prepared);
        return Ok(outcome_from_status_and_execution(
            suite,
            case,
            opt_level,
            effective_status,
            Err(detail),
            Some(PrimaryBackendReport::from_selected(&selected_backend)),
            Vec::new(),
        ));
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
        println!("  source: {}", case.source_label());
        if case.is_graph() {
            for file in &case.graph_files {
                println!("  file: {}", file.display());
            }
            println!("  compiled_as: {}", prepared.compiler_source.display());
        }
        println!("  opt: {}", opt_level.as_str());
        println!("  stages: {}", stage_list);
        println!(
            "  primary_backend: {} ({})",
            selected_backend.kind.as_str(),
            selected_backend.backend.mode_name()
        );
        println!(
            "  primary_backend_detail: {}",
            selected_backend.backend.description()
        );
        println!("  refs: {}", refs);
        if !case.consistency_checks.is_empty() {
            println!("  repeat: {}", case.repeat_count);
        }
    }

    let references = run_reference_compilers(&prepared, case, opt_level, &config.tools);
    let mut artifacts = ExecutionArtifacts {
        requested,
        armfortas: None,
        armfortas_failure: None,
        armfortas_observation: None,
        references,
        reference_observations: Vec::new(),
        consistency_issues: Vec::new(),
    };
    artifacts.reference_observations = artifacts
        .references
        .iter()
        .map(|reference| {
            observed_program_from_reference_result(
                &prepared.compiler_source,
                opt_level,
                default_differential_artifacts(),
                reference,
            )
        })
        .collect();

    match execute_primary_armfortas(
        &prepared,
        opt_level,
        &artifacts.requested,
        &selected_backend,
    ) {
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
                let observed = legacy_success_observed_program(
                    case,
                    &prepared.compiler_source,
                    opt_level,
                    result,
                    !artifacts.references.is_empty(),
                    &config.tools,
                );
                artifacts.armfortas_observation = Some(observed.clone());
                let mut execution = evaluate_observation_expectations(case, &observed);
                if execution.is_ok() && !artifacts.references.is_empty() {
                    let references = artifacts
                        .reference_observations
                        .iter()
                        .map(|observed| observed.observation.clone())
                        .collect::<Vec<_>>();
                    execution = compare_differential(&observed.observation, &references);
                }
                if execution.is_ok() && !case.consistency_checks.is_empty() {
                    artifacts.consistency_issues =
                        if legacy_case_uses_generic_consistency_checks(case) {
                            run_generic_consistency_checks(
                                &CompilerSpec::Named(NamedCompiler::Armfortas),
                                case,
                                &prepared.compiler_source,
                                opt_level,
                                &config.tools,
                            )
                        } else {
                            run_consistency_checks(
                                case,
                                &prepared,
                                opt_level,
                                result,
                                &config.tools,
                            )
                        };
                    if !artifacts.consistency_issues.is_empty() {
                        execution = Err(format_consistency_issues(&artifacts.consistency_issues));
                    }
                }
                execution
            }
        }
        (None, Some(failure)) => {
            let observed =
                legacy_failure_observed_program(&prepared.compiler_source, case, failure);
            artifacts.armfortas_observation = Some(observed.clone());
            let mut execution =
                evaluate_failed_armfortas_with_observed(case, &artifacts, &observed);
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
    let primary_backend = Some(PrimaryBackendReport::from_selected(&selected_backend));

    let mut outcome = outcome_from_status_and_execution(
        suite,
        case,
        opt_level,
        effective_status,
        execution,
        primary_backend,
        consistency_observations,
    );

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

fn legacy_success_observed_program(
    case: &CaseSpec,
    program: &Path,
    opt_level: OptLevel,
    result: &CaptureResult,
    has_references: bool,
    tools: &ToolchainConfig,
) -> ObservedProgram {
    let mut requested_artifacts = expected_artifacts_for_legacy_case(case);
    if has_references {
        requested_artifacts.extend(default_differential_artifacts());
    }

    if legacy_case_uses_generic_observation_execution(case, &case.requested) {
        if let Ok(observation) = observe_compiler(
            &CompilerSpec::Named(NamedCompiler::Armfortas),
            program,
            opt_level,
            &requested_artifacts,
            tools,
        ) {
            if observation.compile_exit_code == 0 {
                return ObservedProgram {
                    observation,
                    requested_artifacts,
                };
            }
        }
    }

    observed_program_from_armfortas_capture(program, opt_level, requested_artifacts, result, None)
}

fn legacy_case_uses_generic_observation_execution(
    case: &CaseSpec,
    requested: &BTreeSet<Stage>,
) -> bool {
    !has_failure_expectation(case)
        && !requested.is_empty()
        && requested
            .iter()
            .all(|stage| matches!(stage, Stage::Asm | Stage::Obj | Stage::Run))
}

fn execute_generic_compare_case_cell(
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
                primary_backend: None,
                consistency_observations: Vec::new(),
            });
        }
    }

    let generic = case
        .generic_compare
        .as_ref()
        .ok_or_else(|| "missing generic compare case configuration".to_string())?;
    let prepared = prepare_case_input(case, suite, opt_level)?;

    if config.verbose {
        let artifacts = generic
            .artifacts
            .iter()
            .map(ArtifactKey::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        println!("  source: {}", case.source_label());
        if case.is_graph() {
            for file in &case.graph_files {
                println!("  file: {}", file.display());
            }
            println!("  compiled_as: {}", prepared.compiler_source.display());
        }
        println!(
            "  compare: {} vs {}",
            generic.left.display_name(),
            generic.right.display_name()
        );
        println!("  opt: {}", opt_level.as_str());
        println!("  artifacts: {}", artifacts);
    }

    let result = run_compare(&CompareConfig {
        left: generic.left.clone(),
        right: generic.right.clone(),
        program: prepared.compiler_source.clone(),
        opt_level,
        artifacts: generic.artifacts.clone(),
        json_report: None,
        markdown_report: None,
        tools: config.tools.clone(),
    });

    let execution = if has_failure_expectation(case) {
        Err("suite-v2 compare cases do not support expect-fail rules".to_string())
    } else if let Ok(result) = &result {
        evaluate_compare_expectations(case, result)
    } else {
        Err(result.unwrap_err())
    };

    let mut outcome = match (effective_status, execution) {
        (EffectiveStatus::Normal, Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Pass,
            detail: String::new(),
            bundle: None,
            primary_backend: None,
            consistency_observations: Vec::new(),
        },
        (EffectiveStatus::Normal, Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Fail,
            detail,
            bundle: None,
            primary_backend: None,
            consistency_observations: Vec::new(),
        },
        (EffectiveStatus::Xfail(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xpass,
            detail: reason,
            bundle: None,
            primary_backend: None,
            consistency_observations: Vec::new(),
        },
        (EffectiveStatus::Xfail(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xfail,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
            primary_backend: None,
            consistency_observations: Vec::new(),
        },
        (EffectiveStatus::Future(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xpass,
            detail: reason,
            bundle: None,
            primary_backend: None,
            consistency_observations: Vec::new(),
        },
        (EffectiveStatus::Future(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Future,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
            primary_backend: None,
            consistency_observations: Vec::new(),
        },
    };

    outcome.detail = outcome.detail.trim().to_string();
    Ok(outcome)
}

fn execute_generic_introspect_case_cell(
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
                primary_backend: None,
                consistency_observations: Vec::new(),
            });
        }
    }

    let generic = case
        .generic_introspect
        .as_ref()
        .ok_or_else(|| "missing generic introspection case configuration".to_string())?;
    let prepared = prepare_case_input(case, suite, opt_level)?;

    if config.verbose {
        let artifacts = generic
            .artifacts
            .iter()
            .map(ArtifactKey::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        println!("  source: {}", case.source_label());
        if case.is_graph() {
            for file in &case.graph_files {
                println!("  file: {}", file.display());
            }
            println!("  compiled_as: {}", prepared.compiler_source.display());
        }
        println!("  compiler: {}", generic.compiler.display_name());
        println!("  opt: {}", opt_level.as_str());
        println!("  artifacts: {}", artifacts);
        if !case.reference_compilers.is_empty() {
            println!(
                "  refs: {}",
                case.reference_compilers
                    .iter()
                    .map(ReferenceCompiler::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !case.consistency_checks.is_empty() {
            println!(
                "  consistency: {}",
                case.consistency_checks
                    .iter()
                    .map(ConsistencyCheck::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("  repeat: {}", case.repeat_count);
        }
    }

    let observed = run_introspect(&IntrospectConfig {
        compiler: generic.compiler.clone(),
        program: prepared.compiler_source.clone(),
        opt_level,
        artifacts: generic.artifacts.clone(),
        json_report: None,
        markdown_report: None,
        all_artifacts: false,
        summary_only: false,
        max_artifact_lines: None,
        tools: config.tools.clone(),
    })?;

    let mut execution = if observed.observation.compile_exit_code == 0 {
        if has_failure_expectation(case) {
            Err(format!(
                "expected {} to fail ({}) but compilation succeeded",
                generic.compiler.display_name(),
                expected_failure_description(case)
            ))
        } else {
            evaluate_observation_expectations(case, &observed)
        }
    } else if has_failure_expectation(case) {
        evaluate_observation_failure_expectations(case, &observed.observation)
    } else {
        Err(compose_observation_failure_detail(&observed.observation))
    };

    if execution.is_ok() && !case.reference_compilers.is_empty() {
        execution = run_generic_differential(
            &generic.compiler,
            &prepared.compiler_source,
            opt_level,
            &case.reference_compilers,
            &config.tools,
        );
    }

    let mut consistency_issues = Vec::new();
    if execution.is_ok() && !case.consistency_checks.is_empty() {
        consistency_issues = run_generic_consistency_checks(
            &generic.compiler,
            case,
            &prepared.compiler_source,
            opt_level,
            &config.tools,
        );
        if !consistency_issues.is_empty() {
            execution = Err(format_consistency_issues(&consistency_issues));
        }
    }

    let consistency_observations = consistency_issues
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
            primary_backend: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Normal, Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Fail,
            detail,
            bundle: None,
            primary_backend: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Xfail(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xpass,
            detail: reason,
            bundle: None,
            primary_backend: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Xfail(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xfail,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
            primary_backend: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Future(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xpass,
            detail: reason,
            bundle: None,
            primary_backend: None,
            consistency_observations: consistency_observations.clone(),
        },
        (EffectiveStatus::Future(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Future,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
            primary_backend: None,
            consistency_observations,
        },
    };

    outcome.detail = outcome.detail.trim().to_string();
    cleanup_consistency_issues(&consistency_issues);
    Ok(outcome)
}

fn prepare_case_input(
    case: &CaseSpec,
    suite: &SuiteSpec,
    opt_level: OptLevel,
) -> Result<PreparedInput, String> {
    if case.graph_files.is_empty() {
        return Ok(PreparedInput {
            compiler_source: case.source.clone(),
            generated_source: None,
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

    let extension = case
        .source
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .unwrap_or("f90");
    let generated_source = temp_root.join(format!(
        "{}_graph.{}",
        sanitize_component(&case.name),
        extension
    ));

    let mut combined = String::new();
    for (index, file) in case.graph_files.iter().enumerate() {
        let text = fs::read_to_string(file)
            .map_err(|e| format!("cannot read graph file '{}': {}", file.display(), e))?;
        if index > 0 {
            combined.push('\n');
        }
        combined.push_str(&text);
        if !text.ends_with('\n') {
            combined.push('\n');
        }
    }

    fs::write(&generated_source, combined).map_err(|e| {
        format!(
            "cannot write generated graph input '{}': {}",
            generated_source.display(),
            e
        )
    })?;

    Ok(PreparedInput {
        compiler_source: generated_source.clone(),
        generated_source: Some(generated_source),
        temp_root: Some(temp_root),
    })
}

fn cleanup_prepared_input(prepared: &PreparedInput) {
    if let Some(temp_root) = &prepared.temp_root {
        let _ = fs::remove_dir_all(temp_root);
    }
}

fn primary_backend_kind_for_case(
    case: &CaseSpec,
    requested: &BTreeSet<Stage>,
    tools: &ToolchainConfig,
) -> PrimaryCaptureBackendKind {
    let cli_observable_only = !requested.is_empty()
        && requested
            .iter()
            .all(|stage| matches!(stage, Stage::Asm | Stage::Obj | Stage::Run));
    let supports_cli_primary = matches!(
        tools.armfortas_adapters().cli(),
        ArmfortasCliAdapter::External(_)
    );
    let capture_checks_required = case
        .consistency_checks
        .iter()
        .any(ConsistencyCheck::requires_capture_result);

    if supports_cli_primary
        && cli_observable_only
        && !has_failure_expectation(case)
        && !capture_checks_required
    {
        PrimaryCaptureBackendKind::Observable
    } else {
        PrimaryCaptureBackendKind::Full
    }
}

fn select_primary_capture_backend(
    case: &CaseSpec,
    requested: &BTreeSet<Stage>,
    opt_level: OptLevel,
    tools: &ToolchainConfig,
) -> SelectedPrimaryBackend {
    let kind = primary_backend_kind_for_case(case, requested, tools);
    let backend: Box<dyn CaptureBackend> = match kind {
        PrimaryCaptureBackendKind::Full => Box::new(tools.armfortas_adapters()),
        PrimaryCaptureBackendKind::Observable => {
            Box::new(tools.cli_observable_capture_backend(next_primary_cli_temp_root(opt_level)))
        }
    };
    SelectedPrimaryBackend { kind, backend }
}

fn execute_primary_armfortas(
    prepared: &PreparedInput,
    opt_level: OptLevel,
    requested: &BTreeSet<Stage>,
    selected: &SelectedPrimaryBackend,
) -> Result<CaptureResult, CaptureFailure> {
    let request = CaptureRequest {
        input: prepared.compiler_source.clone(),
        requested: requested.clone(),
        opt_level,
    };
    selected.backend.capture(&request)
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
            Target::Artifact(artifact) => ensure_artifact_stage(artifact, requested),
            Target::CompareStatus
            | Target::CompareClassification
            | Target::CompareChangedArtifacts
            | Target::CompareDifferenceCount
            | Target::CompareBasis => {}
            Target::RunStdout | Target::RunStderr | Target::RunExitCode => {
                requested.insert(Stage::Run);
            }
        },
        Expectation::FailContains { .. }
        | Expectation::FailEquals { .. }
        | Expectation::FailSourceComments
        | Expectation::FailCommentPatterns(_) => {}
    }
}

fn ensure_consistency_stage(check: ConsistencyCheck, requested: &mut BTreeSet<Stage>) {
    if let Some(stage) = check.required_stage() {
        requested.insert(stage);
    }
}

fn ensure_artifact_stage(artifact: &ArtifactKey, requested: &mut BTreeSet<Stage>) {
    match artifact {
        ArtifactKey::Asm => {
            requested.insert(Stage::Asm);
        }
        ArtifactKey::Obj => {
            requested.insert(Stage::Obj);
        }
        ArtifactKey::Runtime
        | ArtifactKey::Stdout
        | ArtifactKey::Stderr
        | ArtifactKey::ExitCode => {
            requested.insert(Stage::Run);
        }
        ArtifactKey::Extra(name) => {
            if let Some(stage) = armfortas_extra_stage(name) {
                requested.insert(stage);
            }
        }
        ArtifactKey::Diagnostics | ArtifactKey::Executable => {}
    }
}

fn armfortas_extra_stage(name: &str) -> Option<Stage> {
    let (namespace, suffix) = name.split_once('.')?;
    if namespace.eq_ignore_ascii_case("armfortas") {
        Stage::parse(suffix)
    } else {
        None
    }
}

fn evaluate_observation_expectations(
    case: &CaseSpec,
    observed: &ObservedProgram,
) -> Result<(), String> {
    for expectation in &case.expectations {
        match expectation {
            Expectation::CheckComments(target) => {
                let text = observation_target_text(&observed.observation, target)?;
                let source = fs::read_to_string(&case.source)
                    .map_err(|e| format!("cannot read '{}': {}", case.source.display(), e))?;
                let checks = if target_uses_ir_comment_checks(target) {
                    extract_ir_checks(&source)
                } else {
                    extract_checks(&source)
                };
                if checks.is_empty() {
                    let expected_label = if target_uses_ir_comment_checks(target) {
                        "! IR_CHECK: / ! IR_NOT:"
                    } else {
                        "! CHECK:"
                    };
                    return Err(format!(
                        "case '{}' requested check-comments but '{}' has no {} lines",
                        case.name,
                        case.source.display(),
                        expected_label
                    ));
                }
                match_checks(&checks, text, &case.name)?;
            }
            Expectation::Contains { target, needle } => {
                let text = observation_target_text(&observed.observation, target)?;
                if !text.contains(needle) {
                    return Err(format!(
                        "expected {} to contain {:?}\nactual:\n{}",
                        target_name(target),
                        needle,
                        text
                    ));
                }
            }
            Expectation::NotContains { target, needle } => {
                let text = observation_target_text(&observed.observation, target)?;
                if text.contains(needle) {
                    return Err(format!(
                        "expected {} to not contain {:?}\nactual:\n{}",
                        target_name(target),
                        needle,
                        text
                    ));
                }
            }
            Expectation::Equals { target, value } => {
                let text = observation_target_text(&observed.observation, target)?;
                if text.trim_end() != value {
                    return Err(format!(
                        "expected {} to equal {:?}\nactual:\n{}",
                        target_name(target),
                        value,
                        text
                    ));
                }
            }
            Expectation::IntEquals { target, value } => {
                let actual = observation_target_int(&observed.observation, target)?;
                if actual != *value {
                    return Err(format!(
                        "expected {} to equal {}\nactual: {}",
                        target_name(target),
                        value,
                        actual
                    ));
                }
            }
            Expectation::FailContains { .. }
            | Expectation::FailEquals { .. }
            | Expectation::FailSourceComments
            | Expectation::FailCommentPatterns(_) => {}
        }
    }
    Ok(())
}

fn evaluate_compare_expectations(case: &CaseSpec, result: &ComparisonResult) -> Result<(), String> {
    for expectation in &case.expectations {
        match expectation {
            Expectation::CheckComments(_) => {
                return Err("compare cases do not support check-comments expectations".into())
            }
            Expectation::Contains { target, needle } => {
                let text = compare_target_text(result, target)?;
                if !text.contains(needle) {
                    return Err(format!(
                        "expected {} to contain {:?}\nactual:\n{}",
                        target_name(target),
                        needle,
                        text
                    ));
                }
            }
            Expectation::NotContains { target, needle } => {
                let text = compare_target_text(result, target)?;
                if text.contains(needle) {
                    return Err(format!(
                        "expected {} to not contain {:?}\nactual:\n{}",
                        target_name(target),
                        needle,
                        text
                    ));
                }
            }
            Expectation::Equals { target, value } => {
                let text = compare_target_text(result, target)?;
                if text.trim_end() != value {
                    return Err(format!(
                        "expected {} to equal {:?}\nactual:\n{}",
                        target_name(target),
                        value,
                        text
                    ));
                }
            }
            Expectation::IntEquals { target, value } => {
                let actual = compare_target_int(result, target)?;
                if actual != *value {
                    return Err(format!(
                        "expected {} to equal {}\nactual: {}",
                        target_name(target),
                        value,
                        actual
                    ));
                }
            }
            Expectation::FailContains { .. }
            | Expectation::FailEquals { .. }
            | Expectation::FailSourceComments
            | Expectation::FailCommentPatterns(_) => {}
        }
    }
    Ok(())
}

fn evaluate_observation_failure_expectations(
    case: &CaseSpec,
    observation: &CompilerObservation,
) -> Result<(), String> {
    let mut saw_failure_expectation = false;
    let diagnostics = observation_diagnostics_text(observation).unwrap_or_default();
    for expectation in &case.expectations {
        match expectation {
            Expectation::FailContains { stage, needle } => {
                saw_failure_expectation = true;
                let actual_stage = observation_failure_stage(observation);
                if actual_stage != Some(*stage) {
                    let actual = actual_stage.map(|stage| stage.as_str()).unwrap_or("none");
                    return Err(format!(
                        "expected failure stage {} but compiler failed in {}\n{}",
                        stage.as_str(),
                        actual,
                        diagnostics
                    ));
                }
                if !diagnostics.contains(needle) {
                    return Err(format!(
                        "expected failure detail at {} to contain {:?}\nactual:\n{}",
                        stage.as_str(),
                        needle,
                        diagnostics
                    ));
                }
            }
            Expectation::FailEquals { stage, value } => {
                saw_failure_expectation = true;
                let actual_stage = observation_failure_stage(observation);
                if actual_stage != Some(*stage) {
                    let actual = actual_stage.map(|stage| stage.as_str()).unwrap_or("none");
                    return Err(format!(
                        "expected failure stage {} but compiler failed in {}\n{}",
                        stage.as_str(),
                        actual,
                        diagnostics
                    ));
                }
                if diagnostics.trim_end() != value {
                    return Err(format!(
                        "expected failure detail at {} to equal {:?}\nactual:\n{}",
                        stage.as_str(),
                        value,
                        diagnostics
                    ));
                }
            }
            Expectation::FailCommentPatterns(patterns) => {
                saw_failure_expectation = true;
                for needle in patterns {
                    if !diagnostics.contains(needle) {
                        return Err(format!(
                            "expected failure detail to contain source comment {:?}\nactual:\n{}",
                            needle, diagnostics
                        ));
                    }
                }
            }
            Expectation::CheckComments(_)
            | Expectation::Contains { .. }
            | Expectation::NotContains { .. }
            | Expectation::Equals { .. }
            | Expectation::IntEquals { .. }
            | Expectation::FailSourceComments => {}
        }
    }

    if !saw_failure_expectation {
        return Err(format!(
            "{} failed but the case did not declare an expect-fail rule\n{}",
            observation.compiler.display_name(),
            diagnostics
        ));
    }

    Ok(())
}

#[cfg(test)]
fn evaluate_failed_armfortas(
    case: &CaseSpec,
    artifacts: &ExecutionArtifacts,
    failure: &CaptureFailure,
) -> Result<(), String> {
    let observed = legacy_failure_observed_program(&case.source, case, failure);
    evaluate_failed_armfortas_with_observed(case, artifacts, &observed)
}

fn evaluate_failed_armfortas_with_observed(
    case: &CaseSpec,
    artifacts: &ExecutionArtifacts,
    observed: &ObservedProgram,
) -> Result<(), String> {
    if has_failure_expectation(case) {
        evaluate_observation_failure_expectations(case, &observed.observation)
    } else {
        match evaluate_observation_expectations(case, observed) {
            Ok(()) => Err(compose_armfortas_failure_detail(artifacts)),
            Err(detail) if is_missing_stage_detail(&detail) => {
                Err(compose_armfortas_failure_detail(artifacts))
            }
            Err(detail) => Err(detail),
        }
    }
}

fn legacy_failure_observed_program(
    program: &Path,
    case: &CaseSpec,
    failure: &CaptureFailure,
) -> ObservedProgram {
    let partial = failure.partial_result();
    observed_program_from_armfortas_capture(
        program,
        failure.opt_level,
        expected_artifacts_for_legacy_case(case),
        &partial,
        Some(failure),
    )
}

fn has_failure_expectation(case: &CaseSpec) -> bool {
    case.expectations.iter().any(|expectation| {
        matches!(
            expectation,
            Expectation::FailContains { .. }
                | Expectation::FailEquals { .. }
                | Expectation::FailSourceComments
                | Expectation::FailCommentPatterns(_)
        )
    })
}

fn legacy_unavailable_backend_detail(
    case: &CaseSpec,
    selected_backend: &SelectedPrimaryBackend,
) -> Option<String> {
    if selected_backend.kind == PrimaryCaptureBackendKind::Full
        && selected_backend.backend.mode_name() == "unavailable"
    {
        Some(format!(
            "case requires linked armfortas capture, but this build only provides the external-driver surface\nsource: {}\nrequired backend: {}\nuse scripts/bootstrap-linked-armfortas.sh for rich stages and legacy frontend/module suites, or run a generic suite-v2 / observable-only case instead",
            case.source_label(),
            selected_backend.backend.description()
        ))
    } else {
        None
    }
}

fn outcome_from_status_and_execution(
    suite: &SuiteSpec,
    case: &CaseSpec,
    opt_level: OptLevel,
    effective_status: EffectiveStatus,
    execution: Result<(), String>,
    primary_backend: Option<PrimaryBackendReport>,
    consistency_observations: Vec<ConsistencyObservation>,
) -> Outcome {
    match (effective_status, execution) {
        (EffectiveStatus::Normal, Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Pass,
            detail: String::new(),
            bundle: None,
            primary_backend,
            consistency_observations,
        },
        (EffectiveStatus::Normal, Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Fail,
            detail,
            bundle: None,
            primary_backend,
            consistency_observations,
        },
        (EffectiveStatus::Xfail(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xpass,
            detail: reason,
            bundle: None,
            primary_backend,
            consistency_observations,
        },
        (EffectiveStatus::Xfail(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Xfail,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
            primary_backend,
            consistency_observations,
        },
        (EffectiveStatus::Future(reason), Ok(())) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Pass,
            detail: reason,
            bundle: None,
            primary_backend,
            consistency_observations,
        },
        (EffectiveStatus::Future(reason), Err(detail)) => Outcome {
            suite: suite.name.clone(),
            case: case.name.clone(),
            opt_level,
            kind: OutcomeKind::Fail,
            detail: format!("{}\n{}", reason, detail),
            bundle: None,
            primary_backend,
            consistency_observations,
        },
    }
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
            Expectation::FailCommentPatterns(patterns) => {
                for needle in patterns {
                    items.push(format!("comments contain {:?}", needle));
                }
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

fn is_missing_stage_detail(detail: &str) -> bool {
    detail.starts_with("missing captured stage '")
        || detail == "missing captured run stage"
        || detail.starts_with("missing artifact '")
}

fn target_name(target: &Target) -> String {
    match target {
        Target::Stage(stage) => stage.as_str().to_string(),
        Target::Artifact(artifact) => artifact.as_str().to_string(),
        Target::CompareStatus => "compare.status".to_string(),
        Target::CompareClassification => "compare.classification".to_string(),
        Target::CompareChangedArtifacts => "compare.changed_artifacts".to_string(),
        Target::CompareDifferenceCount => "compare.difference_count".to_string(),
        Target::CompareBasis => "compare.basis".to_string(),
        Target::RunStdout => "run.stdout".to_string(),
        Target::RunStderr => "run.stderr".to_string(),
        Target::RunExitCode => "run.exit_code".to_string(),
    }
}

fn compare_target_text(result: &ComparisonResult, target: &Target) -> Result<String, String> {
    match target {
        Target::CompareStatus => Ok(compare_status(result).to_string()),
        Target::CompareClassification => Ok(compare_classification(result).to_string()),
        Target::CompareChangedArtifacts => {
            let changed = compare_changed_artifacts(result);
            if changed.is_empty() {
                Ok("none".to_string())
            } else {
                Ok(changed.join(", "))
            }
        }
        Target::CompareBasis => Ok(result.basis.clone()),
        Target::CompareDifferenceCount => Err(
            "compare.difference_count is numeric; use 'expect compare.difference_count equals <int>'"
                .into(),
        ),
        _ => Err(format!(
            "{} is not a compare text target",
            target_name(target)
        )),
    }
}

fn compare_target_int(result: &ComparisonResult, target: &Target) -> Result<i32, String> {
    match target {
        Target::CompareDifferenceCount => Ok(result.differences.len() as i32),
        _ => Err(format!(
            "{} is textual; use a string matcher instead",
            target_name(target)
        )),
    }
}

fn observation_target_text<'a>(
    observation: &'a CompilerObservation,
    target: &Target,
) -> Result<&'a str, String> {
    match target {
        Target::Stage(stage) => match stage {
            Stage::Asm => observation_artifact_text(observation, &ArtifactKey::Asm),
            Stage::Obj => observation_artifact_text(observation, &ArtifactKey::Obj),
            Stage::Run => {
                Err("run is structured; use run.stdout, run.stderr, or run.exit_code".into())
            }
            other => observation_artifact_text(
                observation,
                &ArtifactKey::Extra(format!("armfortas.{}", other.as_str())),
            ),
        },
        Target::Artifact(artifact) => observation_artifact_text(observation, artifact),
        Target::CompareStatus
        | Target::CompareClassification
        | Target::CompareChangedArtifacts
        | Target::CompareDifferenceCount
        | Target::CompareBasis => {
            Err("compare targets are only valid in compare suite-v2 cases".into())
        }
        Target::RunStdout => observation_run_stdout(observation),
        Target::RunStderr => observation_run_stderr(observation),
        Target::RunExitCode => {
            Err("run.exit_code is numeric; use 'expect run.exit_code equals <int>'".into())
        }
    }
}

fn observation_target_int(
    observation: &CompilerObservation,
    target: &Target,
) -> Result<i32, String> {
    match target {
        Target::RunExitCode => observation_run_exit_code(observation),
        Target::Artifact(ArtifactKey::ExitCode) => observation_run_exit_code(observation),
        Target::CompareStatus
        | Target::CompareClassification
        | Target::CompareChangedArtifacts
        | Target::CompareDifferenceCount
        | Target::CompareBasis => {
            Err("compare targets are only valid in compare suite-v2 cases".into())
        }
        _ => Err(format!(
            "{} is textual; use a string matcher instead",
            target_name(target)
        )),
    }
}

fn observation_artifact_text<'a>(
    observation: &'a CompilerObservation,
    artifact: &ArtifactKey,
) -> Result<&'a str, String> {
    match observation.artifacts.get(artifact) {
        Some(ArtifactValue::Text(text)) => Ok(text),
        Some(ArtifactValue::Int(_)) => Err(format!(
            "artifact '{}' is numeric; use an integer matcher instead",
            artifact.as_str()
        )),
        Some(ArtifactValue::Run(_)) => Err(format!(
            "artifact '{}' is structured runtime data; use run.stdout, run.stderr, or run.exit_code",
            artifact.as_str()
        )),
        Some(ArtifactValue::Path(_)) => Err(format!(
            "artifact '{}' is binary/path data, not text",
            artifact.as_str()
        )),
        None => Err(format!("missing artifact '{}'", artifact.as_str())),
    }
}

fn observation_run_stdout(observation: &CompilerObservation) -> Result<&str, String> {
    if let Some(ArtifactValue::Run(run)) = observation.artifacts.get(&ArtifactKey::Runtime) {
        Ok(&run.stdout)
    } else {
        observation_artifact_text(observation, &ArtifactKey::Stdout)
    }
}

fn observation_run_stderr(observation: &CompilerObservation) -> Result<&str, String> {
    if let Some(ArtifactValue::Run(run)) = observation.artifacts.get(&ArtifactKey::Runtime) {
        Ok(&run.stderr)
    } else {
        observation_artifact_text(observation, &ArtifactKey::Stderr)
    }
}

fn observation_run_exit_code(observation: &CompilerObservation) -> Result<i32, String> {
    if let Some(ArtifactValue::Run(run)) = observation.artifacts.get(&ArtifactKey::Runtime) {
        Ok(run.exit_code)
    } else {
        match observation.artifacts.get(&ArtifactKey::ExitCode) {
            Some(ArtifactValue::Int(value)) => Ok(*value),
            Some(_) => Err("artifact 'exit-code' is not numeric".into()),
            None => Err("missing artifact 'exit-code'".into()),
        }
    }
}

fn observation_diagnostics_text(observation: &CompilerObservation) -> Option<&str> {
    match observation.artifacts.get(&ArtifactKey::Diagnostics) {
        Some(ArtifactValue::Text(text)) => Some(text.as_str()),
        _ => None,
    }
}

fn observation_failure_stage(observation: &CompilerObservation) -> Option<FailureStage> {
    observation
        .provenance
        .failure_stage
        .as_deref()
        .and_then(FailureStage::parse)
}

fn compare_differential(
    armfortas: &CompilerObservation,
    references: &[CompilerObservation],
) -> Result<(), String> {
    let requested = default_differential_artifacts();
    let comparisons = references
        .iter()
        .cloned()
        .map(|reference| compare_observations(armfortas.clone(), reference, &requested))
        .collect::<Vec<_>>();
    let matching_refs = comparisons
        .iter()
        .filter(|comparison| comparison.differences.is_empty())
        .count();

    if matching_refs == comparisons.len() {
        return Ok(());
    }

    let reference_disagreement = if references.len() > 1 {
        let baseline = references[0].clone();
        references[1..].iter().cloned().any(|reference| {
            !compare_observations(baseline.clone(), reference, &requested)
                .differences
                .is_empty()
        })
    } else {
        false
    };

    let classification = if matching_refs == 0 && !reference_disagreement {
        "classification: armfortas-only divergence"
    } else if reference_disagreement {
        "classification: reference disagreement"
    } else {
        "classification: partial disagreement"
    };

    let detail = comparisons
        .iter()
        .filter(|comparison| !comparison.differences.is_empty())
        .map(render_compare_text)
        .collect::<Vec<_>>();

    Err(format!(
        "behavior mismatch against reference compilers\n{}\n\n{}",
        classification,
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

fn compose_observation_failure_detail(observation: &CompilerObservation) -> String {
    if observation.provenance.backend_mode == "unavailable" {
        let mut detail = format!(
            "{} unavailable for requested artifacts in this build",
            observation.compiler.display_name()
        );
        if let Some(diagnostics) = observation_diagnostics_text(observation) {
            detail.push('\n');
            detail.push_str(diagnostics);
        }
        return detail;
    }

    if let Some(diagnostics) = observation_diagnostics_text(observation) {
        if diagnostics.contains("does not support requested artifacts in this adapter") {
            return format!(
                "{} does not support requested artifacts in this adapter\n{}",
                observation.compiler.display_name(),
                diagnostics
            );
        }
    }

    let mut detail = String::new();
    detail.push_str(&format!("{} failed", observation.compiler.display_name()));
    if let Some(stage) = &observation.provenance.failure_stage {
        detail.push_str(&format!(" in {}", stage));
    }
    if let Some(diagnostics) = observation_diagnostics_text(observation) {
        detail.push('\n');
        detail.push_str(diagnostics);
    }
    detail
}

fn run_generic_differential(
    compiler: &CompilerSpec,
    program: &Path,
    opt_level: OptLevel,
    references: &[ReferenceCompiler],
    tools: &ToolchainConfig,
) -> Result<(), String> {
    let requested = default_differential_artifacts();
    let primary = observe_compiler(compiler, program, opt_level, &requested, tools)?;
    let references = references
        .iter()
        .copied()
        .map(reference_compiler_spec)
        .map(|reference| observe_compiler(&reference, program, opt_level, &requested, tools))
        .collect::<Result<Vec<_>, _>>()?;
    compare_differential(&primary, &references)
}

fn expected_artifacts_for_legacy_case(case: &CaseSpec) -> BTreeSet<ArtifactKey> {
    let mut requested = BTreeSet::new();
    for stage in &case.requested {
        requested.insert(stage_to_artifact_key(*stage));
    }
    for expectation in &case.expectations {
        match expectation {
            Expectation::CheckComments(target)
            | Expectation::Contains { target, .. }
            | Expectation::NotContains { target, .. }
            | Expectation::Equals { target, .. }
            | Expectation::IntEquals { target, .. } => match target {
                Target::Stage(stage) => {
                    requested.insert(stage_to_artifact_key(*stage));
                }
                Target::Artifact(artifact) => {
                    requested.insert(artifact.clone());
                }
                Target::RunStdout => {
                    requested.insert(ArtifactKey::Stdout);
                }
                Target::RunStderr => {
                    requested.insert(ArtifactKey::Stderr);
                }
                Target::RunExitCode => {
                    requested.insert(ArtifactKey::ExitCode);
                }
                Target::CompareStatus
                | Target::CompareClassification
                | Target::CompareChangedArtifacts
                | Target::CompareDifferenceCount
                | Target::CompareBasis => {}
            },
            Expectation::FailContains { .. }
            | Expectation::FailEquals { .. }
            | Expectation::FailSourceComments
            | Expectation::FailCommentPatterns(_) => {}
        }
    }
    requested
}

fn legacy_case_uses_generic_consistency_checks(case: &CaseSpec) -> bool {
    !case.consistency_checks.is_empty()
        && case
            .consistency_checks
            .iter()
            .copied()
            .all(|check| check.supports_generic_introspect())
}

fn stage_to_artifact_key(stage: Stage) -> ArtifactKey {
    match stage {
        Stage::Asm => ArtifactKey::Asm,
        Stage::Obj => ArtifactKey::Obj,
        Stage::Run => ArtifactKey::Runtime,
        other => ArtifactKey::Extra(format!("armfortas.{}", other.as_str())),
    }
}

fn observed_program_from_armfortas_capture(
    program: &Path,
    opt_level: OptLevel,
    requested_artifacts: BTreeSet<ArtifactKey>,
    result: &CaptureResult,
    failure: Option<&CaptureFailure>,
) -> ObservedProgram {
    let mut artifacts = BTreeMap::new();
    for (stage, captured) in &result.stages {
        match (stage, captured) {
            (Stage::Asm, CapturedStage::Text(text))
                if requested_artifacts.contains(&ArtifactKey::Asm) =>
            {
                artifacts.insert(ArtifactKey::Asm, ArtifactValue::Text(text.clone()));
            }
            (Stage::Obj, CapturedStage::Text(text))
                if requested_artifacts.contains(&ArtifactKey::Obj) =>
            {
                artifacts.insert(ArtifactKey::Obj, ArtifactValue::Text(text.clone()));
            }
            (Stage::Run, CapturedStage::Run(run)) => {
                insert_run_artifacts(&requested_artifacts, run, &mut artifacts);
            }
            (stage, CapturedStage::Text(text)) => {
                let key = ArtifactKey::Extra(format!("armfortas.{}", stage.as_str()));
                if requested_artifacts.contains(&key) {
                    artifacts.insert(key, ArtifactValue::Text(text.clone()));
                }
            }
            _ => {}
        }
    }
    if let Some(failure) = failure {
        if requested_artifacts.contains(&ArtifactKey::Diagnostics)
            || !artifacts.contains_key(&ArtifactKey::Diagnostics)
        {
            artifacts.insert(
                ArtifactKey::Diagnostics,
                ArtifactValue::Text(failure.detail.clone()),
            );
        }
    }
    let artifacts_captured = artifacts
        .keys()
        .map(|artifact| artifact.as_str().to_string())
        .collect::<Vec<_>>();
    ObservedProgram {
        observation: CompilerObservation {
            compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
            program: program.to_path_buf(),
            opt_level,
            compile_exit_code: if failure.is_some() { 1 } else { 0 },
            artifacts,
            provenance: ObservationProvenance {
                compiler_identity: "armfortas".into(),
                adapter_kind: "named".into(),
                backend_mode: "suite-legacy-capture".into(),
                backend_detail: "legacy suite cell capture converted into generic observation"
                    .into(),
                artifacts_captured,
                comparison_basis: None,
                failure_stage: failure.map(|failure| failure.stage.as_str().to_string()),
            },
        },
        requested_artifacts,
    }
}

fn observed_program_from_reference_result(
    program: &Path,
    opt_level: OptLevel,
    requested_artifacts: BTreeSet<ArtifactKey>,
    reference: &ReferenceResult,
) -> ObservedProgram {
    let mut artifacts = BTreeMap::new();
    let diagnostics = [
        reference.compile_stdout.trim_end(),
        reference.compile_stderr.trim_end(),
    ]
    .iter()
    .filter(|part| !part.is_empty())
    .copied()
    .collect::<Vec<_>>()
    .join("\n");

    if requested_artifacts.contains(&ArtifactKey::Diagnostics) && !diagnostics.is_empty() {
        artifacts.insert(ArtifactKey::Diagnostics, ArtifactValue::Text(diagnostics));
    }

    if let Some(run) = &reference.run {
        insert_run_artifacts(&requested_artifacts, run, &mut artifacts);
    } else if let Some(run_error) = &reference.run_error {
        let diagnostics = artifacts
            .entry(ArtifactKey::Diagnostics)
            .or_insert_with(|| ArtifactValue::Text(String::new()));
        if let ArtifactValue::Text(text) = diagnostics {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("run error: {}", run_error));
        }
    }

    let artifacts_captured = artifacts
        .keys()
        .map(|artifact| artifact.as_str().to_string())
        .collect::<Vec<_>>();
    ObservedProgram {
        observation: CompilerObservation {
            compiler: match reference.compiler {
                ReferenceCompiler::Gfortran => CompilerSpec::Named(NamedCompiler::Gfortran),
                ReferenceCompiler::FlangNew => CompilerSpec::Named(NamedCompiler::FlangNew),
            },
            program: program.to_path_buf(),
            opt_level,
            compile_exit_code: reference.compile_exit_code,
            artifacts,
            provenance: ObservationProvenance {
                compiler_identity: reference.compiler.as_str().to_string(),
                adapter_kind: "named".into(),
                backend_mode: "legacy-reference".into(),
                backend_detail: format!(
                    "legacy differential reference observation via {}",
                    reference.compile_command
                ),
                artifacts_captured,
                comparison_basis: None,
                failure_stage: None,
            },
        },
        requested_artifacts,
    }
}

fn reference_compiler_spec(compiler: ReferenceCompiler) -> CompilerSpec {
    match compiler {
        ReferenceCompiler::Gfortran => CompilerSpec::Named(NamedCompiler::Gfortran),
        ReferenceCompiler::FlangNew => CompilerSpec::Named(NamedCompiler::FlangNew),
    }
}

fn run_generic_consistency_checks(
    compiler: &CompilerSpec,
    case: &CaseSpec,
    source: &Path,
    opt_level: OptLevel,
    tools: &ToolchainConfig,
) -> Vec<ConsistencyIssue> {
    let mut failures = Vec::new();
    for check in &case.consistency_checks {
        let issue = match check {
            ConsistencyCheck::CliAsmReproducible => run_generic_cli_asm_reproducible(
                compiler,
                source,
                opt_level,
                case.repeat_count,
                tools,
            ),
            ConsistencyCheck::CliObjReproducible => run_generic_cli_obj_reproducible(
                compiler,
                source,
                opt_level,
                case.repeat_count,
                tools,
            ),
            ConsistencyCheck::CliRunReproducible => run_generic_cli_run_reproducible(
                compiler,
                source,
                opt_level,
                case.repeat_count,
                tools,
            ),
            _ => Some(ConsistencyIssue {
                check: *check,
                summary: "unsupported generic consistency check".into(),
                repeat_count: None,
                unique_variant_count: None,
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!(
                    "generic compiler cases do not support '{}' yet",
                    check.as_str()
                ),
                temp_root: next_consistency_temp_root(opt_level),
            }),
        };
        if let Some(issue) = issue {
            failures.push(issue);
        }
    }
    failures
}

fn run_generic_cli_asm_reproducible(
    compiler: &CompilerSpec,
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

    let requested = BTreeSet::from([ArtifactKey::Asm]);
    let mut runs = Vec::new();
    for index in 0..repeat_count {
        let observation = match observe_compiler(compiler, source, opt_level, &requested, tools) {
            Ok(observation) => observation,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliAsmReproducible,
                    summary: "compiler observation failed during consistency check".into(),
                    repeat_count: Some(repeat_count),
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let asm = match observation_text_artifact(&observation, &ArtifactKey::Asm) {
            Ok(asm) => asm,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliAsmReproducible,
                    summary: "missing asm artifact during consistency check".into(),
                    repeat_count: Some(repeat_count),
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        runs.push(TextRun {
            label: format!("run {}", index + 1),
            command: observation_command_hint(&observation),
            normalized: normalize_text_artifact(&asm),
        });
    }

    let unique_variant_count = count_unique_strings(runs.iter().map(|run| run.normalized.as_str()));
    if unique_variant_count > 1 {
        let (left, right) = first_distinct_text_pair(&runs).unwrap();
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliAsmReproducible,
            summary: format!(
                "repeat_count={} unique_variants={}",
                repeat_count, unique_variant_count
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_variant_count),
            varying_components: Vec::new(),
            stable_components: Vec::new(),
            detail: format!(
                "asm output was not reproducible for {}\n{}\n{}\n{}",
                compiler.display_name(),
                left.command,
                right.command,
                describe_text_difference(
                    &left.normalized,
                    &right.normalized,
                    &left.label,
                    &right.label
                )
            ),
            temp_root,
        });
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_generic_cli_obj_reproducible(
    compiler: &CompilerSpec,
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

    let requested = BTreeSet::from([ArtifactKey::Obj]);
    let mut rendered_runs = Vec::new();
    let mut object_runs = Vec::new();
    let mut parseable = true;
    for index in 0..repeat_count {
        let observation = match observe_compiler(compiler, source, opt_level, &requested, tools) {
            Ok(observation) => observation,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliObjReproducible,
                    summary: "compiler observation failed during consistency check".into(),
                    repeat_count: Some(repeat_count),
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let obj_text = match observation_text_artifact(&observation, &ArtifactKey::Obj) {
            Ok(text) => text,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliObjReproducible,
                    summary: "missing obj artifact during consistency check".into(),
                    repeat_count: Some(repeat_count),
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let label = format!("run {}", index + 1);
        let command = observation_command_hint(&observation);
        rendered_runs.push(TextRun {
            label: label.clone(),
            command: command.clone(),
            normalized: normalize_text_artifact(&obj_text),
        });
        match parse_object_snapshot_text(&obj_text) {
            Ok(snapshot) => object_runs.push(ObjectRun {
                label,
                command,
                snapshot,
            }),
            Err(_) => parseable = false,
        }
    }

    if parseable {
        let rendered = object_runs
            .iter()
            .map(|run| render_object_snapshot(&run.snapshot))
            .collect::<Vec<_>>();
        let unique_variant_count = count_unique_strings(rendered.iter().map(String::as_str));
        if unique_variant_count > 1 {
            let (left, right) = first_distinct_object_pair(&object_runs).unwrap();
            let snapshots = object_runs
                .iter()
                .map(|run| &run.snapshot)
                .collect::<Vec<_>>();
            let varying = varying_object_components(&snapshots)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let stable = stable_object_components(&snapshots)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>();
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CliObjReproducible,
                summary: format!(
                    "repeat_count={} unique_variants={} varying_components={} stable_components={}",
                    repeat_count,
                    unique_variant_count,
                    join_or_none_from_strings(&varying),
                    join_or_none_from_strings(&stable)
                ),
                repeat_count: Some(repeat_count),
                unique_variant_count: Some(unique_variant_count),
                varying_components: varying,
                stable_components: stable,
                detail: format!(
                    "object output was not reproducible for {}\n{}\n{}\n{}",
                    compiler.display_name(),
                    left.command,
                    right.command,
                    describe_object_difference(
                        &left.snapshot,
                        &right.snapshot,
                        &left.label,
                        &right.label
                    )
                ),
                temp_root,
            });
        }
    } else {
        let unique_variant_count =
            count_unique_strings(rendered_runs.iter().map(|run| run.normalized.as_str()));
        if unique_variant_count > 1 {
            let (left, right) = first_distinct_text_pair(&rendered_runs).unwrap();
            return Some(ConsistencyIssue {
                check: ConsistencyCheck::CliObjReproducible,
                summary: format!(
                    "repeat_count={} unique_variants={}",
                    repeat_count, unique_variant_count
                ),
                repeat_count: Some(repeat_count),
                unique_variant_count: Some(unique_variant_count),
                varying_components: Vec::new(),
                stable_components: Vec::new(),
                detail: format!(
                    "object artifact text was not reproducible for {}\n{}\n{}\n{}",
                    compiler.display_name(),
                    left.command,
                    right.command,
                    describe_text_difference(
                        &left.normalized,
                        &right.normalized,
                        &left.label,
                        &right.label
                    )
                ),
                temp_root,
            });
        }
    }

    let _ = fs::remove_dir_all(&temp_root);
    None
}

fn run_generic_cli_run_reproducible(
    compiler: &CompilerSpec,
    source: &Path,
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

    let requested = BTreeSet::from([ArtifactKey::Runtime]);
    let mut runs = Vec::new();
    for index in 0..repeat_count {
        let observation = match observe_compiler(compiler, source, opt_level, &requested, tools) {
            Ok(observation) => observation,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliRunReproducible,
                    summary: "compiler observation failed during consistency check".into(),
                    repeat_count: Some(repeat_count),
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        let run = match observation_run_capture(&observation) {
            Ok(run) => run,
            Err(detail) => {
                return Some(ConsistencyIssue {
                    check: ConsistencyCheck::CliRunReproducible,
                    summary: "missing runtime artifact during consistency check".into(),
                    repeat_count: Some(repeat_count),
                    unique_variant_count: None,
                    varying_components: Vec::new(),
                    stable_components: Vec::new(),
                    detail,
                    temp_root,
                })
            }
        };
        runs.push(BehaviorRun {
            label: format!("run {}", index + 1),
            command: observation_command_hint(&observation),
            signature: normalize_run_signature(&run),
            run,
        });
    }

    let unique_variant_count = count_unique_run_signatures(runs.iter().map(|run| &run.signature));
    if unique_variant_count > 1 {
        let (left, right) = first_distinct_behavior_pair(&runs).unwrap();
        let signatures = runs.iter().map(|run| &run.signature).collect::<Vec<_>>();
        let varying = varying_run_components(&signatures)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let stable = stable_run_components(&signatures)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        return Some(ConsistencyIssue {
            check: ConsistencyCheck::CliRunReproducible,
            summary: format!(
                "repeat_count={} unique_variants={} varying_components={} stable_components={}",
                repeat_count,
                unique_variant_count,
                join_or_none_from_strings(&varying),
                join_or_none_from_strings(&stable)
            ),
            repeat_count: Some(repeat_count),
            unique_variant_count: Some(unique_variant_count),
            varying_components: varying,
            stable_components: stable,
            detail: format!(
                "runtime behavior was not reproducible for {}\n{}\n{}\n{}",
                compiler.display_name(),
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

fn observation_text_artifact(
    observation: &CompilerObservation,
    artifact: &ArtifactKey,
) -> Result<String, String> {
    match observation.artifacts.get(artifact) {
        Some(ArtifactValue::Text(text)) => Ok(text.clone()),
        Some(ArtifactValue::Int(_)) => Err(format!(
            "artifact '{}' is numeric, not text",
            artifact.as_str()
        )),
        Some(ArtifactValue::Run(_)) => Err(format!(
            "artifact '{}' is structured runtime data, not text",
            artifact.as_str()
        )),
        Some(ArtifactValue::Path(path)) => Err(format!(
            "artifact '{}' is path data ('{}'), not text",
            artifact.as_str(),
            path.display()
        )),
        None => Err(format!("missing artifact '{}'", artifact.as_str())),
    }
}

fn observation_run_capture(observation: &CompilerObservation) -> Result<RunCapture, String> {
    if let Some(ArtifactValue::Run(run)) = observation.artifacts.get(&ArtifactKey::Runtime) {
        return Ok(run.clone());
    }

    Ok(RunCapture {
        exit_code: observation_run_exit_code(observation)?,
        stdout: observation_run_stdout(observation)?.to_string(),
        stderr: observation_run_stderr(observation)?.to_string(),
    })
}

fn observation_command_hint(observation: &CompilerObservation) -> String {
    format!(
        "{} [{}; {}]",
        observation.compiler.display_name(),
        observation.provenance.backend_mode,
        observation.provenance.backend_detail
    )
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
        let issue = match check {
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
            ConsistencyCheck::CliRunReproducible => run_cli_run_reproducible(
                &prepared.compiler_source,
                opt_level,
                case.repeat_count,
                tools,
            ),
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
                &prepared.compiler_source,
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
                &prepared.compiler_source,
                opt_level,
                case.repeat_count,
                capture_result,
                tools,
            ),
        };
        if let Some(issue) = issue {
            failures.push(issue);
        }
    }
    failures
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
    let as_output = match Command::new(tools.system_as_bin())
        .args([
            "-o",
            asm_obj_path.to_str().unwrap(),
            asm_path.to_str().unwrap(),
        ])
        .output()
    {
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
    source: &Path,
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
        let build_command = match compile_with_driver(
            source,
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
        let run = match run_binary_capture(&binary_path, &temp_root, &run_command) {
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

    let capture_command = render_capture_command(source, opt_level, Stage::Asm, tools);
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

    let capture_command = render_capture_command(source, opt_level, Stage::Obj, tools);
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
    source: &Path,
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

    let capture_command = render_capture_command(source, opt_level, Stage::Run, tools);
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
        let build_command = match compile_with_driver(
            source,
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
        let run = match run_binary_capture(&binary_path, &temp_root, &run_command) {
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
    tools: &ToolchainConfig,
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
    let command = render_capture_command(source, opt_level, Stage::Asm, tools);
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
        let text = match capture_text_from_testing(source, opt_level, Stage::Asm, tools) {
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
    tools: &ToolchainConfig,
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

    let command = render_capture_command(source, opt_level, Stage::Obj, tools);
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
        let text = match capture_text_from_testing(source, opt_level, Stage::Obj, tools) {
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
    source: &Path,
    opt_level: OptLevel,
    repeat_count: usize,
    capture_result: &CaptureResult,
    tools: &ToolchainConfig,
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

    let command = render_capture_command(source, opt_level, Stage::Run, tools);
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
        let run = match capture_run_from_testing(source, opt_level, tools) {
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
        .map(|compiler| run_reference_case(&prepared.compiler_source, opt_level, compiler, tools))
        .collect()
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

    let compile = match Command::new(compiler_bin)
        .current_dir(&temp_root)
        .args(&args)
        .output()
    {
        Ok(output) => output,
        Err(err) => {
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
    let emit_mode = match mode {
        DriverEmitMode::Asm => EmitMode::Asm,
        DriverEmitMode::Obj => EmitMode::Obj,
        DriverEmitMode::Binary => EmitMode::Binary,
    };
    tools
        .armfortas_adapters()
        .compile_output(source, opt_level, emit_mode, output)
        .map_err(|detail| format!("{} failed:\n{}", command, detail))?;
    Ok(command)
}

fn render_armfortas_command(
    source: &Path,
    opt_level: OptLevel,
    mode: DriverEmitMode,
    output: &Path,
    tools: &ToolchainConfig,
) -> String {
    let armfortas = tools.armfortas_adapters();
    let mut args = vec![opt_level.as_flag().to_string()];
    match mode {
        DriverEmitMode::Asm => args.push("-S".to_string()),
        DriverEmitMode::Obj => args.push("-c".to_string()),
        DriverEmitMode::Binary => {}
    }
    args.push(source.display().to_string());
    args.push("-o".to_string());
    args.push(output.display().to_string());
    render_command(armfortas.cli_command_name(), &args)
}

fn render_binary_run_command(binary: &Path) -> String {
    render_command(&binary.display().to_string(), &[])
}

fn render_capture_command(
    source: &Path,
    opt_level: OptLevel,
    stage: Stage,
    tools: &ToolchainConfig,
) -> String {
    let armfortas = tools.armfortas_adapters();
    format!(
        "{} {} --stage {} {}",
        armfortas.capture_command_name(),
        opt_level.as_flag(),
        stage.as_str(),
        quote_arg(&source.display().to_string()),
    )
}

fn capture_text_from_testing(
    source: &Path,
    opt_level: OptLevel,
    stage: Stage,
    tools: &ToolchainConfig,
) -> Result<String, String> {
    let command = render_capture_command(source, opt_level, stage, tools);
    let request = CaptureRequest {
        input: source.to_path_buf(),
        requested: BTreeSet::from([stage]),
        opt_level,
    };
    let result = tools
        .armfortas_adapters()
        .capture(&request)
        .map_err(|failure| format!("{} failed:\n{}", command, failure))?;
    capture_text_stage(&result, stage).map(str::to_string)
}

fn capture_run_from_testing(
    source: &Path,
    opt_level: OptLevel,
    tools: &ToolchainConfig,
) -> Result<RunCapture, String> {
    let command = render_capture_command(source, opt_level, Stage::Run, tools);
    let request = CaptureRequest {
        input: source.to_path_buf(),
        requested: BTreeSet::from([Stage::Run]),
        opt_level,
    };
    let result = tools
        .armfortas_adapters()
        .capture(&request)
        .map_err(|failure| format!("{} failed:\n{}", command, failure))?;
    capture_run_stage(&result).cloned()
}

fn capture_text_stage<'a>(result: &'a CaptureResult, stage: Stage) -> Result<&'a str, String> {
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

fn run_binary_capture(
    binary: &Path,
    current_dir: &Path,
    command: &str,
) -> Result<RunCapture, String> {
    let output = Command::new(binary)
        .current_dir(current_dir)
        .output()
        .map_err(|err| {
            format!(
                "{} failed:\ncannot run '{}': {}",
                command,
                binary.display(),
                err
            )
        })?;
    Ok(RunCapture {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
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

fn format_run_signature(signature: &RunSignature) -> String {
    let stdout = if signature.stdout.is_empty() {
        "<empty>".to_string()
    } else {
        signature.stdout.clone()
    };
    let stderr = if signature.stderr.is_empty() {
        "<empty>".to_string()
    } else {
        signature.stderr.clone()
    };
    format!(
        "exit: {}\nstdout:\n{}\nstderr:\n{}",
        signature.exit_code, stdout, stderr
    )
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
    let text = normalize_tool_output(&tool_output(
        tools.otool_bin(),
        &["-t", path.to_str().unwrap()],
    )?);
    let load_commands = normalize_tool_output(&tool_output(
        tools.otool_bin(),
        &["-l", path.to_str().unwrap()],
    )?);
    let relocations = normalize_tool_output(&tool_output(
        tools.otool_bin(),
        &["-rv", path.to_str().unwrap()],
    )?);
    let symbols = normalize_tool_output(&tool_output(
        tools.nm_bin(),
        &["-m", path.to_str().unwrap()],
    )?);

    Ok(ObjectSnapshot {
        text,
        load_commands,
        relocations,
        symbols,
    })
}

fn tool_output(tool: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(tool)
        .args(args)
        .output()
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

    let mut detail = format!(
        "snapshot length differs\n{} lines: {}\n{} lines: {}",
        left_label,
        expected_lines.len(),
        right_label,
        actual_lines.len()
    );
    if let Some(extra) = expected_lines.get(shared) {
        detail.push_str(&format!(
            "\nfirst extra line: {}\n{}: {}",
            shared + 1,
            left_label,
            extra
        ));
    } else if let Some(extra) = actual_lines.get(shared) {
        detail.push_str(&format!(
            "\nfirst extra line: {}\n{}: {}",
            shared + 1,
            right_label,
            extra
        ));
    }
    detail
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
        "differing object components: {}\n{}\n{}",
        component_list,
        format!("first differing component: {}", first_name),
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
            describe_text_difference(&expected.stdout, &actual.stdout, left_label, right_label)
        ),
        "stderr" => format!(
            "differing runtime components: {}\nfirst differing component: stderr\n{}",
            component_list,
            describe_text_difference(&expected.stderr, &actual.stderr, left_label, right_label)
        ),
        _ => unreachable!("only known runtime components are compared"),
    }
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
                .map(|signature| signature.exit_code.to_string())
                .collect::<Vec<_>>(),
        ),
        (
            "stdout",
            signatures
                .iter()
                .map(|signature| signature.stdout.clone())
                .collect::<Vec<_>>(),
        ),
        (
            "stderr",
            signatures
                .iter()
                .map(|signature| signature.stderr.clone())
                .collect::<Vec<_>>(),
        ),
    ];

    components
        .into_iter()
        .filter_map(|(name, values)| {
            let varies = count_unique_strings(values.iter().map(String::as_str)) > 1;
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

fn write_requested_reports(config: &RunConfig, summary: &Summary) -> Result<(), String> {
    if let Some(path) = &config.json_report {
        write_report(path, &render_json_report(summary), "json report")?;
        println!("json report: {}", path.display());
    }
    if let Some(path) = &config.markdown_report {
        write_report(path, &render_markdown_report(summary), "markdown report")?;
        println!("markdown report: {}", path.display());
    }
    Ok(())
}

fn write_report(path: &Path, content: &str, label: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "cannot create parent directory for {} '{}': {}",
                label,
                path.display(),
                e
            )
        })?;
    }
    fs::write(path, content)
        .map_err(|e| format!("cannot write {} '{}': {}", label, path.display(), e))
}

fn render_json_report(summary: &Summary) -> String {
    let mut lines = vec![
        "{".to_string(),
        format!("  \"passed\": {},", summary.passed),
        format!("  \"failed\": {},", summary.failed),
        format!("  \"xfailed\": {},", summary.xfailed),
        format!("  \"xpassed\": {},", summary.xpassed),
        format!("  \"future\": {},", summary.future),
        "  \"outcomes\": [".to_string(),
    ];

    for (index, outcome) in summary.outcomes.iter().enumerate() {
        lines.push("    {".to_string());
        lines.push(format!(
            "      \"suite\": \"{}\",",
            json_escape(&outcome.suite)
        ));
        lines.push(format!(
            "      \"case\": \"{}\",",
            json_escape(&outcome.case)
        ));
        lines.push(format!(
            "      \"opt\": \"{}\",",
            outcome.opt_level.as_str()
        ));
        lines.push(format!(
            "      \"kind\": \"{}\",",
            outcome_kind_name(outcome.kind)
        ));
        match &outcome.primary_backend {
            Some(backend) => {
                lines.push("      \"primary_backend\": {".to_string());
                lines.push(format!(
                    "        \"kind\": \"{}\",",
                    json_escape(&backend.kind)
                ));
                lines.push(format!(
                    "        \"mode\": \"{}\",",
                    json_escape(&backend.mode)
                ));
                lines.push(format!(
                    "        \"detail\": \"{}\"",
                    json_escape(&backend.detail)
                ));
                lines.push("      },".to_string());
            }
            None => lines.push("      \"primary_backend\": null,".to_string()),
        }
        lines.push(format!(
            "      \"detail\": \"{}\",",
            json_escape(&outcome.detail)
        ));
        match &outcome.bundle {
            Some(bundle) => lines.push(format!(
                "      \"bundle\": \"{}\",",
                json_escape(&bundle.display().to_string())
            )),
            None => lines.push("      \"bundle\": null,".to_string()),
        }
        lines.push(format!(
            "      \"consistency\": {}",
            render_json_consistency_observations(&outcome.consistency_observations)
        ));
        lines.push(if index + 1 == summary.outcomes.len() {
            "    }".to_string()
        } else {
            "    },".to_string()
        });
    }

    lines.push("  ],".to_string());
    lines.push("  \"consistency\": {".to_string());
    for (index, (check, rollup)) in summary.consistency.iter().enumerate() {
        lines.push(format!("    \"{}\": {{", check.as_str()));
        lines.push(format!("      \"cells\": {},", rollup.cells));
        lines.push(format!(
            "      \"repeat_counts\": {},",
            json_usize_set(&rollup.repeat_counts)
        ));
        lines.push(format!(
            "      \"unique_variant_counts\": {},",
            json_usize_set(&rollup.unique_variant_counts)
        ));
        lines.push(format!(
            "      \"varying_components\": {},",
            json_string_iter(rollup.varying_components.iter().map(|value| value.as_str()))
        ));
        lines.push(format!(
            "      \"stable_components\": {}",
            json_string_iter(rollup.stable_components.iter().map(|value| value.as_str()))
        ));
        lines.push(if index + 1 == summary.consistency.len() {
            "    }".to_string()
        } else {
            "    },".to_string()
        });
    }
    lines.push("  }".to_string());
    lines.push("}".to_string());
    lines.join("\n") + "\n"
}

fn render_json_consistency_observations(observations: &[ConsistencyObservation]) -> String {
    let mut rendered = String::from("[");
    for (index, observation) in observations.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push('{');
        rendered.push_str(&format!(
            "\"check\":\"{}\",\"summary\":\"{}\",",
            observation.check.as_str(),
            json_escape(&observation.summary)
        ));
        match observation.repeat_count {
            Some(count) => rendered.push_str(&format!("\"repeat_count\":{},", count)),
            None => rendered.push_str("\"repeat_count\":null,"),
        }
        match observation.unique_variant_count {
            Some(count) => rendered.push_str(&format!("\"unique_variant_count\":{},", count)),
            None => rendered.push_str("\"unique_variant_count\":null,"),
        }
        rendered.push_str(&format!(
            "\"varying_components\":{},\"stable_components\":{}",
            json_string_array(&observation.varying_components),
            json_string_array(&observation.stable_components)
        ));
        rendered.push('}');
    }
    rendered.push(']');
    rendered
}

fn render_markdown_report(summary: &Summary) -> String {
    let mut lines = vec![
        "# afs-tests report".to_string(),
        String::new(),
        "## Summary".to_string(),
        String::new(),
        "| kind | count |".to_string(),
        "| --- | ---: |".to_string(),
        format!("| passed | {} |", summary.passed),
        format!("| failed | {} |", summary.failed),
        format!("| xfailed | {} |", summary.xfailed),
        format!("| xpassed | {} |", summary.xpassed),
        format!("| future | {} |", summary.future),
    ];

    if !summary.consistency.is_empty() {
        lines.push(String::new());
        lines.push("## Consistency".to_string());
        lines.push(String::new());
        lines.push("| check | cells | repeats | unique variants | varying | stable |".to_string());
        lines.push("| --- | ---: | --- | --- | --- | --- |".to_string());
        for (check, rollup) in &summary.consistency {
            lines.push(format!(
                "| `{}` | {} | {} | {} | {} | {} |",
                check.as_str(),
                rollup.cells,
                join_usize_set(&rollup.repeat_counts),
                join_usize_set(&rollup.unique_variant_counts),
                join_string_set(&rollup.varying_components),
                join_string_set(&rollup.stable_components),
            ));
        }
    }

    lines.push(String::new());
    lines.push("## Outcomes".to_string());
    for outcome in &summary.outcomes {
        lines.push(String::new());
        lines.push(format!(
            "### `{}` / `{}` / `{}` / `{}`",
            outcome.suite,
            outcome.case,
            outcome.opt_level.as_str(),
            outcome_kind_name(outcome.kind)
        ));
        if let Some(backend) = &outcome.primary_backend {
            lines.push(format!(
                "primary_backend: `{}` (`{}`)",
                backend.kind, backend.mode
            ));
            lines.push(format!("primary_backend_detail: {}", backend.detail));
        }
        if let Some(bundle) = &outcome.bundle {
            lines.push(format!("bundle: `{}`", bundle.display()));
        }
        if !outcome.detail.trim().is_empty() {
            lines.push(String::new());
            lines.push("```text".to_string());
            lines.extend(
                outcome
                    .detail
                    .trim_end()
                    .lines()
                    .map(|line| line.to_string()),
            );
            lines.push("```".to_string());
        }
    }

    lines.join("\n") + "\n"
}

fn outcome_kind_name(kind: OutcomeKind) -> &'static str {
    match kind {
        OutcomeKind::Pass => "pass",
        OutcomeKind::Fail => "fail",
        OutcomeKind::Xfail => "xfail",
        OutcomeKind::Xpass => "xpass",
        OutcomeKind::Future => "future",
    }
}

fn json_escape(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

fn json_string_array(items: &[String]) -> String {
    json_string_iter(items.iter().map(|item| item.as_str()))
}

fn json_string_iter<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let mut rendered = String::from("[");
    for (index, item) in items.enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push('"');
        rendered.push_str(&json_escape(item));
        rendered.push('"');
    }
    rendered.push(']');
    rendered
}

fn json_usize_set(items: &BTreeSet<usize>) -> String {
    let mut rendered = String::from("[");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            rendered.push_str(", ");
        }
        rendered.push_str(&item.to_string());
    }
    rendered.push(']');
    rendered
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
    let primary_backend_kind = outcome
        .primary_backend
        .as_ref()
        .map(|backend| backend.kind.as_str())
        .unwrap_or("none");
    let primary_backend_mode = outcome
        .primary_backend
        .as_ref()
        .map(|backend| backend.mode.as_str())
        .unwrap_or("none");
    let primary_backend_detail = outcome
        .primary_backend
        .as_ref()
        .map(|backend| backend.detail.as_str())
        .unwrap_or("none");
    let metadata = format!(
        "suite: {}\ncase: {}\noutcome: {:?}\nopt: {}\nsource: {}\nrequested_stages: {}\nrepeat_count: {}\nreference_compilers: {}\nconsistency_checks: {}\nprimary_backend_kind: {}\nprimary_backend_mode: {}\nprimary_backend_detail: {}\n",
        suite.name,
        case.name,
        outcome.kind,
        outcome.opt_level.as_str(),
        case.source_label(),
        stage_list,
        case.repeat_count,
        refs,
        consistency,
        primary_backend_kind,
        primary_backend_mode,
        primary_backend_detail
    );
    fs::write(bundle_root.join("metadata.txt"), metadata)
        .map_err(|e| format!("cannot write bundle metadata: {}", e))?;
    fs::write(bundle_root.join("detail.txt"), &outcome.detail)
        .map_err(|e| format!("cannot write bundle detail: {}", e))?;

    write_case_sources_bundle(&bundle_root, case, prepared)?;

    let armfortas_root = bundle_root.join("armfortas");
    fs::create_dir_all(&armfortas_root)
        .map_err(|e| format!("cannot create armfortas bundle dir: {}", e))?;
    write_armfortas_bundle_metadata(&armfortas_root, outcome, artifacts)?;
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
    write_armfortas_observation_bundle(&armfortas_root, prepared, artifacts)?;

    if !artifacts.references.is_empty() {
        let refs_root = bundle_root.join("references");
        fs::create_dir_all(&refs_root)
            .map_err(|e| format!("cannot create references bundle dir: {}", e))?;
        let reference_observations = reference_observations_for_bundle(
            &prepared.compiler_source,
            outcome.opt_level,
            artifacts,
        );
        write_reference_summary_bundle(&refs_root, &artifacts.references, &reference_observations)?;
        for (index, reference) in artifacts.references.iter().enumerate() {
            write_reference_bundle(
                &refs_root,
                &prepared.compiler_source,
                outcome.opt_level,
                reference,
                reference_observations.get(index),
            )?;
        }
    }

    if !artifacts.consistency_issues.is_empty() {
        write_consistency_bundle(&bundle_root, &artifacts.consistency_issues)?;
    }

    Ok(bundle_root)
}

fn write_armfortas_bundle_metadata(
    armfortas_root: &Path,
    outcome: &Outcome,
    artifacts: &ExecutionArtifacts,
) -> Result<(), String> {
    let primary_backend_kind = outcome
        .primary_backend
        .as_ref()
        .map(|backend| backend.kind.as_str())
        .unwrap_or("none");
    let primary_backend_mode = outcome
        .primary_backend
        .as_ref()
        .map(|backend| backend.mode.as_str())
        .unwrap_or("none");
    let primary_backend_detail = outcome
        .primary_backend
        .as_ref()
        .map(|backend| backend.detail.as_str())
        .unwrap_or("none");
    let captured_stages = if let Some(result) = &artifacts.armfortas {
        join_or_none(&result.stages.keys().map(Stage::as_str).collect::<Vec<_>>())
    } else if let Some(failure) = &artifacts.armfortas_failure {
        join_or_none(&failure.stages.keys().map(Stage::as_str).collect::<Vec<_>>())
    } else {
        "none".to_string()
    };
    let error_stage = artifacts
        .armfortas_failure
        .as_ref()
        .map(|failure| failure.stage.as_str())
        .unwrap_or("none");
    let metadata = format!(
        "primary_backend_kind: {}\nprimary_backend_mode: {}\nprimary_backend_detail: {}\ncaptured_stages: {}\nerror_stage: {}\n",
        primary_backend_kind,
        primary_backend_mode,
        primary_backend_detail,
        captured_stages,
        error_stage
    );
    fs::write(armfortas_root.join("metadata.txt"), metadata)
        .map_err(|e| format!("cannot write armfortas bundle metadata: {}", e))
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

    let generated_source = prepared.generated_source.as_ref().ok_or_else(|| {
        format!(
            "graph case '{}' was missing a generated compiler source",
            case.name
        )
    })?;
    let generated_text = fs::read_to_string(generated_source).map_err(|e| {
        format!(
            "cannot read generated graph source '{}': {}",
            generated_source.display(),
            e
        )
    })?;
    fs::write(bundle_root.join("source.f90"), generated_text)
        .map_err(|e| format!("cannot write generated bundle source copy: {}", e))?;

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
            }
        }
    }
    Ok(())
}

fn write_armfortas_observation_bundle(
    armfortas_root: &Path,
    prepared: &PreparedInput,
    artifacts: &ExecutionArtifacts,
) -> Result<(), String> {
    let observed = match observed_program_for_armfortas_bundle(prepared, artifacts) {
        Some(observed) => observed,
        None => return Ok(()),
    };
    let render_config = IntrospectionRenderConfig {
        summary_only: false,
        max_artifact_lines: None,
    };
    fs::write(
        armfortas_root.join("observation.txt"),
        render_introspection_text(&observed, render_config),
    )
    .map_err(|e| format!("cannot write armfortas observation text bundle: {}", e))?;
    fs::write(
        armfortas_root.join("observation.json"),
        render_introspection_json(&observed),
    )
    .map_err(|e| format!("cannot write armfortas observation json bundle: {}", e))?;
    fs::write(
        armfortas_root.join("observation.md"),
        render_introspection_markdown(&observed, render_config),
    )
    .map_err(|e| format!("cannot write armfortas observation markdown bundle: {}", e))?;
    Ok(())
}

fn observed_program_for_armfortas_bundle(
    prepared: &PreparedInput,
    artifacts: &ExecutionArtifacts,
) -> Option<ObservedProgram> {
    if let Some(observed) = &artifacts.armfortas_observation {
        Some(observed.clone())
    } else if let Some(result) = &artifacts.armfortas {
        Some(observed_program_from_armfortas_capture(
            &prepared.compiler_source,
            result.opt_level,
            bundle_artifacts_for_capture_result(result),
            result,
            None,
        ))
    } else if let Some(failure) = &artifacts.armfortas_failure {
        let partial = failure.partial_result();
        Some(observed_program_from_armfortas_capture(
            &prepared.compiler_source,
            failure.opt_level,
            bundle_artifacts_for_capture_failure(failure),
            &partial,
            Some(failure),
        ))
    } else {
        None
    }
}

fn bundle_artifacts_for_capture_result(result: &CaptureResult) -> BTreeSet<ArtifactKey> {
    bundle_artifacts_for_stages(&result.stages)
}

fn bundle_artifacts_for_capture_failure(failure: &CaptureFailure) -> BTreeSet<ArtifactKey> {
    let mut requested = bundle_artifacts_for_stages(&failure.stages);
    requested.insert(ArtifactKey::Diagnostics);
    requested
}

fn bundle_artifacts_for_stages(stages: &BTreeMap<Stage, CapturedStage>) -> BTreeSet<ArtifactKey> {
    let mut requested = BTreeSet::new();
    for (stage, captured) in stages {
        match (stage, captured) {
            (Stage::Asm, CapturedStage::Text(_)) => {
                requested.insert(ArtifactKey::Asm);
            }
            (Stage::Obj, CapturedStage::Text(_)) => {
                requested.insert(ArtifactKey::Obj);
            }
            (Stage::Run, CapturedStage::Run(_)) => {
                requested.insert(ArtifactKey::Runtime);
            }
            (stage, CapturedStage::Text(_)) => {
                requested.insert(ArtifactKey::Extra(format!("armfortas.{}", stage.as_str())));
            }
            _ => {}
        }
    }
    requested
}

fn reference_observations_for_bundle(
    program: &Path,
    opt_level: OptLevel,
    artifacts: &ExecutionArtifacts,
) -> Vec<ObservedProgram> {
    if artifacts.reference_observations.len() == artifacts.references.len() {
        artifacts.reference_observations.clone()
    } else {
        artifacts
            .references
            .iter()
            .map(|reference| {
                observed_program_from_reference_result(
                    program,
                    opt_level,
                    default_differential_artifacts(),
                    reference,
                )
            })
            .collect()
    }
}

fn write_reference_summary_bundle(
    refs_root: &Path,
    references: &[ReferenceResult],
    observations: &[ObservedProgram],
) -> Result<(), String> {
    let summary = render_reference_bundle_summary(references, observations);
    fs::write(refs_root.join("summary.txt"), summary)
        .map_err(|e| format!("cannot write reference summary bundle: {}", e))
}

fn render_reference_bundle_summary(
    references: &[ReferenceResult],
    observations: &[ObservedProgram],
) -> String {
    let mut lines = vec![
        format!("reference_count: {}", references.len()),
        format!(
            "compilers: {}",
            if references.is_empty() {
                "none".to_string()
            } else {
                references
                    .iter()
                    .map(|reference| reference.compiler.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
    ];

    for (reference, observed) in references.iter().zip(observations.iter()) {
        let observation = &observed.observation;
        lines.push(String::new());
        lines.push(format!("compiler: {}", reference.compiler.as_str()));
        lines.push(format!("status: {}", introspection_status(observation)));
        lines.push(format!(
            "compile_exit_code: {}",
            observation.compile_exit_code
        ));
        lines.push(format!("command: {}", reference.compile_command));
        lines.push(format!(
            "generic_artifacts: {}",
            join_or_none_from_strings(
                &observation_generic_artifacts(observation)
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>()
            )
        ));
        lines.push(format!(
            "adapter_extras: {}",
            format_adapter_extra_summary(&observation_adapter_extras(observation))
        ));
    }

    lines.join("\n") + "\n"
}

fn write_reference_bundle(
    root: &Path,
    program: &Path,
    opt_level: OptLevel,
    reference: &ReferenceResult,
    observed: Option<&ObservedProgram>,
) -> Result<(), String> {
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
    write_reference_observation_bundle(&ref_root, program, opt_level, reference, observed)?;
    Ok(())
}

fn write_reference_observation_bundle(
    ref_root: &Path,
    program: &Path,
    opt_level: OptLevel,
    reference: &ReferenceResult,
    observed: Option<&ObservedProgram>,
) -> Result<(), String> {
    let observed = observed.cloned().unwrap_or_else(|| {
        observed_program_from_reference_result(
            program,
            opt_level,
            default_differential_artifacts(),
            reference,
        )
    });
    let render_config = IntrospectionRenderConfig {
        summary_only: false,
        max_artifact_lines: None,
    };
    fs::write(
        ref_root.join("observation.txt"),
        render_introspection_text(&observed, render_config),
    )
    .map_err(|e| format!("cannot write reference observation text bundle: {}", e))?;
    fs::write(
        ref_root.join("observation.json"),
        render_introspection_json(&observed),
    )
    .map_err(|e| format!("cannot write reference observation json bundle: {}", e))?;
    fs::write(
        ref_root.join("observation.md"),
        render_introspection_markdown(&observed, render_config),
    )
    .map_err(|e| format!("cannot write reference observation markdown bundle: {}", e))?;
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

fn next_primary_cli_temp_root(opt_level: OptLevel) -> PathBuf {
    default_report_root().join(".tmp").join(format!(
        "primary_cli_{}_{}",
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
    negative: bool,
    kind: &'static str,
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
                negative: false,
                kind: "CHECK",
            })
        })
        .collect()
}

fn extract_xfail_reason(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        line.trim()
            .strip_prefix("! XFAIL:")
            .map(|rest| rest.trim().to_string())
    })
}

fn extract_error_expected_patterns(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("! ERROR_EXPECTED:")
                .map(|rest| rest.trim().to_string())
        })
        .collect()
}

fn extract_ir_checks(source: &str) -> Vec<Check> {
    source
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("! IR_CHECK:") {
                Some(Check {
                    line_num: i + 1,
                    pattern: rest.trim().to_string(),
                    negative: false,
                    kind: "IR_CHECK",
                })
            } else {
                trimmed.strip_prefix("! IR_NOT:").map(|rest| Check {
                    line_num: i + 1,
                    pattern: rest.trim().to_string(),
                    negative: true,
                    kind: "IR_NOT",
                })
            }
        })
        .collect()
}

fn match_checks(checks: &[Check], output: &str, case_name: &str) -> Result<(), String> {
    let output_lines: Vec<&str> = output.lines().collect();
    let mut output_idx = 0;

    for check in checks {
        if check.negative {
            if output.contains(&check.pattern) {
                return Err(format!(
                    "{}:{}: {} failed: substring '{}' appears in output\nfull output:\n{}",
                    case_name, check.line_num, check.kind, check.pattern, output
                ));
            }
            continue;
        }

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
                "{}:{}: {} failed: expected '{}' not found in remaining output\nfull output:\n{}",
                case_name, check.line_num, check.kind, check.pattern, output
            ));
        }
    }

    Ok(())
}

fn target_uses_ir_comment_checks(target: &Target) -> bool {
    match target {
        Target::Stage(Stage::Ir) => true,
        Target::Artifact(ArtifactKey::Extra(name)) => name == "armfortas.ir",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyBackend {
        mode: &'static str,
        detail: &'static str,
    }

    impl CaptureBackend for DummyBackend {
        fn mode_name(&self) -> &'static str {
            self.mode
        }

        fn description(&self) -> &'static str {
            self.detail
        }

        fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, CaptureFailure> {
            Err(CaptureFailure {
                input: request.input.clone(),
                opt_level: request.opt_level,
                stage: FailureStage::Ir,
                detail: self.detail.to_string(),
                stages: BTreeMap::new(),
            })
        }
    }

    #[cfg(unix)]
    fn bencch_repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[cfg(unix)]
    fn fake_compiler_fixture(name: &str) -> PathBuf {
        bencch_repo_root()
            .join("fixtures")
            .join("fake_compilers")
            .join(name)
    }

    #[cfg(unix)]
    fn runtime_fixture(name: &str) -> PathBuf {
        bencch_repo_root()
            .join("fixtures")
            .join("runtime")
            .join(name)
    }

    fn full_introspection_render_config() -> IntrospectionRenderConfig {
        IntrospectionRenderConfig {
            summary_only: false,
            max_artifact_lines: None,
        }
    }

    #[cfg(unix)]
    fn invalid_fixture(name: &str) -> PathBuf {
        bencch_repo_root()
            .join("fixtures")
            .join("invalid")
            .join(name)
    }

    #[cfg(unix)]
    fn ensure_fixture_executable(path: &Path) {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn command_is_available(name: &str) -> bool {
        Command::new("which")
            .arg(name)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    fn armfortas_smoke_binary() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("BENCCH_ARMFORTAS_SMOKE_BIN") {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }

        let candidate = bencch_repo_root()
            .parent()?
            .join("target")
            .join("debug")
            .join("armfortas");
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    }

    #[cfg(unix)]
    fn stable_runtime_compare_corpus() -> Vec<PathBuf> {
        [
            "allocatable.f90",
            "do_while.f90",
            "exit_cycle.f90",
            "nested_loops.f90",
            "subroutine_call.f90",
            "string_fixed.f90",
            "if_else.f90",
            "mixed_types.f90",
            "select_case.f90",
            "function_call.f90",
            "real_function.f90",
            "where_construct.f90",
        ]
        .into_iter()
        .map(runtime_fixture)
        .collect()
    }

    fn stable_runtime_compare_opt_levels() -> Vec<OptLevel> {
        vec![OptLevel::O0, OptLevel::O1, OptLevel::O2]
    }
    use crate::compiler::test_support::{
        verify_module, BlockParam, FloatWidth, Function, Inst, InstKind, IntWidth, IrType, Module,
        Position, Span, Terminator, ValueId,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn dummy_span() -> Span {
        Span {
            file_id: 0,
            start: Position { line: 1, col: 1 },
            end: Position { line: 1, col: 1 },
        }
    }

    #[test]
    fn primary_backend_selection_uses_observable_backend_for_external_cases() {
        let case = CaseSpec {
            name: "runtime_case".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: vec![ReferenceCompiler::Gfortran],
            consistency_checks: vec![ConsistencyCheck::CliRunReproducible],
            expectations: vec![Expectation::Contains {
                target: Target::RunStdout,
                needle: "42".into(),
            }],
            status_rules: Vec::new(),
        };
        let requested = BTreeSet::from([Stage::Run]);
        let external_tools = ToolchainConfig {
            armfortas: ArmfortasCliAdapter::External("/tmp/armfortas".into()),
            gfortran: "gfortran".into(),
            flang_new: "flang-new".into(),
            system_as: "as".into(),
            otool: "otool".into(),
            nm: "nm".into(),
        };

        assert_eq!(
            primary_backend_kind_for_case(&case, &requested, &external_tools),
            PrimaryCaptureBackendKind::Observable
        );
        let selected =
            select_primary_capture_backend(&case, &requested, OptLevel::O0, &external_tools);
        assert_eq!(selected.backend.mode_name(), "cli-observable");

        let linked_tools = ToolchainConfig {
            armfortas: ArmfortasCliAdapter::Linked,
            ..external_tools.clone()
        };
        assert_eq!(
            primary_backend_kind_for_case(&case, &requested, &linked_tools),
            PrimaryCaptureBackendKind::Full
        );

        let mut capture_check_case = case.clone();
        capture_check_case.consistency_checks = vec![ConsistencyCheck::CaptureRunVsCliRun];
        assert_eq!(
            primary_backend_kind_for_case(&capture_check_case, &requested, &external_tools),
            PrimaryCaptureBackendKind::Full
        );

        let mut failure_case = case.clone();
        failure_case.expectations.push(Expectation::FailContains {
            stage: FailureStage::Run,
            needle: "broken".into(),
        });
        assert_eq!(
            primary_backend_kind_for_case(&failure_case, &requested, &external_tools),
            PrimaryCaptureBackendKind::Full
        );

        let richer_request = BTreeSet::from([Stage::Run, Stage::Asm]);
        assert_eq!(
            primary_backend_kind_for_case(&case, &richer_request, &external_tools),
            PrimaryCaptureBackendKind::Observable
        );

        let asm_only_request = BTreeSet::from([Stage::Asm]);
        assert_eq!(
            primary_backend_kind_for_case(&case, &asm_only_request, &external_tools),
            PrimaryCaptureBackendKind::Observable
        );
    }

    #[test]
    fn legacy_unavailable_backend_detail_is_explicit() {
        let case = CaseSpec {
            name: "frontend_case".into(),
            source: PathBuf::from("frontend.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Tokens]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 1,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::Contains {
                target: Target::Stage(Stage::Tokens),
                needle: "program".into(),
            }],
            status_rules: Vec::new(),
        };
        let backend = SelectedPrimaryBackend {
            kind: PrimaryCaptureBackendKind::Full,
            backend: Box::new(DummyBackend {
                mode: "unavailable",
                detail: "unavailable without linked-armfortas feature",
            }),
        };

        let detail = legacy_unavailable_backend_detail(&case, &backend).unwrap();
        assert!(detail.contains("case requires linked armfortas capture"));
        assert!(detail.contains("scripts/bootstrap-linked-armfortas.sh"));
    }

    #[test]
    fn legacy_cli_consistency_cases_use_generic_observation_path() {
        let cli_only_case = CaseSpec {
            name: "cli-consistency".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: vec![
                ConsistencyCheck::CliAsmReproducible,
                ConsistencyCheck::CliRunReproducible,
            ],
            expectations: vec![Expectation::Contains {
                target: Target::RunStdout,
                needle: "42".into(),
            }],
            status_rules: Vec::new(),
        };
        assert!(legacy_case_uses_generic_consistency_checks(&cli_only_case));

        let mixed_case = CaseSpec {
            consistency_checks: vec![
                ConsistencyCheck::CliRunReproducible,
                ConsistencyCheck::CaptureRunReproducible,
            ],
            ..cli_only_case.clone()
        };
        assert!(!legacy_case_uses_generic_consistency_checks(&mixed_case));
    }

    #[test]
    fn legacy_observable_cases_use_generic_observation_execution() {
        let observable_case = CaseSpec {
            name: "observable".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::Contains {
                target: Target::RunStdout,
                needle: "42".into(),
            }],
            status_rules: Vec::new(),
        };
        assert!(legacy_case_uses_generic_observation_execution(
            &observable_case,
            &observable_case.requested
        ));

        let richer_case = CaseSpec {
            requested: BTreeSet::from([Stage::Run, Stage::Ir]),
            ..observable_case.clone()
        };
        assert!(!legacy_case_uses_generic_observation_execution(
            &richer_case,
            &richer_case.requested
        ));

        let failure_case = CaseSpec {
            expectations: vec![Expectation::FailContains {
                stage: FailureStage::Run,
                needle: "boom".into(),
            }],
            ..observable_case
        };
        assert!(!legacy_case_uses_generic_observation_execution(
            &failure_case,
            &failure_case.requested
        ));
    }

    #[cfg(unix)]
    #[test]
    fn external_cli_primary_execution_returns_observable_stages() {
        let root = std::env::temp_dir().join("afs_tests_external_cli_primary");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let source = root.join("demo.f90");
        fs::write(&source, "program demo\nprint *, 42\nend program\n").unwrap();

        let compiler = root.join("fake-armfortas");
        fs::write(
            &compiler,
            "#!/bin/sh\nmode=bin\nout=\"\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    -S)\n      mode=asm\n      shift\n      ;;\n    -c)\n      mode=obj\n      shift\n      ;;\n    -o)\n      out=\"$2\"\n      shift 2\n      ;;\n    *)\n      shift\n      ;;\n  esac\ndone\nif [ \"$mode\" = asm ]; then\n  cat > \"$out\" <<'EOF'\n.globl _main\n_main:\n  ret\nEOF\nelif [ \"$mode\" = obj ]; then\n  printf 'fake object\\n' > \"$out\"\nelse\n  cat > \"$out\" <<'EOF'\n#!/bin/sh\nprintf '42\\n'\nEOF\n  chmod +x \"$out\"\nfi\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&compiler).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&compiler, perms).unwrap();

        let case = CaseSpec {
            name: "runtime_case".into(),
            source: source.clone(),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Asm, Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![
                Expectation::Contains {
                    target: Target::Stage(Stage::Asm),
                    needle: ".globl _main".into(),
                },
                Expectation::Contains {
                    target: Target::RunStdout,
                    needle: "42".into(),
                },
            ],
            status_rules: Vec::new(),
        };
        let prepared = PreparedInput {
            compiler_source: source.clone(),
            generated_source: None,
            temp_root: None,
        };
        let tools = ToolchainConfig {
            armfortas: ArmfortasCliAdapter::External(compiler.display().to_string()),
            gfortran: "gfortran".into(),
            flang_new: "flang-new".into(),
            system_as: "as".into(),
            otool: "otool".into(),
            nm: "nm".into(),
        };
        let requested = BTreeSet::from([Stage::Asm, Stage::Run]);
        let selected = select_primary_capture_backend(&case, &requested, OptLevel::O0, &tools);
        assert_eq!(selected.kind, PrimaryCaptureBackendKind::Observable);
        assert_eq!(selected.backend.mode_name(), "cli-observable");

        let result =
            execute_primary_armfortas(&prepared, OptLevel::O0, &requested, &selected).unwrap();
        let asm = capture_text_stage(&result, Stage::Asm).unwrap();
        let run = capture_run_stage(&result).unwrap();
        assert!(asm.contains(".globl _main"));
        assert_eq!(run.exit_code, 0);
        assert_eq!(run.stdout, "42\n");
        assert!(run.stderr.is_empty());
        assert_eq!(result.stages.len(), 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn compare_uses_generic_external_driver_observations() {
        let compiler_a = fake_compiler_fixture("match_42_a.sh");
        let compiler_b = fake_compiler_fixture("runtime_41.sh");
        let source = runtime_fixture("mixed_types.f90");
        ensure_fixture_executable(&compiler_a);
        ensure_fixture_executable(&compiler_b);

        let config = CompareConfig {
            left: CompilerSpec::Binary(compiler_a.clone()),
            right: CompilerSpec::Binary(compiler_b.clone()),
            program: source.clone(),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::from([ArtifactKey::Asm]),
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let result = run_compare(&config).unwrap();
        assert_eq!(result.left.provenance.backend_mode, "external-driver");
        assert_eq!(result.right.provenance.backend_mode, "external-driver");
        assert!(result
            .differences
            .iter()
            .any(|difference| difference.artifact == "runtime"));
        assert!(result
            .differences
            .iter()
            .any(|difference| difference.artifact == "asm"));
        let rendered = render_compare_text(&result);
        assert!(rendered.contains("status: diff"));
        assert!(rendered.contains("classification: mixed divergence"));
        assert!(rendered.contains("difference_count: 2"));
    }

    #[cfg(unix)]
    #[test]
    fn compare_fixture_compilers_report_compile_failures() {
        let source = runtime_fixture("mixed_types.f90");
        let compiler_fail = fake_compiler_fixture("compile_fail.sh");
        let compiler_ok = fake_compiler_fixture("match_42_a.sh");
        ensure_fixture_executable(&compiler_fail);
        ensure_fixture_executable(&compiler_ok);

        let config = CompareConfig {
            left: CompilerSpec::Binary(compiler_fail),
            right: CompilerSpec::Binary(compiler_ok),
            program: source,
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::new(),
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let result = run_compare(&config).unwrap();
        assert_eq!(compare_status(&result), "diff");
        assert_eq!(compare_classification(&result), "compile divergence");
        assert!(result
            .differences
            .iter()
            .any(|difference| difference.artifact == "compile-exit-code"));
        let diagnostics = result
            .differences
            .iter()
            .find(|difference| difference.artifact == "diagnostics")
            .unwrap();
        assert!(diagnostics
            .detail
            .contains("fake compiler failure: missing lowering pass"));
    }

    #[test]
    fn compare_rejects_capability_mismatch_for_namespaced_artifacts() {
        let config = CompareConfig {
            left: CompilerSpec::Named(NamedCompiler::Armfortas),
            right: CompilerSpec::Named(NamedCompiler::Gfortran),
            program: runtime_fixture("if_else.f90"),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::from([ArtifactKey::Extra("armfortas.ir".into())]),
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let err = run_compare(&config).unwrap_err();
        assert!(err.contains("compare request is not supported"));
        assert!(err.contains("right gfortran"));
        assert!(err.contains("armfortas.ir"));
    }

    #[test]
    fn compare_executable_artifact_uses_file_contents_not_paths() {
        let root = std::env::temp_dir().join("bencch_compare_executable_paths");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let left_exe = root.join("left.out");
        let right_exe = root.join("right.out");
        fs::write(&left_exe, b"same executable bytes").unwrap();
        fs::write(&right_exe, b"same executable bytes").unwrap();

        let requested = BTreeSet::from([ArtifactKey::Executable]);
        let left = CompilerObservation {
            compiler: CompilerSpec::Binary(PathBuf::from("/tmp/left-compiler")),
            program: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            compile_exit_code: 0,
            artifacts: BTreeMap::from([(
                ArtifactKey::Executable,
                ArtifactValue::Path(left_exe.clone()),
            )]),
            provenance: ObservationProvenance {
                compiler_identity: "left".into(),
                adapter_kind: "explicit-path".into(),
                backend_mode: "external-driver".into(),
                backend_detail: "left detail".into(),
                artifacts_captured: vec!["executable".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        };
        let right = CompilerObservation {
            compiler: CompilerSpec::Binary(PathBuf::from("/tmp/right-compiler")),
            program: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            compile_exit_code: 0,
            artifacts: BTreeMap::from([(
                ArtifactKey::Executable,
                ArtifactValue::Path(right_exe.clone()),
            )]),
            provenance: ObservationProvenance {
                compiler_identity: "right".into(),
                adapter_kind: "explicit-path".into(),
                backend_mode: "external-driver".into(),
                backend_detail: "right detail".into(),
                artifacts_captured: vec!["executable".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        };

        let result = compare_observations(left, right, &requested);
        assert!(result.differences.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compare_executable_artifact_reports_binary_difference() {
        let root = std::env::temp_dir().join("bencch_compare_executable_bytes");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let left_exe = root.join("left.out");
        let right_exe = root.join("right.out");
        fs::write(&left_exe, b"abc").unwrap();
        fs::write(&right_exe, b"axc").unwrap();

        let requested = BTreeSet::from([ArtifactKey::Executable]);
        let left = CompilerObservation {
            compiler: CompilerSpec::Binary(PathBuf::from("/tmp/left-compiler")),
            program: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            compile_exit_code: 0,
            artifacts: BTreeMap::from([(
                ArtifactKey::Executable,
                ArtifactValue::Path(left_exe.clone()),
            )]),
            provenance: ObservationProvenance {
                compiler_identity: "left".into(),
                adapter_kind: "explicit-path".into(),
                backend_mode: "external-driver".into(),
                backend_detail: "left detail".into(),
                artifacts_captured: vec!["executable".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        };
        let right = CompilerObservation {
            compiler: CompilerSpec::Binary(PathBuf::from("/tmp/right-compiler")),
            program: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            compile_exit_code: 0,
            artifacts: BTreeMap::from([(
                ArtifactKey::Executable,
                ArtifactValue::Path(right_exe.clone()),
            )]),
            provenance: ObservationProvenance {
                compiler_identity: "right".into(),
                adapter_kind: "explicit-path".into(),
                backend_mode: "external-driver".into(),
                backend_detail: "right detail".into(),
                artifacts_captured: vec!["executable".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        };

        let result = compare_observations(left, right, &requested);
        assert_eq!(result.differences.len(), 1);
        assert_eq!(result.differences[0].artifact, "executable");
        assert!(result.differences[0]
            .detail
            .contains("first differing byte: 1"));

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn compare_cli_with_fixture_compilers_writes_match_reports() {
        let left = fake_compiler_fixture("match_42_a.sh");
        let right = fake_compiler_fixture("match_42_b.sh");
        let source = runtime_fixture("mixed_types.f90");
        ensure_fixture_executable(&left);
        ensure_fixture_executable(&right);

        let root = std::env::temp_dir().join("bencch_compare_fixture_reports");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let json_report = root.join("compare.json");
        let markdown_report = root.join("compare.md");

        let args = vec![
            "compare".to_string(),
            left.display().to_string(),
            right.display().to_string(),
            "--program".to_string(),
            source.display().to_string(),
            "--artifact".to_string(),
            "asm,obj".to_string(),
            "--json-report".to_string(),
            json_report.display().to_string(),
            "--markdown-report".to_string(),
            markdown_report.display().to_string(),
        ];

        let exit = run_cli_named("bencch", &args);
        assert_eq!(exit, 0);

        let json = fs::read_to_string(&json_report).unwrap();
        assert!(json.contains("\"status\": \"match\""));
        assert!(json.contains("\"classification\": \"match\""));
        assert!(json.contains("\"difference_count\": 0"));
        assert!(json.contains("\"changed_artifacts\": []"));

        let markdown = fs::read_to_string(&markdown_report).unwrap();
        assert!(markdown.contains("status: match"));
        assert!(markdown.contains("classification: match"));
        assert!(markdown.contains("difference_count: 0"));
        assert!(markdown.contains("changed_artifacts: none"));

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn compare_named_real_compilers_match_on_runtime_corpus() {
        if !command_is_available("gfortran") || !command_is_available("flang-new") {
            return;
        }

        for opt_level in stable_runtime_compare_opt_levels() {
            for program in stable_runtime_compare_corpus() {
                let config = CompareConfig {
                    left: CompilerSpec::Named(NamedCompiler::Gfortran),
                    right: CompilerSpec::Named(NamedCompiler::FlangNew),
                    program,
                    opt_level,
                    artifacts: BTreeSet::new(),
                    json_report: None,
                    markdown_report: None,
                    tools: ToolchainConfig::from_env(),
                };

                let result = run_compare(&config).unwrap();
                assert_eq!(compare_status(&result), "match");
                assert_eq!(compare_classification(&result), "match");
                assert!(result.differences.is_empty());
                assert_eq!(result.left.provenance.adapter_kind, "named");
                assert_eq!(result.right.provenance.adapter_kind, "named");
                assert_eq!(result.left.opt_level, opt_level);
                assert_eq!(result.right.opt_level, opt_level);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn compare_armfortas_and_gfortran_match_on_runtime_corpus_when_available() {
        if !command_is_available("gfortran") {
            return;
        }
        let Some(armfortas_bin) = armfortas_smoke_binary() else {
            return;
        };

        let mut tools = ToolchainConfig::from_env();
        tools.armfortas = ArmfortasCliAdapter::External(armfortas_bin.display().to_string());

        for opt_level in stable_runtime_compare_opt_levels() {
            for program in stable_runtime_compare_corpus() {
                let config = CompareConfig {
                    left: CompilerSpec::Named(NamedCompiler::Armfortas),
                    right: CompilerSpec::Named(NamedCompiler::Gfortran),
                    program,
                    opt_level,
                    artifacts: BTreeSet::new(),
                    json_report: None,
                    markdown_report: None,
                    tools: tools.clone(),
                };

                let result = run_compare(&config).unwrap();
                assert_eq!(compare_status(&result), "match");
                assert_eq!(compare_classification(&result), "match");
                assert!(result.differences.is_empty());
                assert_eq!(result.left.provenance.backend_mode, "cli-observable");
                assert_eq!(result.left.opt_level, opt_level);
                assert_eq!(result.right.opt_level, opt_level);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn introspect_armfortas_rich_artifacts_on_runtime_fixture() {
        let config = IntrospectConfig {
            compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
            program: runtime_fixture("if_else.f90"),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::from([
                ArtifactKey::Asm,
                ArtifactKey::Extra("armfortas.tokens".into()),
                ArtifactKey::Extra("armfortas.ir".into()),
            ]),
            json_report: None,
            markdown_report: None,
            all_artifacts: false,
            summary_only: false,
            max_artifact_lines: None,
            tools: ToolchainConfig::from_env(),
        };

        let observed = run_introspect(&config).unwrap();
        let observation = &observed.observation;
        assert_eq!(observation.compile_exit_code, 0);
        assert_eq!(observation.provenance.backend_mode, "linked");
        assert!(observation.artifacts.contains_key(&ArtifactKey::Asm));
        assert!(observation
            .artifacts
            .contains_key(&ArtifactKey::Extra("armfortas.tokens".into())));
        assert!(observation
            .artifacts
            .contains_key(&ArtifactKey::Extra("armfortas.ir".into())));

        let ir = match observation
            .artifacts
            .get(&ArtifactKey::Extra("armfortas.ir".into()))
            .unwrap()
        {
            ArtifactValue::Text(text) => text,
            other => panic!("expected text ir artifact, got {:?}", other),
        };
        assert!(ir.contains("func") || ir.contains("module"));

        let rendered = render_introspection_text(&observed, full_introspection_render_config());
        assert!(rendered.contains("Generic artifacts"));
        assert!(rendered.contains("Adapter extras"));
        assert!(rendered.contains("-- armfortas --"));
        assert!(rendered.contains("== ir =="));
    }

    #[cfg(unix)]
    #[test]
    fn introspect_armfortas_all_artifacts_includes_stage_extras() {
        let config = IntrospectConfig {
            compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
            program: runtime_fixture("if_else.f90"),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::new(),
            json_report: None,
            markdown_report: None,
            all_artifacts: true,
            summary_only: false,
            max_artifact_lines: None,
            tools: ToolchainConfig::from_env(),
        };

        let observed = run_introspect(&config).unwrap();
        let observation = &observed.observation;
        for artifact in [
            ArtifactKey::Asm,
            ArtifactKey::Obj,
            ArtifactKey::Runtime,
            ArtifactKey::Extra("armfortas.preprocess".into()),
            ArtifactKey::Extra("armfortas.tokens".into()),
            ArtifactKey::Extra("armfortas.ast".into()),
            ArtifactKey::Extra("armfortas.sema".into()),
            ArtifactKey::Extra("armfortas.ir".into()),
            ArtifactKey::Extra("armfortas.optir".into()),
            ArtifactKey::Extra("armfortas.mir".into()),
            ArtifactKey::Extra("armfortas.regalloc".into()),
        ] {
            assert!(
                observation.artifacts.contains_key(&artifact),
                "missing artifact {}",
                artifact.as_str()
            );
        }
        assert!(missing_introspection_artifact_names(&observed).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn introspect_armfortas_failure_reports_stage_and_partial_capture() {
        let config = IntrospectConfig {
            compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
            program: invalid_fixture("parse_error.f90"),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::from([
                ArtifactKey::Asm,
                ArtifactKey::Extra("armfortas.tokens".into()),
                ArtifactKey::Extra("armfortas.ir".into()),
            ]),
            json_report: None,
            markdown_report: None,
            all_artifacts: false,
            summary_only: false,
            max_artifact_lines: None,
            tools: ToolchainConfig::from_env(),
        };

        let observed = run_introspect(&config).unwrap();
        let observation = &observed.observation;
        assert_eq!(observation.compile_exit_code, 1);
        assert_eq!(
            observation.provenance.failure_stage.as_deref(),
            Some("parser")
        );
        assert!(observation
            .artifacts
            .contains_key(&ArtifactKey::Diagnostics));
        assert!(observation
            .artifacts
            .contains_key(&ArtifactKey::Extra("armfortas.tokens".into())));
        assert!(missing_introspection_artifact_names(&observed).contains(&"asm".to_string()));
        assert!(
            missing_introspection_artifact_names(&observed).contains(&"armfortas.ir".to_string())
        );

        let rendered = render_introspection_text(&observed, full_introspection_render_config());
        assert!(rendered.contains("status: compile failed"));
        assert!(rendered.contains("failure_stage: parser"));
        assert!(rendered.contains("diagnostic_excerpt:"));
    }

    #[cfg(unix)]
    #[test]
    fn introspect_named_external_compiler_reports_generic_artifacts_when_available() {
        if !command_is_available("gfortran") {
            return;
        }

        let config = IntrospectConfig {
            compiler: CompilerSpec::Named(NamedCompiler::Gfortran),
            program: runtime_fixture("if_else.f90"),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::from([ArtifactKey::Asm, ArtifactKey::Obj, ArtifactKey::Runtime]),
            json_report: None,
            markdown_report: None,
            all_artifacts: false,
            summary_only: false,
            max_artifact_lines: None,
            tools: ToolchainConfig::from_env(),
        };

        let observed = run_introspect(&config).unwrap();
        let observation = &observed.observation;
        assert_eq!(observation.compile_exit_code, 0);
        assert_eq!(observation.provenance.backend_mode, "external-driver");
        assert_eq!(observation.provenance.adapter_kind, "named");
        assert!(observation.artifacts.contains_key(&ArtifactKey::Asm));
        assert!(observation.artifacts.contains_key(&ArtifactKey::Obj));
        assert!(observation.artifacts.contains_key(&ArtifactKey::Runtime));
        assert!(observation_adapter_extras(observation).is_empty());
        assert!(missing_introspection_artifact_names(&observed).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn introspect_explicit_path_compiler_reports_generic_artifacts_when_available() {
        let compiler = fake_compiler_fixture("match_42_a.sh");
        ensure_fixture_executable(&compiler);

        let config = IntrospectConfig {
            compiler: CompilerSpec::Binary(compiler.clone()),
            program: runtime_fixture("if_else.f90"),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::from([ArtifactKey::Asm, ArtifactKey::Obj, ArtifactKey::Runtime]),
            json_report: None,
            markdown_report: None,
            all_artifacts: false,
            summary_only: false,
            max_artifact_lines: None,
            tools: ToolchainConfig::from_env(),
        };

        let observed = run_introspect(&config).unwrap();
        let observation = &observed.observation;
        assert_eq!(observation.compile_exit_code, 0);
        assert_eq!(observation.provenance.backend_mode, "external-driver");
        assert_eq!(observation.provenance.adapter_kind, "explicit-path");
        assert!(observation
            .provenance
            .backend_detail
            .contains("match_42_a.sh"));
        assert!(observation.artifacts.contains_key(&ArtifactKey::Asm));
        assert!(observation.artifacts.contains_key(&ArtifactKey::Obj));
        assert!(observation.artifacts.contains_key(&ArtifactKey::Runtime));
        assert!(observation_adapter_extras(observation).is_empty());
        assert!(missing_introspection_artifact_names(&observed).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn introspect_external_failure_reports_missing_requested_artifacts() {
        let compiler = fake_compiler_fixture("compile_fail.sh");
        ensure_fixture_executable(&compiler);

        let config = IntrospectConfig {
            compiler: CompilerSpec::Binary(compiler),
            program: runtime_fixture("if_else.f90"),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::from([ArtifactKey::Asm, ArtifactKey::Obj, ArtifactKey::Runtime]),
            json_report: None,
            markdown_report: None,
            all_artifacts: false,
            summary_only: false,
            max_artifact_lines: None,
            tools: ToolchainConfig::from_env(),
        };

        let observed = run_introspect(&config).unwrap();
        let observation = &observed.observation;
        assert_eq!(observation.compile_exit_code, 1);
        assert_eq!(observation.provenance.failure_stage, None);
        assert!(observation
            .artifacts
            .contains_key(&ArtifactKey::Diagnostics));
        assert_eq!(
            missing_introspection_artifact_names(&observed),
            vec!["asm".to_string(), "obj".to_string(), "runtime".to_string()]
        );

        let rendered = render_introspection_text(&observed, full_introspection_render_config());
        assert!(rendered.contains("status: compile failed"));
        assert!(rendered.contains("failure_stage: none"));
    }

    #[test]
    fn introspect_named_external_compiler_rejects_namespaced_artifacts() {
        let config = IntrospectConfig {
            compiler: CompilerSpec::Named(NamedCompiler::Gfortran),
            program: runtime_fixture("if_else.f90"),
            opt_level: OptLevel::O0,
            artifacts: BTreeSet::from([ArtifactKey::Extra("armfortas.ir".into())]),
            json_report: None,
            markdown_report: None,
            all_artifacts: false,
            summary_only: false,
            max_artifact_lines: None,
            tools: ToolchainConfig::from_env(),
        };

        let observed = run_introspect(&config).unwrap();
        let observation = &observed.observation;
        assert_eq!(observation.compile_exit_code, 1);
        assert_eq!(observation.provenance.backend_mode, "external-driver");
        assert_eq!(observation.provenance.failure_stage, None);
        let diagnostics = match observation.artifacts.get(&ArtifactKey::Diagnostics) {
            Some(ArtifactValue::Text(text)) => text,
            other => panic!("expected text diagnostics, got {:?}", other),
        };
        assert!(diagnostics.contains("does not support requested artifacts"));
        assert!(diagnostics.contains("armfortas.ir"));
    }

    #[test]
    fn compose_observation_failure_detail_uses_unavailable_wording() {
        let observation = CompilerObservation {
            compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
            program: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            compile_exit_code: 1,
            artifacts: BTreeMap::from([(
                ArtifactKey::Diagnostics,
                ArtifactValue::Text("linked armfortas capture is unavailable in this build".into()),
            )]),
            provenance: ObservationProvenance {
                compiler_identity: "armfortas".into(),
                adapter_kind: "named".into(),
                backend_mode: "unavailable".into(),
                backend_detail: "unavailable without linked-armfortas feature".into(),
                artifacts_captured: vec!["diagnostics".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        };

        let detail = compose_observation_failure_detail(&observation);
        assert!(detail.contains("armfortas unavailable for requested artifacts in this build"));
        assert!(!detail.contains("failed in"));
    }

    #[test]
    fn compose_observation_failure_detail_uses_unsupported_wording() {
        let observation = CompilerObservation {
            compiler: CompilerSpec::Named(NamedCompiler::Gfortran),
            program: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            compile_exit_code: 1,
            artifacts: BTreeMap::from([(
                ArtifactKey::Diagnostics,
                ArtifactValue::Text(
                    "gfortran does not support requested artifacts in this adapter: armfortas.ir"
                        .into(),
                ),
            )]),
            provenance: ObservationProvenance {
                compiler_identity: "gfortran".into(),
                adapter_kind: "named".into(),
                backend_mode: "external-driver".into(),
                backend_detail: "generic external driver adapter using gfortran".into(),
                artifacts_captured: vec!["diagnostics".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        };

        let detail = compose_observation_failure_detail(&observation);
        assert!(detail.contains("gfortran does not support requested artifacts in this adapter"));
        assert!(!detail.contains("gfortran failed"));
    }

    #[test]
    fn execute_generic_introspect_case_reports_capability_mismatch_clearly() {
        let suite = SuiteSpec {
            name: "v2/generic-introspect".into(),
            path: PathBuf::from("suite.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "gfortran-armfortas-ir".into(),
            source: runtime_fixture("if_else.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            generic_introspect: Some(GenericIntrospectCase {
                compiler: CompilerSpec::Named(NamedCompiler::Gfortran),
                artifacts: BTreeSet::from([ArtifactKey::Extra("armfortas.ir".into())]),
            }),
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::Contains {
                target: Target::Artifact(ArtifactKey::Extra("armfortas.ir".into())),
                needle: "func".into(),
            }],
            status_rules: Vec::new(),
        };
        let config = RunConfig {
            suite_filter: None,
            case_filter: None,
            opt_filter: None,
            verbose: false,
            fail_fast: false,
            include_future: false,
            all_stages: false,
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let outcome = execute_case_cell(&suite, &case, OptLevel::O0, &config).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Fail);
        assert!(outcome
            .detail
            .contains("gfortran does not support requested artifacts in this adapter"));
        assert!(!outcome.detail.contains("gfortran failed"));
    }

    #[cfg(unix)]
    #[test]
    fn execute_generic_suite_case_uses_introspect_engine() {
        let compiler = fake_compiler_fixture("match_42_a.sh");
        ensure_fixture_executable(&compiler);

        let suite = SuiteSpec {
            name: "v2/generic-introspect".into(),
            path: PathBuf::from("suite.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "fake-runtime".into(),
            source: runtime_fixture("if_else.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            generic_introspect: Some(GenericIntrospectCase {
                compiler: CompilerSpec::Binary(compiler),
                artifacts: BTreeSet::from([
                    ArtifactKey::Asm,
                    ArtifactKey::Obj,
                    ArtifactKey::Runtime,
                ]),
            }),
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![
                Expectation::Contains {
                    target: Target::Artifact(ArtifactKey::Asm),
                    needle: ".globl _main".into(),
                },
                Expectation::Contains {
                    target: Target::RunStdout,
                    needle: "42".into(),
                },
            ],
            status_rules: Vec::new(),
        };
        let config = RunConfig {
            suite_filter: None,
            case_filter: None,
            opt_filter: None,
            verbose: false,
            fail_fast: false,
            include_future: false,
            all_stages: false,
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let outcome = execute_case_cell(&suite, &case, OptLevel::O0, &config).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Pass);
        assert!(outcome.detail.is_empty());
        assert!(outcome.bundle.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn execute_generic_suite_case_supports_cli_consistency() {
        let compiler = fake_compiler_fixture("match_42_a.sh");
        ensure_fixture_executable(&compiler);

        let suite = SuiteSpec {
            name: "v2/generic-consistency".into(),
            path: PathBuf::from("suite.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "fake-runtime-consistency".into(),
            source: runtime_fixture("if_else.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            generic_introspect: Some(GenericIntrospectCase {
                compiler: CompilerSpec::Binary(compiler),
                artifacts: BTreeSet::from([ArtifactKey::Asm, ArtifactKey::Runtime]),
            }),
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: vec![
                ConsistencyCheck::CliAsmReproducible,
                ConsistencyCheck::CliRunReproducible,
            ],
            expectations: vec![
                Expectation::Contains {
                    target: Target::Artifact(ArtifactKey::Asm),
                    needle: ".globl _main".into(),
                },
                Expectation::Contains {
                    target: Target::RunStdout,
                    needle: "42".into(),
                },
            ],
            status_rules: Vec::new(),
        };
        let config = RunConfig {
            suite_filter: None,
            case_filter: None,
            opt_filter: None,
            verbose: false,
            fail_fast: false,
            include_future: false,
            all_stages: false,
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let outcome = execute_case_cell(&suite, &case, OptLevel::O0, &config).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Pass);
        assert!(outcome.detail.is_empty());
        assert!(outcome.consistency_observations.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn execute_generic_suite_case_supports_differential_when_available() {
        if !command_is_available("gfortran") || !command_is_available("flang-new") {
            return;
        }

        let suite = SuiteSpec {
            name: "v2/generic-differential".into(),
            path: PathBuf::from("suite.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "gfortran-vs-flang".into(),
            source: runtime_fixture("if_else.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            generic_introspect: Some(GenericIntrospectCase {
                compiler: CompilerSpec::Named(NamedCompiler::Gfortran),
                artifacts: BTreeSet::from([ArtifactKey::Runtime]),
            }),
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: vec![ReferenceCompiler::FlangNew],
            consistency_checks: Vec::new(),
            expectations: vec![
                Expectation::Contains {
                    target: Target::RunStdout,
                    needle: "positive".into(),
                },
                Expectation::IntEquals {
                    target: Target::RunExitCode,
                    value: 0,
                },
            ],
            status_rules: Vec::new(),
        };
        let config = RunConfig {
            suite_filter: None,
            case_filter: None,
            opt_filter: None,
            verbose: false,
            fail_fast: false,
            include_future: false,
            all_stages: false,
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let outcome = execute_case_cell(&suite, &case, OptLevel::O0, &config).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Pass);
        assert!(outcome.detail.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn execute_generic_compare_suite_case_uses_compare_engine() {
        let left = fake_compiler_fixture("match_42_a.sh");
        let right = fake_compiler_fixture("runtime_41.sh");
        ensure_fixture_executable(&left);
        ensure_fixture_executable(&right);

        let suite = SuiteSpec {
            name: "v2/generic-compare".into(),
            path: PathBuf::from("suite.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "fake-divergence".into(),
            source: runtime_fixture("if_else.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            generic_introspect: None,
            generic_compare: Some(GenericCompareCase {
                left: CompilerSpec::Binary(left),
                right: CompilerSpec::Binary(right),
                artifacts: BTreeSet::from([
                    ArtifactKey::Diagnostics,
                    ArtifactKey::Runtime,
                    ArtifactKey::Asm,
                ]),
            }),
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![
                Expectation::Equals {
                    target: Target::CompareStatus,
                    value: "diff".into(),
                },
                Expectation::Equals {
                    target: Target::CompareClassification,
                    value: "mixed divergence".into(),
                },
                Expectation::Contains {
                    target: Target::CompareChangedArtifacts,
                    needle: "asm".into(),
                },
                Expectation::Contains {
                    target: Target::CompareChangedArtifacts,
                    needle: "runtime".into(),
                },
                Expectation::IntEquals {
                    target: Target::CompareDifferenceCount,
                    value: 2,
                },
            ],
            status_rules: Vec::new(),
        };
        let config = RunConfig {
            suite_filter: None,
            case_filter: None,
            opt_filter: None,
            verbose: false,
            fail_fast: false,
            include_future: false,
            all_stages: false,
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let outcome = execute_case_cell(&suite, &case, OptLevel::O0, &config).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Pass);
    }

    #[test]
    fn execute_generic_compare_suite_case_reports_capability_mismatch() {
        let suite = SuiteSpec {
            name: "v2/generic-compare".into(),
            path: PathBuf::from("suite.afs"),
            cases: Vec::new(),
        };
        let case = CaseSpec {
            name: "armfortas-ir-vs-gfortran".into(),
            source: runtime_fixture("if_else.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            generic_introspect: None,
            generic_compare: Some(GenericCompareCase {
                left: CompilerSpec::Named(NamedCompiler::Armfortas),
                right: CompilerSpec::Named(NamedCompiler::Gfortran),
                artifacts: BTreeSet::from([
                    ArtifactKey::Diagnostics,
                    ArtifactKey::Runtime,
                    ArtifactKey::Extra("armfortas.ir".into()),
                ]),
            }),
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::Equals {
                target: Target::CompareStatus,
                value: "match".into(),
            }],
            status_rules: Vec::new(),
        };
        let config = RunConfig {
            suite_filter: None,
            case_filter: None,
            opt_filter: None,
            verbose: false,
            fail_fast: false,
            include_future: false,
            all_stages: false,
            json_report: None,
            markdown_report: None,
            tools: ToolchainConfig::from_env(),
        };

        let outcome = execute_case_cell(&suite, &case, OptLevel::O0, &config).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::Fail);
        assert!(outcome.detail.contains("compare request is not supported"));
        assert!(outcome.detail.contains("armfortas.ir"));
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
        assert!(suite.cases[0].generic_introspect.is_none());
        assert!(matches!(
            suite.cases[0].expectations[2],
            Expectation::NotContains {
                target: Target::Artifact(ArtifactKey::Asm),
                ..
            }
        ));
        assert_eq!(suite.cases[0].opt_levels, vec![OptLevel::O0]);
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn parses_generic_compiler_case() {
        let root = std::env::temp_dir().join("bencch_generic_parser_spec.afs");
        fs::write(
            &root,
            r#"suite "v2/generic-introspect"

case "fake-runtime"
source "../../fixtures/runtime/if_else.f90"
compiler gfortran => asm, obj, runtime
expect asm contains ".globl _main"
expect run.stdout contains "42"
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        let case = &suite.cases[0];
        let generic = case.generic_introspect.as_ref().unwrap();
        assert_eq!(
            generic.compiler,
            CompilerSpec::Named(NamedCompiler::Gfortran)
        );
        assert!(generic.artifacts.contains(&ArtifactKey::Asm));
        assert!(generic.artifacts.contains(&ArtifactKey::Obj));
        assert!(generic.artifacts.contains(&ArtifactKey::Runtime));
        assert!(case.requested.is_empty());
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn parses_generic_compiler_case_with_differential_and_cli_consistency() {
        let root = std::env::temp_dir().join("bencch_generic_differential_parser_spec.afs");
        fs::write(
            &root,
            r#"suite "v2/generic-differential"

case "gfortran_runtime_matrix"
source "../../fixtures/runtime/if_else.f90"
opts => O0, O1, O2
repeat => 3
compiler gfortran => runtime, asm
differential => flang-new
consistency => cli_asm_reproducible, cli_run_reproducible
expect run.stdout check-comments
expect run.exit_code equals 0
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        let case = &suite.cases[0];
        let generic = case.generic_introspect.as_ref().unwrap();
        assert_eq!(
            generic.compiler,
            CompilerSpec::Named(NamedCompiler::Gfortran)
        );
        assert!(generic.artifacts.contains(&ArtifactKey::Runtime));
        assert!(generic.artifacts.contains(&ArtifactKey::Asm));
        assert_eq!(case.reference_compilers, vec![ReferenceCompiler::FlangNew]);
        assert_eq!(
            case.consistency_checks,
            vec![
                ConsistencyCheck::CliAsmReproducible,
                ConsistencyCheck::CliRunReproducible,
            ]
        );
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn rejects_capture_consistency_on_generic_compiler_case() {
        let root = std::env::temp_dir().join("bencch_generic_capture_consistency_parser_spec.afs");
        fs::write(
            &root,
            r#"suite "v2/generic-consistency"

case "armfortas_capture_run"
source "../../fixtures/runtime/if_else.f90"
compiler armfortas => runtime
consistency => capture_run_reproducible
expect run.exit_code equals 0
end
"#,
        )
        .unwrap();

        let err = parse_suite_file(&root).unwrap_err();
        assert!(err.contains("generic compiler cases only support"));
        assert!(err.contains("capture_run_reproducible"));
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn parses_generic_compare_case() {
        let root = std::env::temp_dir().join("bencch_generic_compare_parser_spec.afs");
        fs::write(
            &root,
            r#"suite "v2/generic-compare"

case "fake-match"
source "../../fixtures/runtime/if_else.f90"
opts => O0, O1, O2
compare gfortran flang-new => asm
expect compare.status equals "match"
expect compare.difference_count equals 0
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        let case = &suite.cases[0];
        let generic = case.generic_compare.as_ref().unwrap();
        assert_eq!(generic.left, CompilerSpec::Named(NamedCompiler::Gfortran));
        assert_eq!(generic.right, CompilerSpec::Named(NamedCompiler::FlangNew));
        assert!(generic.artifacts.contains(&ArtifactKey::Asm));
        assert!(generic.artifacts.contains(&ArtifactKey::Diagnostics));
        assert!(generic.artifacts.contains(&ArtifactKey::Runtime));
        assert_eq!(
            case.opt_levels,
            vec![OptLevel::O0, OptLevel::O1, OptLevel::O2]
        );
        let _ = fs::remove_file(&root);
    }

    #[test]
    fn parses_generic_compare_case_with_namespaced_artifact() {
        let root = std::env::temp_dir().join("bencch_generic_compare_namespaced_spec.afs");
        fs::write(
            &root,
            r#"suite "v2/generic-compare"

case "armfortas-ir"
source "../../fixtures/runtime/if_else.f90"
compare armfortas armfortas => armfortas.ir
expect compare.status equals "match"
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&root).unwrap();
        let case = &suite.cases[0];
        let generic = case.generic_compare.as_ref().unwrap();
        assert_eq!(generic.left, CompilerSpec::Named(NamedCompiler::Armfortas));
        assert_eq!(generic.right, CompilerSpec::Named(NamedCompiler::Armfortas));
        assert!(generic
            .artifacts
            .contains(&ArtifactKey::Extra("armfortas.ir".into())));
        assert!(generic.artifacts.contains(&ArtifactKey::Diagnostics));
        assert!(generic.artifacts.contains(&ArtifactKey::Runtime));

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
            "--json-report".to_string(),
            "/tmp/report.json".to_string(),
            "--markdown-report".to_string(),
            "/tmp/report.md".to_string(),
            "--armfortas-bin".to_string(),
            "/tmp/armfortas".to_string(),
            "--gfortran-bin".to_string(),
            "/tmp/gfortran".to_string(),
            "--flang-bin".to_string(),
            "/tmp/flang-new".to_string(),
            "--as-bin".to_string(),
            "/tmp/as".to_string(),
            "--otool-bin".to_string(),
            "/tmp/otool".to_string(),
            "--nm-bin".to_string(),
            "/tmp/nm".to_string(),
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
            config.json_report.as_deref(),
            Some(Path::new("/tmp/report.json"))
        );
        assert_eq!(
            config.markdown_report.as_deref(),
            Some(Path::new("/tmp/report.md"))
        );
        assert_eq!(
            config.tools.armfortas,
            ArmfortasCliAdapter::External("/tmp/armfortas".into())
        );
        assert_eq!(config.tools.gfortran, "/tmp/gfortran");
        assert_eq!(config.tools.flang_new, "/tmp/flang-new");
        assert_eq!(config.tools.system_as, "/tmp/as");
        assert_eq!(config.tools.otool, "/tmp/otool");
        assert_eq!(config.tools.nm, "/tmp/nm");
    }

    #[test]
    fn parse_cli_collects_list_config() {
        let args = vec![
            "list".to_string(),
            "--suite".to_string(),
            "v2/generic".to_string(),
            "--verbose".to_string(),
            "--armfortas-bin".to_string(),
            "/tmp/armfortas".to_string(),
        ];

        let command = parse_cli(&args).unwrap();
        let config = match command {
            CommandKind::List(config) => config,
            other => panic!(
                "expected list command, got {:?}",
                std::mem::discriminant(&other)
            ),
        };

        assert_eq!(config.suite_filter.as_deref(), Some("v2/generic"));
        assert!(config.verbose);
        assert_eq!(
            config.tools.armfortas,
            ArmfortasCliAdapter::External("/tmp/armfortas".into())
        );
    }

    #[test]
    fn parse_cli_collects_doctor_tool_overrides() {
        let args = vec![
            "doctor".to_string(),
            "--json-report".to_string(),
            "/tmp/doctor.json".to_string(),
            "--markdown-report".to_string(),
            "/tmp/doctor.md".to_string(),
            "--armfortas-bin".to_string(),
            "/tmp/armfortas".to_string(),
            "--gfortran-bin".to_string(),
            "/tmp/gfortran".to_string(),
            "--flang-bin".to_string(),
            "/tmp/flang-new".to_string(),
            "--as-bin".to_string(),
            "/tmp/as".to_string(),
            "--otool-bin".to_string(),
            "/tmp/otool".to_string(),
            "--nm-bin".to_string(),
            "/tmp/nm".to_string(),
        ];

        let command = parse_cli(&args).unwrap();
        let config = match command {
            CommandKind::Doctor(config) => config,
            other => panic!(
                "expected doctor command, got {:?}",
                std::mem::discriminant(&other)
            ),
        };

        assert_eq!(
            config.tools.armfortas,
            ArmfortasCliAdapter::External("/tmp/armfortas".into())
        );
        assert_eq!(config.tools.gfortran, "/tmp/gfortran");
        assert_eq!(config.tools.flang_new, "/tmp/flang-new");
        assert_eq!(config.tools.system_as, "/tmp/as");
        assert_eq!(config.tools.otool, "/tmp/otool");
        assert_eq!(config.tools.nm, "/tmp/nm");
        assert_eq!(
            config.json_report.as_deref(),
            Some(Path::new("/tmp/doctor.json"))
        );
        assert_eq!(
            config.markdown_report.as_deref(),
            Some(Path::new("/tmp/doctor.md"))
        );
    }

    #[test]
    fn parse_cli_collects_compare_config() {
        let args = vec![
            "compare".to_string(),
            "armfortas".to_string(),
            "/tmp/other-compiler".to_string(),
            "--program".to_string(),
            "/tmp/demo.f90".to_string(),
            "--opt".to_string(),
            "O2".to_string(),
            "--artifact".to_string(),
            "asm,obj,armfortas.ir".to_string(),
            "--json-report".to_string(),
            "/tmp/compare.json".to_string(),
            "--markdown-report".to_string(),
            "/tmp/compare.md".to_string(),
        ];

        let command = parse_cli(&args).unwrap();
        let config = match command {
            CommandKind::Compare(config) => config,
            other => panic!(
                "expected compare command, got {:?}",
                std::mem::discriminant(&other)
            ),
        };

        assert_eq!(config.left, CompilerSpec::Named(NamedCompiler::Armfortas));
        assert_eq!(
            config.right,
            CompilerSpec::Binary(PathBuf::from("/tmp/other-compiler"))
        );
        assert_eq!(config.program, PathBuf::from("/tmp/demo.f90"));
        assert_eq!(config.opt_level, OptLevel::O2);
        assert!(config.artifacts.contains(&ArtifactKey::Asm));
        assert!(config.artifacts.contains(&ArtifactKey::Obj));
        assert!(config
            .artifacts
            .contains(&ArtifactKey::Extra("armfortas.ir".into())));
        assert_eq!(
            config.json_report.as_deref(),
            Some(Path::new("/tmp/compare.json"))
        );
        assert_eq!(
            config.markdown_report.as_deref(),
            Some(Path::new("/tmp/compare.md"))
        );
    }

    #[test]
    fn parse_cli_collects_introspect_config() {
        let args = vec![
            "introspect".to_string(),
            "armfortas".to_string(),
            "/tmp/demo.f90".to_string(),
            "--artifact".to_string(),
            "armfortas.ir,asm".to_string(),
            "--all".to_string(),
            "--summary-only".to_string(),
            "--max-artifact-lines".to_string(),
            "12".to_string(),
            "--json-report".to_string(),
            "/tmp/introspect.json".to_string(),
        ];

        let command = parse_cli(&args).unwrap();
        let config = match command {
            CommandKind::Introspect(config) => config,
            other => panic!(
                "expected introspect command, got {:?}",
                std::mem::discriminant(&other)
            ),
        };

        assert_eq!(
            config.compiler,
            CompilerSpec::Named(NamedCompiler::Armfortas)
        );
        assert_eq!(config.program, PathBuf::from("/tmp/demo.f90"));
        assert!(config.artifacts.contains(&ArtifactKey::Asm));
        assert!(config
            .artifacts
            .contains(&ArtifactKey::Extra("armfortas.ir".into())));
        assert!(config.all_artifacts);
        assert!(config.summary_only);
        assert_eq!(config.max_artifact_lines, Some(12));
        assert_eq!(
            config.json_report.as_deref(),
            Some(Path::new("/tmp/introspect.json"))
        );
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
    fn parses_failure_expectation_from_source_comments() {
        let root = std::env::temp_dir().join("afs_tests_failure_comment_spec");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("fixtures")).unwrap();
        fs::create_dir_all(root.join("suites")).unwrap();

        let source = root.join("fixtures/error_expected.f90");
        fs::write(
            &source,
            "! ERROR_EXPECTED: hidden\nprogram error_expected\n  print *, hidden\nend program\n",
        )
        .unwrap();

        let suite_path = root.join("suites/spec.afs");
        fs::write(
            &suite_path,
            r#"suite "v2/comment-failure"

case "error_expected_comments"
source "../fixtures/error_expected.f90"
compiler armfortas => diagnostics
expect-fail comments
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&suite_path).unwrap();
        match &suite.cases[0].expectations[0] {
            Expectation::FailCommentPatterns(patterns) => {
                assert_eq!(patterns, &vec!["hidden".to_string()]);
            }
            other => panic!("expected source-comment failure expectation, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_xfail_comments_from_source() {
        let root = std::env::temp_dir().join("afs_tests_xfail_comment_spec");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("fixtures")).unwrap();
        fs::create_dir_all(root.join("suites")).unwrap();

        let source = root.join("fixtures/xfail_case.f90");
        fs::write(
            &source,
            "! XFAIL: audit BLOCKING-1 (demo)\nprogram xfail_case\nend program\n",
        )
        .unwrap();

        let suite_path = root.join("suites/spec.afs");
        fs::write(
            &suite_path,
            r#"suite "v2/comment-xfail"

case "xfail_comments"
source "../fixtures/xfail_case.f90"
compiler armfortas => runtime
xfail comments
end
"#,
        )
        .unwrap();

        let suite = parse_suite_file(&suite_path).unwrap();
        match status_for_opt(&suite.cases[0], OptLevel::O0) {
            EffectiveStatus::Xfail(reason) => {
                assert_eq!(reason, "audit BLOCKING-1 (demo)");
            }
            other => panic!("expected xfail status, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn check_matching_preserves_order() {
        let checks = vec![
            Check {
                line_num: 1,
                pattern: "alpha".into(),
                negative: false,
                kind: "CHECK",
            },
            Check {
                line_num: 2,
                pattern: "omega".into(),
                negative: false,
                kind: "CHECK",
            },
        ];
        assert!(match_checks(&checks, "alpha\nmiddle\nomega\n", "demo").is_ok());
        assert!(match_checks(&checks, "omega\nalpha\n", "demo").is_err());
    }

    #[test]
    fn ir_check_matching_supports_negative_patterns() {
        let checks = vec![
            Check {
                line_num: 1,
                pattern: "func @demo".into(),
                negative: false,
                kind: "IR_CHECK",
            },
            Check {
                line_num: 2,
                pattern: "zeroinit".into(),
                negative: true,
                kind: "IR_NOT",
            },
        ];
        let ok_ir = "module main\n\n  func @demo() -> void {\n    entry():\n      ret void\n  }\n";
        assert!(match_checks(&checks, ok_ir, "demo").is_ok());

        let bad_ir = "module main\n  global @value: i32 = zeroinit\n  func @demo() -> void {\n    entry():\n      ret void\n  }\n";
        let err = match_checks(&checks, bad_ir, "demo").unwrap_err();
        assert!(err.contains("IR_NOT failed"));
    }

    fn run_only_result(stdout: &str, stderr: &str, exit_code: i32) -> CaptureResult {
        CaptureResult {
            input: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            stages: std::collections::BTreeMap::from([(
                Stage::Run,
                CapturedStage::Run(RunCapture {
                    exit_code,
                    stdout: stdout.into(),
                    stderr: stderr.into(),
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
                stdout: stdout.into(),
                stderr: stderr.into(),
            }),
            run_error: None,
        }
    }

    fn differential_armfortas_observation(
        stdout: &str,
        stderr: &str,
        exit_code: i32,
    ) -> CompilerObservation {
        observed_program_from_armfortas_capture(
            Path::new("demo.f90"),
            OptLevel::O0,
            default_differential_artifacts(),
            &run_only_result(stdout, stderr, exit_code),
            None,
        )
        .observation
    }

    fn differential_reference_observation(
        compiler: ReferenceCompiler,
        stdout: &str,
        stderr: &str,
        exit_code: i32,
    ) -> CompilerObservation {
        observed_program_from_reference_result(
            Path::new("demo.f90"),
            OptLevel::O0,
            default_differential_artifacts(),
            &reference_run(compiler, stdout, stderr, exit_code),
        )
        .observation
    }

    #[test]
    fn not_contains_expectation_checks_text_absence() {
        let case = CaseSpec {
            name: "no_reserved_register".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Asm]),
            generic_introspect: None,
            generic_compare: None,
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
        let observed = observed_program_from_armfortas_capture(
            Path::new("demo.f90"),
            OptLevel::O0,
            expected_artifacts_for_legacy_case(&case),
            &result,
            None,
        );
        assert!(evaluate_observation_expectations(&case, &observed).is_ok());

        let bad = CaptureResult {
            input: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            stages: std::collections::BTreeMap::from([(
                Stage::Asm,
                CapturedStage::Text("mov x18, x0\nret\n".into()),
            )]),
        };
        let observed = observed_program_from_armfortas_capture(
            Path::new("demo.f90"),
            OptLevel::O0,
            expected_artifacts_for_legacy_case(&case),
            &bad,
            None,
        );
        let err = evaluate_observation_expectations(&case, &observed).unwrap_err();
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
            generic_introspect: None,
            generic_compare: None,
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
                stdout: "oops\n".into(),
                stderr: "broken\n".into(),
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
            armfortas_observation: Some(ObservedProgram {
                observation: CompilerObservation {
                    compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
                    program: source.clone(),
                    opt_level: OptLevel::O0,
                    compile_exit_code: 1,
                    artifacts: BTreeMap::from([
                        (
                            ArtifactKey::Diagnostics,
                            ArtifactValue::Text("cached observation failure".into()),
                        ),
                        (
                            ArtifactKey::Extra("armfortas.ast".into()),
                            ArtifactValue::Text("program hello".into()),
                        ),
                    ]),
                    provenance: ObservationProvenance {
                        compiler_identity: "armfortas".into(),
                        adapter_kind: "named".into(),
                        backend_mode: "linked".into(),
                        backend_detail: "linked armfortas::testing capture adapter".into(),
                        artifacts_captured: vec!["diagnostics".into(), "armfortas.ast".into()],
                        comparison_basis: None,
                        failure_stage: Some("sema".into()),
                    },
                },
                requested_artifacts: BTreeSet::from([
                    ArtifactKey::Diagnostics,
                    ArtifactKey::Extra("armfortas.ast".into()),
                ]),
            }),
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
            reference_observations: vec![ObservedProgram {
                observation: CompilerObservation {
                    compiler: CompilerSpec::Named(NamedCompiler::Gfortran),
                    program: source.clone(),
                    opt_level: OptLevel::O0,
                    compile_exit_code: 0,
                    artifacts: BTreeMap::from([(
                        ArtifactKey::Asm,
                        ArtifactValue::Text(".globl _main".into()),
                    )]),
                    provenance: ObservationProvenance {
                        compiler_identity: "gfortran".into(),
                        adapter_kind: "named".into(),
                        backend_mode: "legacy-reference".into(),
                        backend_detail: "cached reference observation".into(),
                        artifacts_captured: vec!["asm".into()],
                        comparison_basis: None,
                        failure_stage: None,
                    },
                },
                requested_artifacts: BTreeSet::from([ArtifactKey::Asm]),
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
            primary_backend: Some(PrimaryBackendReport {
                kind: "full".into(),
                mode: "linked".into(),
                detail: "linked armfortas::testing capture adapter".into(),
            }),
            consistency_observations: Vec::new(),
        };
        let prepared = PreparedInput {
            compiler_source: source.clone(),
            generated_source: None,
            temp_root: None,
        };

        let bundle = write_failure_bundle(&suite, &case, &prepared, &outcome, &artifacts).unwrap();
        assert!(bundle.join("metadata.txt").exists());
        assert!(bundle.join("detail.txt").exists());
        assert!(bundle.join("source.f90").exists());
        assert!(bundle.join("armfortas").join("ir.txt").exists());
        assert!(bundle.join("armfortas").join("metadata.txt").exists());
        assert!(bundle.join("armfortas").join("observation.txt").exists());
        assert!(bundle.join("armfortas").join("observation.json").exists());
        assert!(bundle.join("armfortas").join("observation.md").exists());
        assert!(bundle.join("armfortas").join("run.stdout.txt").exists());
        assert!(bundle.join("armfortas").join("error.txt").exists());
        assert!(bundle
            .join("references")
            .join("gfortran")
            .join("observation.txt")
            .exists());
        assert!(bundle
            .join("references")
            .join("gfortran")
            .join("observation.json")
            .exists());
        assert!(bundle
            .join("references")
            .join("gfortran")
            .join("observation.md")
            .exists());
        assert!(bundle.join("references").join("summary.txt").exists());
        assert!(bundle
            .join("references")
            .join("gfortran")
            .join("run.stdout.txt")
            .exists());
        assert!(bundle.join("consistency").join("summary.txt").exists());
        let metadata = fs::read_to_string(bundle.join("metadata.txt")).unwrap();
        assert!(metadata.contains("primary_backend_kind: full"));
        assert!(metadata.contains("primary_backend_mode: linked"));
        assert!(
            metadata.contains("primary_backend_detail: linked armfortas::testing capture adapter")
        );
        let armfortas_metadata =
            fs::read_to_string(bundle.join("armfortas").join("metadata.txt")).unwrap();
        assert!(armfortas_metadata.contains("primary_backend_kind: full"));
        assert!(armfortas_metadata.contains("primary_backend_mode: linked"));
        assert!(armfortas_metadata
            .contains("primary_backend_detail: linked armfortas::testing capture adapter"));
        assert!(armfortas_metadata.contains("captured_stages: ir, run"));
        assert!(armfortas_metadata.contains("error_stage: sema"));
        let observation =
            fs::read_to_string(bundle.join("armfortas").join("observation.txt")).unwrap();
        assert!(observation.contains("Introspect"));
        assert!(observation.contains("compiler: armfortas"));
        assert!(observation.contains("failure_stage: sema"));
        assert!(observation.contains("generic_artifacts: diagnostics"));
        assert!(observation.contains("adapter_extras: armfortas(ast)"));
        assert!(observation.contains("cached observation failure"));
        let reference_observation = fs::read_to_string(
            bundle
                .join("references")
                .join("gfortran")
                .join("observation.txt"),
        )
        .unwrap();
        assert!(reference_observation.contains("Introspect"));
        assert!(reference_observation.contains("compiler: gfortran"));
        assert!(reference_observation.contains("status: compile ok"));
        assert!(reference_observation.contains("requested_artifacts: asm"));
        assert!(reference_observation.contains("generic_artifacts: asm"));
        assert!(reference_observation.contains("cached reference observation"));
        let reference_summary =
            fs::read_to_string(bundle.join("references").join("summary.txt")).unwrap();
        assert!(reference_summary.contains("reference_count: 1"));
        assert!(reference_summary.contains("compilers: gfortran"));
        assert!(reference_summary.contains("compiler: gfortran"));
        assert!(reference_summary.contains("status: compile ok"));
        assert!(reference_summary.contains("compile_exit_code: 0"));
        assert!(reference_summary.contains("command: gfortran hello.f90 -o hello"));
        assert!(reference_summary.contains("generic_artifacts: asm"));
        assert!(reference_summary.contains("adapter_extras: none"));
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
    fn materializes_graph_input_in_declared_file_order() {
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
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };

        let prepared = prepare_case_input(&case, &suite, OptLevel::O0).unwrap();
        let generated = fs::read_to_string(&prepared.compiler_source).unwrap();
        assert!(generated.contains("module math_values"));
        assert!(generated.contains("program main"));
        assert!(
            generated.find("module math_values").unwrap() < generated.find("program main").unwrap()
        );

        cleanup_prepared_input(&prepared);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn graph_failure_bundle_writes_authored_sources() {
        let root = std::env::temp_dir().join("afs_tests_graph_bundle");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let module = root.join("math_values.f90");
        let main = root.join("main.f90");
        let generated = root.join("generated.f90");
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
        fs::write(&generated, "module math_values\n integer :: answer = 42\nend module\n\nprogram main\n use math_values\n print *, answer\nend program\n").unwrap();

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
            generic_introspect: None,
            generic_compare: None,
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
            primary_backend: Some(PrimaryBackendReport {
                kind: "full".into(),
                mode: "linked".into(),
                detail: "linked armfortas::testing capture adapter".into(),
            }),
            consistency_observations: Vec::new(),
        };
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Run]),
            armfortas: Some(run_only_result("42\n", "", 0)),
            armfortas_failure: None,
            armfortas_observation: None,
            references: Vec::new(),
            reference_observations: Vec::new(),
            consistency_issues: Vec::new(),
        };
        let prepared = PreparedInput {
            compiler_source: generated.clone(),
            generated_source: Some(generated.clone()),
            temp_root: None,
        };

        let bundle = write_failure_bundle(&suite, &case, &prepared, &outcome, &artifacts).unwrap();
        assert!(bundle.join("source.f90").exists());
        assert!(bundle.join("sources").join("00_math_values.f90").exists());
        assert!(bundle.join("sources").join("01_main.f90").exists());

        let _ = fs::remove_dir_all(bundle);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn armfortas_bundle_observation_prefers_cached_observation() {
        let prepared = PreparedInput {
            compiler_source: PathBuf::from("demo.f90"),
            generated_source: None,
            temp_root: None,
        };
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Run]),
            armfortas: Some(run_only_result("42\n", "", 0)),
            armfortas_failure: None,
            armfortas_observation: Some(ObservedProgram {
                observation: CompilerObservation {
                    compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
                    program: PathBuf::from("demo.f90"),
                    opt_level: OptLevel::O0,
                    compile_exit_code: 0,
                    artifacts: BTreeMap::from([(
                        ArtifactKey::Extra("armfortas.sema".into()),
                        ArtifactValue::Text("ok".into()),
                    )]),
                    provenance: ObservationProvenance {
                        compiler_identity: "armfortas".into(),
                        adapter_kind: "named".into(),
                        backend_mode: "linked".into(),
                        backend_detail: "linked armfortas::testing capture adapter".into(),
                        artifacts_captured: vec!["armfortas.sema".into()],
                        comparison_basis: None,
                        failure_stage: None,
                    },
                },
                requested_artifacts: BTreeSet::from([ArtifactKey::Extra("armfortas.sema".into())]),
            }),
            references: Vec::new(),
            reference_observations: Vec::new(),
            consistency_issues: Vec::new(),
        };

        let observed = observed_program_for_armfortas_bundle(&prepared, &artifacts).unwrap();
        assert!(observed
            .observation
            .artifacts
            .contains_key(&ArtifactKey::Extra("armfortas.sema".into())));
        assert!(!observed
            .observation
            .artifacts
            .contains_key(&ArtifactKey::Runtime));
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
    fn render_reports_include_outcomes() {
        let mut summary = Summary::default();
        summary.record_outcome(&Outcome {
            suite: "modules/runtime-graphs".into(),
            case: "module_chain_runtime".into(),
            opt_level: OptLevel::O0,
            kind: OutcomeKind::Xfail,
            detail: "expected 42, got 0".into(),
            bundle: Some(PathBuf::from("/tmp/bundle")),
            primary_backend: Some(PrimaryBackendReport {
                kind: "observable".into(),
                mode: "cli-observable".into(),
                detail: "cli-observable armfortas driver capture adapter".into(),
            }),
            consistency_observations: vec![ConsistencyObservation {
                check: ConsistencyCheck::CliRunReproducible,
                summary: "repeat_count=3 unique_variants=1".into(),
                repeat_count: Some(3),
                unique_variant_count: Some(1),
                varying_components: Vec::new(),
                stable_components: vec!["exit_code".into(), "stdout".into(), "stderr".into()],
            }],
        });

        let json = render_json_report(&summary);
        assert!(json.contains("\"outcomes\": ["));
        assert!(json.contains("\"suite\": \"modules/runtime-graphs\""));
        assert!(json.contains("\"bundle\": \"/tmp/bundle\""));
        assert!(json.contains("\"primary_backend\": {"));
        assert!(json.contains("\"mode\": \"cli-observable\""));

        let markdown = render_markdown_report(&summary);
        assert!(markdown.contains("# afs-tests report"));
        assert!(markdown
            .contains("### `modules/runtime-graphs` / `module_chain_runtime` / `O0` / `xfail`"));
        assert!(markdown.contains("primary_backend: `observable` (`cli-observable`)"));
        assert!(markdown.contains("bundle: `/tmp/bundle`"));
        assert!(markdown.contains("expected 42, got 0"));
    }

    #[test]
    fn render_generic_reports_include_provenance() {
        let observation = CompilerObservation {
            compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
            program: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            compile_exit_code: 0,
            artifacts: BTreeMap::from([
                (
                    ArtifactKey::Asm,
                    ArtifactValue::Text(".globl _main\n".into()),
                ),
                (
                    ArtifactKey::Extra("armfortas.ir".into()),
                    ArtifactValue::Text("module main".into()),
                ),
            ]),
            provenance: ObservationProvenance {
                compiler_identity: "armfortas".into(),
                adapter_kind: "named".into(),
                backend_mode: "linked".into(),
                backend_detail: "linked armfortas::testing capture adapter".into(),
                artifacts_captured: vec!["asm".into(), "armfortas.ir".into()],
                comparison_basis: None,
                failure_stage: None,
            },
        };
        let compare = ComparisonResult {
            left: observation.clone(),
            right: CompilerObservation {
                compiler: CompilerSpec::Named(NamedCompiler::Gfortran),
                program: PathBuf::from("demo.f90"),
                opt_level: OptLevel::O0,
                compile_exit_code: 0,
                artifacts: BTreeMap::from([(
                    ArtifactKey::Asm,
                    ArtifactValue::Text(".arch armv8.5-a".into()),
                )]),
                provenance: ObservationProvenance {
                    compiler_identity: "gfortran".into(),
                    adapter_kind: "named".into(),
                    backend_mode: "external-driver".into(),
                    backend_detail: "generic external driver adapter using gfortran".into(),
                    artifacts_captured: vec!["asm".into()],
                    comparison_basis: Some("compile-status, diagnostics, runtime, asm".into()),
                    failure_stage: None,
                },
            },
            basis: "compile-status, diagnostics, runtime, asm".into(),
            differences: vec![ArtifactDifference {
                artifact: "asm".into(),
                detail: "first differing line: 1".into(),
            }],
        };

        let observed = ObservedProgram {
            observation: observation.clone(),
            requested_artifacts: BTreeSet::from([
                ArtifactKey::Asm,
                ArtifactKey::Extra("armfortas.ir".into()),
                ArtifactKey::Extra("armfortas.tokens".into()),
            ]),
        };

        let introspection_text =
            render_introspection_text(&observed, full_introspection_render_config());
        assert!(introspection_text.contains("status: compile ok"));
        assert!(introspection_text.contains("artifact_count: 2"));
        assert!(
            introspection_text.contains("requested_artifacts: asm, armfortas.ir, armfortas.tokens")
        );
        assert!(introspection_text.contains("missing_artifacts: armfortas.tokens"));
        assert!(introspection_text.contains("generic_artifacts: asm"));
        assert!(introspection_text.contains("adapter_extras: armfortas(ir)"));
        assert!(introspection_text.contains("Generic artifacts"));
        assert!(introspection_text.contains("Adapter extras"));

        let introspection_json = render_introspection_json(&observed);
        assert!(introspection_json.contains("\"status\": \"compile ok\""));
        assert!(introspection_json.contains("\"artifact_count\": 2"));
        assert!(introspection_json.contains(
            "\"requested_artifacts\": [\"asm\", \"armfortas.ir\", \"armfortas.tokens\"]"
        ));
        assert!(introspection_json.contains("\"missing_artifacts\": [\"armfortas.tokens\"]"));
        assert!(introspection_json.contains("\"artifact_summaries\":"));
        assert!(introspection_json.contains("\"asm\": {\"kind\":\"text\""));
        assert!(introspection_json.contains("\"line_count\":1"));
        assert!(introspection_json.contains("\"generic_artifacts\": [\"asm\"]"));
        assert!(introspection_json.contains("\"adapter_extras\": {\"armfortas\": [\"ir\"]}"));
        assert!(introspection_json.contains("\"backend_mode\": \"linked\""));
        assert!(introspection_json.contains("\"armfortas.ir\""));

        let introspection_markdown =
            render_introspection_markdown(&observed, full_introspection_render_config());
        assert!(introspection_markdown.contains("# bencch introspect report"));
        assert!(introspection_markdown.contains("status: compile ok"));
        assert!(introspection_markdown.contains("failure_stage: `none`"));
        assert!(introspection_markdown.contains("artifact_count: 2"));
        assert!(introspection_markdown
            .contains("requested_artifacts: `asm`, `armfortas.ir`, `armfortas.tokens`"));
        assert!(introspection_markdown.contains("missing_artifacts: `armfortas.tokens`"));
        assert!(introspection_markdown.contains("## Generic artifacts"));
        assert!(introspection_markdown.contains("## Adapter extras"));
        assert!(introspection_markdown.contains("### `armfortas`"));
        assert!(introspection_markdown.contains("#### `ir`"));

        let compare_markdown = render_compare_markdown(&compare);
        assert!(compare_markdown.contains("# bencch compare report"));
        assert!(compare_markdown.contains("status: diff"));
        assert!(compare_markdown.contains("classification: artifact divergence"));
        assert!(compare_markdown.contains("difference_count: 1"));
        assert!(compare_markdown.contains("changed_artifacts: asm"));
        assert!(compare_markdown.contains("backend_mode: `external-driver`"));
        assert!(compare_markdown.contains("### `asm`"));

        let compare_json = render_compare_json(&compare);
        assert!(compare_json.contains("\"status\": \"diff\""));
        assert!(compare_json.contains("\"classification\": \"artifact divergence\""));
        assert!(compare_json.contains("\"difference_count\": 1"));
        assert!(compare_json.contains("\"changed_artifacts\": [\"asm\"]"));
    }

    #[test]
    fn render_failure_introspection_reports_stage_and_excerpt() {
        let observed = ObservedProgram {
            observation: CompilerObservation {
                compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
                program: PathBuf::from("broken.f90"),
                opt_level: OptLevel::O0,
                compile_exit_code: 1,
                artifacts: BTreeMap::from([
                    (
                        ArtifactKey::Diagnostics,
                        ArtifactValue::Text("undefined symbol: missing_value\nmore detail".into()),
                    ),
                    (
                        ArtifactKey::Extra("armfortas.tokens".into()),
                        ArtifactValue::Text("token stream".into()),
                    ),
                ]),
                provenance: ObservationProvenance {
                    compiler_identity: "armfortas".into(),
                    adapter_kind: "named".into(),
                    backend_mode: "linked".into(),
                    backend_detail: "linked armfortas::testing capture adapter".into(),
                    artifacts_captured: vec!["diagnostics".into(), "armfortas.tokens".into()],
                    comparison_basis: None,
                    failure_stage: Some("sema".into()),
                },
            },
            requested_artifacts: BTreeSet::from([
                ArtifactKey::Asm,
                ArtifactKey::Extra("armfortas.tokens".into()),
            ]),
        };

        let text = render_introspection_text(&observed, full_introspection_render_config());
        assert!(text.contains("status: compile failed"));
        assert!(text.contains("failure_stage: sema"));
        assert!(text.contains("diagnostic_excerpt: undefined symbol: missing_value"));
        assert!(text.contains("missing_artifacts: asm"));

        let json = render_introspection_json(&observed);
        assert!(json.contains("\"stage\": \"sema\""));
        assert!(json.contains("\"diagnostic_excerpt\": \"undefined symbol: missing_value\""));
        assert!(json.contains("\"failure_stage\": \"sema\""));
        assert!(json.contains("\"diagnostics\": {\"kind\":\"text\""));
        assert!(json.contains("\"summary\":\"text, 2 lines, "));
        assert!(json.contains("\"line_count\":2"));

        let markdown = render_introspection_markdown(&observed, full_introspection_render_config());
        assert!(markdown.contains("status: compile failed"));
        assert!(markdown.contains("failure_stage: `sema`"));
        assert!(markdown.contains("diagnostic_excerpt: `undefined symbol: missing_value`"));
    }

    #[test]
    fn render_introspection_summary_only_omits_artifact_bodies() {
        let observed = ObservedProgram {
            observation: CompilerObservation {
                compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
                program: PathBuf::from("demo.f90"),
                opt_level: OptLevel::O0,
                compile_exit_code: 0,
                artifacts: BTreeMap::from([(
                    ArtifactKey::Extra("armfortas.tokens".into()),
                    ArtifactValue::Text("line1\nline2\nline3".into()),
                )]),
                provenance: ObservationProvenance {
                    compiler_identity: "armfortas".into(),
                    adapter_kind: "named".into(),
                    backend_mode: "linked".into(),
                    backend_detail: "linked armfortas::testing capture adapter".into(),
                    artifacts_captured: vec!["armfortas.tokens".into()],
                    comparison_basis: None,
                    failure_stage: None,
                },
            },
            requested_artifacts: BTreeSet::from([ArtifactKey::Extra("armfortas.tokens".into())]),
        };

        let config = IntrospectionRenderConfig {
            summary_only: true,
            max_artifact_lines: Some(1),
        };
        let text = render_introspection_text(&observed, config);
        assert!(text.contains("content_mode: summary-only"));
        assert!(text.contains("summary: text, 3 lines, 17 chars"));
        assert!(text.contains("[content omitted by --summary-only]"));
        assert!(!text.contains("line2"));

        let markdown = render_introspection_markdown(&observed, config);
        assert!(markdown.contains("content_mode: `summary-only`"));
        assert!(markdown.contains("[content omitted by --summary-only]"));
    }

    #[test]
    fn render_introspection_truncates_large_artifacts() {
        let observed = ObservedProgram {
            observation: CompilerObservation {
                compiler: CompilerSpec::Named(NamedCompiler::Armfortas),
                program: PathBuf::from("demo.f90"),
                opt_level: OptLevel::O0,
                compile_exit_code: 0,
                artifacts: BTreeMap::from([(
                    ArtifactKey::Asm,
                    ArtifactValue::Text("a\nb\nc\nd".into()),
                )]),
                provenance: ObservationProvenance {
                    compiler_identity: "armfortas".into(),
                    adapter_kind: "named".into(),
                    backend_mode: "linked".into(),
                    backend_detail: "linked armfortas::testing capture adapter".into(),
                    artifacts_captured: vec!["asm".into()],
                    comparison_basis: None,
                    failure_stage: None,
                },
            },
            requested_artifacts: BTreeSet::from([ArtifactKey::Asm]),
        };

        let config = IntrospectionRenderConfig {
            summary_only: false,
            max_artifact_lines: Some(2),
        };
        let text = render_introspection_text(&observed, config);
        assert!(text.contains("content_mode: first 2 lines per artifact"));
        assert!(text.contains("a\nb\n... (truncated; showing first 2 of 4 lines)"));
        assert!(!text.contains("\nc\nd"));

        let markdown = render_introspection_markdown(&observed, config);
        assert!(markdown.contains("content_mode: `first 2 lines per artifact`"));
        assert!(markdown.contains("... (truncated; showing first 2 of 4 lines)"));
    }

    #[test]
    fn write_requested_reports_emits_files() {
        let root = std::env::temp_dir().join("afs_tests_report_output");
        let _ = fs::remove_dir_all(&root);
        let json_path = root.join("result.json");
        let markdown_path = root.join("result.md");
        let config = RunConfig {
            suite_filter: None,
            case_filter: None,
            opt_filter: None,
            verbose: false,
            fail_fast: false,
            include_future: false,
            all_stages: false,
            json_report: Some(json_path.clone()),
            markdown_report: Some(markdown_path.clone()),
            tools: ToolchainConfig::from_env(),
        };
        let mut summary = Summary::default();
        summary.record_outcome(&Outcome {
            suite: "frontend/parser".into(),
            case: "where_construct".into(),
            opt_level: OptLevel::O0,
            kind: OutcomeKind::Pass,
            detail: String::new(),
            bundle: None,
            primary_backend: Some(PrimaryBackendReport {
                kind: "full".into(),
                mode: "linked".into(),
                detail: "linked armfortas::testing capture adapter".into(),
            }),
            consistency_observations: Vec::new(),
        });

        write_requested_reports(&config, &summary).unwrap();

        let json = fs::read_to_string(&json_path).unwrap();
        let markdown = fs::read_to_string(&markdown_path).unwrap();
        assert!(json.contains("\"passed\": 1"));
        assert!(markdown.contains("| passed | 1 |"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn render_doctor_report_includes_tool_status() {
        let root = std::env::temp_dir().join("afs_tests_doctor_paths");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let armfortas_bin = root.join("armfortas");
        let gfortran_bin = root.join("gfortran");
        fs::write(&armfortas_bin, "").unwrap();
        fs::write(&gfortran_bin, "").unwrap();

        let config = DoctorConfig {
            tools: ToolchainConfig {
                armfortas: ArmfortasCliAdapter::External(armfortas_bin.display().to_string()),
                gfortran: gfortran_bin.display().to_string(),
                flang_new: "/tmp/does-not-exist-flang".into(),
                system_as: "/tmp/does-not-exist-as".into(),
                otool: "/tmp/does-not-exist-otool".into(),
                nm: "/tmp/does-not-exist-nm".into(),
            },
            json_report: None,
            markdown_report: None,
        };

        let rendered = render_doctor_report(&config);
        assert!(rendered.contains("Doctor"));
        assert!(rendered.contains("armfortas_cli_adapter: external armfortas binary adapter"));
        assert!(rendered.contains("armfortas_cli_mode: external"));
        assert!(rendered
            .contains("armfortas_capture_adapter: linked armfortas::testing capture adapter"));
        assert!(rendered.contains("armfortas_capture_mode: linked"));
        assert!(rendered.contains("armfortas_capture_manifest:"));
        assert!(
            rendered.contains("primary_backend_full: linked armfortas::testing capture adapter")
        );
        assert!(rendered.contains(
            "linked_mode_surface: rich armfortas stages, legacy frontend/module suites, capture consistency"
        ));
        assert!(rendered.contains(
            "primary_backend_observable: cli-observable armfortas driver capture adapter"
        ));
        assert!(rendered.contains(
            "primary_backend_selection: observable backend is selected for asm/obj/run-only cells"
        ));
        assert!(rendered.contains("named_compiler.armfortas: cli=external capture=linked"));
        assert!(rendered.contains(
            "named_compiler.armfortas.generic_artifacts: diagnostics, exit-code, stdout, stderr, asm, obj, executable, runtime"
        ));
        assert!(rendered.contains("named_compiler.armfortas.adapter_extras: armfortas("));
        assert!(rendered.contains("named_compiler.armfortas.unavailable_artifacts: none"));
        assert!(rendered.contains("named_compiler.gfortran:"));
        assert!(rendered.contains(
            "named_compiler.gfortran.generic_artifacts: diagnostics, exit-code, stdout, stderr, asm, obj, executable, runtime"
        ));
        assert!(rendered.contains("named_compiler.gfortran.adapter_extras: none"));
        assert!(rendered.contains(
            "explicit_compiler_path: any filesystem path passed to compare/introspect uses the generic external-driver adapter"
        ));
        assert!(rendered.contains(
            "explicit_compiler_path.generic_artifacts: diagnostics, exit-code, stdout, stderr, asm, obj, executable, runtime"
        ));
        assert!(rendered.contains(&format!(
            "configured={} resolved={}",
            armfortas_bin.display(),
            armfortas_bin.display()
        )));
        assert!(rendered.contains(&format!(
            "configured={} resolved={}",
            gfortran_bin.display(),
            gfortran_bin.display()
        )));
        assert!(rendered.contains("configured=/tmp/does-not-exist-flang resolved=missing"));
        let rendered_json = render_doctor_json(&config);
        let rendered_markdown = render_doctor_markdown(&config);
        assert!(rendered_json.contains("\"command\": \"doctor\""));
        assert!(rendered_json.contains("\"workspace\": {"));
        assert!(rendered_json.contains("\"named_compilers\": {"));
        assert!(rendered_json.contains("\"tools\": {"));
        assert!(rendered_json.contains("\"named_compiler.armfortas.adapter_extras\""));
        assert!(rendered_markdown.contains("# bencch doctor report"));
        assert!(rendered_markdown.contains("| `named_compiler.armfortas` |"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn case_discovery_lines_report_capability_block_for_generic_introspect() {
        let case = CaseSpec {
            name: "unsupported_extra".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::new(),
            generic_introspect: Some(GenericIntrospectCase {
                compiler: CompilerSpec::Named(NamedCompiler::Gfortran),
                artifacts: BTreeSet::from([ArtifactKey::Extra("armfortas.ir".into())]),
            }),
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };

        let lines = case_discovery_lines(&case, &ToolchainConfig::from_env());
        assert!(lines.contains(&"capability_status: blocked".to_string()));
        assert!(lines
            .iter()
            .any(|line| line.contains("unsupported in this adapter: armfortas.ir")));
    }

    #[test]
    fn case_discovery_lines_distinguish_legacy_surfaces() {
        let observable_case = CaseSpec {
            name: "observable".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 2,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };
        let linked_case = CaseSpec {
            name: "linked".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Tokens]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0, OptLevel::O1],
            repeat_count: 2,
            reference_compilers: vec![ReferenceCompiler::Gfortran],
            consistency_checks: vec![ConsistencyCheck::CaptureAsmReproducible],
            expectations: Vec::new(),
            status_rules: Vec::new(),
        };

        let tools = ToolchainConfig {
            armfortas: ArmfortasCliAdapter::External("/tmp/armfortas".into()),
            ..ToolchainConfig::from_env()
        };
        let observable_lines = case_discovery_lines(&observable_case, &tools);
        let linked_lines = case_discovery_lines(&linked_case, &tools);

        assert!(observable_lines.contains(&"surface: observable-only legacy path".to_string()));
        assert!(observable_lines.contains(&"capability_status: ready".to_string()));
        assert!(linked_lines.contains(&"surface: linked armfortas capture".to_string()));
        assert!(linked_lines
            .iter()
            .any(|line| line.contains("differential: gfortran")));
        assert!(linked_lines
            .iter()
            .any(|line| line.contains("consistency: capture_asm_reproducible")));
    }

    #[test]
    fn write_doctor_reports_emits_files() {
        let root = std::env::temp_dir().join("afs_tests_doctor_report_output");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let json_path = root.join("doctor.json");
        let markdown_path = root.join("doctor.md");
        let config = DoctorConfig {
            tools: ToolchainConfig::from_env(),
            json_report: Some(json_path.clone()),
            markdown_report: Some(markdown_path.clone()),
        };

        write_doctor_reports(&config).unwrap();

        let json = fs::read_to_string(&json_path).unwrap();
        let markdown = fs::read_to_string(&markdown_path).unwrap();
        assert!(json.contains("\"command\": \"doctor\""));
        assert!(json.contains("\"workspace_root\""));
        assert!(markdown.contains("# bencch doctor report"));
        assert!(markdown.contains("| `workspace_root` |"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn differential_reports_armfortas_only_divergence() {
        let armfortas = differential_armfortas_observation("0\n", "", 0);
        let refs = vec![
            differential_reference_observation(ReferenceCompiler::Gfortran, "42\n", "", 0),
            differential_reference_observation(ReferenceCompiler::FlangNew, "42\n", "", 0),
        ];

        let err = compare_differential(&armfortas, &refs).unwrap_err();
        assert!(err.contains("classification: armfortas-only divergence"));
        assert!(err.contains("basis: compile-status, diagnostics, runtime"));
    }

    #[test]
    fn differential_reports_reference_disagreement() {
        let armfortas = differential_armfortas_observation("42\n", "", 0);
        let refs = vec![
            differential_reference_observation(ReferenceCompiler::Gfortran, "42\n", "", 0),
            differential_reference_observation(ReferenceCompiler::FlangNew, "99\n", "", 0),
        ];

        let err = compare_differential(&armfortas, &refs).unwrap_err();
        assert!(err.contains("classification: reference disagreement"));
    }

    #[test]
    fn differential_tolerates_numeric_formatting_differences() {
        let armfortas = differential_armfortas_observation("     5.5000000E0\n", "", 0);
        let refs = vec![
            differential_reference_observation(
                ReferenceCompiler::Gfortran,
                "   5.50000000\n",
                "",
                0,
            ),
            differential_reference_observation(ReferenceCompiler::FlangNew, " 5.5\n", "", 0),
        ];

        assert!(compare_differential(&armfortas, &refs).is_ok());
    }

    #[test]
    fn consistency_diff_reports_first_mismatch() {
        let detail = describe_text_difference("alpha\nbeta\n", "alpha\ngamma\n", "left", "right");
        assert!(detail.contains("first differing line: 2"));
        assert!(detail.contains("left: beta"));
        assert!(detail.contains("right: gamma"));
    }

    #[test]
    fn consistency_diff_reports_first_extra_line() {
        let detail = describe_text_difference("alpha\nbeta\n", "", "left", "right");
        assert!(detail.contains("snapshot length differs"));
        assert!(detail.contains("first extra line: 1"));
        assert!(detail.contains("left: alpha"));
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
            stdout: "alpha\nbeta\n".into(),
            stderr: String::new(),
        };
        let right = RunCapture {
            exit_code: 0,
            stdout: "alpha\ngamma\n".into(),
            stderr: String::new(),
        };

        let detail = describe_run_difference(&left, &right, "capture run", "cli run 2");
        assert!(detail.contains("differing runtime components: stdout"));
        assert!(detail.contains("first differing component: stdout"));
        assert!(detail.contains("capture run: beta"));
        assert!(detail.contains("cli run 2: gamma"));
    }

    #[test]
    fn run_component_variation_classifies_stdout_only_instability() {
        let first = RunSignature {
            exit_code: 0,
            stdout: "alpha".into(),
            stderr: String::new(),
        };
        let second = RunSignature {
            exit_code: 0,
            stdout: "beta".into(),
            stderr: String::new(),
        };
        let signatures = vec![&first, &second];

        assert_eq!(varying_run_components(&signatures), vec!["stdout"]);
        assert_eq!(
            stable_run_components(&signatures),
            vec!["exit_code", "stderr"]
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
        let mut module = Module::new("verify".into());
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
        let mut module = Module::new("verify".into());
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
        let mut module = Module::new("verify".into());
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

    #[test]
    fn failure_expectation_precedes_partial_stage_checks() {
        let case = CaseSpec {
            name: "missing_then".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Tokens, Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![
                Expectation::Contains {
                    target: Target::RunStdout,
                    needle: "42".into(),
                },
                Expectation::FailContains {
                    stage: FailureStage::Parser,
                    needle: "expected 'then'".into(),
                },
            ],
            status_rules: Vec::new(),
        };
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Tokens, Stage::Run]),
            armfortas: None,
            armfortas_failure: None,
            armfortas_observation: None,
            references: Vec::new(),
            reference_observations: Vec::new(),
            consistency_issues: Vec::new(),
        };
        let failure = CaptureFailure {
            input: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            stage: FailureStage::Parser,
            detail: "expected 'then'".into(),
            stages: BTreeMap::from([(Stage::Tokens, CapturedStage::Text("if\n".into()))]),
        };

        assert!(evaluate_failed_armfortas(&case, &artifacts, &failure).is_ok());
    }

    #[test]
    fn legacy_capture_failures_use_observation_failure_semantics() {
        let case = CaseSpec {
            name: "hidden_use_only".into(),
            source: PathBuf::from("demo.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::FailCommentPatterns(vec!["hidden".into()])],
            status_rules: Vec::new(),
        };
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Run]),
            armfortas: None,
            armfortas_failure: None,
            armfortas_observation: None,
            references: Vec::new(),
            reference_observations: Vec::new(),
            consistency_issues: Vec::new(),
        };
        let failure = CaptureFailure {
            input: PathBuf::from("demo.f90"),
            opt_level: OptLevel::O0,
            stage: FailureStage::Sema,
            detail: "demo.f90:4:3: semantic error: hidden".into(),
            stages: BTreeMap::new(),
        };

        assert!(evaluate_failed_armfortas(&case, &artifacts, &failure).is_ok());
    }

    #[test]
    fn legacy_failure_observed_program_uses_prepared_source_and_partial_stages() {
        let case = CaseSpec {
            name: "missing_then".into(),
            source: PathBuf::from("authored.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Tokens, Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::FailContains {
                stage: FailureStage::Parser,
                needle: "expected 'then'".into(),
            }],
            status_rules: Vec::new(),
        };
        let failure = CaptureFailure {
            input: PathBuf::from("generated.f90"),
            opt_level: OptLevel::O0,
            stage: FailureStage::Parser,
            detail: "expected 'then'".into(),
            stages: BTreeMap::from([(Stage::Tokens, CapturedStage::Text("if\n".into()))]),
        };

        let observed = legacy_failure_observed_program(Path::new("prepared.f90"), &case, &failure);
        assert_eq!(observed.observation.program, PathBuf::from("prepared.f90"));
        assert_eq!(observed.observation.compile_exit_code, 1);
        assert_eq!(
            observed.observation.provenance.failure_stage.as_deref(),
            Some("parser")
        );
        assert!(observed
            .observation
            .artifacts
            .contains_key(&ArtifactKey::Diagnostics));
        assert!(observed
            .observation
            .artifacts
            .contains_key(&ArtifactKey::Extra("armfortas.tokens".into())));
    }

    #[test]
    fn unexpected_capture_failure_reports_compiler_failure_detail() {
        let case = CaseSpec {
            name: "module_procedure_runtime".into(),
            source: PathBuf::from("graph.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::Contains {
                target: Target::RunStdout,
                needle: "42".into(),
            }],
            status_rules: Vec::new(),
        };
        let failure = CaptureFailure {
            input: PathBuf::from("graph.f90"),
            opt_level: OptLevel::O0,
            stage: FailureStage::Run,
            detail: "Undefined symbols for architecture arm64:\n  \"_add_one\"".into(),
            stages: BTreeMap::new(),
        };
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Run]),
            armfortas: None,
            armfortas_failure: Some(failure.clone()),
            armfortas_observation: None,
            references: Vec::new(),
            reference_observations: Vec::new(),
            consistency_issues: Vec::new(),
        };

        let err = evaluate_failed_armfortas(&case, &artifacts, &failure).unwrap_err();
        assert!(err.contains("armfortas failed in run"));
        assert!(err.contains("_add_one"));
        assert!(!err.contains("missing captured run stage"));
    }

    #[test]
    fn partial_stage_expectation_failure_is_preserved_on_capture_failure() {
        let case = CaseSpec {
            name: "module_procedure_backend".into(),
            source: PathBuf::from("graph.f90"),
            graph_files: Vec::new(),
            requested: BTreeSet::from([Stage::Asm, Stage::Obj, Stage::Run]),
            generic_introspect: None,
            generic_compare: None,
            opt_levels: vec![OptLevel::O0],
            repeat_count: 3,
            reference_compilers: Vec::new(),
            consistency_checks: Vec::new(),
            expectations: vec![Expectation::Contains {
                target: Target::Stage(Stage::Asm),
                needle: ".globl _add_one".into(),
            }],
            status_rules: Vec::new(),
        };
        let failure = CaptureFailure {
            input: PathBuf::from("graph.f90"),
            opt_level: OptLevel::O0,
            stage: FailureStage::Run,
            detail: "Undefined symbols for architecture arm64:\n  \"_add_one\"".into(),
            stages: BTreeMap::from([(Stage::Asm, CapturedStage::Text(".globl _main\n".into()))]),
        };
        let artifacts = ExecutionArtifacts {
            requested: BTreeSet::from([Stage::Asm, Stage::Obj, Stage::Run]),
            armfortas: None,
            armfortas_failure: Some(failure.clone()),
            armfortas_observation: None,
            references: Vec::new(),
            reference_observations: Vec::new(),
            consistency_issues: Vec::new(),
        };

        let err = evaluate_failed_armfortas(&case, &artifacts, &failure).unwrap_err();
        assert!(err.contains("expected asm to contain"));
        assert!(!err.contains("armfortas failed in run"));
    }
}

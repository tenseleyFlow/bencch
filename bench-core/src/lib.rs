use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    Ofast,
}

impl OptLevel {
    pub fn parse_flag(flag: &str) -> Option<Self> {
        match flag.to_ascii_lowercase().as_str() {
            "o0" => Some(Self::O0),
            "o1" => Some(Self::O1),
            "o2" => Some(Self::O2),
            "o3" => Some(Self::O3),
            "ofast" => Some(Self::Ofast),
            _ => None,
        }
    }

    pub fn as_flag(&self) -> &'static str {
        match self {
            Self::O0 => "-O0",
            Self::O1 => "-O1",
            Self::O2 => "-O2",
            Self::O3 => "-O3",
            Self::Ofast => "-Ofast",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::O0 => "O0",
            Self::O1 => "O1",
            Self::O2 => "O2",
            Self::O3 => "O3",
            Self::Ofast => "Ofast",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    Preprocess,
    Tokens,
    Ast,
    Sema,
    Ir,
    OptIr,
    Mir,
    Regalloc,
    Asm,
    Obj,
    Run,
}

impl Stage {
    pub const ALL: [Stage; 11] = [
        Stage::Preprocess,
        Stage::Tokens,
        Stage::Ast,
        Stage::Sema,
        Stage::Ir,
        Stage::OptIr,
        Stage::Mir,
        Stage::Regalloc,
        Stage::Asm,
        Stage::Obj,
        Stage::Run,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "preprocess" => Some(Self::Preprocess),
            "tokens" => Some(Self::Tokens),
            "ast" => Some(Self::Ast),
            "sema" => Some(Self::Sema),
            "ir" => Some(Self::Ir),
            "optir" => Some(Self::OptIr),
            "mir" => Some(Self::Mir),
            "regalloc" => Some(Self::Regalloc),
            "asm" => Some(Self::Asm),
            "obj" => Some(Self::Obj),
            "run" => Some(Self::Run),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Preprocess => "preprocess",
            Self::Tokens => "tokens",
            Self::Ast => "ast",
            Self::Sema => "sema",
            Self::Ir => "ir",
            Self::OptIr => "optir",
            Self::Mir => "mir",
            Self::Regalloc => "regalloc",
            Self::Asm => "asm",
            Self::Obj => "obj",
            Self::Run => "run",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStage {
    Preprocess,
    Lexer,
    Parser,
    Sema,
    Ir,
    Obj,
    Run,
}

impl FailureStage {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "preprocess" => Some(Self::Preprocess),
            "lexer" | "tokens" => Some(Self::Lexer),
            "parser" | "parse" | "ast" => Some(Self::Parser),
            "sema" => Some(Self::Sema),
            "ir" | "optir" => Some(Self::Ir),
            "obj" | "asm" => Some(Self::Obj),
            "run" => Some(Self::Run),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Preprocess => "preprocess",
            Self::Lexer => "lexer",
            Self::Parser => "parser",
            Self::Sema => "sema",
            Self::Ir => "ir",
            Self::Obj => "obj",
            Self::Run => "run",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureRequest {
    pub input: PathBuf,
    pub requested: BTreeSet<Stage>,
    pub opt_level: OptLevel,
}

impl CaptureRequest {
    pub fn new(input: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            requested: BTreeSet::new(),
            opt_level: OptLevel::O0,
        }
    }

    pub fn with_stage(mut self, stage: Stage) -> Self {
        self.requested.insert(stage);
        self
    }

    pub fn with_all_stages(mut self) -> Self {
        self.requested.extend(Stage::ALL);
        self
    }

    pub fn with_opt_level(mut self, opt_level: OptLevel) -> Self {
        self.opt_level = opt_level;
        self
    }
}

pub trait CaptureBackend {
    fn mode_name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, CaptureFailure>;
}

#[derive(Debug, Clone)]
pub struct CaptureResult {
    pub input: PathBuf,
    pub opt_level: OptLevel,
    pub stages: BTreeMap<Stage, CapturedStage>,
}

impl CaptureResult {
    pub fn get(&self, stage: Stage) -> Option<&CapturedStage> {
        self.stages.get(&stage)
    }
}

#[derive(Debug, Clone)]
pub struct CaptureFailure {
    pub input: PathBuf,
    pub opt_level: OptLevel,
    pub stage: FailureStage,
    pub detail: String,
    pub stages: BTreeMap<Stage, CapturedStage>,
}

impl CaptureFailure {
    pub fn partial_result(&self) -> CaptureResult {
        CaptureResult {
            input: self.input.clone(),
            opt_level: self.opt_level,
            stages: self.stages.clone(),
        }
    }
}

impl fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.stage.as_str(), self.detail)
    }
}

impl std::error::Error for CaptureFailure {}

#[derive(Debug, Clone)]
pub enum CapturedStage {
    Text(String),
    Run(RunCapture),
}

impl CapturedStage {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Run(_) => None,
        }
    }

    pub fn as_run(&self) -> Option<&RunCapture> {
        match self {
            Self::Text(_) => None,
            Self::Run(run) => Some(run),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCapture {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NamedCompiler {
    Armfortas,
    Gfortran,
    FlangNew,
}

impl NamedCompiler {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "armfortas" | "afs" => Some(Self::Armfortas),
            "gfortran" => Some(Self::Gfortran),
            "flang-new" | "flang_new" | "flang" => Some(Self::FlangNew),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Armfortas => "armfortas",
            Self::Gfortran => "gfortran",
            Self::FlangNew => "flang-new",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CompilerSpec {
    Named(NamedCompiler),
    Binary(PathBuf),
}

impl CompilerSpec {
    pub fn parse(value: &str) -> Self {
        if let Some(named) = NamedCompiler::parse(value) {
            Self::Named(named)
        } else {
            Self::Binary(PathBuf::from(value))
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Named(named) => named.as_str().to_string(),
            Self::Binary(path) => path.display().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactKey {
    Diagnostics,
    ExitCode,
    Stdout,
    Stderr,
    Asm,
    Obj,
    Executable,
    Runtime,
    Extra(String),
}

impl ArtifactKey {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "diagnostics" | "diag" => Some(Self::Diagnostics),
            "exit-code" | "exit_code" | "exitcode" => Some(Self::ExitCode),
            "stdout" => Some(Self::Stdout),
            "stderr" => Some(Self::Stderr),
            "asm" => Some(Self::Asm),
            "obj" => Some(Self::Obj),
            "executable" | "binary" => Some(Self::Executable),
            "runtime" | "run" => Some(Self::Runtime),
            other if other.contains('.') => Some(Self::Extra(other.to_string())),
            _ => None,
        }
    }

    pub fn parse_list(value: &str) -> Result<BTreeSet<Self>, String> {
        value
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| Self::parse(part).ok_or_else(|| format!("unknown artifact '{}'", part)))
            .collect()
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Diagnostics => "diagnostics",
            Self::ExitCode => "exit-code",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Asm => "asm",
            Self::Obj => "obj",
            Self::Executable => "executable",
            Self::Runtime => "runtime",
            Self::Extra(name) => name.as_str(),
        }
    }

    pub fn is_generic(&self) -> bool {
        !matches!(self, Self::Extra(_))
    }

    pub fn extra_parts(&self) -> Option<(&str, &str)> {
        match self {
            Self::Extra(name) => name.split_once('.'),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactValue {
    Text(String),
    Int(i32),
    Run(RunCapture),
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationProvenance {
    pub compiler_identity: String,
    pub adapter_kind: String,
    pub backend_mode: String,
    pub backend_detail: String,
    pub artifacts_captured: Vec<String>,
    pub comparison_basis: Option<String>,
    pub failure_stage: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilerObservation {
    pub compiler: CompilerSpec,
    pub program: PathBuf,
    pub opt_level: OptLevel,
    pub compile_exit_code: i32,
    pub artifacts: BTreeMap<ArtifactKey, ArtifactValue>,
    pub provenance: ObservationProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDifference {
    pub artifact: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparisonResult {
    pub left: CompilerObservation,
    pub right: CompilerObservation,
    pub basis: String,
    pub differences: Vec<ArtifactDifference>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_spec_parses_named_and_binary_inputs() {
        assert_eq!(
            CompilerSpec::parse("armfortas"),
            CompilerSpec::Named(NamedCompiler::Armfortas)
        );
        assert_eq!(
            CompilerSpec::parse("flang-new"),
            CompilerSpec::Named(NamedCompiler::FlangNew)
        );
        assert_eq!(
            CompilerSpec::parse("/tmp/compiler"),
            CompilerSpec::Binary(PathBuf::from("/tmp/compiler"))
        );
    }

    #[test]
    fn artifact_key_parses_generic_and_namespaced_values() {
        assert_eq!(ArtifactKey::parse("asm"), Some(ArtifactKey::Asm));
        assert_eq!(
            ArtifactKey::parse("armfortas.ir"),
            Some(ArtifactKey::Extra("armfortas.ir".into()))
        );
        let parsed = ArtifactKey::parse_list("asm,obj,armfortas.ir").unwrap();
        assert!(parsed.contains(&ArtifactKey::Asm));
        assert!(parsed.contains(&ArtifactKey::Obj));
        assert!(parsed.contains(&ArtifactKey::Extra("armfortas.ir".into())));
    }

    #[test]
    fn artifact_key_reports_namespace_parts() {
        let generic = ArtifactKey::Asm;
        assert!(generic.is_generic());
        assert_eq!(generic.extra_parts(), None);

        let extra = ArtifactKey::Extra("armfortas.ir".into());
        assert!(!extra.is_generic());
        assert_eq!(extra.extra_parts(), Some(("armfortas", "ir")));

        let malformed = ArtifactKey::Extra("odd".into());
        assert_eq!(malformed.extra_parts(), None);
    }
}

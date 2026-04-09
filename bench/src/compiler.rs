use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(feature = "linked-armfortas")]
use std::collections::BTreeSet;

pub use bencch_core::{
    CaptureBackend, CaptureFailure, CaptureRequest, CaptureResult, CapturedStage, FailureStage,
    OptLevel, RunCapture, Stage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitMode {
    Asm,
    Obj,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArmfortasCliAdapter {
    Linked,
    External(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmfortasCaptureAdapter {
    Linked,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmfortasAdapters {
    cli: ArmfortasCliAdapter,
    capture: ArmfortasCaptureAdapter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliObservableCaptureBackend {
    cli: ArmfortasCliAdapter,
    work_root: PathBuf,
    otool: String,
    nm: String,
}

impl ArmfortasAdapters {
    pub fn new(cli: ArmfortasCliAdapter) -> Self {
        Self {
            cli,
            capture: default_capture_adapter(),
        }
    }

    pub fn cli(&self) -> &ArmfortasCliAdapter {
        &self.cli
    }

    pub fn cli_mode_name(&self) -> &'static str {
        match self.cli {
            ArmfortasCliAdapter::Linked => "linked",
            ArmfortasCliAdapter::External(_) => "external",
        }
    }

    pub fn cli_description(&self) -> &'static str {
        match self.cli {
            ArmfortasCliAdapter::Linked => "linked armfortas crate driver adapter",
            ArmfortasCliAdapter::External(_) => "external armfortas binary adapter",
        }
    }

    pub fn cli_command_name(&self) -> &str {
        match &self.cli {
            ArmfortasCliAdapter::Linked => "armfortas (linked)",
            ArmfortasCliAdapter::External(binary) => binary,
        }
    }

    pub fn capture_command_name(&self) -> &'static str {
        match self.capture {
            ArmfortasCaptureAdapter::Linked => "armfortas::testing capture (linked)",
            ArmfortasCaptureAdapter::Unavailable => "armfortas::testing capture (unavailable)",
        }
    }

    pub fn capture_mode_name(&self) -> &'static str {
        match self.capture {
            ArmfortasCaptureAdapter::Linked => "linked",
            ArmfortasCaptureAdapter::Unavailable => "unavailable",
        }
    }

    pub fn capture_description(&self) -> &'static str {
        match self.capture {
            ArmfortasCaptureAdapter::Linked => "linked armfortas::testing capture adapter",
            ArmfortasCaptureAdapter::Unavailable => "unavailable without linked-armfortas feature",
        }
    }

    pub fn capture_root(&self) -> Option<PathBuf> {
        match self.capture {
            ArmfortasCaptureAdapter::Linked => Some(linked_adapter_root()),
            ArmfortasCaptureAdapter::Unavailable => None,
        }
    }

    pub fn compile_output(
        &self,
        input: &Path,
        opt_level: OptLevel,
        mode: EmitMode,
        output: &Path,
    ) -> Result<(), String> {
        match &self.cli {
            ArmfortasCliAdapter::Linked => linked_compile_output(input, opt_level, mode, output),
            ArmfortasCliAdapter::External(binary) => {
                external_compile_output(binary, input, opt_level, mode, output)
            }
        }
    }
}

impl CliObservableCaptureBackend {
    pub fn new(cli: ArmfortasCliAdapter, work_root: PathBuf, otool: String, nm: String) -> Self {
        Self {
            cli,
            work_root,
            otool,
            nm,
        }
    }
}

impl CaptureBackend for ArmfortasAdapters {
    fn mode_name(&self) -> &'static str {
        self.capture_mode_name()
    }

    fn description(&self) -> &'static str {
        self.capture_description()
    }

    fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, CaptureFailure> {
        match self.capture {
            ArmfortasCaptureAdapter::Linked => linked_capture_from_path(request),
            ArmfortasCaptureAdapter::Unavailable => Err(CaptureFailure {
                input: request.input.clone(),
                opt_level: request.opt_level,
                stage: FailureStage::Ir,
                detail: "linked armfortas capture is unavailable in this build; use scripts/bootstrap-linked-armfortas.sh or request only asm/obj/run from an external armfortas binary".into(),
                stages: BTreeMap::new(),
            }),
        }
    }
}

impl CaptureBackend for CliObservableCaptureBackend {
    fn mode_name(&self) -> &'static str {
        "cli-observable"
    }

    fn description(&self) -> &'static str {
        "cli-observable armfortas driver capture adapter"
    }

    fn capture(&self, request: &CaptureRequest) -> Result<CaptureResult, CaptureFailure> {
        let unsupported = request
            .requested
            .iter()
            .filter(|stage| !matches!(stage, Stage::Asm | Stage::Obj | Stage::Run))
            .copied()
            .collect::<Vec<_>>();
        if !unsupported.is_empty() {
            return Err(CaptureFailure {
                input: request.input.clone(),
                opt_level: request.opt_level,
                stage: FailureStage::Ir,
                detail: format!(
                    "cli-observable capture backend only supports asm, obj, and run; requested {}",
                    unsupported
                        .iter()
                        .map(Stage::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                stages: BTreeMap::new(),
            });
        }

        if let Err(err) = fs::create_dir_all(&self.work_root) {
            return Err(CaptureFailure {
                input: request.input.clone(),
                opt_level: request.opt_level,
                stage: FailureStage::Obj,
                detail: format!(
                    "cannot create cli-observable temp dir '{}': {}",
                    self.work_root.display(),
                    err
                ),
                stages: BTreeMap::new(),
            });
        }

        let adapters = ArmfortasAdapters::new(self.cli.clone());
        let mut stages = BTreeMap::new();

        if request.requested.contains(&Stage::Asm) {
            let asm_path = self.work_root.join("capture.s");
            let command = render_driver_command(
                adapters.cli_command_name(),
                &request.input,
                request.opt_level,
                EmitMode::Asm,
                &asm_path,
            );
            match adapters.compile_output(
                &request.input,
                request.opt_level,
                EmitMode::Asm,
                &asm_path,
            ) {
                Ok(()) => {}
                Err(detail) => {
                    cleanup_dir(&self.work_root);
                    return Err(CaptureFailure {
                        input: request.input.clone(),
                        opt_level: request.opt_level,
                        stage: FailureStage::Obj,
                        detail: format!("{}\n{}", command, detail),
                        stages,
                    });
                }
            }
            let asm_text = match read_text_artifact(&asm_path) {
                Ok(text) => text,
                Err(detail) => {
                    cleanup_dir(&self.work_root);
                    return Err(CaptureFailure {
                        input: request.input.clone(),
                        opt_level: request.opt_level,
                        stage: FailureStage::Obj,
                        detail,
                        stages,
                    });
                }
            };
            stages.insert(Stage::Asm, CapturedStage::Text(asm_text));
        }

        if request.requested.contains(&Stage::Obj) {
            let obj_path = self.work_root.join("capture.o");
            let command = render_driver_command(
                adapters.cli_command_name(),
                &request.input,
                request.opt_level,
                EmitMode::Obj,
                &obj_path,
            );
            match adapters.compile_output(
                &request.input,
                request.opt_level,
                EmitMode::Obj,
                &obj_path,
            ) {
                Ok(()) => {}
                Err(detail) => {
                    cleanup_dir(&self.work_root);
                    return Err(CaptureFailure {
                        input: request.input.clone(),
                        opt_level: request.opt_level,
                        stage: FailureStage::Obj,
                        detail: format!("{}\n{}", command, detail),
                        stages,
                    });
                }
            }
            let obj_text = match object_snapshot_text(&obj_path, &self.otool, &self.nm) {
                Ok(text) => text,
                Err(detail) => {
                    cleanup_dir(&self.work_root);
                    return Err(CaptureFailure {
                        input: request.input.clone(),
                        opt_level: request.opt_level,
                        stage: FailureStage::Obj,
                        detail: format!("{}\n{}", command, detail),
                        stages,
                    });
                }
            };
            stages.insert(Stage::Obj, CapturedStage::Text(obj_text));
        }

        if request.requested.contains(&Stage::Run) {
            let binary_path = self.work_root.join("capture.out");
            let build_command = render_driver_command(
                adapters.cli_command_name(),
                &request.input,
                request.opt_level,
                EmitMode::Binary,
                &binary_path,
            );
            match adapters.compile_output(
                &request.input,
                request.opt_level,
                EmitMode::Binary,
                &binary_path,
            ) {
                Ok(()) => {}
                Err(detail) => {
                    cleanup_dir(&self.work_root);
                    return Err(CaptureFailure {
                        input: request.input.clone(),
                        opt_level: request.opt_level,
                        stage: FailureStage::Obj,
                        detail: format!("{}\n{}", build_command, detail),
                        stages,
                    });
                }
            }
            let run_command = render_binary_run_command(&binary_path);
            let run = match run_binary_capture(&binary_path, &self.work_root, &run_command) {
                Ok(run) => run,
                Err(detail) => {
                    cleanup_dir(&self.work_root);
                    return Err(CaptureFailure {
                        input: request.input.clone(),
                        opt_level: request.opt_level,
                        stage: FailureStage::Run,
                        detail: format!("build: {}\n{}", build_command, detail),
                        stages,
                    });
                }
            };
            stages.insert(Stage::Run, CapturedStage::Run(run));
        }

        cleanup_dir(&self.work_root);
        Ok(CaptureResult {
            input: request.input.clone(),
            opt_level: request.opt_level,
            stages,
        })
    }
}

pub fn linked_adapter_root() -> PathBuf {
    linked_adapter_root_from(
        option_env!("BENCCH_LINKED_ARMFORTAS_ROOT"),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

pub fn linked_capture_available() -> bool {
    matches!(default_capture_adapter(), ArmfortasCaptureAdapter::Linked)
}

fn default_capture_adapter() -> ArmfortasCaptureAdapter {
    if cfg!(feature = "linked-armfortas") {
        ArmfortasCaptureAdapter::Linked
    } else {
        ArmfortasCaptureAdapter::Unavailable
    }
}

fn linked_adapter_root_from(configured_root: Option<&str>, manifest_dir: &Path) -> PathBuf {
    match configured_root {
        Some(root) => {
            let root = Path::new(root);
            if root.is_absolute() {
                root.to_path_buf()
            } else {
                manifest_dir.join(root)
            }
        }
        None => manifest_dir.join("../.."),
    }
}

#[cfg(feature = "linked-armfortas")]
fn linked_compile_output(
    input: &Path,
    opt_level: OptLevel,
    mode: EmitMode,
    output: &Path,
) -> Result<(), String> {
    let opts = armfortas::driver::Options {
        input: input.to_path_buf(),
        output: Some(PathBuf::from(output)),
        emit_asm: matches!(mode, EmitMode::Asm),
        emit_obj: matches!(mode, EmitMode::Obj),
        emit_ir: false,
        preprocess_only: false,
        opt_level: into_driver_opt_level(opt_level),
    };

    armfortas::driver::compile(&opts)
}

#[cfg(not(feature = "linked-armfortas"))]
fn linked_compile_output(
    _input: &Path,
    _opt_level: OptLevel,
    _mode: EmitMode,
    _output: &Path,
) -> Result<(), String> {
    Err(
        "linked armfortas driver is unavailable in this build; use scripts/bootstrap-linked-armfortas.sh or provide --armfortas-bin".into(),
    )
}

fn external_compile_output(
    binary: &str,
    input: &Path,
    opt_level: OptLevel,
    mode: EmitMode,
    output: &Path,
) -> Result<(), String> {
    let mut args = vec![opt_level.as_flag().to_string()];
    match mode {
        EmitMode::Asm => args.push("-S".to_string()),
        EmitMode::Obj => args.push("-c".to_string()),
        EmitMode::Binary => {}
    }
    args.push(input.display().to_string());
    args.push("-o".to_string());
    args.push(output.display().to_string());

    let compile = Command::new(binary)
        .args(&args)
        .output()
        .map_err(|err| format!("cannot run '{}': {}", binary, err))?;
    if !compile.status.success() {
        let stderr = String::from_utf8_lossy(&compile.stderr);
        return Err(stderr.trim_end().to_string());
    }
    Ok(())
}

#[cfg(feature = "linked-armfortas")]
fn linked_capture_from_path(request: &CaptureRequest) -> Result<CaptureResult, CaptureFailure> {
    let arm_request = armfortas::testing::CaptureRequest {
        input: request.input.clone(),
        requested: request
            .requested
            .iter()
            .copied()
            .map(into_arm_stage)
            .collect::<BTreeSet<_>>(),
        opt_level: into_driver_opt_level(request.opt_level),
    };

    armfortas::testing::capture_from_path(&arm_request)
        .map(into_bench_capture_result)
        .map_err(into_bench_capture_failure)
}

#[cfg(not(feature = "linked-armfortas"))]
fn linked_capture_from_path(request: &CaptureRequest) -> Result<CaptureResult, CaptureFailure> {
    Err(CaptureFailure {
        input: request.input.clone(),
        opt_level: request.opt_level,
        stage: FailureStage::Ir,
        detail: "linked armfortas capture is unavailable in this build; use scripts/bootstrap-linked-armfortas.sh or request only asm/obj/run from an external armfortas binary".into(),
        stages: BTreeMap::new(),
    })
}

fn cleanup_dir(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn read_text_artifact(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path.display(), e))
}

pub(crate) fn object_snapshot_text(path: &Path, otool: &str, nm: &str) -> Result<String, String> {
    let text = normalize_tool_output(&tool_output(otool, &["-t", path.to_str().unwrap()])?);
    let load_commands =
        normalize_tool_output(&tool_output(otool, &["-l", path.to_str().unwrap()])?);
    let relocations = normalize_tool_output(&tool_output(otool, &["-rv", path.to_str().unwrap()])?);
    let symbols = normalize_tool_output(&tool_output(nm, &["-m", path.to_str().unwrap()])?);
    Ok(format!(
        "== text ==\n{}\n\n== load_commands ==\n{}\n\n== relocations ==\n{}\n\n== symbols ==\n{}",
        text, load_commands, relocations, symbols
    ))
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

fn render_driver_command(
    command: &str,
    input: &Path,
    opt_level: OptLevel,
    mode: EmitMode,
    output: &Path,
) -> String {
    let mut args = vec![opt_level.as_flag().to_string()];
    match mode {
        EmitMode::Asm => args.push("-S".to_string()),
        EmitMode::Obj => args.push("-c".to_string()),
        EmitMode::Binary => {}
    }
    args.push(input.display().to_string());
    args.push("-o".to_string());
    args.push(output.display().to_string());
    render_command(command, &args)
}

fn render_binary_run_command(binary: &Path) -> String {
    render_command(&binary.display().to_string(), &[])
}

fn render_command(command: &str, args: &[String]) -> String {
    let mut parts = vec![quote_arg(command)];
    parts.extend(args.iter().map(|arg| quote_arg(arg)));
    parts.join(" ")
}

fn quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        "''".to_string()
    } else if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "/._-+".contains(ch))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
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

#[cfg(feature = "linked-armfortas")]
fn into_driver_opt_level(opt_level: OptLevel) -> armfortas::driver::OptLevel {
    match opt_level {
        OptLevel::O0 => armfortas::driver::OptLevel::O0,
        OptLevel::O1 => armfortas::driver::OptLevel::O1,
        OptLevel::O2 => armfortas::driver::OptLevel::O2,
        OptLevel::O3 => armfortas::driver::OptLevel::O3,
        OptLevel::Ofast => armfortas::driver::OptLevel::Ofast,
    }
}

#[cfg(feature = "linked-armfortas")]
fn from_driver_opt_level(opt_level: armfortas::driver::OptLevel) -> OptLevel {
    match opt_level {
        armfortas::driver::OptLevel::O0 => OptLevel::O0,
        armfortas::driver::OptLevel::O1 => OptLevel::O1,
        armfortas::driver::OptLevel::O2 => OptLevel::O2,
        armfortas::driver::OptLevel::O3 => OptLevel::O3,
        armfortas::driver::OptLevel::Ofast => OptLevel::Ofast,
    }
}

#[cfg(feature = "linked-armfortas")]
fn into_arm_stage(stage: Stage) -> armfortas::testing::Stage {
    match stage {
        Stage::Preprocess => armfortas::testing::Stage::Preprocess,
        Stage::Tokens => armfortas::testing::Stage::Tokens,
        Stage::Ast => armfortas::testing::Stage::Ast,
        Stage::Sema => armfortas::testing::Stage::Sema,
        Stage::Ir => armfortas::testing::Stage::Ir,
        Stage::OptIr => armfortas::testing::Stage::OptIr,
        Stage::Mir => armfortas::testing::Stage::Mir,
        Stage::Regalloc => armfortas::testing::Stage::Regalloc,
        Stage::Asm => armfortas::testing::Stage::Asm,
        Stage::Obj => armfortas::testing::Stage::Obj,
        Stage::Run => armfortas::testing::Stage::Run,
    }
}

#[cfg(feature = "linked-armfortas")]
fn from_arm_stage(stage: armfortas::testing::Stage) -> Stage {
    match stage {
        armfortas::testing::Stage::Preprocess => Stage::Preprocess,
        armfortas::testing::Stage::Tokens => Stage::Tokens,
        armfortas::testing::Stage::Ast => Stage::Ast,
        armfortas::testing::Stage::Sema => Stage::Sema,
        armfortas::testing::Stage::Ir => Stage::Ir,
        armfortas::testing::Stage::OptIr => Stage::OptIr,
        armfortas::testing::Stage::Mir => Stage::Mir,
        armfortas::testing::Stage::Regalloc => Stage::Regalloc,
        armfortas::testing::Stage::Asm => Stage::Asm,
        armfortas::testing::Stage::Obj => Stage::Obj,
        armfortas::testing::Stage::Run => Stage::Run,
    }
}

#[cfg(feature = "linked-armfortas")]
fn from_arm_failure_stage(stage: armfortas::testing::FailureStage) -> FailureStage {
    match stage {
        armfortas::testing::FailureStage::Preprocess => FailureStage::Preprocess,
        armfortas::testing::FailureStage::Lexer => FailureStage::Lexer,
        armfortas::testing::FailureStage::Parser => FailureStage::Parser,
        armfortas::testing::FailureStage::Sema => FailureStage::Sema,
        armfortas::testing::FailureStage::Ir => FailureStage::Ir,
        armfortas::testing::FailureStage::Obj => FailureStage::Obj,
        armfortas::testing::FailureStage::Run => FailureStage::Run,
    }
}

#[cfg(feature = "linked-armfortas")]
fn from_arm_captured_stage(stage: armfortas::testing::CapturedStage) -> CapturedStage {
    match stage {
        armfortas::testing::CapturedStage::Text(text) => CapturedStage::Text(text),
        armfortas::testing::CapturedStage::Run(run) => CapturedStage::Run(RunCapture {
            exit_code: run.exit_code,
            stdout: run.stdout,
            stderr: run.stderr,
        }),
    }
}

#[cfg(feature = "linked-armfortas")]
fn into_bench_capture_result(result: armfortas::testing::CaptureResult) -> CaptureResult {
    CaptureResult {
        input: result.input,
        opt_level: from_driver_opt_level(result.opt_level),
        stages: result
            .stages
            .into_iter()
            .map(|(stage, captured)| (from_arm_stage(stage), from_arm_captured_stage(captured)))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[cfg(feature = "linked-armfortas")]
fn into_bench_capture_failure(failure: armfortas::testing::CaptureFailure) -> CaptureFailure {
    CaptureFailure {
        input: failure.input,
        opt_level: from_driver_opt_level(failure.opt_level),
        stage: from_arm_failure_stage(failure.stage),
        detail: failure.detail,
        stages: failure
            .stages
            .into_iter()
            .map(|(stage, captured)| (from_arm_stage(stage), from_arm_captured_stage(captured)))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[cfg(all(test, feature = "linked-armfortas"))]
pub mod test_support {
    pub use armfortas::ir::inst::{
        BlockParam, Function, Inst, InstKind, Module, Terminator, ValueId,
    };
    pub use armfortas::ir::types::{FloatWidth, IntWidth, IrType};
    pub use armfortas::ir::verify::verify_module;
    pub use armfortas::lexer::{Position, Span};
}

#[cfg(test)]
mod tests {
    use super::linked_adapter_root_from;
    use std::path::{Path, PathBuf};

    #[test]
    fn linked_adapter_root_prefers_configured_absolute_root() {
        let manifest_dir = Path::new("/tmp/generated/bench");
        let resolved = linked_adapter_root_from(Some("/tmp/armfortas-root"), manifest_dir);
        assert_eq!(resolved, PathBuf::from("/tmp/armfortas-root"));
    }

    #[test]
    fn linked_adapter_root_falls_back_to_embedded_layout() {
        let manifest_dir = Path::new("/tmp/bencch/bench");
        let resolved = linked_adapter_root_from(None, manifest_dir);
        assert_eq!(resolved, PathBuf::from("/tmp/bencch/bench/../.."));
    }
}

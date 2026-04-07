use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use bencch_core::{
    CaptureFailure, CaptureRequest, CaptureResult, CapturedStage, FailureStage, OptLevel,
    RunCapture, Stage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitMode {
    Asm,
    Obj,
    Binary,
}

pub fn linked_adapter_description() -> &'static str {
    "linked armfortas crate adapter"
}

pub fn linked_adapter_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn compile_output(
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

pub fn capture_from_path(request: &CaptureRequest) -> Result<CaptureResult, CaptureFailure> {
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

fn into_driver_opt_level(opt_level: OptLevel) -> armfortas::driver::OptLevel {
    match opt_level {
        OptLevel::O0 => armfortas::driver::OptLevel::O0,
        OptLevel::O1 => armfortas::driver::OptLevel::O1,
        OptLevel::O2 => armfortas::driver::OptLevel::O2,
        OptLevel::O3 => armfortas::driver::OptLevel::O3,
        OptLevel::Ofast => armfortas::driver::OptLevel::Ofast,
    }
}

fn from_driver_opt_level(opt_level: armfortas::driver::OptLevel) -> OptLevel {
    match opt_level {
        armfortas::driver::OptLevel::O0 => OptLevel::O0,
        armfortas::driver::OptLevel::O1 => OptLevel::O1,
        armfortas::driver::OptLevel::O2 => OptLevel::O2,
        armfortas::driver::OptLevel::O3 => OptLevel::O3,
        armfortas::driver::OptLevel::Ofast => OptLevel::Ofast,
    }
}

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

#[cfg(test)]
pub mod test_support {
    pub use armfortas::ir::inst::{
        BlockParam, Function, Inst, InstKind, Module, Terminator, ValueId,
    };
    pub use armfortas::ir::types::{FloatWidth, IntWidth, IrType};
    pub use armfortas::ir::verify::verify_module;
    pub use armfortas::lexer::{Position, Span};
}

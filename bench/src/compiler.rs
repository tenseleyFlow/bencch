use std::path::{Path, PathBuf};

pub use armfortas::driver::OptLevel;
pub use armfortas::testing::{
    capture_from_path, CaptureFailure, CaptureRequest, CaptureResult, CapturedStage,
    FailureStage, RunCapture, Stage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitMode {
    Asm,
    Obj,
    Binary,
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
        opt_level,
    };

    armfortas::driver::compile(&opts)
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

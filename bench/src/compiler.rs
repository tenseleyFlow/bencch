use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use armfortas::testing::managed_process::{run as run_managed, CommandClass};

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

/// Extract `! FLAGS:` extra compiler flags from a fixture. One
/// dialect with the root harness (tests/run_programs.rs): at most one
/// line, non-empty, and harness-owned flags (-o/-S/-c/-O*) rejected.
fn fixture_flags(input: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(input)
        .map_err(|e| format!("cannot read {}: {}", input.display(), e))?;
    let mut flags: Option<Vec<String>> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("! FLAGS:") {
            if flags.is_some() {
                return Err(format!(
                    "{}: multiple FLAGS annotations; combine into one line",
                    input.display()
                ));
            }
            let toks: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
            if toks.is_empty() {
                return Err(format!(
                    "{}: FLAGS annotation with no flags",
                    input.display()
                ));
            }
            for t in &toks {
                if t == "-o" || t == "-S" || t == "-c" || t.starts_with("-O") {
                    return Err(format!(
                        "{}: FLAGS may not contain harness-owned flag '{}'",
                        input.display(),
                        t
                    ));
                }
            }
            flags = Some(toks);
        }
    }
    Ok(flags.unwrap_or_default())
}

pub fn compile_output(
    input: &Path,
    opt_level: OptLevel,
    mode: EmitMode,
    output: &Path,
) -> Result<(), String> {
    // Apply the fixture's `! FLAGS:` line (x09, one-dialect rule):
    // route through the driver's CLI parser so textual flags mean
    // exactly what they mean on the command line.
    let flags = fixture_flags(input)?;
    if !flags.is_empty() {
        let mut argv: Vec<String> = flags;
        argv.push(into_driver_opt_level(opt_level).as_flag().to_string());
        match mode {
            EmitMode::Asm => argv.push("-S".into()),
            EmitMode::Obj => argv.push("-c".into()),
            EmitMode::Binary => {}
        }
        argv.push(input.to_string_lossy().into_owned());
        argv.push("-o".into());
        argv.push(output.to_string_lossy().into_owned());
        let opts = armfortas::driver::Options::from_args(&argv)?;
        return armfortas::driver::compile(&opts);
    }

    let opts = armfortas::driver::Options {
        input: input.to_path_buf(),
        output: Some(PathBuf::from(output)),
        emit_asm: matches!(mode, EmitMode::Asm),
        emit_obj: matches!(mode, EmitMode::Obj),
        opt_level: into_driver_opt_level(opt_level),
        ..armfortas::driver::Options::default()
    };

    armfortas::driver::compile(&opts)
}

pub fn compile_graph_output(
    inputs: &[PathBuf],
    opt_level: OptLevel,
    output: &Path,
    module_output_dir: &Path,
) -> Result<(), String> {
    let (input, extra_inputs) = inputs
        .split_first()
        .ok_or_else(|| "graph compile requires at least one source".to_string())?;
    for source in inputs {
        let flags = fixture_flags(source)?;
        if !flags.is_empty() {
            return Err(format!(
                "graph source '{}' carries `! FLAGS: {}`; graph-wide flag semantics are not defined",
                source.display(),
                flags.join(" ")
            ));
        }
    }
    fs::create_dir_all(module_output_dir).map_err(|error| {
        format!(
            "cannot create graph module directory '{}': {}",
            module_output_dir.display(),
            error
        )
    })?;

    let opts = armfortas::driver::Options {
        input: input.clone(),
        extra_inputs: extra_inputs.to_vec(),
        output: Some(output.to_path_buf()),
        opt_level: into_driver_opt_level(opt_level),
        module_search_paths: vec![module_output_dir.to_path_buf()],
        module_output_dir: Some(module_output_dir.to_path_buf()),
        ..armfortas::driver::Options::default()
    };
    armfortas::driver::execute(&opts)
}

pub fn capture_from_path(request: &CaptureRequest) -> Result<CaptureResult, CaptureFailure> {
    // The capture surface has no flags channel; a FLAGS-carrying
    // fixture compiled without its flags could silently diverge from
    // the root harness. Refuse loudly (x09, one-dialect rule).
    let input_path = request.input.clone();
    match fixture_flags(&input_path) {
        Ok(flags) if flags.is_empty() => {}
        Ok(flags) => {
            return Err(CaptureFailure {
                input: input_path,
                opt_level: request.opt_level,
                stage: FailureStage::Ir,
                detail: format!(
                    "fixture carries `! FLAGS: {}` which the bencch capture path does not apply; \
                     use compile_output or extend CaptureRequest with a flags channel",
                    flags.join(" ")
                ),
                stages: BTreeMap::new(),
            });
        }
        Err(detail) => {
            return Err(CaptureFailure {
                input: input_path,
                opt_level: request.opt_level,
                stage: FailureStage::Preprocess,
                detail,
                stages: BTreeMap::new(),
            });
        }
    }
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

pub fn capture_graph(
    entry: &Path,
    inputs: &[PathBuf],
    requested: &BTreeSet<Stage>,
    opt_level: OptLevel,
    work_root: &Path,
) -> Result<CaptureResult, CaptureFailure> {
    let failure = |stage, detail, stages| CaptureFailure {
        input: entry.to_path_buf(),
        opt_level,
        stage,
        detail,
        stages,
    };
    if inputs.is_empty() {
        return Err(failure(
            FailureStage::Preprocess,
            "graph capture requires at least one source".into(),
            BTreeMap::new(),
        ));
    }
    if !inputs.iter().any(|source| source == entry) {
        return Err(failure(
            FailureStage::Preprocess,
            format!(
                "graph entry '{}' is not present in the authored source list",
                entry.display()
            ),
            BTreeMap::new(),
        ));
    }
    if let Err(error) = fs::create_dir_all(work_root) {
        return Err(failure(
            FailureStage::Preprocess,
            format!(
                "cannot create graph work directory '{}': {}",
                work_root.display(),
                error
            ),
            BTreeMap::new(),
        ));
    }
    for source in inputs {
        match fixture_flags(source) {
            Ok(flags) if flags.is_empty() => {}
            Ok(flags) => {
                return Err(failure(
                    FailureStage::Ir,
                    format!(
                        "graph source '{}' carries `! FLAGS: {}`; graph-wide flag semantics are not defined",
                        source.display(),
                        flags.join(" ")
                    ),
                    BTreeMap::new(),
                ));
            }
            Err(detail) => {
                return Err(failure(FailureStage::Preprocess, detail, BTreeMap::new()));
            }
        }
    }

    let order = graph_compilation_order(inputs, work_root).map_err(|detail| {
        failure(
            FailureStage::Preprocess,
            format!("cannot resolve graph compilation order: {}", detail),
            BTreeMap::new(),
        )
    })?;
    let capture_stages = requested
        .iter()
        .copied()
        .filter(|stage| *stage != Stage::Run)
        .collect::<BTreeSet<_>>();
    let needs_module_artifacts = capture_stages.iter().any(|stage| {
        matches!(
            stage,
            Stage::Sema
                | Stage::Ir
                | Stage::OptIr
                | Stage::Mir
                | Stage::Regalloc
                | Stage::Asm
                | Stage::Obj
        )
    });
    let needs_link = requested.contains(&Stage::Obj) || requested.contains(&Stage::Run);
    let mut stages = BTreeMap::new();
    let mut objects = vec![None; inputs.len()];

    for (position, &source_index) in order.iter().enumerate() {
        let source = &inputs[source_index];
        if !capture_stages.is_empty() {
            let request = CaptureRequest {
                input: source.clone(),
                requested: capture_stages.clone(),
                opt_level,
            };
            match capture_from_path_with_module_search_paths(&request, &[work_root.to_path_buf()]) {
                Ok(result) => {
                    append_graph_stages(&mut stages, source_index, source, result.stages)
                        .map_err(|detail| failure(FailureStage::Ir, detail, stages.clone()))?;
                }
                Err(mut member_failure) => {
                    append_graph_stages(
                        &mut stages,
                        source_index,
                        source,
                        std::mem::take(&mut member_failure.stages),
                    )
                    .map_err(|detail| failure(FailureStage::Ir, detail, stages.clone()))?;
                    member_failure.stages = stages;
                    member_failure.detail = format!(
                        "graph member [{}] '{}' failed:\n{}",
                        source_index,
                        source.display(),
                        member_failure.detail
                    );
                    return Err(member_failure);
                }
            }
        }

        let is_last = position + 1 == order.len();
        if needs_link || (needs_module_artifacts && !is_last) {
            let object = work_root.join(format!("unit_{:04}.o", source_index));
            compile_graph_member(source, opt_level, &object, work_root).map_err(|detail| {
                failure(
                    FailureStage::Obj,
                    format!(
                        "graph member [{}] '{}' failed while producing its module/object artifacts:\n{}",
                        source_index,
                        source.display(),
                        detail
                    ),
                    stages.clone(),
                )
            })?;
            objects[source_index] = Some(object);
        }
    }

    if needs_link {
        let object_paths = objects
            .into_iter()
            .enumerate()
            .map(|(index, object)| {
                object.ok_or_else(|| {
                    format!(
                        "graph member [{}] '{}' did not produce an object",
                        index,
                        inputs[index].display()
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|detail| failure(FailureStage::Obj, detail, stages.clone()))?;
        let binary = work_root.join("graph.out");
        link_graph_objects(&object_paths, opt_level, &binary).map_err(|detail| {
            failure(
                if requested.contains(&Stage::Obj) {
                    FailureStage::Obj
                } else {
                    FailureStage::Run
                },
                format!("graph object link failed:\n{}", detail),
                stages.clone(),
            )
        })?;

        if requested.contains(&Stage::Run) {
            let sandbox = work_root.join("run-sandbox");
            fs::create_dir_all(&sandbox).map_err(|error| {
                failure(
                    FailureStage::Run,
                    format!(
                        "cannot create graph run sandbox '{}': {}",
                        sandbox.display(),
                        error
                    ),
                    stages.clone(),
                )
            })?;
            let output = run_managed(
                Command::new(&binary).current_dir(&sandbox),
                CommandClass::Run,
            )
            .map_err(|error| {
                failure(
                    FailureStage::Run,
                    format!("cannot run graph binary '{}': {}", binary.display(), error),
                    stages.clone(),
                )
            })?;
            let files = snapshot_sandbox_files(&sandbox)
                .map_err(|detail| failure(FailureStage::Run, detail, stages.clone()))?;
            stages.insert(
                Stage::Run,
                CapturedStage::Run(RunCapture {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    files: Some(files),
                }),
            );
        }
    }

    Ok(CaptureResult {
        input: entry.to_path_buf(),
        opt_level,
        stages,
    })
}

fn capture_from_path_with_module_search_paths(
    request: &CaptureRequest,
    module_search_paths: &[PathBuf],
) -> Result<CaptureResult, CaptureFailure> {
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
    armfortas::testing::capture_from_path_with_module_search_paths(
        &arm_request,
        module_search_paths,
    )
    .map(into_bench_capture_result)
    .map_err(into_bench_capture_failure)
}

fn snapshot_sandbox_files(sandbox: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    fn collect(
        root: &Path,
        directory: &Path,
        files: &mut BTreeMap<String, Vec<u8>>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                collect(root, &path, files)?;
            } else {
                let relative = path.strip_prefix(root).expect("entry is below sandbox");
                files.insert(
                    relative.to_string_lossy().replace('\\', "/"),
                    fs::read(&path)?,
                );
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    collect(sandbox, sandbox, &mut files)
        .map_err(|error| format!("cannot snapshot sandbox '{}': {}", sandbox.display(), error))?;
    Ok(files)
}

fn graph_compilation_order(inputs: &[PathBuf], work_root: &Path) -> Result<Vec<usize>, String> {
    let host = armfortas::target::TargetSpec::host();
    let dependencies = inputs
        .iter()
        .map(|source| {
            let mut config = armfortas::preprocess::PreprocConfig::for_target(&host);
            config.filename = source.to_string_lossy().into_owned();
            config.fixed_form = matches!(
                armfortas::lexer::detect_source_form(&source.to_string_lossy()),
                armfortas::lexer::SourceForm::FixedForm
            );
            config.include_paths.push(work_root.to_path_buf());
            armfortas::driver::dep_scan::scan_file(source, &config)
        })
        .collect::<Result<Vec<_>, _>>()?;
    armfortas::driver::dep_scan::resolve_compilation_order(&dependencies)
}

fn compile_graph_member(
    source: &Path,
    opt_level: OptLevel,
    object: &Path,
    work_root: &Path,
) -> Result<(), String> {
    let opts = armfortas::driver::Options {
        input: source.to_path_buf(),
        output: Some(object.to_path_buf()),
        emit_obj: true,
        opt_level: into_driver_opt_level(opt_level),
        module_search_paths: vec![work_root.to_path_buf()],
        module_output_dir: Some(work_root.to_path_buf()),
        ..armfortas::driver::Options::default()
    };
    armfortas::driver::compile(&opts)
}

fn link_graph_objects(
    objects: &[PathBuf],
    opt_level: OptLevel,
    output: &Path,
) -> Result<(), String> {
    let (input, extra_inputs) = objects
        .split_first()
        .ok_or_else(|| "graph link requires at least one object".to_string())?;
    let opts = armfortas::driver::Options {
        input: input.clone(),
        extra_inputs: extra_inputs.to_vec(),
        output: Some(output.to_path_buf()),
        opt_level: into_driver_opt_level(opt_level),
        ..armfortas::driver::Options::default()
    };
    armfortas::driver::execute(&opts)
}

fn append_graph_stages(
    aggregate: &mut BTreeMap<Stage, CapturedStage>,
    source_index: usize,
    source: &Path,
    member_stages: BTreeMap<Stage, CapturedStage>,
) -> Result<(), String> {
    for (stage, captured) in member_stages {
        let CapturedStage::Text(text) = captured else {
            return Err(format!(
                "graph member capture unexpectedly produced non-text stage '{}'",
                stage.as_str()
            ));
        };
        let section = format!(
            "===== graph source [{:04}] {} =====\n{}",
            source_index,
            source.display(),
            text
        );
        match aggregate.entry(stage) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(CapturedStage::Text(section));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let CapturedStage::Text(existing) = entry.get_mut() else {
                    return Err(format!(
                        "graph aggregate for stage '{}' was not text",
                        stage.as_str()
                    ));
                };
                existing.push_str("\n\n");
                existing.push_str(&section);
            }
        }
    }
    Ok(())
}

fn into_driver_opt_level(opt_level: OptLevel) -> armfortas::driver::OptLevel {
    match opt_level {
        OptLevel::O0 => armfortas::driver::OptLevel::O0,
        OptLevel::O1 => armfortas::driver::OptLevel::O1,
        OptLevel::O2 => armfortas::driver::OptLevel::O2,
        OptLevel::O3 => armfortas::driver::OptLevel::O3,
        OptLevel::Os => armfortas::driver::OptLevel::Os,
        OptLevel::Ofast => armfortas::driver::OptLevel::Ofast,
    }
}

fn from_driver_opt_level(opt_level: armfortas::driver::OptLevel) -> OptLevel {
    match opt_level {
        armfortas::driver::OptLevel::O0 => OptLevel::O0,
        armfortas::driver::OptLevel::O1 => OptLevel::O1,
        armfortas::driver::OptLevel::O2 => OptLevel::O2,
        armfortas::driver::OptLevel::O3 => OptLevel::O3,
        armfortas::driver::OptLevel::Os => OptLevel::Os,
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
            files: Some(run.files),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        capture_from_path, from_driver_opt_level, into_driver_opt_level, CaptureRequest,
        FailureStage, OptLevel, Stage,
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn driver_opt_level_round_trips_os() {
        let driver = into_driver_opt_level(OptLevel::Os);
        assert_eq!(driver, armfortas::driver::OptLevel::Os);
        assert_eq!(from_driver_opt_level(driver), OptLevel::Os);
    }

    #[test]
    fn capture_rejects_malformed_flags_before_compilation() {
        for (name, annotations, expected) in [
            (
                "duplicate",
                "! FLAGS: -fdefault-integer-8\n! FLAGS: -fdefault-real-8\n",
                "multiple FLAGS annotations",
            ),
            ("empty", "! FLAGS:\n", "FLAGS annotation with no flags"),
            (
                "harness_owned",
                "! FLAGS: -O2\n",
                "FLAGS may not contain harness-owned flag '-O2'",
            ),
        ] {
            let root = std::env::temp_dir().join(format!(
                "bencch_malformed_flags_{}_{}_{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
                name
            ));
            fs::create_dir_all(&root).unwrap();
            let source = root.join("case.f90");
            fs::write(
                &source,
                format!(
                    "program malformed_flags\n  print *, 1\nend program malformed_flags\n{annotations}"
                ),
            )
            .unwrap();
            let request = CaptureRequest {
                input: source,
                requested: BTreeSet::from([Stage::Ir]),
                opt_level: OptLevel::O0,
            };

            let result = capture_from_path(&request);
            let _ = fs::remove_dir_all(&root);
            let failure = result.expect_err("malformed FLAGS must fail before compilation");
            assert_eq!(failure.stage, FailureStage::Preprocess);
            assert!(failure.stages.is_empty());
            assert!(
                failure.detail.contains(expected),
                "unexpected failure for {name}: {}",
                failure.detail
            );
        }
    }

    #[test]
    fn capture_distinguishes_absent_and_well_formed_flags() {
        let root = std::env::temp_dir().join(format!(
            "bencch_valid_flags_{}_{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let plain_source = root.join("plain.f90");
        let flagged_source = root.join("flagged.f90");
        let program = "program flags_contract\nend program flags_contract\n";
        fs::write(&plain_source, program).unwrap();
        fs::write(
            &flagged_source,
            format!("{program}! FLAGS: -fdefault-integer-8\n"),
        )
        .unwrap();

        let plain = capture_from_path(&CaptureRequest {
            input: plain_source,
            requested: BTreeSet::from([Stage::Ir]),
            opt_level: OptLevel::O0,
        });
        let flagged = capture_from_path(&CaptureRequest {
            input: flagged_source,
            requested: BTreeSet::from([Stage::Ir]),
            opt_level: OptLevel::O0,
        });
        let _ = fs::remove_dir_all(&root);

        assert!(plain.is_ok(), "a fixture without FLAGS must still capture");
        let failure = flagged.expect_err("well-formed FLAGS need an explicit capture channel");
        assert_eq!(failure.stage, FailureStage::Ir);
        assert!(failure.stages.is_empty());
        assert!(
            failure
                .detail
                .contains("bencch capture path does not apply"),
            "{}",
            failure.detail
        );
    }
}

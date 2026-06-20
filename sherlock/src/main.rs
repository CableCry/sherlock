//! Orchestrator CLI: builds the profiler `.so` + analyzer, compiles & launches
//! the target under `LD_PRELOAD`, runs the dashboard, cleans up. See DESIGN.md.

use profiler::injection::{AllocEvent, RING_CAPACITY, SHM_NAME_ENV_VAR};
use profiler::ring_buffer::ShmRingBuffer;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn is_workspace_root(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file() && dir.join("profiler").join("Cargo.toml").is_file()
}

fn walk_up_for_workspace(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if is_workspace_root(d) {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn find_workspace_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SHERLOCK_WORKSPACE") {
        let p = PathBuf::from(p);
        if is_workspace_root(&p) {
            return Some(p);
        }
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(root) = walk_up_for_workspace(&cwd)
    {
        return Some(root);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && let Some(root) = walk_up_for_workspace(dir)
    {
        return Some(root);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .filter(|p| is_workspace_root(p))
}

fn compiler_for(source: &Path) -> &'static str {
    match source.extension().and_then(|e| e.to_str()) {
        Some("c") => "cc",
        _ => "c++",
    }
}

fn run_cargo(workspace_root: &Path, args: &[&str], what: &str) -> std::io::Result<()> {
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .args(args)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!("{what} failed ({status})")));
    }
    Ok(())
}

fn build_profiler_so(workspace_root: &Path) -> std::io::Result<()> {
    run_cargo(
        workspace_root,
        &[
            "rustc",
            "-p",
            "profiler",
            "--lib",
            "--crate-type",
            "cdylib",
            "--features",
            "profiler-hooks",
        ],
        "cargo rustc -p profiler (cdylib)",
    )
}

fn build_analyzer(workspace_root: &Path) -> std::io::Result<()> {
    run_cargo(
        workspace_root,
        &["build", "-p", "analyzer"],
        "cargo build -p analyzer",
    )
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("sherlock: {msg}");
    std::process::exit(1);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(source) = args.next() else {
        eprintln!("usage: sherlock <program.c|program.cpp> [extra compiler flags...]");
        std::process::exit(2);
    };
    let extra_flags: Vec<String> = args.collect();

    let source_path = PathBuf::from(&source);
    if !source_path.is_file() {
        eprintln!("sherlock: no such file: {source}");
        std::process::exit(2);
    }

    let Some(workspace_root) = find_workspace_root() else {
        fail(
            "could not locate the sherlock workspace (need the source tree to build the \
             profiler/analyzer). Run from inside the checkout or set SHERLOCK_WORKSPACE.",
        );
    };
    let target_dir = workspace_root.join("target").join("debug");

    eprintln!("sherlock: building profiler and analyzer...");
    if let Err(e) = build_profiler_so(&workspace_root) {
        fail(e);
    }
    if let Err(e) = build_analyzer(&workspace_root) {
        fail(e);
    }

    let libprofiler = target_dir.join("libprofiler.so");
    let analyzer_bin = target_dir.join("analyzer");
    if !libprofiler.is_file() || !analyzer_bin.is_file() {
        fail(format!(
            "expected build artifacts missing under {}",
            target_dir.display()
        ));
    }

    let run_dir = std::env::temp_dir().join(format!("sherlock-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&run_dir) {
        fail(format!("failed to create {}: {e}", run_dir.display()));
    }

    let stem = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("target");
    let compiled = run_dir.join(stem);
    let compiler = compiler_for(&source_path);

    eprintln!("sherlock: compiling {source} with {compiler} -g -fno-omit-frame-pointer -O0 ...");
    let build_status = Command::new(compiler)
        .arg("-g")
        .arg("-fno-omit-frame-pointer")
        .arg("-O0")
        .args(&extra_flags)
        .arg("-o")
        .arg(&compiled)
        .arg(&source_path)
        .status();
    match build_status {
        Ok(status) if status.success() => {}
        Ok(status) => fail(format!("{compiler} exited with {status}")),
        Err(e) => fail(format!("failed to run {compiler}: {e}")),
    }

    let shm_name = format!("/profiler_ring_{}", std::process::id());
    let report_path = run_dir.join("report.folded");
    let log_path = run_dir.join("target.log");
    let log_file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => fail(format!(
            "failed to create log file {}: {e}",
            log_path.display()
        )),
    };
    let log_file_err = match log_file.try_clone() {
        Ok(f) => f,
        Err(e) => fail(format!("failed to clone log file handle: {e}")),
    };

    eprintln!(
        "sherlock: launching {} (output -> {})",
        compiled.display(),
        log_path.display()
    );
    let mut target_child = match Command::new(&compiled)
        .env("LD_PRELOAD", &libprofiler)
        .env(SHM_NAME_ENV_VAR, &shm_name)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
    {
        Ok(child) => child,
        Err(e) => fail(format!("failed to launch {}: {e}", compiled.display())),
    };
    let target_pid = target_child.id();

    eprintln!("sherlock: starting dashboard (press q to quit)...");
    let analyzer_status = Command::new(&analyzer_bin)
        .arg("--binary")
        .arg(&compiled)
        .arg("--pid")
        .arg(target_pid.to_string())
        .arg("--shm-name")
        .arg(&shm_name)
        .arg("--export")
        .arg(&report_path)
        .status();

    match target_child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = target_child.kill();
            let _ = target_child.wait();
        }
        Err(_) => {}
    }

    let _ = ShmRingBuffer::<AllocEvent, RING_CAPACITY>::unlink(&shm_name);

    if let Err(e) = analyzer_status {
        fail(format!("failed to run analyzer: {e}"));
    }

    eprintln!(
        "sherlock: done. target output -> {}",
        log_path.display()
    );
    if report_path.is_file() {
        eprintln!(
            "sherlock: live-allocation flamegraph stacks -> {} \
             (e.g. `flamegraph.pl {}` or drop into speedscope.app)",
            report_path.display(),
            report_path.display()
        );
    }
}

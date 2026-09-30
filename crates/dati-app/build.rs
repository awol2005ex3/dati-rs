//! Builds the embedded frontend so `cargo build` alone produces the single executable.
//!
//! The Java backend does the same thing in `backend/app/pom.xml`: its `standalone` profile runs
//! `pnpm run build:all` and copies `frontend/dist` into the jar. Here `rust-embed` reads that very
//! folder instead. Building the UI is never fatal: a machine without pnpm or without network still
//! gets a working API binary, and `DATI_SKIP_FRONTEND=1` opts out explicitly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

/// Paths whose changes are worth a rebuild. The generated ones among them (`frontend/src/*.d.ts`,
/// `docs/user-guide/.vitepress/.temp`) make this script run again after a frontend build; the
/// staleness check below is what decides whether pnpm then actually runs.
const WATCHED: [&str; 7] = [
    "frontend/src",
    "frontend/index.html",
    "frontend/package.json",
    "frontend/pnpm-lock.yaml",
    "frontend/vite.config.ts",
    "frontend/dist",
    "docs/user-guide",
];

/// npm's Windows shim for pnpm is a `.cmd`, which `CreateProcess` never finds by itself.
#[cfg(windows)]
const PNPM: [&str; 2] = ["pnpm.cmd", "pnpm"];
#[cfg(not(windows))]
const PNPM: [&str; 1] = ["pnpm"];

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .ancestors()
        .nth(2)
        .expect("dati-app lives in crates/<package>")
        .to_path_buf();
    let frontend = repo.join("frontend");

    for watched in WATCHED {
        let path = repo.join(watched);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-env-changed=DATI_SKIP_FRONTEND");
    println!("cargo:rerun-if-env-changed=VITE_APP_VERSION");

    if is_stale(&frontend, &repo.join("docs/user-guide"))
        && let Err(reason) = build_frontend(&repo, &frontend)
    {
        println!("cargo:warning=embedded frontend skipped: {reason}");
    }
    // `rust-embed` takes an empty folder the easy way and a missing one the hard way.
    let _ = std::fs::create_dir_all(frontend.join("dist"));
}

/// Whether the assets are older than the sources they are built from.
///
/// `pnpm run build:all` generates the docs first and the SPA bundle last, and vite writes
/// `dist/index.html` after every chunk, so it is newer than anything - including the codegen files
/// the build itself rewrites under `frontend/src`.
fn is_stale(frontend: &Path, docs: &Path) -> bool {
    let built = frontend.join("dist").join("index.html");
    match built.metadata().and_then(|meta| meta.modified()) {
        Err(_) => true,
        Ok(built_at) => is_newer(&frontend.join("src"), built_at) || is_newer(docs, built_at),
    }
}

fn is_newer(path: &Path, than: SystemTime) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_dir() {
        return metadata.modified().is_ok_and(|modified| modified > than);
    }
    path.read_dir()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| is_newer(&entry.path(), than))
}

fn build_frontend(repo: &Path, frontend: &Path) -> Result<(), String> {
    if std::env::var_os("DATI_SKIP_FRONTEND").is_some_and(|value| value != "0") {
        return Ok(());
    }
    if !frontend.join("package.json").is_file() {
        return Err(format!("{} has no package.json", frontend.display()));
    }
    println!("cargo:warning=building the embedded frontend with pnpm");
    if !frontend.join("node_modules").is_dir() {
        run(frontend, &["install", "--frozen-lockfile"])?;
    }
    link_root_node_modules(repo, frontend)?;
    run(frontend, &["run", "build:all"])
}

/// Runs pnpm and reports the tail of its stderr, so a failed UI build says why in the warning.
fn run(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let mut unavailable = String::from("pnpm is not on PATH");
    for program in PNPM {
        let Ok(output) = Command::new(program).current_dir(cwd).args(args).output() else {
            unavailable = format!("`{program}` is not runnable");
            continue;
        };
        if output.status.success() {
            return Ok(());
        }
        let detail = String::from_utf8_lossy(&output.stderr);
        let tail = detail.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        return Err(format!("pnpm {} failed: {tail}", args.join(" ")));
    }
    Err(unavailable)
}

/// VitePress lives outside the pnpm workspace, so its own imports only resolve when the repository
/// root exposes the frontend's `node_modules` - the Java Dockerfile symlinks exactly that, and a
/// junction is the Windows equivalent that needs no elevated token.
fn link_root_node_modules(repo: &Path, frontend: &Path) -> Result<(), String> {
    let target = frontend.join("node_modules");
    let link = repo.join("node_modules");
    if link.exists() || !target.is_dir() {
        return Ok(());
    }
    link_node_modules(&link, &target).map_err(|err| {
        format!(
            "cannot link {} to {}: {err}",
            link.display(),
            target.display()
        )
    })
}

#[cfg(windows)]
fn link_node_modules(link: &Path, target: &Path) -> std::io::Result<()> {
    // `mklink` is a cmd builtin, so it has to be asked for through cmd itself.
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "mklink exited with {status}"
        )))
    }
}

#[cfg(unix)]
fn link_node_modules(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

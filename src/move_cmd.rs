use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use crate::cli::Cli;
use crate::config::resolve_move_target_dir;

const MAX_COMMAND_OUTPUT_CHARS: usize = 220;
const MAX_ROBOCOPY_MT: usize = 128;

#[derive(Debug, Clone)]
pub(crate) struct MoveFailureEntry {
    pub(crate) source: PathBuf,
    pub(crate) destination: Option<PathBuf>,
    pub(crate) backend: &'static str,
    pub(crate) command: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) error: String,
}

#[derive(Debug)]
pub(crate) struct MoveOutcome {
    pub(crate) total: usize,
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
    pub(crate) failures: Vec<MoveFailureEntry>,
}

#[derive(Debug, Clone, Copy)]
enum MoveBackend {
    CmdMove,
    RoboCopy,
    Unknown,
}

impl MoveBackend {
    fn label(self) -> &'static str {
        match self {
            MoveBackend::CmdMove => "cmd-move",
            MoveBackend::RoboCopy => "robocopy",
            MoveBackend::Unknown => "unknown",
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct MoveSuccess {
    destination: PathBuf,
    backend: MoveBackend,
    command: String,
}

pub(crate) fn run_move(
    cli: &Cli,
    sources: Vec<PathBuf>,
    target_dir_override: Option<PathBuf>,
    mt: Option<usize>,
) -> Result<MoveOutcome> {
    #[cfg(not(windows))]
    {
        let _ = (cli, sources, target_dir_override, mt);
        bail!("`move` command is only supported on Windows");
    }

    #[cfg(windows)]
    {
        run_move_windows(cli, sources, target_dir_override, mt)
    }
}

#[cfg(windows)]
fn run_move_windows(
    cli: &Cli,
    sources: Vec<PathBuf>,
    target_dir_override: Option<PathBuf>,
    mt: Option<usize>,
) -> Result<MoveOutcome> {
    if let Some(value) = mt {
        if value > MAX_ROBOCOPY_MT {
            bail!("--mt must be between 1 and {MAX_ROBOCOPY_MT} for robocopy");
        }
    }

    let target_dir = resolve_move_target_dir(&cli.config, target_dir_override)?;
    let total = sources.len();
    if total == 0 {
        return Ok(MoveOutcome {
            total: 0,
            succeeded: 0,
            failed: 0,
            failures: Vec::new(),
        });
    }

    if !cli.dry_run {
        fs::create_dir_all(&target_dir).with_context(|| {
            format!(
                "failed to create move target directory {}",
                target_dir.display()
            )
        })?;
    }

    let mut succeeded = 0usize;
    let mut failures = Vec::new();

    for source in sources {
        match move_one_with_windows_commands(&source, &target_dir, mt, cli.dry_run) {
            Ok(success) => {
                succeeded += 1;
                if cli.dry_run {
                    info!(
                        source = %source.display(),
                        destination = %success.destination.display(),
                        backend = success.backend.label(),
                        command = %success.command,
                        "dry-run: would move"
                    );
                } else {
                    info!(
                        source = %source.display(),
                        destination = %success.destination.display(),
                        backend = success.backend.label(),
                        command = %success.command,
                        "moved"
                    );
                }
            }
            Err(failure) => {
                let failure = *failure;
                warn!(
                    source = %failure.source.display(),
                    destination = ?failure.destination.as_ref().map(|p| p.display().to_string()),
                    backend = failure.backend,
                    command = %failure.command,
                    exit_code = ?failure.exit_code,
                    error = %failure.error,
                    "move failed, skipping"
                );
                failures.push(failure);
            }
        }
    }

    Ok(MoveOutcome {
        total,
        succeeded,
        failed: failures.len(),
        failures,
    })
}

#[cfg(windows)]
fn move_one_with_windows_commands(
    source: &Path,
    target_dir: &Path,
    mt: Option<usize>,
    dry_run: bool,
) -> std::result::Result<MoveSuccess, Box<MoveFailureEntry>> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(err) => {
            return Err(Box::new(MoveFailureEntry {
                source: source.to_path_buf(),
                destination: None,
                backend: MoveBackend::Unknown.label(),
                command: "<unavailable>".to_string(),
                exit_code: err.raw_os_error(),
                error: format!("failed to read source metadata: {err}"),
            }));
        }
    };

    let backend = if metadata.is_dir() {
        MoveBackend::RoboCopy
    } else {
        MoveBackend::CmdMove
    };
    let destination = match build_destination_path(source, target_dir) {
        Ok(destination) => destination,
        Err(err) => {
            return Err(Box::new(MoveFailureEntry {
                source: source.to_path_buf(),
                destination: None,
                backend: backend.label(),
                command: "<unavailable>".to_string(),
                exit_code: None,
                error: err.to_string(),
            }));
        }
    };

    if source == destination {
        return Err(Box::new(MoveFailureEntry {
            source: source.to_path_buf(),
            destination: Some(destination),
            backend: backend.label(),
            command: "<unavailable>".to_string(),
            exit_code: None,
            error: "source and destination are the same path".to_string(),
        }));
    }
    if destination.exists() {
        return Err(Box::new(MoveFailureEntry {
            source: source.to_path_buf(),
            destination: Some(destination),
            backend: backend.label(),
            command: "<unavailable>".to_string(),
            exit_code: None,
            error: "destination already exists, refusing to overwrite".to_string(),
        }));
    }

    match backend {
        MoveBackend::CmdMove => run_cmd_move(source, &destination, dry_run),
        MoveBackend::RoboCopy => run_robocopy_move(source, &destination, mt, dry_run),
        MoveBackend::Unknown => Err(Box::new(MoveFailureEntry {
            source: source.to_path_buf(),
            destination: Some(destination),
            backend: backend.label(),
            command: "<unavailable>".to_string(),
            exit_code: None,
            error: "unknown move backend".to_string(),
        })),
    }
}

pub(crate) fn build_destination_path(source: &Path, target_dir: &Path) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("source path has no file name: {}", source.display()))?;
    Ok(target_dir.join(file_name))
}

#[cfg(windows)]
fn run_cmd_move(
    source: &Path,
    destination: &Path,
    dry_run: bool,
) -> std::result::Result<MoveSuccess, Box<MoveFailureEntry>> {
    let args = vec![
        OsString::from("/C"),
        OsString::from("move"),
        OsString::from("/Y"),
        source.as_os_str().to_os_string(),
        destination.as_os_str().to_os_string(),
    ];
    let command = format_command("cmd", &args);
    if dry_run {
        return Ok(MoveSuccess {
            destination: destination.to_path_buf(),
            backend: MoveBackend::CmdMove,
            command,
        });
    }

    let output = Command::new("cmd").args(&args).output();
    let output = match output {
        Ok(output) => output,
        Err(err) => {
            return Err(Box::new(MoveFailureEntry {
                source: source.to_path_buf(),
                destination: Some(destination.to_path_buf()),
                backend: MoveBackend::CmdMove.label(),
                command,
                exit_code: err.raw_os_error(),
                error: format!("failed to spawn cmd move: {err}"),
            }));
        }
    };

    if output.status.success() {
        return Ok(MoveSuccess {
            destination: destination.to_path_buf(),
            backend: MoveBackend::CmdMove,
            command,
        });
    }

    Err(Box::new(MoveFailureEntry {
        source: source.to_path_buf(),
        destination: Some(destination.to_path_buf()),
        backend: MoveBackend::CmdMove.label(),
        command,
        exit_code: output.status.code(),
        error: command_error_text(&output),
    }))
}

#[cfg(windows)]
fn run_robocopy_move(
    source: &Path,
    destination: &Path,
    mt: Option<usize>,
    dry_run: bool,
) -> std::result::Result<MoveSuccess, Box<MoveFailureEntry>> {
    let mut args = vec![
        source.as_os_str().to_os_string(),
        destination.as_os_str().to_os_string(),
        OsString::from("/E"),
        OsString::from("/MOVE"),
        OsString::from("/R:1"),
        OsString::from("/W:1"),
        OsString::from("/NFL"),
        OsString::from("/NDL"),
        OsString::from("/NJH"),
        OsString::from("/NJS"),
        OsString::from("/NP"),
    ];
    if let Some(value) = mt {
        args.push(OsString::from(format!("/MT:{value}")));
    }

    let command = format_command("robocopy", &args);
    if dry_run {
        return Ok(MoveSuccess {
            destination: destination.to_path_buf(),
            backend: MoveBackend::RoboCopy,
            command,
        });
    }

    let output = Command::new("robocopy").args(&args).output();
    let output = match output {
        Ok(output) => output,
        Err(err) => {
            return Err(Box::new(MoveFailureEntry {
                source: source.to_path_buf(),
                destination: Some(destination.to_path_buf()),
                backend: MoveBackend::RoboCopy.label(),
                command,
                exit_code: err.raw_os_error(),
                error: format!("failed to spawn robocopy: {err}"),
            }));
        }
    };

    let exit_code = output.status.code();
    if let Some(code) = exit_code {
        if is_robocopy_success(code) {
            remove_empty_source_dir(source);
            return Ok(MoveSuccess {
                destination: destination.to_path_buf(),
                backend: MoveBackend::RoboCopy,
                command,
            });
        }
    }

    Err(Box::new(MoveFailureEntry {
        source: source.to_path_buf(),
        destination: Some(destination.to_path_buf()),
        backend: MoveBackend::RoboCopy.label(),
        command,
        exit_code,
        error: command_error_text(&output),
    }))
}

#[cfg(windows)]
fn remove_empty_source_dir(path: &Path) {
    if let Err(err) = fs::remove_dir(path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!(
                path = %path.display(),
                error = %err,
                "source directory cleanup skipped"
            );
        }
    }
}

pub(crate) fn is_robocopy_success(code: i32) -> bool {
    code < 8
}

#[cfg(windows)]
fn command_error_text(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return truncate_for_log(&stderr, MAX_COMMAND_OUTPUT_CHARS);
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return truncate_for_log(&stdout, MAX_COMMAND_OUTPUT_CHARS);
    }

    match output.status.code() {
        Some(code) => format!("command exited with code {code}"),
        None => "command terminated without exit code".to_string(),
    }
}

#[cfg(windows)]
fn truncate_for_log(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in input.chars().take(max_chars) {
        out.push(ch);
    }
    if input.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[cfg(windows)]
fn format_command(program: &str, args: &[OsString]) -> String {
    let mut parts = vec![program.to_string()];
    for arg in args {
        parts.push(quote_arg(arg));
    }
    parts.join(" ")
}

#[cfg(windows)]
fn quote_arg(arg: &OsStr) -> String {
    let value = arg.to_string_lossy();
    if value.is_empty() {
        return "\"\"".to_string();
    }

    let needs_quotes = value.contains(' ') || value.contains('\t') || value.contains('"');
    if !needs_quotes {
        return value.to_string();
    }
    format!("\"{}\"", value.replace('"', "\\\""))
}

pub(crate) fn print_move_summary(outcome: &MoveOutcome, dry_run: bool) {
    if dry_run {
        println!("Mode: dry-run (nothing was moved)");
    } else {
        println!("Mode: move");
    }

    println!("Total: {}", outcome.total);
    println!("Succeeded: {}", outcome.succeeded);
    println!("Failed: {}", outcome.failed);

    if !outcome.failures.is_empty() {
        println!();
        println!("Move failures:");
        for failure in &outcome.failures {
            let destination = failure
                .destination
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let exit_code = failure
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!(
                "- source={} | destination={} | backend={} | exit_code={} | command={} | {}",
                failure.source.display(),
                destination,
                failure.backend,
                exit_code,
                failure.command,
                failure.error
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robocopy_exit_code_threshold() {
        assert!(is_robocopy_success(0));
        assert!(is_robocopy_success(1));
        assert!(is_robocopy_success(7));
        assert!(!is_robocopy_success(8));
    }

    #[test]
    fn build_destination_uses_source_name() {
        let destination = build_destination_path(Path::new("C:\\a\\b.txt"), Path::new("D:\\dest"))
            .expect("destination should build");
        assert_eq!(destination, PathBuf::from("D:\\dest\\b.txt"));
    }
}

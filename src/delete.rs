use std::fs;
use std::io::IsTerminal;
use std::io::{self, ErrorKind};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use thiserror::Error;
use tracing::{info, warn};

use crate::cli::{Cli, DeleteStrategy};
use crate::config::resolve_target_paths;

const DELETE_RETRY_DELAYS_MS: [u64; 5] = [50, 100, 200, 400, 800];
#[cfg(windows)]
const FILE_ATTRIBUTE_READONLY: u32 = 0x0001;
#[cfg(windows)]
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0002;
#[cfg(windows)]
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0004;
#[cfg(windows)]
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
#[cfg(windows)]
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0020;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
#[cfg(windows)]
const MAX_COMMAND_OUTPUT_CHARS: usize = 160;

#[derive(Debug, Error)]
pub(crate) enum DeleteError {
    #[error("path does not exist")]
    Missing,
    #[error("failed to read metadata: {0}")]
    Metadata(#[source] std::io::Error),
    #[error("failed to delete file: {0}")]
    RemoveFile(#[source] std::io::Error),
    #[error("failed to delete directory: {0}")]
    RemoveDir(#[source] std::io::Error),
}

impl DeleteError {
    fn raw_os_error(&self) -> Option<i32> {
        match self {
            DeleteError::Missing => None,
            DeleteError::Metadata(err) => err.raw_os_error(),
            DeleteError::RemoveFile(err) => err.raw_os_error(),
            DeleteError::RemoveDir(err) => err.raw_os_error(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FailureEntry {
    pub(crate) path: PathBuf,
    pub(crate) failed_path: PathBuf,
    pub(crate) error: String,
    pub(crate) os_code: Option<i32>,
    pub(crate) path_details: String,
}

#[derive(Default)]
struct DeleteStats {
    processed: AtomicUsize,
    succeeded: AtomicUsize,
    failed: AtomicUsize,
}

#[derive(Debug)]
pub(crate) struct RunOutcome {
    pub(crate) total: usize,
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
    pub(crate) failures: Vec<FailureEntry>,
}

struct ProgressController {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
pub(crate) struct PathDeleteError {
    pub(crate) failed_path: PathBuf,
    pub(crate) error: DeleteError,
}

impl PathDeleteError {
    fn new(failed_path: PathBuf, error: DeleteError) -> Self {
        Self { failed_path, error }
    }

    fn raw_os_error(&self) -> Option<i32> {
        self.error.raw_os_error()
    }
}

impl ProgressController {
    fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn run(cli: Cli) -> Result<RunOutcome> {
    let target_paths = resolve_target_paths(&cli)?;
    let total = target_paths.len();
    let dry_run = cli.dry_run;
    let repair_permissions = cli.repair_permissions;
    let delete_strategy = cli.delete_strategy;

    if total == 0 {
        return Ok(RunOutcome {
            total,
            succeeded: 0,
            failed: 0,
            failures: Vec::new(),
        });
    }

    let thread_pool = build_thread_pool(cli.threads)?;
    let stats = Arc::new(DeleteStats::default());
    let failures = Arc::new(Mutex::new(Vec::<FailureEntry>::new()));
    let progress = maybe_start_progress(total, stats.clone(), !cli.no_progress);

    thread_pool.install(|| {
        target_paths.par_iter().for_each(|path| {
            let issues = delete_path(path, dry_run, repair_permissions, delete_strategy);
            if issues.is_empty() {
                stats.succeeded.fetch_add(1, Ordering::Relaxed);
                if dry_run {
                    info!(path = %path.display(), "dry-run: would delete");
                } else {
                    info!(path = %path.display(), "deleted");
                }
            } else {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                let mut guard = failures
                    .lock()
                    .expect("failure list mutex should not be poisoned");
                for issue in issues {
                    let os_code = issue.raw_os_error();
                    let failed_path = issue.failed_path.clone();
                    let path_details = collect_path_details(&failed_path);
                    let error_text = issue.error.to_string();
                    warn!(
                        target_path = %path.display(),
                        failed_path = %failed_path.display(),
                        error = %error_text,
                        os_code = ?os_code,
                        path_details = %path_details,
                        "delete failed, skipped entry"
                    );
                    guard.push(FailureEntry {
                        path: path.clone(),
                        failed_path,
                        error: error_text,
                        os_code,
                        path_details,
                    });
                }
            }
            stats.processed.fetch_add(1, Ordering::Relaxed);
        });
    });

    if let Some(progress) = progress {
        progress.finish();
    }

    let succeeded = stats.succeeded.load(Ordering::Relaxed);
    let failed = stats.failed.load(Ordering::Relaxed);
    let failure_list = failures
        .lock()
        .expect("failure list mutex should not be poisoned")
        .clone();

    Ok(RunOutcome {
        total,
        succeeded,
        failed,
        failures: failure_list,
    })
}

pub(crate) fn build_thread_pool(threads: Option<usize>) -> Result<rayon::ThreadPool> {
    let mut builder = ThreadPoolBuilder::new();
    if let Some(count) = threads {
        builder = builder.num_threads(count);
    }
    builder.build().context("failed to build rayon thread pool")
}

fn maybe_start_progress(
    total: usize,
    stats: Arc<DeleteStats>,
    progress_enabled: bool,
) -> Option<ProgressController> {
    if !progress_enabled || !std::io::stdout().is_terminal() {
        return None;
    }

    let progress = ProgressBar::new(total as u64);
    let style = ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("=>-");
    progress.set_style(style);
    progress.enable_steady_tick(Duration::from_millis(120));

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let progress_clone = progress.clone();
    let stats_clone = stats.clone();
    let worker = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            refresh_progress(&progress_clone, &stats_clone);
            thread::sleep(Duration::from_millis(200));
        }
        refresh_progress(&progress_clone, &stats_clone);
        let succeeded = stats_clone.succeeded.load(Ordering::Relaxed);
        let failed = stats_clone.failed.load(Ordering::Relaxed);
        progress_clone.finish_with_message(format!("ok: {succeeded} fail: {failed}"));
    });

    Some(ProgressController {
        stop,
        worker: Some(worker),
    })
}

fn refresh_progress(progress: &ProgressBar, stats: &DeleteStats) {
    let processed = stats.processed.load(Ordering::Relaxed);
    let succeeded = stats.succeeded.load(Ordering::Relaxed);
    let failed = stats.failed.load(Ordering::Relaxed);
    progress.set_position(processed as u64);
    progress.set_message(format!("ok: {succeeded} fail: {failed}"));
}

pub(crate) fn delete_path(
    path: &Path,
    dry_run: bool,
    repair_permissions: bool,
    delete_strategy: DeleteStrategy,
) -> Vec<PathDeleteError> {
    let mut issues = Vec::new();
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            PathDeleteError::new(path.to_path_buf(), DeleteError::Missing)
        } else {
            PathDeleteError::new(path.to_path_buf(), DeleteError::Metadata(err))
        }
    });
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(err) => {
            issues.push(err);
            return issues;
        }
    };

    if dry_run {
        return issues;
    }

    let file_type = metadata.file_type();
    if file_type.is_dir() {
        if is_directory_reparse_point(&metadata, &file_type) {
            if let Err(err) = delete_directory_link(path, repair_permissions) {
                issues.push(PathDeleteError::new(
                    path.to_path_buf(),
                    DeleteError::RemoveDir(err),
                ));
            }
        } else {
            match delete_strategy {
                DeleteStrategy::Native => {
                    if let Err(err) = delete_directory_tree_native(path, repair_permissions) {
                        issues.push(PathDeleteError::new(
                            path.to_path_buf(),
                            DeleteError::RemoveDir(err),
                        ));
                    }
                }
                DeleteStrategy::Recursive => {
                    delete_directory_tree_recursive(path, repair_permissions, &mut issues);
                }
            }
        }
    } else if let Err(err) = delete_file(path, &metadata, repair_permissions) {
        issues.push(PathDeleteError::new(
            path.to_path_buf(),
            DeleteError::RemoveFile(err),
        ));
    }

    issues
}

fn delete_directory_tree_recursive(
    path: &Path,
    repair_permissions: bool,
    issues: &mut Vec<PathDeleteError>,
) {
    let entries = read_dir_with_repair(path, repair_permissions)
        .map_err(|err| PathDeleteError::new(path.to_path_buf(), DeleteError::RemoveDir(err)));
    let entries = match entries {
        Ok(entries) => entries,
        Err(err) => {
            issues.push(err);
            return;
        }
    };
    let issue_count_before_children = issues.len();

    for entry in entries {
        let entry = entry
            .map_err(|err| PathDeleteError::new(path.to_path_buf(), DeleteError::RemoveDir(err)));
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                issues.push(err);
                continue;
            }
        };
        let child_path = entry.path();
        let child_metadata = fs::symlink_metadata(&child_path).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                PathDeleteError::new(child_path.clone(), DeleteError::Missing)
            } else {
                PathDeleteError::new(child_path.clone(), DeleteError::Metadata(err))
            }
        });
        let child_metadata = match child_metadata {
            Ok(metadata) => metadata,
            Err(err) => {
                issues.push(err);
                continue;
            }
        };

        let child_type = child_metadata.file_type();
        if child_type.is_dir() {
            if is_directory_reparse_point(&child_metadata, &child_type) {
                if let Err(err) = delete_directory_link(&child_path, repair_permissions) {
                    issues.push(PathDeleteError::new(
                        child_path.clone(),
                        DeleteError::RemoveDir(err),
                    ));
                }
            } else {
                delete_directory_tree_recursive(&child_path, repair_permissions, issues);
            }
        } else if let Err(err) = delete_file(&child_path, &child_metadata, repair_permissions) {
            issues.push(PathDeleteError::new(
                child_path.clone(),
                DeleteError::RemoveFile(err),
            ));
        }
    }

    if let Err(err) = delete_directory_node(path, repair_permissions) {
        let child_has_issues = issues.len() > issue_count_before_children;
        let only_due_to_children = child_has_issues && matches!(err.raw_os_error(), Some(145));
        if !only_due_to_children {
            issues.push(PathDeleteError::new(
                path.to_path_buf(),
                DeleteError::RemoveDir(err),
            ));
        }
    }
}

fn delete_directory_tree_native(path: &Path, repair_permissions: bool) -> io::Result<()> {
    let mut cleared_attributes = false;
    let mut repaired_permissions = false;

    retry_delete_operation(|| match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) => {
            if err.kind() == ErrorKind::NotFound {
                return Ok(());
            }
            if !cleared_attributes && should_try_attribute_fix(&err) {
                if let Err(clear_err) = clear_readonly_recursive(path) {
                    warn!(
                        path = %path.display(),
                        error = %clear_err,
                        "failed to clear readonly attributes before retry"
                    );
                }
                cleared_attributes = true;
            }
            if repair_permissions
                && !repaired_permissions
                && should_try_permission_repair(&err)
                && try_repair_permissions(path, true)
            {
                repaired_permissions = true;
            }
            Err(err)
        }
    })
}

fn read_dir_with_repair(path: &Path, repair_permissions: bool) -> io::Result<fs::ReadDir> {
    let mut attempted_repair = false;
    loop {
        match fs::read_dir(path) {
            Ok(entries) => return Ok(entries),
            Err(err) => {
                if repair_permissions
                    && !attempted_repair
                    && should_try_permission_repair(&err)
                    && try_repair_permissions(path, true)
                {
                    attempted_repair = true;
                    continue;
                }
                return Err(err);
            }
        }
    }
}

fn delete_directory_link(path: &Path, repair_permissions: bool) -> io::Result<()> {
    let mut repaired_permissions = false;
    retry_delete_operation(|| match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(err) => {
            if err.kind() == ErrorKind::NotFound {
                return Ok(());
            }
            if repair_permissions
                && !repaired_permissions
                && should_try_permission_repair(&err)
                && try_repair_permissions(path, false)
            {
                repaired_permissions = true;
            }
            Err(err)
        }
    })
}

fn delete_directory_node(path: &Path, repair_permissions: bool) -> io::Result<()> {
    let mut repaired_permissions = false;
    retry_delete_operation(|| match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(err) => {
            if err.kind() == ErrorKind::NotFound {
                return Ok(());
            }
            if repair_permissions
                && !repaired_permissions
                && should_try_permission_repair(&err)
                && try_repair_permissions(path, false)
            {
                repaired_permissions = true;
            }
            Err(err)
        }
    })
}

fn delete_file(path: &Path, metadata: &fs::Metadata, repair_permissions: bool) -> io::Result<()> {
    let mut cleared_attributes = false;
    let mut repaired_permissions = false;
    let was_readonly = metadata.permissions().readonly();

    retry_delete_operation(|| match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) => {
            if err.kind() == ErrorKind::NotFound {
                return Ok(());
            }
            if !cleared_attributes && was_readonly && should_try_attribute_fix(&err) {
                clear_readonly(path)?;
                cleared_attributes = true;
            }
            if repair_permissions
                && !repaired_permissions
                && should_try_permission_repair(&err)
                && try_repair_permissions(path, false)
            {
                repaired_permissions = true;
            }
            Err(err)
        }
    })
}

fn retry_delete_operation<F>(mut op: F) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
{
    for (attempt, delay_ms) in DELETE_RETRY_DELAYS_MS.iter().enumerate() {
        match op() {
            Ok(()) => return Ok(()),
            Err(err) => {
                let last_attempt = attempt + 1 == DELETE_RETRY_DELAYS_MS.len();
                if last_attempt || !is_retriable_delete_error(&err) {
                    return Err(err);
                }
                thread::sleep(Duration::from_millis(*delay_ms));
            }
        }
    }

    Err(io::Error::new(
        ErrorKind::Other,
        "delete retry loop terminated unexpectedly",
    ))
}

fn should_try_attribute_fix(err: &io::Error) -> bool {
    is_retriable_delete_error(err)
}

fn should_try_permission_repair(err: &io::Error) -> bool {
    matches!(err.kind(), ErrorKind::PermissionDenied) || matches!(err.raw_os_error(), Some(5))
}

pub(crate) fn is_retriable_delete_error(err: &io::Error) -> bool {
    if matches!(
        err.kind(),
        ErrorKind::PermissionDenied | ErrorKind::WouldBlock
    ) {
        return true;
    }

    matches!(err.raw_os_error(), Some(5 | 32 | 33 | 145))
}

fn try_repair_permissions(path: &Path, recursive: bool) -> bool {
    match repair_permissions_for_path(path, recursive) {
        Ok(()) => {
            info!(
                path = %path.display(),
                recursive,
                "permission repair completed (takeown/icacls)"
            );
            true
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                recursive,
                error = %err,
                "permission repair failed"
            );
            false
        }
    }
}

#[cfg(windows)]
fn repair_permissions_for_path(path: &Path, recursive: bool) -> io::Result<()> {
    let mut takeown = Command::new("takeown");
    takeown.arg("/F").arg(path).arg("/A").arg("/D").arg("Y");
    if recursive {
        takeown.arg("/R");
    }
    run_command_checked("takeown", &mut takeown)?;

    let mut icacls = Command::new("icacls");
    icacls
        .arg(path)
        .arg("/grant")
        .arg("*S-1-5-32-544:F")
        .arg("/C");
    if recursive {
        icacls.arg("/T");
    }
    run_command_checked("icacls", &mut icacls)
}

#[cfg(not(windows))]
fn repair_permissions_for_path(_path: &Path, _recursive: bool) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "permission repair mode requires Windows",
    ))
}

#[cfg(windows)]
fn run_command_checked(name: &str, command: &mut Command) -> io::Result<()> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let stderr = if stderr.is_empty() {
        "<no stderr output>"
    } else {
        stderr
    };
    let truncated = truncate_for_log(stderr, MAX_COMMAND_OUTPUT_CHARS);
    Err(io::Error::new(
        ErrorKind::PermissionDenied,
        format!("{name} failed: {truncated}"),
    ))
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

fn clear_readonly(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    clear_readonly_with_metadata(path, &metadata)
}

fn clear_readonly_recursive(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    clear_readonly_with_metadata(path, &metadata)?;

    let file_type = metadata.file_type();
    if file_type.is_dir() && !is_directory_reparse_point(&metadata, &file_type) {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let child_path = entry.path();
            clear_readonly_recursive(&child_path)?;
        }
    }

    Ok(())
}

fn clear_readonly_with_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }

    Ok(())
}

fn collect_path_details(path: &Path) -> String {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            let readonly = metadata.permissions().readonly();

            #[cfg(windows)]
            {
                let attrs = metadata.file_attributes();
                let flags = format_windows_attribute_flags(attrs);
                return format!(
                    "exists=true kind={kind} readonly={readonly} attrs=0x{attrs:08X} flags={flags}"
                );
            }

            #[cfg(not(windows))]
            {
                return format!("exists=true kind={kind} readonly={readonly}");
            }
        }
        Err(err) => format!("exists=false metadata_error={err}"),
    }
}

#[cfg(windows)]
fn format_windows_attribute_flags(attrs: u32) -> String {
    let mut flags = Vec::new();
    if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        flags.push("READONLY");
    }
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        flags.push("HIDDEN");
    }
    if attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
        flags.push("SYSTEM");
    }
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        flags.push("DIRECTORY");
    }
    if attrs & FILE_ATTRIBUTE_ARCHIVE != 0 {
        flags.push("ARCHIVE");
    }
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        flags.push("REPARSE_POINT");
    }

    if flags.is_empty() {
        "NONE".to_string()
    } else {
        flags.join("|")
    }
}

fn is_directory_reparse_point(metadata: &fs::Metadata, file_type: &fs::FileType) -> bool {
    if !file_type.is_dir() {
        return false;
    }

    if file_type.is_symlink() {
        return true;
    }

    #[cfg(windows)]
    {
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

pub(crate) fn print_summary(outcome: &RunOutcome, dry_run: bool) {
    if dry_run {
        println!("Mode: dry-run (nothing was deleted)");
    } else {
        println!("Mode: delete");
    }

    println!("Total: {}", outcome.total);
    println!("Succeeded: {}", outcome.succeeded);
    println!("Failed: {}", outcome.failed);
    println!("Skipped entries: {}", outcome.failures.len());

    if !outcome.failures.is_empty() {
        println!();
        println!("Skipped/Failed entries:");
        for failure in &outcome.failures {
            match failure.os_code {
                Some(code) => println!(
                    "- target={} | failed_at={} | {} | win32={code} | {}",
                    failure.path.display(),
                    failure.failed_path.display(),
                    failure.error,
                    failure.path_details
                ),
                None => println!(
                    "- target={} | failed_at={} | {} | win32=unknown | {}",
                    failure.path.display(),
                    failure.failed_path.display(),
                    failure.error,
                    failure.path_details
                ),
            }
        }
    }
}

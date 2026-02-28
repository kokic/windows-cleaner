use std::fs;
use std::io::IsTerminal;
use std::io::{self, ErrorKind};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process;
#[cfg(windows)]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use serde::Deserialize;
use thiserror::Error;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

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

#[derive(Parser, Debug)]
#[command(
    name = "windows-cleaner",
    version,
    about = "Delete configured paths or move files and back-link with symlinks"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<AppCommand>,

    #[arg(
        short,
        long,
        value_name = "FILE",
        default_value = "cleaner.toml",
        help = "Path to TOML config file"
    )]
    config: PathBuf,

    #[arg(
        short = 'p',
        long = "path",
        value_name = "PATH",
        help = "Delete this path directly; repeat to pass multiple paths. If set, config file is ignored"
    )]
    paths: Vec<PathBuf>,

    #[arg(
        long,
        value_name = "N",
        value_parser = parse_threads,
        help = "Worker thread count for parallel deletion. If omitted, Rayon uses its default strategy (typically based on available machine parallelism)"
    )]
    threads: Option<usize>,

    #[arg(long, help = "Disable progress bar and realtime counters")]
    no_progress: bool,

    #[arg(long, help = "Only simulate deletion without removing anything")]
    dry_run: bool,

    #[arg(
        long,
        value_enum,
        default_value_t = DeleteStrategy::Native,
        help = "Directory delete strategy: native (faster, uses remove_dir_all) or recursive (slower, skips failed entries and reports exact failed child paths at the end)"
    )]
    delete_strategy: DeleteStrategy,

    #[arg(
        long,
        help = "Attempt permission repair with takeown/icacls when access is denied"
    )]
    repair_permissions: bool,
}

#[derive(Debug, Clone, Subcommand)]
enum AppCommand {
    Init {
        #[arg(
            short,
            long,
            value_name = "FILE",
            default_value = "cleaner.toml",
            help = "Output file path for template TOML config"
        )]
        output: PathBuf,

        #[arg(long, help = "Overwrite file if it already exists")]
        force: bool,
    },
    MoveLink {
        #[arg(
            short = 's',
            long = "source",
            value_name = "FILE",
            required = true,
            help = "Source path (file or directory). Repeat to move multiple entries"
        )]
        sources: Vec<PathBuf>,

        #[arg(
            short = 'd',
            long = "target-dir",
            value_name = "DIR",
            help = "Destination directory. If omitted, uses move_target_dir from config"
        )]
        target_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
enum DeleteStrategy {
    Native,
    Recursive,
}

#[derive(Debug, Deserialize)]
struct CleanerConfig {
    paths: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct MoveConfig {
    move_target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct MoveFailureEntry {
    source: PathBuf,
    destination: Option<PathBuf>,
    error: String,
}

#[derive(Debug)]
struct MoveOutcome {
    total: usize,
    succeeded: usize,
    failed: usize,
    failures: Vec<MoveFailureEntry>,
}

#[derive(Debug, Clone, Copy)]
enum SourceKind {
    File,
    Directory,
}

#[derive(Debug, Error)]
enum DeleteError {
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
struct FailureEntry {
    path: PathBuf,
    failed_path: PathBuf,
    error: String,
    os_code: Option<i32>,
    path_details: String,
}

#[derive(Default)]
struct DeleteStats {
    processed: AtomicUsize,
    succeeded: AtomicUsize,
    failed: AtomicUsize,
}

#[derive(Debug)]
struct RunOutcome {
    total: usize,
    succeeded: usize,
    failed: usize,
    failures: Vec<FailureEntry>,
}

struct ProgressController {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
struct PathDeleteError {
    failed_path: PathBuf,
    error: DeleteError,
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

fn main() {
    init_tracing();

    let cli = Cli::parse();
    if let Some(command) = cli.command.clone() {
        match command {
            AppCommand::Init { output, force } => match write_template_config(&output, force) {
                Ok(()) => {
                    println!("Template config written: {}", output.display());
                    process::exit(0);
                }
                Err(err) => {
                    eprintln!("Fatal: {err:#}");
                    process::exit(2);
                }
            },
            AppCommand::MoveLink {
                sources,
                target_dir,
            } => match run_move_link(&cli, sources, target_dir) {
                Ok(outcome) => {
                    print_move_summary(&outcome, cli.dry_run);
                    if outcome.failed > 0 {
                        process::exit(1);
                    }
                    process::exit(0);
                }
                Err(err) => {
                    eprintln!("Fatal: {err:#}");
                    process::exit(2);
                }
            },
        }
    }

    let dry_run = cli.dry_run;

    match run(cli) {
        Ok(outcome) => {
            print_summary(&outcome, dry_run);
            if outcome.failed > 0 {
                process::exit(1);
            }
        }
        Err(err) => {
            eprintln!("Fatal: {err:#}");
            process::exit(2);
        }
    }
}

fn write_template_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "template file already exists at {} (use --force to overwrite)",
            path.display()
        );
    }

    let template = r#"move_target_dir = "D:\\archive"

paths = [
  "C:\\Windows\\Temp\\old-cache.tmp",
  "C:\\Users\\Public\\Downloads\\to-delete",
]
"#;

    fs::write(path, template)
        .with_context(|| format!("failed to write template config at {}", path.display()))?;
    Ok(())
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .without_time()
        .try_init();
}

fn parse_threads(input: &str) -> std::result::Result<usize, String> {
    let value = input
        .parse::<usize>()
        .map_err(|_| "threads must be a positive integer".to_string())?;
    if value == 0 {
        return Err("threads must be >= 1".to_string());
    }
    Ok(value)
}

fn run_move_link(
    cli: &Cli,
    sources: Vec<PathBuf>,
    target_dir_override: Option<PathBuf>,
) -> Result<MoveOutcome> {
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
        match move_and_link_one_path(&source, &target_dir, cli.dry_run) {
            Ok(destination) => {
                succeeded += 1;
                if cli.dry_run {
                    info!(
                        source = %source.display(),
                        destination = %destination.display(),
                        "dry-run: would move and create symlink"
                    );
                } else {
                    info!(
                        source = %source.display(),
                        destination = %destination.display(),
                        "moved and linked"
                    );
                }
            }
            Err(err) => {
                let destination = build_destination_path(&source, &target_dir).ok();
                warn!(
                    source = %source.display(),
                    destination = ?destination.as_ref().map(|p| p.display().to_string()),
                    error = %err,
                    "move-link failed, skipping"
                );
                failures.push(MoveFailureEntry {
                    source: source.clone(),
                    destination,
                    error: err.to_string(),
                });
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

fn resolve_move_target_dir(config_path: &Path, override_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        if dir.as_os_str().is_empty() {
            bail!("target directory argument cannot be empty");
        }
        return Ok(dir);
    }

    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config at {}", config_path.display()))?;
    let config: MoveConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse TOML config at {}", config_path.display()))?;

    match config.move_target_dir {
        Some(dir) if !dir.as_os_str().is_empty() => Ok(dir),
        _ => Err(anyhow!(
            "move target directory is not set; pass --target-dir or add move_target_dir to config"
        )),
    }
}

fn move_and_link_one_path(source: &Path, target_dir: &Path, dry_run: bool) -> Result<PathBuf> {
    let metadata = fs::metadata(source)
        .with_context(|| format!("failed to read source metadata: {}", source.display()))?;
    let kind = if metadata.is_dir() {
        SourceKind::Directory
    } else {
        SourceKind::File
    };

    let destination = build_destination_path(source, target_dir)?;
    if source == destination {
        bail!(
            "source and destination are the same path: {}",
            source.display()
        );
    }
    if destination.exists() {
        bail!(
            "destination already exists, refusing to overwrite: {}",
            destination.display()
        );
    }

    if dry_run {
        return Ok(destination);
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create destination parent directory {}",
                parent.display()
            )
        })?;
    }

    move_path_with_fallback(source, &destination, kind).with_context(|| {
        format!(
            "failed to move {} from {} to {}",
            kind_label(kind),
            source.display(),
            destination.display()
        )
    })?;

    let link_target = fs::canonicalize(&destination).unwrap_or_else(|_| destination.clone());
    if let Err(link_err) = create_symlink_for_kind(kind, &link_target, source) {
        let rollback = move_path_with_fallback(&destination, source, kind);
        return match rollback {
            Ok(()) => Err(anyhow!(
                "failed to create symlink at {} -> {}: {}",
                source.display(),
                link_target.display(),
                link_err
            )),
            Err(rollback_err) => Err(anyhow!(
                "failed to create symlink at {} -> {}: {}; rollback failed: {}",
                source.display(),
                link_target.display(),
                link_err,
                rollback_err
            )),
        };
    }

    Ok(destination)
}

fn build_destination_path(source: &Path, target_dir: &Path) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("source path has no file name: {}", source.display()))?;
    Ok(target_dir.join(file_name))
}

fn move_path_with_fallback(source: &Path, destination: &Path, kind: SourceKind) -> io::Result<()> {
    match kind {
        SourceKind::File => move_file_with_fallback(source, destination),
        SourceKind::Directory => move_directory_with_fallback(source, destination),
    }
}

fn move_file_with_fallback(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device_error(&err) => {
            fs::copy(source, destination)?;
            fs::remove_file(source)
        }
        Err(err) => Err(err),
    }
}

fn move_directory_with_fallback(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device_error(&err) => {
            copy_dir_recursive(source, destination)?;
            fs::remove_dir_all(source)
        }
        Err(err) => Err(err),
    }
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        let file_type = metadata.file_type();

        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path)?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&source_path)?;
            create_symlink_like_source(&source_path, &link_target, &dest_path)?;
        } else {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                format!("unsupported entry type: {}", source_path.display()),
            ));
        }
    }

    Ok(())
}

fn kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::File => "file",
        SourceKind::Directory => "directory",
    }
}

fn is_cross_device_error(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(17))
}

fn create_symlink_for_kind(kind: SourceKind, target: &Path, link: &Path) -> io::Result<()> {
    match kind {
        SourceKind::File => create_file_symlink(target, link),
        SourceKind::Directory => create_dir_symlink(target, link),
    }
}

fn create_symlink_like_source(source: &Path, target: &Path, link: &Path) -> io::Result<()> {
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        source
            .parent()
            .map(|parent| parent.join(target))
            .unwrap_or_else(|| target.to_path_buf())
    };
    let kind = if resolved.is_dir() {
        SourceKind::Directory
    } else {
        SourceKind::File
    };
    create_symlink_for_kind(kind, target, link)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(any(windows, unix)))]
fn create_file_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "symbolic links are not supported on this platform",
    ))
}

#[cfg(windows)]
fn create_dir_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(unix)]
fn create_dir_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(any(windows, unix)))]
fn create_dir_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "symbolic links are not supported on this platform",
    ))
}

fn run(cli: Cli) -> Result<RunOutcome> {
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

fn resolve_target_paths(cli: &Cli) -> Result<Vec<PathBuf>> {
    if !cli.paths.is_empty() {
        return Ok(cli.paths.clone());
    }
    let config = read_config(&cli.config)?;
    Ok(config.paths)
}

fn build_thread_pool(threads: Option<usize>) -> Result<rayon::ThreadPool> {
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

fn read_config(path: &Path) -> Result<CleanerConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let config: CleanerConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse TOML config at {}", path.display()))?;
    Ok(config)
}

fn delete_path(
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
    } else {
        if let Err(err) = delete_file(path, &metadata, repair_permissions) {
            issues.push(PathDeleteError::new(
                path.to_path_buf(),
                DeleteError::RemoveFile(err),
            ));
        }
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
        } else {
            if let Err(err) = delete_file(&child_path, &child_metadata, repair_permissions) {
                issues.push(PathDeleteError::new(
                    child_path.clone(),
                    DeleteError::RemoveFile(err),
                ));
            }
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

fn is_retriable_delete_error(err: &io::Error) -> bool {
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

fn print_summary(outcome: &RunOutcome, dry_run: bool) {
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

fn print_move_summary(outcome: &MoveOutcome, dry_run: bool) {
    if dry_run {
        println!("Mode: dry-run (nothing was moved)");
    } else {
        println!("Mode: move-link");
    }

    println!("Total: {}", outcome.total);
    println!("Succeeded: {}", outcome.succeeded);
    println!("Failed: {}", outcome.failed);

    if !outcome.failures.is_empty() {
        println!();
        println!("Move-link failures:");
        for failure in &outcome.failures {
            match &failure.destination {
                Some(destination) => println!(
                    "- source={} | destination={} | {}",
                    failure.source.display(),
                    destination.display(),
                    failure.error
                ),
                None => println!(
                    "- source={} | destination=unknown | {}",
                    failure.source.display(),
                    failure.error
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_toml_config() {
        let raw = r#"
paths = [
  "C:\\Temp\\foo.txt",
  "D:\\Cache\\bar"
]
"#;
        let parsed: CleanerConfig = toml::from_str(raw).expect("config should parse");
        assert_eq!(parsed.paths.len(), 2);
    }

    #[test]
    fn delete_file_works() {
        let dir = tempdir().expect("temp dir should be created");
        let file = dir.path().join("a.tmp");
        fs::write(&file, "data").expect("temp file should be written");

        let issues = delete_path(&file, false, false, DeleteStrategy::Native);
        assert!(issues.is_empty());
        assert!(!file.exists());
    }

    #[test]
    fn delete_directory_works() {
        let dir = tempdir().expect("temp dir should be created");
        let nested = dir.path().join("nested");
        let child = nested.join("child.txt");
        fs::create_dir_all(&nested).expect("nested dir should be created");
        fs::write(&child, "data").expect("child file should be written");

        let issues = delete_path(&nested, false, false, DeleteStrategy::Native);
        assert!(issues.is_empty());
        assert!(!nested.exists());
    }

    #[test]
    fn dry_run_does_not_delete() {
        let dir = tempdir().expect("temp dir should be created");
        let file = dir.path().join("stay.tmp");
        fs::write(&file, "data").expect("temp file should be written");

        let issues = delete_path(&file, true, false, DeleteStrategy::Native);
        assert!(issues.is_empty());
        assert!(file.exists());
    }

    #[test]
    fn delete_readonly_file_works() {
        let dir = tempdir().expect("temp dir should be created");
        let file = dir.path().join("readonly.tmp");
        fs::write(&file, "data").expect("temp file should be written");

        let mut permissions = fs::metadata(&file)
            .expect("metadata should be available")
            .permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).expect("readonly flag should be set");

        let issues = delete_path(&file, false, false, DeleteStrategy::Native);
        assert!(issues.is_empty());
        assert!(!file.exists());
    }

    #[test]
    fn missing_path_returns_missing_error() {
        let dir = tempdir().expect("temp dir should be created");
        let missing = dir.path().join("missing.tmp");

        let issues = delete_path(&missing, false, false, DeleteStrategy::Native);
        assert_eq!(issues.len(), 1);
        assert!(matches!(&issues[0].error, DeleteError::Missing));
    }

    #[test]
    fn zero_thread_count_is_rejected_by_clap() {
        let cli = Cli::try_parse_from(["windows-cleaner", "--threads", "0"]);
        assert!(cli.is_err());
    }

    #[test]
    fn parse_direct_paths_from_cli() {
        let cli = Cli::try_parse_from(["windows-cleaner", "--path", "C:\\a", "--path", "D:\\b\\c"])
            .expect("cli args should parse");
        assert_eq!(cli.paths.len(), 2);
    }

    #[test]
    fn parse_repair_permissions_flag() {
        let cli = Cli::try_parse_from(["windows-cleaner", "--repair-permissions"])
            .expect("cli args should parse");
        assert!(cli.repair_permissions);
    }

    #[test]
    fn parse_init_command() {
        let cli = Cli::try_parse_from(["windows-cleaner", "init"]).expect("cli args should parse");
        match cli.command {
            Some(AppCommand::Init { output, force }) => {
                assert_eq!(output, PathBuf::from("cleaner.toml"));
                assert!(!force);
            }
            _ => panic!("expected init command"),
        }
    }

    #[test]
    fn parse_move_link_command() {
        let cli = Cli::try_parse_from([
            "windows-cleaner",
            "move-link",
            "--source",
            "C:\\a.txt",
            "--source",
            "D:\\b.txt",
            "--target-dir",
            "E:\\dest",
        ])
        .expect("cli args should parse");
        match cli.command {
            Some(AppCommand::MoveLink {
                sources,
                target_dir,
            }) => {
                assert_eq!(sources.len(), 2);
                assert_eq!(target_dir, Some(PathBuf::from("E:\\dest")));
            }
            _ => panic!("expected move-link command"),
        }
    }

    #[test]
    fn parse_delete_strategy_flag() {
        let cli = Cli::try_parse_from(["windows-cleaner", "--delete-strategy", "recursive"])
            .expect("cli args should parse");
        assert_eq!(cli.delete_strategy, DeleteStrategy::Recursive);
    }

    #[test]
    fn empty_path_list_is_allowed() {
        let raw = "paths = []";
        let parsed: CleanerConfig = toml::from_str(raw).expect("empty config should parse");
        assert!(parsed.paths.is_empty());
    }

    #[test]
    fn missing_paths_field_is_rejected() {
        let raw = r#"
name = "invalid"
"#;
        let parsed: std::result::Result<CleanerConfig, _> = toml::from_str(raw);
        assert!(parsed.is_err());
    }

    #[test]
    fn directory_missing_returns_missing_error() {
        let dir = tempdir().expect("temp dir should be created");
        let missing = dir.path().join("missing-dir");
        let issues = delete_path(&missing, false, false, DeleteStrategy::Native);
        assert_eq!(issues.len(), 1);
        assert!(matches!(&issues[0].error, DeleteError::Missing));
    }

    #[test]
    fn retriable_delete_codes_are_detected() {
        assert!(is_retriable_delete_error(&io::Error::from_raw_os_error(5)));
        assert!(is_retriable_delete_error(&io::Error::from_raw_os_error(32)));
        assert!(!is_retriable_delete_error(&io::Error::from_raw_os_error(2)));
    }

    #[test]
    fn build_thread_pool_with_threads() {
        let pool = build_thread_pool(Some(2)).expect("thread pool should build");
        let sum = pool.install(|| (1..=4).into_par_iter().sum::<i32>());
        assert_eq!(sum, 10);
    }

    #[test]
    fn build_thread_pool_without_threads() {
        let pool = build_thread_pool(None).expect("thread pool should build");
        let sum = pool.install(|| (1..=3).into_par_iter().sum::<i32>());
        assert_eq!(sum, 6);
    }

    #[test]
    fn print_summary_handles_no_failures() {
        let outcome = RunOutcome {
            total: 1,
            succeeded: 1,
            failed: 0,
            failures: Vec::new(),
        };

        print_summary(&outcome, false);
    }

    #[test]
    fn print_summary_handles_failures() {
        let outcome = RunOutcome {
            total: 1,
            succeeded: 0,
            failed: 1,
            failures: vec![FailureEntry {
                path: PathBuf::from("C:\\\\bad"),
                failed_path: PathBuf::from("C:\\\\bad\\\\denied"),
                error: "failed".to_string(),
                os_code: Some(5),
                path_details:
                    "exists=true kind=dir readonly=false attrs=0x00000010 flags=DIRECTORY"
                        .to_string(),
            }],
        };

        print_summary(&outcome, true);
    }

    #[test]
    fn read_config_fails_for_missing_file() {
        let dir = tempdir().expect("temp dir should be created");
        let path = dir.path().join("missing.toml");
        let err = read_config(&path).expect_err("missing config should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("failed to read config"));
    }

    #[test]
    fn write_template_config_creates_file() {
        let dir = tempdir().expect("temp dir should be created");
        let path = dir.path().join("template.toml");
        write_template_config(&path, false).expect("template should be written");
        let content = fs::read_to_string(&path).expect("template should be readable");
        assert!(content.contains("paths = ["));
        assert!(content.contains("move_target_dir"));
    }

    #[test]
    fn resolve_move_target_dir_prefers_argument() {
        let dir = tempdir().expect("temp dir should be created");
        let config_path = dir.path().join("cleaner.toml");
        fs::write(&config_path, "move_target_dir = \"D:\\\\from-config\"")
            .expect("config should be written");
        let override_dir = PathBuf::from("D:\\override");
        let resolved = resolve_move_target_dir(&config_path, Some(override_dir.clone()))
            .expect("target dir should resolve");
        assert_eq!(resolved, override_dir);
    }

    #[test]
    fn resolve_move_target_dir_from_config() {
        let dir = tempdir().expect("temp dir should be created");
        let config_path = dir.path().join("cleaner.toml");
        fs::write(&config_path, "move_target_dir = \"D:\\\\from-config\"")
            .expect("config should be written");
        let resolved =
            resolve_move_target_dir(&config_path, None).expect("target dir should resolve");
        assert_eq!(resolved, PathBuf::from("D:\\from-config"));
    }

    #[test]
    fn move_and_link_one_path_dry_run_keeps_file_source() {
        let dir = tempdir().expect("temp dir should be created");
        let source = dir.path().join("a.txt");
        let target_dir = dir.path().join("dest");
        fs::write(&source, "data").expect("source file should be written");

        let destination = move_and_link_one_path(&source, &target_dir, true)
            .expect("dry run should calculate destination");
        assert!(source.exists());
        assert!(!destination.exists());
    }

    #[test]
    fn move_and_link_one_path_dry_run_keeps_directory_source() {
        let dir = tempdir().expect("temp dir should be created");
        let source = dir.path().join("folder");
        let target_dir = dir.path().join("dest");
        fs::create_dir_all(source.join("inner")).expect("source directory should be created");
        fs::write(source.join("inner").join("a.txt"), "data")
            .expect("source file should be written");

        let destination = move_and_link_one_path(&source, &target_dir, true)
            .expect("dry run should calculate destination");
        assert!(source.exists());
        assert!(!destination.exists());
    }

    #[test]
    fn run_move_link_dry_run_uses_config_target() {
        let dir = tempdir().expect("temp dir should be created");
        let source = dir.path().join("a.txt");
        let config_path = dir.path().join("cleaner.toml");
        fs::write(&source, "data").expect("source file should be written");
        fs::write(
            &config_path,
            "move_target_dir = \"D:\\\\archive\"\npaths = []\n",
        )
        .expect("config should be written");

        let cli = Cli {
            command: None,
            config: config_path,
            paths: Vec::new(),
            threads: Some(1),
            no_progress: true,
            dry_run: true,
            delete_strategy: DeleteStrategy::Native,
            repair_permissions: false,
        };
        let outcome =
            run_move_link(&cli, vec![source.clone()], None).expect("move-link should run");
        assert_eq!(outcome.total, 1);
        assert_eq!(outcome.succeeded, 1);
        assert_eq!(outcome.failed, 0);
        assert!(source.exists());
    }

    #[test]
    fn run_handles_empty_config() {
        let dir = tempdir().expect("temp dir should be created");
        let config_path = dir.path().join("cleaner.toml");
        fs::write(&config_path, "paths = []").expect("config should be written");

        let cli = Cli {
            command: None,
            config: config_path,
            paths: Vec::new(),
            threads: Some(1),
            no_progress: true,
            dry_run: false,
            delete_strategy: DeleteStrategy::Native,
            repair_permissions: false,
        };
        let outcome = run(cli).expect("run should succeed");
        assert_eq!(outcome.total, 0);
        assert_eq!(outcome.failed, 0);
    }

    #[test]
    fn run_records_failed_paths() {
        let dir = tempdir().expect("temp dir should be created");
        let existing = dir.path().join("exists.tmp");
        fs::write(&existing, "data").expect("temp file should be written");
        let missing = dir.path().join("missing.tmp");
        let config_path = dir.path().join("cleaner.toml");

        let config = format!(
            "paths = [\"{}\", \"{}\"]",
            existing.display().to_string().replace('\\', "\\\\"),
            missing.display().to_string().replace('\\', "\\\\")
        );
        fs::write(&config_path, config).expect("config should be written");

        let cli = Cli {
            command: None,
            config: config_path,
            paths: Vec::new(),
            threads: Some(1),
            no_progress: true,
            dry_run: false,
            delete_strategy: DeleteStrategy::Native,
            repair_permissions: false,
        };
        let outcome = run(cli).expect("run should succeed");
        assert_eq!(outcome.total, 2);
        assert_eq!(outcome.succeeded, 1);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].path, missing);
        assert_eq!(outcome.failures[0].failed_path, missing);
        assert!(!outcome.failures[0].path_details.is_empty());
    }

    #[test]
    fn run_with_direct_paths_ignores_missing_config() {
        let dir = tempdir().expect("temp dir should be created");
        let existing = dir.path().join("direct.tmp");
        fs::write(&existing, "data").expect("temp file should be written");
        let missing_config = dir.path().join("missing.toml");

        let cli = Cli {
            command: None,
            config: missing_config,
            paths: vec![existing.clone()],
            threads: Some(1),
            no_progress: true,
            dry_run: false,
            delete_strategy: DeleteStrategy::Native,
            repair_permissions: false,
        };
        let outcome = run(cli).expect("run should succeed");
        assert_eq!(outcome.total, 1);
        assert_eq!(outcome.succeeded, 1);
        assert_eq!(outcome.failed, 0);
        assert!(!existing.exists());
    }
}

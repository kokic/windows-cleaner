use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use tracing::{info, warn};

use crate::cli::Cli;
use crate::config::resolve_move_target_dir;

#[derive(Debug, Clone)]
pub(crate) struct MoveFailureEntry {
    pub(crate) source: PathBuf,
    pub(crate) destination: Option<PathBuf>,
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
enum SourceKind {
    File,
    Directory,
}

pub(crate) fn run_move_link(
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

pub(crate) fn move_and_link_one_path(
    source: &Path,
    target_dir: &Path,
    dry_run: bool,
) -> Result<PathBuf> {
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

pub(crate) fn build_destination_path(source: &Path, target_dir: &Path) -> Result<PathBuf> {
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

pub(crate) fn print_move_summary(outcome: &MoveOutcome, dry_run: bool) {
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

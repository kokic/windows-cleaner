use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "windows-cleaner",
    version,
    about = "Delete configured paths, or move files / directories and create symlinks back to original locations"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<AppCommand>,

    #[arg(
        short,
        long,
        value_name = "FILE",
        default_value = "cleaner.toml",
        help = "Path to TOML config file (used by delete mode and move-link fallback settings)"
    )]
    pub(crate) config: PathBuf,

    #[arg(
        short = 'p',
        long = "path",
        value_name = "PATH",
        help = "Delete mode only: delete this path directly; repeat to pass multiple paths. If set, config paths are ignored"
    )]
    pub(crate) paths: Vec<PathBuf>,

    #[arg(
        long,
        value_name = "N",
        value_parser = parse_threads,
        help = "Delete mode only: worker thread count. If omitted, Rayon uses its default strategy (typically based on available machine parallelism)"
    )]
    pub(crate) threads: Option<usize>,

    #[arg(
        long,
        help = "Delete mode only: disable progress bar and realtime counters"
    )]
    pub(crate) no_progress: bool,

    #[arg(long, help = "Simulate operations only; do not modify files")]
    pub(crate) dry_run: bool,

    #[arg(
        long,
        value_enum,
        default_value_t = DeleteStrategy::Native,
        help = "Delete mode only: directory strategy. native is faster (remove_dir_all); recursive is slower but reports exact skipped child paths"
    )]
    pub(crate) delete_strategy: DeleteStrategy,

    #[arg(
        long,
        help = "Delete mode only: attempt permission repair with takeown / icacls when access is denied"
    )]
    pub(crate) repair_permissions: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub(crate) enum AppCommand {
    #[command(visible_alias = "i")]
    Init {
        #[arg(
            short,
            long,
            value_name = "FILE",
            default_value = "cleaner.toml",
            help = "Output path for generated template TOML config"
        )]
        output: PathBuf,

        #[arg(long, help = "Overwrite file if it already exists")]
        force: bool,
    },

    #[command(visible_alias = "m")]
    MoveLink {
        #[arg(
            short = 's',
            long = "source",
            value_name = "PATH",
            required = true,
            help = "Source file or directory path. Repeat to move multiple entries"
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
pub(crate) enum DeleteStrategy {
    Native,
    Recursive,
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

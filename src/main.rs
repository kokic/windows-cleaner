mod cli;
mod config;
mod delete;
mod logging;
mod rename;
mod link;

use std::process;

use clap::Parser;

use crate::cli::{AppCommand, Cli};

#[cfg(test)]
pub(crate) use crate::cli::*;
#[cfg(test)]
pub(crate) use crate::config::*;
#[cfg(test)]
pub(crate) use crate::delete::*;
#[cfg(test)]
pub(crate) use crate::link::*;

fn main() {
    logging::init_tracing();

    let cli = Cli::parse();
    if let Some(command) = cli.command.clone() {
        match command {
            AppCommand::Init { output, force } => {
                match config::write_template_config(&output, force) {
                    Ok(()) => {
                        println!("Template config written: {}", output.display());
                        process::exit(0);
                    }
                    Err(err) => {
                        eprintln!("Fatal: {err:#}");
                        process::exit(2);
                    }
                }
            }
            AppCommand::MoveLink { source, target_dir } => {
                match link::run_move_link(&cli, vec![source], Some(target_dir)) {
                    Ok(outcome) => {
                        link::print_move_summary(&outcome, cli.dry_run);
                        if outcome.failed > 0 {
                            process::exit(1);
                        }
                        process::exit(0);
                    }
                    Err(err) => {
                        eprintln!("Fatal: {err:#}");
                        process::exit(2);
                    }
                }
            }
            AppCommand::Move {
                source,
                target_dir,
                mt,
            } => match rename::run_move(&cli, vec![source], Some(target_dir), mt) {
                Ok(outcome) => {
                    rename::print_move_summary(&outcome, cli.dry_run);
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

    match delete::run(cli) {
        Ok(outcome) => {
            delete::print_summary(&outcome, dry_run);
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
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io;
    use std::path::PathBuf;

    use rayon::prelude::*;
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
        let cli = Cli::try_parse_from(["windows-cleaner", "move-link", "C:\\a.txt", "E:\\dest"])
            .expect("cli args should parse");
        match cli.command {
            Some(AppCommand::MoveLink { source, target_dir }) => {
                assert_eq!(source, PathBuf::from("C:\\a.txt"));
                assert_eq!(target_dir, PathBuf::from("E:\\dest"));
            }
            _ => panic!("expected move-link command"),
        }
    }

    #[test]
    fn parse_move_command() {
        let cli = Cli::try_parse_from([
            "windows-cleaner",
            "move",
            "C:\\a.txt",
            "E:\\dest",
            "--mt",
            "16",
        ])
        .expect("cli args should parse");
        match cli.command {
            Some(AppCommand::Move {
                source,
                target_dir,
                mt,
            }) => {
                assert_eq!(source, PathBuf::from("C:\\a.txt"));
                assert_eq!(target_dir, PathBuf::from("E:\\dest"));
                assert_eq!(mt, Some(16));
            }
            _ => panic!("expected move command"),
        }
    }

    #[test]
    fn parse_move_link_alias_command() {
        let cli = Cli::try_parse_from(["windows-cleaner", "ml", "C:\\a.txt", "E:\\dest"])
            .expect("cli args should parse");
        assert!(matches!(cli.command, Some(AppCommand::MoveLink { .. })));
    }

    #[test]
    fn parse_move_alias_command() {
        let cli = Cli::try_parse_from([
            "windows-cleaner",
            "m",
            "C:\\a.txt",
            "E:\\dest",
            "--mt",
            "16",
        ])
        .expect("cli args should parse");
        assert!(matches!(cli.command, Some(AppCommand::Move { .. })));
    }

    #[test]
    fn old_move_link_flag_syntax_is_rejected() {
        let cli = Cli::try_parse_from([
            "windows-cleaner",
            "move-link",
            "--source",
            "C:\\a.txt",
            "--target-dir",
            "E:\\dest",
        ]);
        assert!(cli.is_err());
    }

    #[test]
    fn old_move_flag_syntax_is_rejected() {
        let cli = Cli::try_parse_from([
            "windows-cleaner",
            "move",
            "--source",
            "C:\\a.txt",
            "--target-dir",
            "E:\\dest",
        ]);
        assert!(cli.is_err());
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

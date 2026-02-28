use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::cli::Cli;

#[derive(Debug, Deserialize)]
pub(crate) struct CleanerConfig {
    pub(crate) paths: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct MoveConfig {
    #[serde(alias = "move-target-dir")]
    move_target_dir: Option<PathBuf>,
}

pub(crate) fn write_template_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "template file already exists at {} (use --force to overwrite)",
            path.display()
        );
    }

    let template = r#"# Used by `move-link` when --target-dir is not provided.
move_target_dir = "D:\\archive"

# Used by delete mode when --path is not provided.
paths = [
  "C:\\Windows\\Temp\\old-cache.tmp",
  "C:\\Users\\Public\\Downloads\\to-delete",
]
"#;

    fs::write(path, template)
        .with_context(|| format!("failed to write template config at {}", path.display()))?;
    Ok(())
}

pub(crate) fn read_config(path: &Path) -> Result<CleanerConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let config: CleanerConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse TOML config at {}", path.display()))?;
    Ok(config)
}

pub(crate) fn resolve_target_paths(cli: &Cli) -> Result<Vec<PathBuf>> {
    if !cli.paths.is_empty() {
        return Ok(cli.paths.clone());
    }
    let config = read_config(&cli.config)?;
    Ok(config.paths)
}

pub(crate) fn resolve_move_target_dir(
    config_path: &Path,
    override_dir: Option<PathBuf>,
) -> Result<PathBuf> {
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

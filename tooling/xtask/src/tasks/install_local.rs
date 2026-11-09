#![allow(clippy::disallowed_methods, reason = "tooling is exempt")]

use std::process::Command;

use anyhow::{Context as _, Result, bail};
use clap::Parser;

use crate::workspace::load_workspace;

#[derive(Parser)]
pub struct InstallLocalArgs {}

pub fn run_install_local(_args: InstallLocalArgs) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("install-local is only supported on Linux");
    }

    let workspace = load_workspace()?;
    let script = workspace.workspace_root.join("script/install-linux-local");
    let status = Command::new(&script)
        .current_dir(&workspace.workspace_root)
        .status()
        .with_context(|| format!("failed to run {script}"))?;
    if !status.success() {
        bail!("{script} failed with {status}");
    }
    Ok(())
}

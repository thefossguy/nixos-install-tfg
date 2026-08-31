use crate::command_helpers::{create_nix3_command, did_command_exit_successfully};
use crate::config::{NixOSInstallerConfig, evaluate_nixos_system_derivation_path};
use crate::{get_formatted_process_stderr, log_then_output, log_then_status};

use std::error::Error;
use std::process::Command;

use nanologger::info;

fn bootstrap_flake_store_path(
    nixos_installer_config: &NixOSInstallerConfig,
) -> Result<(), Box<dyn Error>> {
    info!("Bootstrapping the flake store path");

    let mut nix_copy_cmd = create_nix3_command();
    nix_copy_cmd.args([
        "copy",
        "--to",
        &nixos_installer_config.mount_path,
        &nixos_installer_config.flake_store_path,
    ]);
    let nix_copy_cmd_process_result = log_then_output!(nix_copy_cmd);
    if did_command_exit_successfully(&nix_copy_cmd_process_result) {
        let mut nix_flake_archive_cmd = create_nix3_command();
        nix_flake_archive_cmd.args([
            "--store",
            &nixos_installer_config.mount_path,
            "flake",
            "archive",
            format!(
                "{}{}",
                nixos_installer_config.mount_path, nixos_installer_config.flake_store_path
            )
            .as_str(),
        ]);
        match log_then_status!(nix_flake_archive_cmd) {
            Err(e) => {
                Err(format!("The `nix flake archive` command to bootstrap NixOS failed{e}").into())
            }
            Ok(exit_status) => {
                if exit_status.success() {
                    Ok(())
                } else {
                    Err("The `nix flake archive` command to bootstrap NixOS failed".into())
                }
            }
        }
    } else {
        let nix_copy_cmd_process_output = nix_copy_cmd_process_result?;
        Err(format!(
            "Could not bootstrap the flake store path{}",
            get_formatted_process_stderr!(nix_copy_cmd_process_output)
        )
        .into())
    }
}

fn bootstrap_nixos(nixos_installer_config: &NixOSInstallerConfig) -> Result<(), Box<dyn Error>> {
    info!("Bootstrapping NixOS");

    let mut nix_build_cmd = create_nix3_command();
    nix_build_cmd.args([
        "build",
        "--store",
        &nixos_installer_config.mount_path,
        "--keep-going",
        "--cores",
        "1",
    ]);
    if nixos_installer_config.substitute_only {
        nix_build_cmd.args([
            "--max-jobs",
            "0",
            &nixos_installer_config.nixos_system_out_path,
        ]);
    } else {
        nix_build_cmd.args([
            "--max-jobs",
            "1",
            format!("{}^*", nixos_installer_config.nixos_system_derivation_path).as_str(),
        ]);
    }
    match log_then_status!(nix_build_cmd) {
        Err(e) => Err(format!("The `nix build` command to bootstrap NixOS failed{e}").into()),
        Ok(exit_status) => {
            if exit_status.success() {
                Ok(())
            } else {
                Err("The `nix build` command to bootstrap NixOS failed".into())
            }
        }
    }
}

fn do_nixos_install(nixos_installer_config: &NixOSInstallerConfig) -> Result<(), Box<dyn Error>> {
    info!("Installing NixOS");

    let mut nixos_install_cmd = Command::new("nixos-install");
    nixos_install_cmd.args([
        "--show-trace",
        "--max-jobs",
        "1",
        "--cores",
        "1",
        "--no-root-password",
        "--no-channel-copy",
        "--root",
        &nixos_installer_config.mount_path,
        "--store-path",
        &nixos_installer_config.nixos_system_out_path,
    ]);
    let nixos_install_cmd_process_result = log_then_output!(nixos_install_cmd);
    if did_command_exit_successfully(&nixos_install_cmd_process_result) {
        Ok(())
    } else {
        let nixos_install_cmd_process_output = nixos_install_cmd_process_result?;
        Err(format!(
            "Could not Install NixOS{}",
            get_formatted_process_stderr!(nixos_install_cmd_process_output)
        )
        .into())
    }
}

pub fn install_nixos(nixos_installer_config: &NixOSInstallerConfig) -> Result<(), Box<dyn Error>> {
    if !nixos_installer_config.substitute_only {
        // prepare the `$MOUNT_PATH/nix/store` for a non-substitution bootstrap of NixOS
        bootstrap_flake_store_path(nixos_installer_config)?;
        let new_nixos_system_derivation_path = evaluate_nixos_system_derivation_path(
            &nixos_installer_config.mount_path,
            &nixos_installer_config.flake_store_path,
            &nixos_installer_config.hostname,
        )?;
        if new_nixos_system_derivation_path != nixos_installer_config.nixos_system_derivation_path {
            return Err(format!("The newly computed NixOS system derivation path '{}' is not the same as the old one '{}'", new_nixos_system_derivation_path, nixos_installer_config.nixos_system_derivation_path).into());
        }
    }

    bootstrap_nixos(nixos_installer_config)?;
    do_nixos_install(nixos_installer_config)?;
    crate::disk_helpers::unmount_everything(&nixos_installer_config.canonical_target_drive)?;
    Ok(())
}

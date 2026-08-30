use crate::command_helpers::{
    create_nix_instantiate_command, create_nix3_command, did_command_exit_successfully,
};
use crate::disk_helpers::{NixOSFileSystemMountPath, SupportedFileSystems, sort_paths};
use crate::{
    get_formatted_process_stderr, get_process_stdout, log_then_output, make_formatted_error,
};

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use nanologger::error;

const NIX_CURRENT_SYSTEM: &str = if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "aarch64-linux"
} else if cfg!(all(target_os = "linux", target_arch = "riscv64")) {
    "riscv64-linux"
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "x86_64-linux"
} else {
    panic!("Unsupported target.")
};

#[derive(Debug)]
struct MandatoryArgsWrapped {
    hostname: Option<String>,
    canonical_target_drive: Option<String>,
    partition_drive: Option<bool>,
    format_partitions: Option<bool>,
    substitute_only: Option<bool>,
    update_lockfile: bool,
    flake_path: Option<String>,
    create_flake_store_path_gc_root: bool,
}
#[derive(Debug)]
struct MandatoryArgsUnwrapped {
    hostname: String,
    canonical_target_drive: String,
    partition_drive: bool,
    format_partitions: bool,
    substitute_only: bool,
    flake_store_path: String,
}
#[derive(Debug)]
struct VerifiedArgs {
    hostname: String,
    canonical_target_drive: String,
    partition_drive: bool,
    format_partitions: bool,
    substitute_only: bool,
    flake_store_path: String,
    mount_path: String,
    target_drive_by_id: String,
}

#[derive(Debug)]
pub struct NixOSInstallerConfig {
    // gets accepted from the CLI
    pub hostname: String,
    pub canonical_target_drive: String,
    pub mount_path: String,
    pub partition_drive: bool,
    pub format_partitions: bool,
    pub substitute_only: bool,

    /*
     * LUKS
    pub with_luks: bool,
    pub luks_device_label: String,
    pub luks_device_challenge_hex: String,
    pub luks_device_uuid: String,
    */
    // gets evaluated at runtime
    pub target_drive_by_id: String,
    pub flake_store_path: String,
    pub efi_part_uuid: String,
    pub efi_part_mount_opts: String,
    pub root_part_uuid: String,
    pub root_part_mount_opts: String,
    pub root_part_fs_type: SupportedFileSystems,
    pub non_efi_mount_paths: Vec<String>,
    pub nixos_system_derivation_path: String,
    pub nixos_system_out_path: String,
}

fn user_check() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata("/proc/self") {
        Ok(proc_self_metadata) => {
            let uid = proc_self_metadata.uid();
            if uid == 0 {
                Ok(())
            } else {
                Err("The installer needs superuser privileges".into())
            }
        },
        Err(e) => Err(format!("Could not determine the metadata of `/proc/self` to determine the UID of user running the process{}", make_formatted_error!(e)).into()),
    }
}

fn perform_git_pull(flake_path: &str) -> Result<(), Box<dyn Error>> {
    let mut git_pull_cmd = Command::new("git");
    git_pull_cmd.args(["-C", flake_path, "pull", "--ff-only"]);
    let git_pull_cmd_process_result = log_then_output!(git_pull_cmd);
    if did_command_exit_successfully(&git_pull_cmd_process_result) {
        Ok(())
    } else {
        let git_pull_cmd_process_output = git_pull_cmd_process_result?;
        Err(format!(
            "The `git pull` command failed{}",
            get_formatted_process_stderr!(git_pull_cmd_process_output)
        )
        .into())
    }
}

fn do_lockfile_update(flake_path: &str, update_lockfile: bool) -> Result<(), Box<dyn Error>> {
    if update_lockfile {
        let mut nix_flake_update_cmd = create_nix3_command();
        nix_flake_update_cmd.args(["flake", "update", "--flake", flake_path]);
        let nix_flake_update_cmd_process_result = log_then_output!(nix_flake_update_cmd);
        if did_command_exit_successfully(&nix_flake_update_cmd_process_result) {
            Ok(())
        } else {
            let nix_flake_update_cmd_process_output = nix_flake_update_cmd_process_result?;
            Err(format!(
                "The `nix flake update` command failed{}",
                get_formatted_process_stderr!(nix_flake_update_cmd_process_output)
            )
            .into())
        }
    } else {
        Ok(())
    }
}

fn get_flake_store_path(
    flake_path: &str,
    update_lockfile: bool,
    flake_store_path_gc_root: bool,
) -> Result<String, Box<dyn Error>> {
    perform_git_pull(flake_path)?;
    do_lockfile_update(flake_path, update_lockfile)?;

    let mut nix_flake_archive_cmd = create_nix3_command();
    nix_flake_archive_cmd.args(["flake", "archive", "--json", flake_path]);
    let nix_flake_archive_cmd_process_result = log_then_output!(nix_flake_archive_cmd);
    if did_command_exit_successfully(&nix_flake_archive_cmd_process_result) {
        let nix_flake_archive_cmd_process_output = nix_flake_archive_cmd_process_result?;
        let nix_flake_archive_cmd_process_stdout =
            get_process_stdout!(nix_flake_archive_cmd_process_output);
        match nojson::RawJson::parse(&nix_flake_archive_cmd_process_stdout) {
            Err(_) => Err(format!(
                "Could not parse the archived flake's output{}",
                get_formatted_process_stderr!(nix_flake_archive_cmd_process_output)
            )
            .into()),
            Ok(flake_metadata) => {
                let flake_store_path: String = flake_metadata
                    .value()
                    .to_member("path")?
                    .required()?
                    .try_into()?;

                if flake_store_path.is_empty() {
                    Err("Could not determine the flake store path".into())
                } else if flake_store_path_gc_root {
                    let tmpdir = std::env::var("TMPDIR").unwrap_or("/tmp".to_string());
                    let our_tmpdir = format!("{tmpdir}/nixos-install-tfg");
                    fs::create_dir_all(&our_tmpdir)?;

                    let mut nix_build_cmd = create_nix3_command();
                    nix_build_cmd.args([
                        "build",
                        "--out-link",
                        format!("{our_tmpdir}/result-flake-store-path").as_str(),
                        &flake_store_path,
                    ]);
                    let nix_build_cmd_process_result = log_then_output!(nix_build_cmd);
                    if did_command_exit_successfully(&nix_build_cmd_process_result) {
                        Ok(flake_store_path)
                    } else {
                        let nix_build_cmd_process_output = nix_build_cmd_process_result?;
                        Err(format!("The `nix build` command to create a GC root for the flake store path failed{}", get_formatted_process_stderr!(nix_build_cmd_process_output)).into())
                    }
                } else {
                    Ok(flake_store_path)
                }
            }
        }
    } else {
        let nix_flake_archive_cmd_process_output = nix_flake_archive_cmd_process_result?;
        Err(format!(
            "The `nix flake archive` command failed{}",
            get_formatted_process_stderr!(nix_flake_archive_cmd_process_output)
        )
        .into())
    }
}

fn enforce_mandatory_args(
    current_args: MandatoryArgsWrapped,
) -> Result<MandatoryArgsUnwrapped, Box<dyn Error>> {
    let mut error_out = false;

    let hostname = if let Some(hostname) = current_args.hostname {
        hostname
    } else {
        error_out = true;
        error!("`--hostname` must be specified");
        String::new()
    };

    let canonical_target_drive =
        if let Some(canonical_target_drive) = current_args.canonical_target_drive {
            canonical_target_drive
        } else {
            error_out = true;
            error!("`--target-drive` must be specified");
            String::new()
        };

    let format_partitions = if let Some(format_partitions) = current_args.format_partitions {
        format_partitions
    } else {
        error_out = true;
        error!("`--format-partitions` or `--no-format-partitions` must be specified");
        false
    };

    let partition_drive = if let Some(partition_drive) = current_args.partition_drive {
        match (partition_drive, format_partitions) {
            (true, false) => {
                error_out = true;
                error!("`--no-format-partitions` and `--partition-drive` are mutually exclusive");
                false
            }
            _ => partition_drive,
        }
    } else {
        error_out = true;
        error!("`--partition-drive` or `--no-partition-drive` must be specified");
        false
    };

    let substitute_only = if let Some(substitute_only) = current_args.substitute_only {
        substitute_only
    } else {
        error_out = true;
        error!("`--substitute-only` or `--do-build` must be specified");
        false
    };

    let flake_path = if let Some(flake_path) = current_args.flake_path {
        flake_path
    } else {
        error_out = true;
        error!("`--flake-path` must be specified");
        String::new()
    };

    if error_out {
        Err("Please look at the errors above".into())
    } else {
        let flake_store_path = get_flake_store_path(
            &flake_path,
            current_args.update_lockfile,
            current_args.create_flake_store_path_gc_root,
        )?;
        Ok(MandatoryArgsUnwrapped {
            hostname,
            canonical_target_drive,
            partition_drive,
            format_partitions,
            substitute_only,
            flake_store_path,
        })
    }
}

fn verify_hostname(hostname: &str, flake_store_path: &str) -> Result<(), Box<dyn Error>> {
    let mut nix_instantiate_cmd = create_nix_instantiate_command();
    nix_instantiate_cmd
        .args([
            "--eval",
            "--expr",
            format!("builtins.elem \"{hostname}\" (builtins.attrNames (builtins.getFlake \"{flake_store_path}\").outputs.nixosConfigurations)").as_str(),
        ]);
    let nix_instantiate_cmd_process_result = log_then_output!(nix_instantiate_cmd);
    if did_command_exit_successfully(&nix_instantiate_cmd_process_result) {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        let nix_instantiate_cmd_process_stdout =
            get_process_stdout!(nix_instantiate_cmd_process_output);
        let nix_instantiate_cmd_process_formatted_stderr =
            get_formatted_process_stderr!(nix_instantiate_cmd_process_output);
        match nix_instantiate_cmd_process_stdout.as_str() {
            "true" => {
                let mut nix_instantiate_cmd = create_nix_instantiate_command();
                nix_instantiate_cmd
                        .args([
                            "--eval",
                            "--expr",
                            format!("(builtins.getFlake \"{flake_store_path}\").outputs.nixosConfigurations.{hostname}.pkgs.stdenv.hostPlatform.system == \"{NIX_CURRENT_SYSTEM}\"").as_str(),
                        ]);
                let nix_instantiate_cmd_process_result = log_then_output!(nix_instantiate_cmd);
                if did_command_exit_successfully(&nix_instantiate_cmd_process_result) {
                    let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
                    let nixos_config_system =
                        get_process_stdout!(nix_instantiate_cmd_process_output);
                    let nix_instantiate_cmd_process_formatted_stderr =
                        get_formatted_process_stderr!(nix_instantiate_cmd_process_output);
                    match nixos_config_system.as_str() {
                                "true" => Ok(()),
                                "false" => Err("The target NixOS configuration's Nix system is different than your host's system".into()),
                                _ => Err(format!("Invalid result: '{nixos_config_system}'{nix_instantiate_cmd_process_formatted_stderr}").into()),
                            }
                } else {
                    let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
                    Err(format!("The `nix-instantiate` command to determine the NixOS configuration's hostPlatform failed{}", get_formatted_process_stderr!(nix_instantiate_cmd_process_output)).into())
                }
            }
            "false" => {
                Err("There is no NixOS configuration for your hostname in your flake".into())
            }
            _ => Err(format!(
                "Invalid result: '{nix_instantiate_cmd_process_stdout}'{nix_instantiate_cmd_process_formatted_stderr}"
            )
            .into()),
        }
    } else {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        Err(format!("The `nix-instantiate` command to check whether the specified hostname exists in your Nix flake, failed{}", get_formatted_process_stderr!(nix_instantiate_cmd_process_output)).into())
    }
}

fn verify_target_drive(canonical_target_drive: &str) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::FileTypeExt;
    match fs::canonicalize(canonical_target_drive) {
        Ok(canonical_target_drive_path) => {
            if canonical_target_drive_path.exists() {
                match fs::metadata(&canonical_target_drive_path) {
                    Err(e) => Err(format!(
                        "Could not determine the metadata for target drive{}",
                        make_formatted_error!(e)
                    )
                    .into()),
                    Ok(canonical_target_drive_path_metadata) => {
                        if canonical_target_drive_path_metadata
                            .file_type()
                            .is_block_device()
                        {
                            if let Some(canonical_target_drive_path_as_str) =
                                canonical_target_drive_path.to_str()
                            {
                                if canonical_target_drive == canonical_target_drive_path_as_str {
                                    Ok(())
                                } else {
                                    Err(format!(
                                        "The real path of the target drive is '{}', not '{}'",
                                        canonical_target_drive_path.display(),
                                        canonical_target_drive
                                    )
                                    .into())
                                }
                            } else {
                                Err(format!("Could not convert the value of `PathBuf` type from '{}' to the `&str` type", canonical_target_drive_path.display()).into())
                            }
                        } else {
                            Err("The target drive is not a block device".into())
                        }
                    }
                }
            } else {
                Err("The target drive is not even a valid path".into())
            }
        }
        Err(e) => Err(format!(
            "Could not canonicalize '{}'{}",
            canonical_target_drive,
            make_formatted_error!(e)
        )
        .into()),
    }
}

fn get_target_drive_by_id(canonical_target_drive: &str) -> Result<String, Box<dyn Error>> {
    let canonical_target_drive_path = Path::new(canonical_target_drive);

    fs::read_dir("/dev/disk/by-id")?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .find_map(|path| {
            fs::canonicalize(&path)
                .ok()
                .filter(|candidate| candidate == canonical_target_drive_path)
                .map(|_| path.display().to_string())
        })
        .ok_or_else(|| "Could not determine the by-id path of the target drive".into())
}

fn verify_args(
    current_args: MandatoryArgsUnwrapped,
    mount_path: String,
) -> Result<VerifiedArgs, Box<dyn Error>> {
    verify_hostname(&current_args.hostname, &current_args.flake_store_path)?;
    verify_target_drive(&current_args.canonical_target_drive)?;
    let target_drive_by_id = get_target_drive_by_id(&current_args.canonical_target_drive)?;

    Ok(VerifiedArgs {
        hostname: current_args.hostname,
        canonical_target_drive: current_args.canonical_target_drive,
        partition_drive: current_args.partition_drive,
        format_partitions: current_args.format_partitions,
        substitute_only: current_args.substitute_only,
        flake_store_path: current_args.flake_store_path,
        mount_path,
        target_drive_by_id,
    })
}

fn evaluate_partition_uuid(
    flake_store_path: &str,
    hostname: &str,
    nixos_file_system_mount_path: &NixOSFileSystemMountPath,
) -> Result<String, Box<dyn Error>> {
    let mut nix_instantiate_cmd = create_nix_instantiate_command();
    nix_instantiate_cmd
        .args([
            "--eval",
            "--raw",
            "--expr",
            format!("(builtins.getFlake \"{flake_store_path}\").outputs.nixosConfigurations.{hostname}.config.fileSystems.\"{nixos_file_system_mount_path}\".device").as_str(),
        ]);
    let nix_instantiate_cmd_process_result = log_then_output!(nix_instantiate_cmd);
    if did_command_exit_successfully(&nix_instantiate_cmd_process_result) {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        let partition_by_uuid = get_process_stdout!(nix_instantiate_cmd_process_output);
        if partition_by_uuid.is_empty() {
            Err(format!(
                "Could not determine the device path for the {nixos_file_system_mount_path:?} partition",
            )
            .into())
        } else {
            match partition_by_uuid.rsplit('/').next() {
                Some(partition_uuid) => Ok(partition_uuid.to_string()),
                None => Err(format!(
                    "Could not get the {nixos_file_system_mount_path:?} partition's UUID from '{partition_by_uuid}'"
                )
                .into()),
            }
        }
    } else {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        Err(format!(
            "The `nix-instantiate` command to get the {:?} partition device failed{}",
            nixos_file_system_mount_path,
            get_formatted_process_stderr!(nix_instantiate_cmd_process_output)
        )
        .into())
    }
}

fn evaluate_partition_mount_options(
    flake_store_path: &str,
    hostname: &str,
    nixos_file_system_mount_path: &NixOSFileSystemMountPath,
) -> Result<String, Box<dyn Error>> {
    let mut nix_instantiate_cmd = create_nix_instantiate_command();
    nix_instantiate_cmd
        .args([
            "--eval",
            "--raw",
            "--expr",
            format!("let
            startsWith = stringPrefix: stringToMatch: builtins.substring 0 (builtins.stringLength stringPrefix) stringToMatch == stringPrefix;
            fsOptions = (builtins.getFlake \"{flake_store_path}\").outputs.nixosConfigurations.{hostname}.config.fileSystems.\"{nixos_file_system_mount_path}\".options;
            filteredFsOptions = builtins.filter (fsOption: !(startsWith \"subvol=\" fsOption)) fsOptions;
            in builtins.concatStringsSep \",\" filteredFsOptions").as_str(),
        ]);
    let nix_instantiate_cmd_process_result = log_then_output!(nix_instantiate_cmd);
    if did_command_exit_successfully(&nix_instantiate_cmd_process_result) {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        let mount_options = get_process_stdout!(nix_instantiate_cmd_process_output);
        if mount_options.is_empty() {
            Err(format!(
                "Could not determine the mount options for the {nixos_file_system_mount_path:?} partition",

            )
            .into())
        } else {
            Ok(mount_options)
        }
    } else {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        Err(format!(
            "The `nix-instantiate` command to get the {:?} partition's mount options failed{}",
            nixos_file_system_mount_path,
            get_formatted_process_stderr!(nix_instantiate_cmd_process_output)
        )
        .into())
    }
}

fn evaluate_root_part_fs_type(
    flake_store_path: &str,
    hostname: &str,
) -> Result<SupportedFileSystems, Box<dyn Error>> {
    let mut nix_instantiate_cmd = create_nix_instantiate_command();
    nix_instantiate_cmd
        .args([
            "--eval",
            "--raw",
            "--expr",
            format!("(builtins.getFlake \"{flake_store_path}\").outputs.nixosConfigurations.{hostname}.config.fileSystems.\"/\".fsType").as_str(),
        ]);
    let nix_instantiate_cmd_process_result = log_then_output!(nix_instantiate_cmd);
    if did_command_exit_successfully(&nix_instantiate_cmd_process_result) {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        let root_part_fs_type = get_process_stdout!(nix_instantiate_cmd_process_output);
        match root_part_fs_type.as_str() {
            "btrfs" => Ok(SupportedFileSystems::Btrfs),
            "zfs" => Ok(SupportedFileSystems::Zfs),
            _ => Err(format!("The filesystem '{root_part_fs_type}' is unsupported").into()),
        }
    } else {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        Err(format!(
            "The `nix-instantiate` command to get the Root partition's filesystem failed{}",
            get_formatted_process_stderr!(nix_instantiate_cmd_process_output)
        )
        .into())
    }
}

fn evaluate_non_efi_mount_paths(
    flake_store_path: &str,
    hostname: &str,
    root_part_fs_type: &SupportedFileSystems,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut nix_instantiate_cmd = create_nix_instantiate_command();
    nix_instantiate_cmd
        .args([
            "--eval",
            "--raw",
            "--expr",
            format!("let
            config = (builtins.getFlake \"{flake_store_path}\").outputs.nixosConfigurations.{hostname}.config;
            filteredFileSystems = builtins.filter (fs: config.fileSystems.${{fs}}.fsType == \"{root_part_fs_type:?}\") (builtins.attrNames config.fileSystems);
            in builtins.concatStringsSep \",\" filteredFileSystems").as_str(),
        ]);
    let nix_instantiate_cmd_process_result = log_then_output!(nix_instantiate_cmd);
    if did_command_exit_successfully(&nix_instantiate_cmd_process_result) {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        let non_efi_mount_paths = get_process_stdout!(nix_instantiate_cmd_process_output);
        if non_efi_mount_paths.is_empty() {
            Err("Could not determine the non-EFI partitions".into())
        } else {
            // sort_paths
            let mut non_efi_mount_paths: Vec<String> = non_efi_mount_paths
                .split(',')
                .map(ToString::to_string)
                .collect();
            sort_paths(&mut non_efi_mount_paths, false);
            Ok(non_efi_mount_paths)
        }
    } else {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        Err(format!(
            "The `nix-instantiate` command to get the non-EFI mount paths failed{}",
            get_formatted_process_stderr!(nix_instantiate_cmd_process_output)
        )
        .into())
    }
}

pub fn evaluate_nixos_system_derivation_path(
    eval_nix_store: &str,
    flake_store_path: &str,
    hostname: &str,
) -> Result<String, Box<dyn Error>> {
    let mut nix_instantiate_cmd = create_nix_instantiate_command();
    nix_instantiate_cmd
        .args([
            "--eval-store", eval_nix_store,
            "--expr",
            format!("(builtins.getFlake \"{flake_store_path}\").outputs.nixosConfigurations.{hostname}.config.system.build.toplevel").as_str(),
        ]);
    let nix_instantiate_cmd_process_result = log_then_output!(nix_instantiate_cmd);
    if did_command_exit_successfully(&nix_instantiate_cmd_process_result) {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        let nixos_system_derivation_path = get_process_stdout!(nix_instantiate_cmd_process_output);
        nanologger::info!(
            "nixos_system_derivation_path: {}",
            nixos_system_derivation_path
        );
        if nixos_system_derivation_path.is_empty() {
            Err("Could not determine the derivation path for your NixOS configuration".into())
        } else {
            Ok(nixos_system_derivation_path)
        }
    } else {
        let nix_instantiate_cmd_process_output = nix_instantiate_cmd_process_result?;
        Err(format!("The `nix-instantiate` command to get the NixOS configuration's derivation path, failed{}", get_formatted_process_stderr!(nix_instantiate_cmd_process_output)).into())
    }
}

fn evaluate_nixos_system_out_path(
    nixos_system_derivation_path: &str,
) -> Result<String, Box<dyn Error>> {
    let mut nix_store_cmd = Command::new("nix-store");
    nix_store_cmd.args(["--query", "--outputs", nixos_system_derivation_path]);
    let nix_store_cmd_process_result = log_then_output!(nix_store_cmd);
    if did_command_exit_successfully(&nix_store_cmd_process_result) {
        let nix_store_cmd_process_output = nix_store_cmd_process_result?;
        let nixos_system_out_paths = get_process_stdout!(nix_store_cmd_process_output);
        let nix_store_cmd_process_formatted_stderr =
            get_formatted_process_stderr!(nix_store_cmd_process_output);
        if nixos_system_out_paths.is_empty() {
            Err(format!(
                "Could not determine the output path of derivation '{nixos_system_derivation_path}'{nix_store_cmd_process_formatted_stderr}",
            )
            .into())
        } else {
            match nixos_system_out_paths.lines().next() {
                None => Err(format!(
                    "Could not determine the first line in output '{nixos_system_out_paths}'",
                )
                .into()),
                Some(nixos_system_out_path) => Ok(nixos_system_out_path.to_string()),
            }
        }
    } else {
        let nix_store_cmd_process_output = nix_store_cmd_process_result?;
        Err(format!(
            "Could not determine the output path of derivation '{nixos_system_derivation_path}'{}",
            get_formatted_process_stderr!(nix_store_cmd_process_output)
        )
        .into())
    }
}

fn make_nixos_installer_config(
    verified_args: VerifiedArgs,
) -> Result<NixOSInstallerConfig, Box<dyn Error>> {
    let efi_part_uuid = evaluate_partition_uuid(
        &verified_args.flake_store_path,
        &verified_args.hostname,
        &NixOSFileSystemMountPath::Efi,
    )?;
    let efi_part_mount_opts = evaluate_partition_mount_options(
        &verified_args.flake_store_path,
        &verified_args.hostname,
        &NixOSFileSystemMountPath::Efi,
    )?;
    let root_part_uuid = evaluate_partition_uuid(
        &verified_args.flake_store_path,
        &verified_args.hostname,
        &NixOSFileSystemMountPath::Root,
    )?;
    let root_part_mount_opts = evaluate_partition_mount_options(
        &verified_args.flake_store_path,
        &verified_args.hostname,
        &NixOSFileSystemMountPath::Root,
    )?;
    let root_part_fs_type =
        evaluate_root_part_fs_type(&verified_args.flake_store_path, &verified_args.hostname)?;
    let non_efi_mount_paths = evaluate_non_efi_mount_paths(
        &verified_args.flake_store_path,
        &verified_args.hostname,
        &root_part_fs_type,
    )?;
    let nixos_system_derivation_path = evaluate_nixos_system_derivation_path(
        "local:///",
        &verified_args.flake_store_path,
        &verified_args.hostname,
    )?;
    let nixos_system_out_path = evaluate_nixos_system_out_path(&nixos_system_derivation_path)?;

    Ok(NixOSInstallerConfig {
        hostname: verified_args.hostname,
        canonical_target_drive: verified_args.canonical_target_drive,
        mount_path: verified_args.mount_path,
        partition_drive: verified_args.partition_drive,
        format_partitions: verified_args.format_partitions,
        substitute_only: verified_args.substitute_only,

        target_drive_by_id: verified_args.target_drive_by_id,
        flake_store_path: verified_args.flake_store_path,
        efi_part_uuid,
        efi_part_mount_opts,
        root_part_uuid,
        root_part_mount_opts,
        root_part_fs_type,
        non_efi_mount_paths,
        nixos_system_derivation_path,
        nixos_system_out_path,
    })
}

pub fn configure() -> Result<NixOSInstallerConfig, Box<dyn Error>> {
    use lexopt::prelude::*;

    user_check()?;

    let mut hostname = None;
    let mut canonical_target_drive = None;
    let mut mount_path = "/mnt".to_string();
    let mut partition_drive = None;
    let mut format_partitions = None;
    let mut substitute_only = None;

    let mut update_lockfile = false;
    let mut flake_path = None;
    let mut create_flake_store_path_gc_root = true;

    let mut lexopt_parser = lexopt::Parser::from_env();
    while let Some(arg) = lexopt_parser.next()? {
        match arg {
            Long("hostname") => hostname = Some(lexopt_parser.value()?.string()?),
            Long("target-drive") => canonical_target_drive = Some(lexopt_parser.value()?.string()?),
            Long("flake-path") => flake_path = Some(lexopt_parser.value()?.string()?),
            Long("mount-path") => mount_path = lexopt_parser.value()?.string()?,

            Long("update-lockfile") => update_lockfile = true,
            Long("no-update-lockfile") => update_lockfile = false,

            Long("no-flake-store-path-gc-root") => create_flake_store_path_gc_root = false,

            Long("partition-drive") => partition_drive = Some(true),
            Long("no-partition-drive") => partition_drive = Some(false),

            Long("format-partitions") => format_partitions = Some(true),
            Long("no-format-partitions") => format_partitions = Some(false),

            Long("substitute-only") => substitute_only = Some(true),
            Long("do-build") => substitute_only = Some(false),

            _ => return Err(arg.unexpected().into()),
        }
    }

    let mandatory_args = enforce_mandatory_args(MandatoryArgsWrapped {
        hostname,
        canonical_target_drive,
        partition_drive,
        format_partitions,
        substitute_only,
        update_lockfile,
        flake_path,
        create_flake_store_path_gc_root,
    })?;
    let verified_args = verify_args(mandatory_args, mount_path)?;
    make_nixos_installer_config(verified_args)
}

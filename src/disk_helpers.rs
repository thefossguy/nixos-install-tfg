use crate::command_helpers::{did_command_exit_successfully, retry_command_on_ebusy};
use crate::config::NixOSInstallerConfig;
use crate::{get_formatted_process_stderr, log_then_output};

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;
use std::process::Command;

use nanologger::info;

pub enum SupportedFileSystems {
    Btrfs,
    Zfs,
}
impl fmt::Debug for SupportedFileSystems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SupportedFileSystems::Zfs => write!(f, "zfs"),
            SupportedFileSystems::Btrfs => write!(f, "btrfs"),
        }
    }
}
impl fmt::Display for SupportedFileSystems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SupportedFileSystems::Zfs => write!(f, "ZFS"),
            SupportedFileSystems::Btrfs => write!(f, "Btrfs"),
        }
    }
}

pub enum NixOSFileSystemMountPath {
    Efi,
    Root,
}
impl fmt::Debug for NixOSFileSystemMountPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NixOSFileSystemMountPath::Efi => write!(f, "EFI"),
            NixOSFileSystemMountPath::Root => write!(f, "Root"),
        }
    }
}

impl fmt::Display for NixOSFileSystemMountPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NixOSFileSystemMountPath::Efi => write!(f, "/boot"),
            NixOSFileSystemMountPath::Root => write!(f, "/"),
        }
    }
}

pub fn sort_paths(paths: &mut [String], inverse_tree: bool) {
    paths.sort_by(|a, b| {
        if inverse_tree {
            b.split('/')
                .count()
                .cmp(&a.split('/').count())
                .then_with(|| b.cmp(a))
        } else {
            a.split('/')
                .count()
                .cmp(&b.split('/').count())
                .then_with(|| a.cmp(b))
        }
    });
}

fn settle_udev(canonical_target_drive: &str) -> Result<(), Box<dyn Error>> {
    let mut partprobe_cmd = Command::new("partprobe");
    partprobe_cmd.arg(canonical_target_drive);
    let partprobe_cmd_process_result = log_then_output!(partprobe_cmd);

    if did_command_exit_successfully(&partprobe_cmd_process_result) {
        let mut udevadm_cmd = Command::new("udevadm");
        udevadm_cmd.arg("settle");
        let udevadm_cmd_process_result = log_then_output!(udevadm_cmd);
        if did_command_exit_successfully(&udevadm_cmd_process_result) {
            Ok(())
        } else {
            let udevadm_cmd_process_output = udevadm_cmd_process_result?;
            Err(format!(
                "The `udevadm` command failed{}",
                get_formatted_process_stderr!(udevadm_cmd_process_output)
            )
            .into())
        }
    } else {
        let partprobe_cmd_process_output = partprobe_cmd_process_result?;
        Err(format!(
            "The `partprobe` command failed{}",
            get_formatted_process_stderr!(partprobe_cmd_process_output)
        )
        .into())
    }
}

pub fn unmount_everything(canonical_target_drive: &str) -> Result<(), Box<dyn Error>> {
    let proc_mounts = fs::read_to_string("/proc/mounts")?;
    let mut mount_points: Vec<String> = proc_mounts
        .lines()
        .filter(|proc_mounts_line| proc_mounts_line.starts_with(canonical_target_drive))
        .filter_map(|proc_mounts_line| proc_mounts_line.split(' ').nth(1))
        .map(ToString::to_string)
        .collect();
    if mount_points.is_empty() {
        Ok(())
    } else {
        sort_paths(&mut mount_points, true);

        let mut umount_cmd = Command::new("umount");
        umount_cmd.arg("--force").args(&mount_points);
        let umount_cmd_process_result = log_then_output!(umount_cmd);
        if did_command_exit_successfully(&umount_cmd_process_result) {
            Ok(())
        } else {
            let umount_cmd_process_output = umount_cmd_process_result?;
            Err(format!(
                "Could not unmount all paths, some of these may be dangling {:?}{}",
                mount_points,
                get_formatted_process_stderr!(umount_cmd_process_output)
            )
            .into())
        }
    }
}

fn discover_target_drive_partitions(
    nixos_installer_config: &NixOSInstallerConfig,
) -> Result<Vec<(u8, String)>, Box<dyn Error>> {
    let starts_with_pattern = format!("{}-part", nixos_installer_config.target_drive_by_id);
    let mut target_drive_partitions: Vec<(u8, String)> = fs::read_dir("/dev/disk/by-id")?
        .filter_map(|entry| entry.ok().map(|e| e.path().display().to_string()))
        .filter_map(|partition| {
            let suffix = partition.strip_prefix(&starts_with_pattern)?;
            Some((suffix.parse::<u8>().ok()?, partition))
        })
        .collect();
    if target_drive_partitions.len() == 2 {
        target_drive_partitions.sort();
        Ok(target_drive_partitions)
    } else {
        Err(format!(
            "The target drive should contain only 2 partitions, found: {target_drive_partitions:?}",
        )
        .into())
    }
}

fn partition_target_drive(
    nixos_installer_config: &NixOSInstallerConfig,
) -> Result<Vec<(u8, String)>, Box<dyn Error>> {
    if nixos_installer_config.partition_drive {
        info!(
            "Partitioning '{}'",
            nixos_installer_config.target_drive_by_id
        );

        let mut wipefs_cmd = Command::new("wipefs");
        wipefs_cmd.args([
            "--all",
            "--force",
            &nixos_installer_config.target_drive_by_id,
        ]);
        let wipefs_cmd_process_result = log_then_output!(wipefs_cmd);
        if !did_command_exit_successfully(&wipefs_cmd_process_result) {
            let wipefs_cmd_process_output = wipefs_cmd_process_result?;
            return Err(format!(
                "Could not wipe the target disk{}",
                get_formatted_process_stderr!(wipefs_cmd_process_output)
            )
            .into());
        }

        let mut parted_cmd = Command::new("parted");
        parted_cmd.args([
            "--align",
            "optimal",
            "--fix",
            "--script",
            &nixos_installer_config.target_drive_by_id,
            "mklabel",
            "gpt",
            "mkpart",
            "primary",
            "fat32",
            "64MiB",
            "1088MiB",
            "set",
            "1",
            "esp",
            "on",
            "mkpart",
            "primary",
            "ext4",
            "1088MiB",
            "100%",
        ]);
        let parted_cmd_process_result = log_then_output!(parted_cmd);
        if !did_command_exit_successfully(&parted_cmd_process_result) {
            let parted_cmd_process_output = parted_cmd_process_result?;
            return Err(format!(
                "Disk partitioning failed{}",
                get_formatted_process_stderr!(parted_cmd_process_output)
            )
            .into());
        }
    } else {
        info!(
            "Not partitioning '{}'",
            nixos_installer_config.target_drive_by_id
        );
    }

    settle_udev(&nixos_installer_config.canonical_target_drive)?;
    discover_target_drive_partitions(nixos_installer_config)
}

fn make_efi_filesystem(
    nixos_installer_config: &NixOSInstallerConfig,
    block_device: &str,
) -> Result<(), Box<dyn Error>> {
    if nixos_installer_config.format_partitions {
        info!("Formatting '/boot'");

        let volume_id = nixos_installer_config.efi_part_uuid.replace('-', "");
        let mkfs_cmd_process_result = retry_command_on_ebusy(10, || {
            let mut mkfs_cmd = Command::new("mkfs.fat");
            mkfs_cmd.args(["-F", "32", "-n", &volume_id, "-i", &volume_id, block_device]);
            log_then_output!(mkfs_cmd)
        });

        if did_command_exit_successfully(&mkfs_cmd_process_result) {
            settle_udev(&nixos_installer_config.canonical_target_drive)?;
            Ok(())
        } else {
            let mkfs_cmd_process_output = mkfs_cmd_process_result?;
            Err(format!(
                "The `mkfs` command to create an EFI filesystem failed{}",
                get_formatted_process_stderr!(mkfs_cmd_process_output)
            )
            .into())
        }
    } else {
        info!("Not formatting '/boot'");
        Ok(())
    }
}

fn make_root_filesystem(
    nixos_installer_config: &NixOSInstallerConfig,
    block_device: &str,
) -> Result<(), Box<dyn Error>> {
    if nixos_installer_config.format_partitions {
        info!("Formatting '/'");

        let fs_label = format!("{}-rootfs", nixos_installer_config.hostname);
        let mkfs_cmd_process_result = match nixos_installer_config.root_part_fs_type {
            SupportedFileSystems::Btrfs => retry_command_on_ebusy(10, || {
                let mut mkfs_cmd = Command::new("mkfs.btrfs");
                mkfs_cmd.args([
                    "--force",
                    "--checksum",
                    "sha256", // use sha256 for checksumming over blake2
                    "--metadata",
                    "dup",
                    "--features",
                    "squota",
                    "--label",
                    &fs_label,
                    "--uuid",
                    &nixos_installer_config.root_part_uuid,
                    block_device,
                ]);

                log_then_output!(mkfs_cmd)
            }),
            SupportedFileSystems::Zfs => todo!(),
        };

        if did_command_exit_successfully(&mkfs_cmd_process_result) {
            settle_udev(&nixos_installer_config.canonical_target_drive)?;
            Ok(())
        } else {
            let mkfs_cmd_process_output = mkfs_cmd_process_result?;
            Err(format!(
                "The `mkfs` command to create a filesystem for rootfs failed{}",
                get_formatted_process_stderr!(mkfs_cmd_process_output)
            )
            .into())
        }
    } else {
        info!("Not formatting '/'");
        Ok(())
    }
}

fn perform_filesystem_mount(
    mount_options: &str,
    block_device: &str,
    mount_path: &str,
) -> Result<(), Box<dyn Error>> {
    // `mount` has `--mkdir` but this isolates directory creation
    // failure point from `mount`
    fs::create_dir_all(mount_path)?;
    let mount_cmd_process_result = retry_command_on_ebusy(10, || {
        let mut mount_cmd = Command::new("mount");
        mount_cmd.args(["--options", mount_options, block_device, mount_path]);
        log_then_output!(mount_cmd)
    });
    if did_command_exit_successfully(&mount_cmd_process_result) {
        Ok(())
    } else {
        let mount_cmd_process_output = mount_cmd_process_result?;
        Err(format!(
            "The mount command failed{}",
            get_formatted_process_stderr!(mount_cmd_process_output)
        )
        .into())
    }
}

fn make_btrfs_subvolume(subvolume_path: &str) -> Result<(), Box<dyn Error>> {
    if Path::new(&subvolume_path).exists() {
        Ok(())
    } else {
        let mut btrfs_cmd = Command::new("btrfs");
        btrfs_cmd.args(["subvolume", "create", subvolume_path]);

        let btrfs_cmd_process_result = log_then_output!(btrfs_cmd);
        if did_command_exit_successfully(&btrfs_cmd_process_result) {
            Ok(())
        } else {
            let btrfs_cmd_process_output = btrfs_cmd_process_result?;
            Err(format!(
                "Could not create the btrfs subvolume{}",
                get_formatted_process_stderr!(btrfs_cmd_process_output)
            )
            .into())
        }
    }
}

fn create_fresh_snapshot(subvolume: &str, subvolume_path: &str) -> Result<(), Box<dyn Error>> {
    let snapshot_name_separator = "+";
    let snapshot_name_suffix = "fresh";
    let snapshot_path = format!("{subvolume_path}{snapshot_name_separator}{snapshot_name_suffix}");
    let mut btrfs_cmd = Command::new("btrfs");
    btrfs_cmd.args([
        "subvolume",
        "snapshot",
        "-r", // make read-only snapshot, no long flag
        subvolume_path,
        &snapshot_path,
    ]);

    let btrfs_cmd_process_result = log_then_output!(btrfs_cmd);
    if did_command_exit_successfully(&btrfs_cmd_process_result) {
        Ok(())
    } else {
        let btrfs_cmd_process_output = btrfs_cmd_process_result?;
        Err(format!(
            "Could not create a fresh snapshot for subvolume '{}'{}",
            subvolume,
            get_formatted_process_stderr!(btrfs_cmd_process_output)
        )
        .into())
    }
}

fn set_subvolume_limit(subvolume_limit: &str, subvolume_path: &str) -> Result<(), Box<dyn Error>> {
    let mut btrfs_cmd = Command::new("btrfs");
    btrfs_cmd.args(["qgroup", "limit", subvolume_limit, subvolume_path]);
    let btrfs_cmd_process_result = log_then_output!(btrfs_cmd);
    if did_command_exit_successfully(&btrfs_cmd_process_result) {
        Ok(())
    } else {
        let btrfs_cmd_process_output = btrfs_cmd_process_result?;
        Err(format!(
            "Could not enforce subvolume limit{}",
            get_formatted_process_stderr!(btrfs_cmd_process_output)
        )
        .into())
    }
}

fn make_btrfs_subvolume_string(subvolume_mount_path: &str) -> String {
    subvolume_mount_path.replacen('/', "@", 1).replace('/', "-")
}

fn mount_root_filesystems(
    nixos_installer_config: &NixOSInstallerConfig,
) -> Result<(), Box<dyn Error>> {
    let block_device = format!(
        "/dev/disk/by-uuid/{}",
        nixos_installer_config.root_part_uuid
    );
    perform_filesystem_mount(
        &nixos_installer_config.root_part_mount_opts,
        &block_device,
        &nixos_installer_config.mount_path,
    )?;

    for non_efi_mnt_path in &nixos_installer_config.non_efi_mount_paths {
        let subvolume = make_btrfs_subvolume_string(non_efi_mnt_path);
        let subvolume_path = format!("{}/{subvolume}", nixos_installer_config.mount_path);
        make_btrfs_subvolume(&subvolume_path)?;

        if non_efi_mnt_path == "/" || non_efi_mnt_path == "/tmp" {
            create_fresh_snapshot(&subvolume, &subvolume_path)?;
        }

        if let Some(subvolume_limit) = match non_efi_mnt_path.as_str() {
            "/" => Some("16G".to_string()),
            "/home" => Some("128G".to_string()),
            "/var" => Some("8G".to_string()),
            _ => None,
        } {
            set_subvolume_limit(&subvolume_limit, &subvolume_path)?;
        }
    }

    let mut umount_cmd = Command::new("umount");
    umount_cmd.args(["--force", &nixos_installer_config.mount_path]);
    let umount_cmd_process_result = log_then_output!(umount_cmd);
    if !did_command_exit_successfully(&umount_cmd_process_result) {
        let umount_cmd_process_output = umount_cmd_process_result?;
        return Err(format!(
            "The `umount` command failed{}",
            get_formatted_process_stderr!(umount_cmd_process_output)
        )
        .into());
    }

    for non_efi_mnt_path in &nixos_installer_config.non_efi_mount_paths {
        let subvolume = make_btrfs_subvolume_string(non_efi_mnt_path);
        let mount_options = format!(
            "{},subvol={}",
            nixos_installer_config.root_part_mount_opts, subvolume
        );
        let subvolume_mount_path =
            format!("{}{non_efi_mnt_path}", nixos_installer_config.mount_path);
        perform_filesystem_mount(&mount_options, &block_device, &subvolume_mount_path)?;
    }

    Ok(())
}

fn mount_efi_filesystem(
    nixos_installer_config: &NixOSInstallerConfig,
) -> Result<(), Box<dyn Error>> {
    let block_device = format!("/dev/disk/by-uuid/{}", nixos_installer_config.efi_part_uuid);
    let mount_path = format!("{}/boot", nixos_installer_config.mount_path);
    perform_filesystem_mount(
        &nixos_installer_config.efi_part_mount_opts,
        &block_device,
        &mount_path,
    )
}

pub fn perform_disk_ops(
    nixos_installer_config: &NixOSInstallerConfig,
) -> Result<(), Box<dyn Error>> {
    unmount_everything(&nixos_installer_config.canonical_target_drive)?;

    let target_drive_partitions = partition_target_drive(nixos_installer_config)?;
    info!("Target drive partitions: {:?}", target_drive_partitions);

    make_efi_filesystem(nixos_installer_config, &target_drive_partitions[0].1)?;
    make_root_filesystem(nixos_installer_config, &target_drive_partitions[1].1)?;

    mount_root_filesystems(nixos_installer_config)?;
    mount_efi_filesystem(nixos_installer_config)?;

    Ok(())
}

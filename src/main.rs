mod command_helpers;
mod config;
mod disk_helpers;
mod installer;

use std::error::Error;

use nanologger::info;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn Error>> {
    nanologger::LoggerBuilder::new()
        .level(nanologger::LogLevel::Trace)
        .timestamps(true)
        .source_location(true)
        .init()?;

    let nixos_installer_config = config::configure()?;
    info!("{:#?}\n", nixos_installer_config);

    for count in (0..3).rev() {
        if count == 0 {
            return Err("You have exhausted the number of interactions allowed".into());
        }
        let mut user_choice_string = String::new();
        nanologger::info!("Is this okay? [y|Y|n|N]");
        std::io::stdin().read_line(&mut user_choice_string)?;
        user_choice_string = user_choice_string.trim().to_string();
        match user_choice_string.as_str() {
            "y" | "Y" => break,
            "n" | "N" => return Err("Aborted due to user choice".into()),
            _ => (),
        }
    }

    disk_helpers::perform_disk_ops(&nixos_installer_config)?;
    installer::install_nixos(&nixos_installer_config)?;

    info!("NixOS is now installed! :)");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn Error>> {
    Err("Linux or bust".into())
}

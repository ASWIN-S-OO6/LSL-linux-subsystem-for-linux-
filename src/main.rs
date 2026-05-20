mod config;
mod distro;
mod network;
mod runtime;

use std::io;
use std::path::PathBuf;
use std::process::Command;
use clap::{Parser, Subcommand};
use colored::Colorize;

#[derive(Parser)]
#[command(name = "lsl")]
#[command(about = "Linux Subsystem Layer — A lightweight subsystem manager for Linux hosts", long_about = None)]
struct Cli {
    #[arg(short, long, help = "Specify the distro to run")]
    distro: Option<String>,

    #[arg(short, long, help = "Run the command as root inside the guest")]
    root: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true, help = "Command and arguments to execute inside the distro")]
    command_args: Vec<String>,

    #[command(subcommand)]
    subcommand: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Install a supported guest distribution (e.g. 'kali')")]
    Install {
        #[arg(help = "Name of the distribution (currently only 'kali' is supported)")]
        distro: String,
    },

    #[command(about = "Import a guest distribution from a local rootfs tarball")]
    Import {
        #[arg(help = "Name to assign to the imported distribution")]
        name: String,
        #[arg(help = "Path to the local rootfs.tar.xz or rootfs.tar.gz file")]
        tar_path: String,
    },

    #[command(about = "Unregister and completely delete a guest distribution")]
    Unregister {
        #[arg(help = "Name of the distribution to unregister")]
        name: String,
    },

    #[command(about = "List all registered distributions and their status")]
    List,

    #[command(about = "Stop the background execution of a running distribution")]
    Stop {
        #[arg(help = "Name of the distribution to stop")]
        name: String,
    },

    #[command(about = "Set a distribution as the default for shell execution")]
    Default {
        #[arg(help = "Name of the distribution to set as default")]
        name: String,
    },

    #[command(about = "Run a command inside the default or specified distribution")]
    Run {
        #[arg(short, long, help = "Run command as root")]
        root: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, help = "Command and arguments to run")]
        command_args: Vec<String>,
    },
}

fn check_root_privileges() {
    let uid = unsafe { libc::geteuid() };
    if uid != 0 {
        eprintln!(
            "{} {}",
            "Error:".red().bold(),
            "LSL requires root privileges. Please rerun this command using 'sudo'."
        );
        std::process::exit(1);
    }
}

fn spawn_separate_terminal() -> bool {
    let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("./target/debug/lsl"));
    let sudo_user = std::env::var("SUDO_USER").ok();

    // Authorize local connections to the X server under the current user's authority
    let _ = Command::new("xhost").arg("+local:").status();

    let emulators = ["konsole", "gnome-terminal", "xfce4-terminal", "xterm"];
    let mut chosen_emulator = None;
    for emu in &emulators {
        if Command::new("which").arg(emu).output().map(|o| o.status.success()).unwrap_or(false) {
            chosen_emulator = Some(*emu);
            break;
        }
    }

    let Some(emu) = chosen_emulator else {
        return false;
    };

    // Construct the command to execute inside the terminal, preserving GUI environment variables
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    let xauth = std::env::var("XAUTHORITY").unwrap_or_default();
    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    let inner_cmd = format!(
        "sudo env LSL_ALREADY_SPAWNED=1 DISPLAY='{}' XAUTHORITY='{}' WAYLAND_DISPLAY='{}' {}",
        display, xauth, wayland, current_exe.to_str().unwrap()
    );

    // If we are root (running under sudo), we should run the terminal emulator as the host user
    // so it can connect to the X/Wayland display server.
    if let Some(user) = sudo_user {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        let xauth = std::env::var("XAUTHORITY").unwrap_or_default();
        let status = match emu {
            "konsole" => {
                Command::new("sudo")
                    .args(&["-u", &user, "env", &format!("DISPLAY={}", display), &format!("XAUTHORITY={}", xauth), "konsole", "-e", "sh", "-c", &inner_cmd])
                    .status()
            }
            "gnome-terminal" => {
                Command::new("sudo")
                    .args(&["-u", &user, "env", &format!("DISPLAY={}", display), &format!("XAUTHORITY={}", xauth), "gnome-terminal", "--", "sh", "-c", &inner_cmd])
                    .status()
            }
            "xfce4-terminal" => {
                Command::new("sudo")
                    .args(&["-u", &user, "env", &format!("DISPLAY={}", display), &format!("XAUTHORITY={}", xauth), "xfce4-terminal", "-e", &format!("sh -c \"{}\"", inner_cmd)])
                    .status()
            }
            _ => { // xterm
                Command::new("sudo")
                    .args(&["-u", &user, "env", &format!("DISPLAY={}", display), &format!("XAUTHORITY={}", xauth), "xterm", "-e", "sh", "-c", &inner_cmd])
                    .status()
            }
        };
        status.map(|s| s.success()).unwrap_or(false)
    } else {
        // If we are already the normal user (not under sudo)
        let status = match emu {
            "konsole" => {
                Command::new("konsole")
                    .args(&["-e", "sh", "-c", &inner_cmd])
                    .status()
            }
            "gnome-terminal" => {
                Command::new("gnome-terminal")
                    .args(&["--", "sh", "-c", &inner_cmd])
                    .status()
            }
            "xfce4-terminal" => {
                Command::new("xfce4-terminal")
                    .args(&["-e", &format!("sh -c \"{}\"", inner_cmd)])
                    .status()
            }
            _ => { // xterm
                Command::new("xterm")
                    .args(&["-e", "sh", "-c", &inner_cmd])
                    .status()
            }
        };
        status.map(|s| s.success()).unwrap_or(false)
    }
}

fn main() -> io::Result<()> {
    // Parse arguments using clap
    let cli = Cli::parse();

    // Check if we want to run the default interactive shell in a separate terminal
    if cli.subcommand.is_none() && cli.command_args.is_empty() && std::env::var("LSL_ALREADY_SPAWNED").is_err() {
        if spawn_separate_terminal() {
            std::process::exit(0);
        }
    }

    // Check root privileges for all commands (clap handles --help and --version internally first)
    check_root_privileges();

    // Ensure LSL directories exist
    config::ensure_dirs()?;


    // Route based on command
    match cli.subcommand {
        Some(Commands::Install { distro }) => {
            if distro.to_lowercase() == "kali" {
                match distro::install_kali() {
                    Ok(_) => println!("{}", "Installation complete! Run 'sudo lsl' to enter the Kali Linux shell.".green().bold()),
                    Err(e) => eprintln!("{} Failed to install Kali: {}", "Error:".red().bold(), e),
                }
            } else {
                eprintln!("{} Distribution '{}' is not supported for auto-installation. Currently, only 'kali' is supported.", "Error:".red().bold(), distro);
            }
        }
        Some(Commands::Import { name, tar_path }) => {
            match distro::import_distro(&name, &tar_path) {
                Ok(_) => println!("{}", "Import complete!".green().bold()),
                Err(e) => eprintln!("{} Failed to import distro: {}", "Error:".red().bold(), e),
            }
        }
        Some(Commands::Unregister { name }) => {
            if let Err(e) = distro::unregister_distro(&name) {
                eprintln!("{} Failed to unregister distro: {}", "Error:".red().bold(), e);
            }
        }
        Some(Commands::List) => {
            list_distros()?;
        }
        Some(Commands::Stop { name }) => {
            if let Err(e) = runtime::stop_distro(&name) {
                eprintln!("{} Failed to stop distro: {}", "Error:".red().bold(), e);
            }
        }
        Some(Commands::Default { name }) => {
            let mut global_cfg = config::GlobalConfig::load();
            if global_cfg.distros.contains_key(&name) {
                global_cfg.default_distro = Some(name.clone());
                global_cfg.save()?;
                println!("{}", format!("Distro '{}' set as default.", name).green());
            } else {
                eprintln!("{} Distro '{}' is not registered.", "Error:".red().bold(), name);
            }
        }
        Some(Commands::Run { root, command_args }) => {
            let global_cfg = config::GlobalConfig::load();
            let target_distro = cli.distro.or(global_cfg.default_distro).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "No default distro set. Install one first using 'lsl install kali'")
            })?;
            
            let exit_code = runtime::run_command_in_distro(&target_distro, &command_args, root)?;
            std::process::exit(exit_code);
        }
        None => {
            // Check if we passed trailing args directly without "run" subcommand (e.g. "lsl ls -la")
            let global_cfg = config::GlobalConfig::load();
            let target_distro = cli.distro.or(global_cfg.default_distro).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "No default distro set. Install one first using 'lsl install kali'")
            })?;

            let exit_code = runtime::run_command_in_distro(&target_distro, &cli.command_args, cli.root)?;
            std::process::exit(exit_code);
        }
    }

    Ok(())
}

fn list_distros() -> io::Result<()> {
    let config = config::GlobalConfig::load();
    if config.distros.is_empty() {
        println!("No distros registered. Use 'lsl install kali' to install Kali Linux.");
        return Ok(());
    }

    println!();
    println!("{:<20} {:<12} {:<15} {:<18}", "DISTRO NAME", "STATUS", "IP ADDRESS", "MAC ADDRESS");
    println!("{}", "-".repeat(65));
    
    for (name, distro) in &config.distros {
        let is_running = runtime::is_distro_running(name);
        let status_str = if is_running { "Running" } else { "Stopped" };
        let is_default = if config.default_distro.as_deref() == Some(name) { " (default)" } else { "" };
        let name_str = format!("{}{}", name, is_default);
        
        let p_name = format!("{:<20}", name_str);
        let p_status = if is_running { format!("{:<12}", status_str).green() } else { format!("{:<12}", status_str).red() };
        let p_ip = format!("{:<15}", distro.ip_address);
        let p_mac = format!("{:<18}", distro.mac_address);
        
        println!("{}{}{}{}", p_name, p_status, p_ip, p_mac);
    }
    println!();
    Ok(())
}

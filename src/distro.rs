use std::fs::{self, File, OpenOptions};
use std::io::{self, Write, BufRead, BufReader};
use std::path::Path;
use std::process::Command;
use crate::config::{DistroConfig, GlobalConfig, get_distro_dir, LSL_DIR};
use colored::Colorize;

// Fetches the latest Kali Linux download path from the index
fn get_latest_kali_path() -> io::Result<String> {
    let index_url = "https://images.linuxcontainers.org/meta/1.0/index-system";
    let cache_dir = Path::new(LSL_DIR).join("cache");
    let index_path = cache_dir.join("index-system");

    println!("{}", "Fetching latest Kali release details from images.linuxcontainers.org...".blue());
    
    let status = Command::new("curl")
        .args(&["-sSL", "-o", index_path.to_str().unwrap(), index_url])
        .status()?;

    if !status.success() {
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to download index-system"));
    }

    let file = File::open(&index_path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        // Format: kali;current;amd64;default;20260519_17:14;/images/kali/current/amd64/default/20260519_17:14/
        if line.starts_with("kali;current;amd64;default;") {
            let parts: Vec<&str> = line.split(';').collect();
            if parts.len() >= 6 {
                return Ok(parts[5].to_string());
            }
        }
    }

    Err(io::Error::new(io::ErrorKind::NotFound, "Kali amd64 default image not found in index-system"))
}

pub fn install_kali() -> io::Result<DistroConfig> {
    let distro_dir = get_distro_dir("kali");
    if distro_dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Kali is already installed. Unregister it first if you want to reinstall.",
        ));
    }

    let relative_path = get_latest_kali_path()?;
    let rootfs_url = format!("https://images.linuxcontainers.org{}rootfs.tar.xz", relative_path);
    let cache_dir = Path::new(LSL_DIR).join("cache");
    let tarball_path = cache_dir.join("kali-rootfs.tar.xz");

    println!("Downloading Kali rootfs from: {}", rootfs_url.cyan());
    
    // Download using curl with a progress bar
    let status = Command::new("curl")
        .args(&["-L", "-#", "-o", tarball_path.to_str().unwrap(), &rootfs_url])
        .status()?;

    if !status.success() {
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to download Kali rootfs"));
    }

    import_distro("kali", tarball_path.to_str().unwrap())
}

pub fn import_distro(name: &str, tar_path: &str) -> io::Result<DistroConfig> {
    let distro_dir = get_distro_dir(name);
    if distro_dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Distro '{}' is already registered.", name),
        ));
    }

    let rootfs_dir = distro_dir.join("rootfs");
    let diff_dir = distro_dir.join("diff");
    let work_dir = distro_dir.join("work");
    let merged_dir = distro_dir.join("merged");

    fs::create_dir_all(&rootfs_dir)?;
    fs::create_dir_all(&diff_dir)?;
    fs::create_dir_all(&work_dir)?;
    fs::create_dir_all(&merged_dir)?;

    println!("Extracting rootfs to {} ... (this may take a minute)", rootfs_dir.to_str().unwrap().cyan());
    
    let status = Command::new("tar")
        .args(&["-xf", tar_path, "-C", rootfs_dir.to_str().unwrap()])
        .status()?;

    if !status.success() {
        // Cleanup on failure
        let _ = fs::remove_dir_all(&distro_dir);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to extract rootfs tarball"));
    }

    // Determine subnet index based on existing distros
    let mut config = GlobalConfig::load();
    let index = config.distros.len() + 1;
    let ip_address = "dhcp".to_string();
    let mac_address = format!("02:42:0a:58:00:{:02x}", index);

    let distro_config = DistroConfig {
        name: name.to_string(),
        path: distro_dir.to_str().unwrap().to_string(),
        ip_address,
        mac_address,
        default_user: "root".to_string(), // Can be synced to host user on boot
    };

    // Auto-configure the guest user credentials
    let (host_user, uid, gid) = get_host_user_details();
    if host_user != "root" {
        if let Err(e) = setup_distro_user(&rootfs_dir, &host_user, uid, gid) {
            println!("Warning: Failed to setup host user in guest: {}", e);
        }
    }

    config.distros.insert(name.to_string(), distro_config.clone());
    if config.default_distro.is_none() {
        config.default_distro = Some(name.to_string());
    }
    config.save()?;

    println!("{}", format!("Distro '{}' successfully registered!", name).green().bold());
    Ok(distro_config)
}

pub fn unregister_distro(name: &str) -> io::Result<()> {
    let mut config = GlobalConfig::load();
    if !config.distros.contains_key(name) {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("Distro '{}' not found.", name)));
    }

    let distro_dir = get_distro_dir(name);
    if distro_dir.exists() {
        println!("Deleting distro directory: {}", distro_dir.to_str().unwrap().cyan());
        // Note: Make sure it's unmounted before deleting!
        let _ = Command::new("umount").arg("-l").arg(distro_dir.join("merged")).status();
        fs::remove_dir_all(&distro_dir)?;
    }

    config.distros.remove(name);
    if config.default_distro.as_deref() == Some(name) {
        config.default_distro = config.distros.keys().next().cloned();
    }
    config.save()?;

    println!("{}", format!("Distro '{}' successfully unregistered.", name).green());
    Ok(())
}

pub fn get_host_user_details() -> (String, u32, u32) {
    let sudo_user = std::env::var("SUDO_USER").ok();
    let sudo_uid = std::env::var("SUDO_UID").ok().and_then(|s| s.parse::<u32>().ok());
    let sudo_gid = std::env::var("SUDO_GID").ok().and_then(|s| s.parse::<u32>().ok());

    match (sudo_user, sudo_uid, sudo_gid) {
        (Some(user), Some(uid), Some(gid)) => (user, uid, gid),
        _ => {
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };
            let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
            (user, uid, gid)
        }
    }
}

pub fn setup_distro_user(distro_root: &Path, username: &str, uid: u32, gid: u32) -> io::Result<()> {
    let passwd_path = distro_root.join("etc/passwd");
    let group_path = distro_root.join("etc/group");
    let shadow_path = distro_root.join("etc/shadow");

    // Check if user already exists
    let user_exists = {
        let file = File::open(&passwd_path)?;
        let reader = BufReader::new(file);
        reader.lines().filter_map(Result::ok).any(|line| line.starts_with(&format!("{}:", username)))
    };
    if !user_exists {
        // Append to etc/passwd
        let mut passwd_file = OpenOptions::new().append(true).open(&passwd_path)?;
        writeln!(passwd_file, "{}:x:{}:{}:{}:/home/{}:/bin/bash", username, uid, gid, username, username)?;

        // Append to etc/group
        let mut group_file = OpenOptions::new().append(true).open(&group_path)?;
        writeln!(group_file, "{}:x:{}:", username, gid)?;

        // Append to etc/shadow
        let mut shadow_file = OpenOptions::new().append(true).open(&shadow_path)?;
        writeln!(shadow_file, "{}:*:19000:0:99999:7:::", username)?;

        // Create home directory inside rootfs
        let guest_home = distro_root.join("home").join(username);
        fs::create_dir_all(&guest_home)?;
        
        // Change owner of home directory in the guest
        // We can run chown using guest's uid and gid, but since we are running as root on host,
        // we can set the owner to guest's uid/gid (which are the same as host's uid/gid)
        let status = Command::new("chown")
            .arg(format!("{}:{}", uid, gid))
            .arg(guest_home.to_str().unwrap())
            .status()?;
        if !status.success() {
            println!("Warning: failed to chown home directory for {}", username);
        }
    }

    // Add user to sudo group in the guest
    add_user_to_guest_group(distro_root, username, "sudo")?;

    // Allow passwordless sudo for user in guest
    let sudoers_d = distro_root.join("etc/sudoers.d");
    fs::create_dir_all(&sudoers_d)?;
    let sudoers_user = sudoers_d.join("lsl-user");
    let mut f = File::create(&sudoers_user)?;
    writeln!(f, "{} ALL=(ALL:ALL) NOPASSWD: ALL", username)?;
    
    // Set permissions to 0440 (read-only for owner/group, none for others)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&sudoers_user, fs::Permissions::from_mode(0o440))?;
    }

    // Configure /etc/hosts inside the guest to resolve hostname and prevent sudo warnings
    let hosts_path = distro_root.join("etc/hosts");
    if let Ok(mut hosts_file) = File::create(&hosts_path) {
        let _ = writeln!(hosts_file, "127.0.0.1\tlocalhost");
        let _ = writeln!(hosts_file, "127.0.1.1\tlsl-kali");
        let _ = writeln!(hosts_file, "::1\t\tlocalhost ip6-localhost ip6-loopback");
    }

    Ok(())
}

fn add_user_to_guest_group(distro_root: &Path, username: &str, group_name: &str) -> io::Result<()> {
    let group_path = distro_root.join("etc/group");
    if !group_path.exists() {
        return Ok(());
    }

    let file = File::open(&group_path)?;
    let reader = BufReader::new(file);
    let mut lines = Vec::new();
    let mut modified = false;

    for line in reader.lines() {
        let mut line = line?;
        if line.starts_with(&format!("{}:", group_name)) {
            // e.g. sudo:x:27:
            // or sudo:x:27:alice
            if !line.contains(username) {
                if line.ends_with(':') {
                    line.push_str(username);
                } else {
                    line.push_str(&format!(",{}", username));
                }
                modified = true;
            }
        }
        lines.push(line);
    }

    if modified {
        let mut file = File::create(&group_path)?;
        for line in lines {
            writeln!(file, "{}", line)?;
        }
    }

    Ok(())
}

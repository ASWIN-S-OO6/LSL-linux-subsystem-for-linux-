use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use nix::sched::{unshare, setns, CloneFlags};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, chroot, chdir, Gid, Uid};
use nix::mount::{mount, umount2, MsFlags, MntFlags};
use colored::Colorize;

use crate::config::{GlobalConfig, get_distro_dir, LSL_DIR};
use crate::distro::get_host_user_details;
use crate::network::{setup_network, cleanup_network};

// Helper for raw pivot_root syscall to ensure compatibility across Nix crate versions
fn sys_pivot_root<P: AsRef<Path>>(new_root: P, put_old: P) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let new_root_c = CString::new(new_root.as_ref().as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let put_old_c = CString::new(put_old.as_ref().as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let ret = unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            new_root_c.as_ptr(),
            put_old_c.as_ptr(),
        )
    };

    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn get_distro_init_pid(name: &str) -> Option<u32> {
    let pid_file = Path::new(LSL_DIR).join("run").join(format!("{}.pid", name));
    if !pid_file.exists() {
        return None;
    }

    let pid_str = fs::read_to_string(&pid_file).ok()?;
    let pid = pid_str.trim().parse::<u32>().ok()?;

    // Verify if process is actually running and has active namespaces
    let ns_path = format!("/proc/{}/ns/uts", pid);
    if Path::new(&ns_path).exists() {
        Some(pid)
    } else {
        // Clean stale PID file
        let _ = fs::remove_file(pid_file);
        None
    }
}

pub fn is_distro_running(name: &str) -> bool {
    get_distro_init_pid(name).is_some()
}

pub fn boot_distro(name: &str) -> io::Result<u32> {
    let config = GlobalConfig::load();
    let distro_config = config.distros.get(name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Distro '{}' not registered", name)))?;

    println!("{}", format!("Booting distro '{}'...", name).blue().bold());

    let distro_dir = get_distro_dir(name);
    let rootfs_dir = distro_dir.join("rootfs");
    let diff_dir = distro_dir.join("diff");
    let work_dir = distro_dir.join("work");
    let merged_dir = distro_dir.join("merged");

    // Ensure mount directories exist
    fs::create_dir_all(&diff_dir)?;
    fs::create_dir_all(&work_dir)?;
    fs::create_dir_all(&merged_dir)?;

    // Mount overlayfs on host first so that the host mount namespace sees the merged contents,
    // allowing host user directory mapping (~/LSL/<distro_name>) to see the actual files.
    // Unmount first in case of a stale mount.
    let _ = umount2(&merged_dir, MntFlags::MNT_DETACH);
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        rootfs_dir.to_str().unwrap(),
        diff_dir.to_str().unwrap(),
        work_dir.to_str().unwrap()
    );
    mount(
        Some("overlay"),
        &merged_dir,
        Some("overlay"),
        MsFlags::empty(),
        Some(opts.as_str())
    ).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to mount overlayfs on host: {}", e)))?;

    // Fix permissions for guest ping binaries to allow unprivileged ICMP raw socket creation
    let ping_path1 = merged_dir.join("bin/ping");
    let ping_path2 = merged_dir.join("usr/bin/ping");
    for path in &[ping_path1, ping_path2] {
        if path.exists() {
            let _ = Command::new("chmod").args(&["u+s", path.to_str().unwrap()]).status();
        }
    }

    // Ensure guest sudoers configuration is correct
    let sudoers_dir = merged_dir.join("etc/sudoers.d");
    let _ = fs::create_dir_all(&sudoers_dir);
    let sudoers_file = sudoers_dir.join("lsl-sudoers");
    if let Ok(mut f) = fs::File::create(&sudoers_file) {
        use std::io::Write;
        let _ = writeln!(f, "%sudo ALL=(ALL:ALL) ALL");
        let _ = writeln!(f, "%wheel ALL=(ALL:ALL) ALL");
        // Set permissions to 0440
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&sudoers_file, fs::Permissions::from_mode(0o440));
    }

    let (_host_user, host_uid, _host_gid) = get_host_user_details();

    // Create a pipe for communicating the child 2 PID back to the parent
    let (r_fd, w_fd) = nix::unistd::pipe()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to create pipe: {}", e)))?;

    // First Fork: Create Child 1 which will unshare namespaces
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child: child1 }) => {
            // Parent: close writer end of pipe
            let _ = nix::unistd::close(w_fd);

            // Wait for Child 1 to exit (reap Child 1)
            let _ = waitpid(child1, None);

            // Read Child 2's PID from the pipe
            let mut r_file = unsafe { fs::File::from_raw_fd(r_fd) };
            use std::io::Read;
            let mut buf = [0u8; 4];
            if r_file.read_exact(&mut buf).is_err() {
                return Err(io::Error::new(io::ErrorKind::Other, "Failed to read guest init PID from pipe"));
            }
            let pid = i32::from_ne_bytes(buf) as u32;

            // Wait a brief moment for Child 2 to initialize
            thread::sleep(Duration::from_millis(150));

            // Check if Child 2 (guest init) is still running
            // We use libc::kill with signal 0 as Child 2 is not a direct child of the parent process (it was spawned by Child 1)
            let is_running = unsafe {
                let ret = libc::kill(pid as i32, 0);
                ret == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            };

            if !is_running {
                let log_path = Path::new(LSL_DIR).join("run").join(format!("{}-boot.log", name));
                let error_msg = if log_path.exists() {
                    fs::read_to_string(&log_path).unwrap_or_else(|_| "Unknown initialization error".to_string())
                } else {
                    "Subsystem init process exited early".to_string()
                };
                return Err(io::Error::new(io::ErrorKind::Other, format!("Subsystem init process failed to boot:\n{}", error_msg)));
            }

            // Set up network (runs in host namespaces!)
            if let Err(e) = setup_network(name, pid, &distro_config.ip_address, &distro_config.mac_address) {
                println!("{}", format!("Warning: Network configuration failed: {}", e).yellow());
                return Err(e);
            }

            // Check again if child is still running after network setup
            let is_running_post = unsafe {
                let ret = libc::kill(pid as i32, 0);
                ret == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            };

            if !is_running_post {
                let log_path = Path::new(LSL_DIR).join("run").join(format!("{}-boot.log", name));
                let error_msg = if log_path.exists() {
                    fs::read_to_string(&log_path).unwrap_or_else(|_| "Unknown initialization error".to_string())
                } else {
                    "Subsystem init process exited early after network setup".to_string()
                };
                return Err(io::Error::new(io::ErrorKind::Other, format!("Subsystem init process failed to boot:\n{}", error_msg)));
            }

            // Write child PID to run directory
            let pid_file = Path::new(LSL_DIR).join("run").join(format!("{}.pid", name));
            fs::write(&pid_file, pid.to_string())?;

            // Set up host filesystem mapping in ~/LSL/<distro_name>
            let home_dir = get_host_home_dir();
            let lsl_mount_point = home_dir.join("LSL").join(name);
            let _ = fs::create_dir_all(&lsl_mount_point);
            let (_, host_uid, host_gid) = get_host_user_details();
            let lsl_parent = home_dir.join("LSL");
            let _ = nix::unistd::chown(&lsl_parent, Some(Uid::from_raw(host_uid)), Some(Gid::from_raw(host_gid)));
            let _ = nix::unistd::chown(&lsl_mount_point, Some(Uid::from_raw(host_uid)), Some(Gid::from_raw(host_gid)));

            // Bind mount the container's merged directory to the host mount point
            let _ = mount(Some(&merged_dir), &lsl_mount_point, Some("none"), MsFlags::MS_BIND, None::<&str>);

            println!("{}", format!("Distro '{}' booted successfully (PID {}).", name, pid).green());
            Ok(pid)
        }
        Ok(ForkResult::Child) => {
            // == CHILD 1 ==
            // Close reader end
            let _ = nix::unistd::close(r_fd);

            // Unshare namespaces (Mount, PID, UTS, IPC, Net)
            if let Err(_) = unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWNET) {
                std::process::exit(1);
            }

            // Make mount propagation private inside the new mount namespace
            let _ = mount(None::<&str>, "/", None::<&str>, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None::<&str>);

            // Second Fork: Create Child 2 inside the new PID namespace
            match unsafe { fork() } {
                Ok(ForkResult::Parent { child: child2 }) => {
                    // Write Child 2's PID to Parent via the pipe
                    let child2_pid = child2.as_raw();
                    let pid_bytes = child2_pid.to_ne_bytes();
                    let mut w_file = unsafe { fs::File::from_raw_fd(w_fd) };
                    let _ = w_file.write_all(&pid_bytes);
                    let _ = w_file.flush();
                    std::process::exit(0); // Exit Child 1 immediately
                }
                Ok(ForkResult::Child) => {
                    // == CHILD 2 (Daemon Init PID 1 inside container) ==
                    let _ = nix::unistd::close(w_fd);

                    // Redirect stdout & stderr to log file for diagnostics
                    let log_path = Path::new(LSL_DIR).join("run").join(format!("{}-boot.log", name));
                    if let Ok(log_file) = fs::OpenOptions::new().create(true).write(true).truncate(true).open(&log_path) {
                        use std::os::unix::io::AsRawFd;
                        unsafe {
                            libc::dup2(log_file.as_raw_fd(), 1);
                            libc::dup2(log_file.as_raw_fd(), 2);
                        }
                    }

            // Set hostname inside UTS namespace
            let hostname_path_upper = diff_dir.join("etc/hostname");
            let hostname_path_lower = rootfs_dir.join("etc/hostname");
            let hostname = {
                let raw_hostname = if let Ok(content) = fs::read_to_string(&hostname_path_upper) {
                    content.trim().to_string()
                } else if let Ok(content) = fs::read_to_string(&hostname_path_lower) {
                    content.trim().to_string()
                } else {
                    format!("lsl-{}", name)
                };
                if raw_hostname == "LXC_NAME" || raw_hostname.is_empty() {
                    format!("lsl-{}", name)
                } else {
                    raw_hostname
                }
            };
            let hostname_c = std::ffi::CString::new(hostname).unwrap();
            let _ = unsafe { libc::sethostname(hostname_c.as_ptr(), hostname_c.as_bytes().len() as libc::size_t) };



            // Setup proc
            let proc_dir = merged_dir.join("proc");
            fs::create_dir_all(&proc_dir).unwrap();
            mount(
                Some("proc"),
                &proc_dir,
                Some("proc"),
                MsFlags::empty(),
                None::<&str>
            ).expect("Failed to mount procfs");

            // Setup sys
            let sys_dir = merged_dir.join("sys");
            fs::create_dir_all(&sys_dir).unwrap();
            mount(
                Some("sysfs"),
                &sys_dir,
                Some("sysfs"),
                MsFlags::empty(),
                None::<&str>
            ).expect("Failed to mount sysfs");

            // Bind mount /dev
            let dev_dir = merged_dir.join("dev");
            fs::create_dir_all(&dev_dir).unwrap();
            mount(
                Some("/dev"),
                &dev_dir,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>
            ).expect("Failed to bind mount /dev");

            // Setup devpts (isolated terminal sessions)
            let pts_dir = dev_dir.join("pts");
            fs::create_dir_all(&pts_dir).unwrap();
            mount(
                Some("devpts"),
                &pts_dir,
                Some("devpts"),
                MsFlags::empty(),
                Some("newinstance,ptmxmode=0666,mode=620")
            ).expect("Failed to mount devpts");

            // Setup tmpfs on /tmp
            let tmp_dir = merged_dir.join("tmp");
            fs::create_dir_all(&tmp_dir).unwrap();
            mount(
                Some("tmpfs"),
                &tmp_dir,
                Some("tmpfs"),
                MsFlags::empty(),
                Some("mode=1777")
            ).expect("Failed to mount tmpfs on /tmp");

            // Bind mount X11 socket if it exists on host
            let x11_host = Path::new("/tmp/.X11-unix");
            if x11_host.exists() {
                let x11_guest = tmp_dir.join(".X11-unix");
                fs::create_dir_all(&x11_guest).unwrap();
                let _ = mount(
                    Some(x11_host),
                    &x11_guest,
                    None::<&str>,
                    MsFlags::MS_BIND | MsFlags::MS_REC,
                    None::<&str>
                );
            }

            // Setup tmpfs on /run
            let run_dir = merged_dir.join("run");
            fs::create_dir_all(&run_dir).unwrap();
            mount(
                Some("tmpfs"),
                &run_dir,
                Some("tmpfs"),
                MsFlags::empty(),
                Some("mode=755")
            ).expect("Failed to mount tmpfs on /run");

            // Bind mount host user runtime directory (for Wayland / Audio sockets)
            let user_run_host = format!("/run/user/{}", host_uid);
            let user_run_host_path = Path::new(&user_run_host);
            if user_run_host_path.exists() {
                let user_run_guest = run_dir.join("user").join(host_uid.to_string());
                fs::create_dir_all(&user_run_guest).unwrap();
                let _ = mount(
                    Some(user_run_host_path),
                    &user_run_guest,
                    None::<&str>,
                    MsFlags::MS_BIND | MsFlags::MS_REC,
                    None::<&str>
                );
            }

            // Bind mount host /home to /mnt/host inside the guest for host file access
            let mnt_host_dir = merged_dir.join("mnt/host");
            fs::create_dir_all(&mnt_host_dir).unwrap();
            let _ = mount(
                Some("/home"),
                &mnt_host_dir,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>
            );

            // Setup resolv.conf inside container using public nameservers
            let etc_dir = merged_dir.join("etc");
            fs::create_dir_all(&etc_dir).unwrap();
            let resolv_conf = etc_dir.join("resolv.conf");
            if let Ok(mut f) = File::create(&resolv_conf) {
                let _ = writeln!(f, "nameserver 1.1.1.1");
                let _ = writeln!(f, "nameserver 8.8.8.8");
            }

            // Pivot Root or Chroot
            let old_root_dir = merged_dir.join(".old_root");
            let _ = fs::create_dir_all(&old_root_dir);

            let _pivot_ok = if let Err(_) = sys_pivot_root(&merged_dir, &old_root_dir) {
                // Fallback to standard chroot
                chroot(&merged_dir).expect("Failed to chroot into rootfs");
                chdir("/").expect("Failed to chdir inside rootfs");
                false
            } else {
                chdir("/").expect("Failed to chdir inside rootfs");
                // Lazy unmount the host root filesystem so the guest cannot access it
                let _ = umount2("/.old_root", MntFlags::MNT_DETACH);
                let _ = fs::remove_dir("/.old_root");
                true
            };

            // Infinite loop to keep namespaces alive and reap zombie child processes
            loop {
                match waitpid(None, None) {
                    Ok(_status) => {
                        // Reap any other pending zombies non-blockingly
                        while let Ok(state) = waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                            if state == WaitStatus::StillAlive {
                                break;
                            }
                        }
                    }
                    Err(nix::Error::ECHILD) => {
                        thread::sleep(Duration::from_secs(2));
                    }
                    Err(_) => {
                        thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        }
        Err(_) => std::process::exit(1),
    }
}
Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("Fork failed: {}", e))),
}
}

pub fn stop_distro(name: &str) -> io::Result<()> {
    // Clean up host bind mount (~/LSL/<name>)
    let home_dir = get_host_home_dir();
    let lsl_mount_point = home_dir.join("LSL").join(name);
    if lsl_mount_point.exists() {
        let _ = umount2(&lsl_mount_point, MntFlags::MNT_DETACH);
        let _ = fs::remove_dir(&lsl_mount_point);
    }

    if !is_distro_running(name) {
        println!("{}", format!("Distro '{}' is already stopped.", name).yellow());
        // Clean up network anyway to ensure no stale interfaces are left
        let _ = cleanup_network(name);
        return Ok(());
    }

    let pid = get_distro_init_pid(name).unwrap();
    println!("{}", format!("Stopping distro '{}' (Init PID: {})...", name, pid).blue());

    // Send SIGKILL to the container's init process
    // This will terminate the init process, and because it is PID 1 in the PID namespace,
    // the kernel will terminate all other processes in the namespace and clean it up.
    let ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    if ret == -1 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
    }

    // Wait a brief moment for processes to exit
    thread::sleep(Duration::from_millis(200));

    // Clean up PID file
    let pid_file = Path::new(LSL_DIR).join("run").join(format!("{}.pid", name));
    if pid_file.exists() {
        let _ = fs::remove_file(pid_file);
    }

    // Also unmount overlayfs on host
    let distro_dir = get_distro_dir(name);
    let merged_dir = distro_dir.join("merged");
    let _ = umount2(&merged_dir, MntFlags::MNT_DETACH);

    // Clean up networking configuration (NAT rules, veth pairs)
    cleanup_network(name)?;

    println!("{}", format!("Distro '{}' stopped successfully.", name).green());
    Ok(())
}

pub fn run_command_in_distro(name: &str, command_args: &[String], run_as_root: bool) -> io::Result<i32> {
    // 1. Ensure distro is running
    let mut init_pid = match get_distro_init_pid(name) {
        Some(pid) => pid,
        None => boot_distro(name)?,
    };

    // Check if user setup has been completed inside the guest
    let global_cfg = GlobalConfig::load();
    if !global_cfg.distros.contains_key(name) {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("Distro '{}' not found in config", name)));
    }
    
    let distro_dir = get_distro_dir(name);
    let setup_completed_file = distro_dir.join("merged").join("etc/lsl_setup_completed");
    
    let setup_needed = !setup_completed_file.exists();

    if setup_needed {
        // Run interactive setup!
        let (username, hostname, password, root_password, package_choice) = prompt_for_user_setup(name)?;
        
        println!("{}", "[LSL] Running guest user configuration...".cyan());

        // 0. Write /etc/hostname inside the guest
        let hostname_file_merged = distro_dir.join("merged").join("etc/hostname");
        let hostname_file_diff = distro_dir.join("diff").join("etc/hostname");
        let _ = fs::write(&hostname_file_merged, hostname.clone());
        let _ = fs::write(&hostname_file_diff, hostname.clone());

        // Rewrite /etc/hosts with new hostname mapping
        let hosts_file = distro_dir.join("merged").join("etc/hosts");
        if let Ok(mut f) = File::create(&hosts_file) {
            let _ = writeln!(f, "127.0.0.1\tlocalhost");
            let _ = writeln!(f, "127.0.1.1\t{}", hostname);
            let _ = writeln!(f, "::1\t\tlocalhost ip6-localhost ip6-loopback");
        }
        
        // 1. Configure system timezone and locales (Arch Linux style setup)
        println!("{}", "[LSL] Configuring system timezone and locales...".cyan());
        let host_tz = fs::read_to_string("/etc/timezone").unwrap_or_else(|_| "UTC".to_string()).trim().to_string();
        let tz_script = format!(
            "ln -sf /usr/share/zoneinfo/{} /etc/localtime && echo '{}' > /etc/timezone",
            host_tz, host_tz
        );
        let _ = execute_setup_cmd_in_guest(init_pid, &[
            "bash".to_string(),
            "-c".to_string(),
            tz_script
        ]);

        // 2. Update package database
        println!("{}", "[LSL] Updating package database...".cyan());
        let _ = execute_setup_cmd_in_guest(init_pid, &["apt-get".to_string(), "update".to_string()]);
        
        // 3. Install locales, zsh, kali-defaults, prompt plugins, and dbus-x11
        println!("{}", "[LSL] Installing Zsh, locales, DHCP client, networking, DBus, and Kali default configurations...".cyan());
        let _ = execute_setup_cmd_in_guest(init_pid, &[
            "apt-get".to_string(),
            "install".to_string(),
            "-y".to_string(),
            "locales".to_string(),
            "zsh".to_string(),
            "kali-defaults".to_string(),
            "zsh-autosuggestions".to_string(),
            "zsh-syntax-highlighting".to_string(),
            "isc-dhcp-client".to_string(),
            "iputils-ping".to_string(),
            "net-tools".to_string(),
            "iproute2".to_string(),
            "curl".to_string(),
            "wget".to_string(),
            "ca-certificates".to_string(),
            "dbus-x11".to_string(),
            "fastfetch".to_string()
        ]);

        // 4. Generate localizations
        println!("{}", "[LSL] Generating locale (en_US.UTF-8)...".cyan());
        let _ = execute_setup_cmd_in_guest(init_pid, &[
            "sed".to_string(),
            "-i".to_string(),
            "s/# en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/".to_string(),
            "/etc/locale.gen".to_string()
        ]);
        let _ = execute_setup_cmd_in_guest(init_pid, &["locale-gen".to_string()]);
        let _ = execute_setup_cmd_in_guest(init_pid, &[
            "update-locale".to_string(),
            "LANG=en_US.UTF-8".to_string()
        ]);

        // 5. Install chosen Kali package set
        match package_choice.as_str() {
            "core" => {
                println!("{}", "[LSL] Installing Kali Core tools (this may take a few minutes)...".cyan());
                let _ = execute_setup_cmd_in_guest(init_pid, &[
                    "apt-get".to_string(),
                    "install".to_string(),
                    "-y".to_string(),
                    "kali-linux-core".to_string()
                ]);
            }
            "headless" => {
                println!("{}", "[LSL] Installing Kali Headless security suite (this will take several minutes)...".cyan());
                let _ = execute_setup_cmd_in_guest(init_pid, &[
                    "apt-get".to_string(),
                    "install".to_string(),
                    "-y".to_string(),
                    "kali-linux-headless".to_string()
                ]);
            }
            _ => {
                println!("{}", "[LSL] Skipping optional packages (minimal install)...".cyan());
            }
        }
        
        // 3. Create or configure user account inside guest
        // Ensure the guest user has the same UID/GID as the host user for seamless GUI/audio integration
        println!("{}", format!("[LSL] Configuring guest user account '{}'...", username).cyan());
        let (_host_user, host_uid, host_gid) = get_host_user_details();
        let user_setup_script = format!(
            "existing_group=$(getent group {1} | cut -d: -f1); \
             if [ ! -z \"$existing_group\" ] && [ \"$existing_group\" != \"{0}\" ]; then \
                 groupmod -n \"{0}\" \"$existing_group\"; \
             fi; \
             existing_user=$(getent passwd {2} | cut -d: -f1); \
             if [ ! -z \"$existing_user\" ] && [ \"$existing_user\" != \"{0}\" ]; then \
                 usermod -l \"{0}\" -d \"/home/{0}\" -m \"$existing_user\"; \
             else \
                 if ! getent group \"{0}\" >/dev/null; then \
                     groupadd -g {1} \"{0}\" || groupadd \"{0}\"; \
                 fi; \
                 if ! getent passwd \"{0}\" >/dev/null; then \
                     useradd -u {2} -g {1} -m -s /bin/zsh \"{0}\" || useradd -m -s /bin/zsh \"{0}\"; \
                 fi; \
             fi; \
             usermod -s /bin/zsh \"{0}\"",
            username, host_gid, host_uid
        );

        let _ = execute_setup_cmd_in_guest(init_pid, &[
            "bash".to_string(),
            "-c".to_string(),
            user_setup_script
        ]);
        
        // 4. Set user and root passwords inside guest
        println!("{}", "[LSL] Setting passwords...".cyan());
        let chpasswd_script = format!(
            "echo '{0}:{1}' | chpasswd && echo 'root:{2}' | chpasswd",
            username, password, root_password
        );
        let _ = execute_setup_cmd_in_guest(init_pid, &[
            "bash".to_string(),
            "-c".to_string(),
            chpasswd_script
        ]);
        
        // 5. Add user to sudoers groups
        println!("{}", "[LSL] Adding user to sudoers groups...".cyan());
        let _ = execute_setup_cmd_in_guest(init_pid, &["groupadd".to_string(), "-f".to_string(), "sudo".to_string()]);
        let _ = execute_setup_cmd_in_guest(init_pid, &["groupadd".to_string(), "-f".to_string(), "wheel".to_string()]);
        let _ = execute_setup_cmd_in_guest(init_pid, &["usermod".to_string(), "-aG".to_string(), "sudo".to_string(), username.clone()]);
        let _ = execute_setup_cmd_in_guest(init_pid, &["usermod".to_string(), "-aG".to_string(), "wheel".to_string(), username.clone()]);
        
        // 6. Copy skeleton `.zshrc` to user home and set permissions
        let copy_script = format!(
            "cp /etc/skel/.zshrc /home/{0}/.zshrc && chown -R {0}:{0} /home/{0}",
            username
        );
        let _ = execute_setup_cmd_in_guest(init_pid, &[
            "bash".to_string(),
            "-c".to_string(),
            copy_script
        ]);
        
        // 7. Update distro default user in config
        let mut mut_cfg = GlobalConfig::load();
        if let Some(d) = mut_cfg.distros.get_mut(name) {
            d.default_user = username.clone();
        }
        mut_cfg.save()?;

        // 8. Touch setup completed file inside the guest
        let _ = std::fs::File::create(&setup_completed_file);
        
        println!("{}", "User setup completed successfully!".green().bold());

        // Restart the distro init daemon so UTS hostname, environment, and network are refreshed with the new config!
        println!("{}", "[LSL] Restarting subsystem to apply user configuration...".cyan());
        stop_distro(name)?;
        init_pid = boot_distro(name)?;
    }

    let target_user = if run_as_root {
        "root".to_string()
    } else {
        let global_cfg = GlobalConfig::load();
        if let Some(d) = global_cfg.distros.get(name) {
            d.default_user.clone()
        } else {
            "root".to_string()
        }
    };

    let guest_uid_gid = if target_user == "root" {
        None
    } else {
        get_guest_user_ids_from_host(name, &target_user)
    };

    // Ensure fastfetch is installed and configured on interactive login shell sessions
    if command_args.is_empty() {
        // Ensure /etc/profile.d/fastfetch.sh exists inside the guest filesystem
        let profile_d = distro_dir.join("merged").join("etc/profile.d");
        if let Err(e) = fs::create_dir_all(&profile_d) {
            eprintln!("Warning: Failed to create profile.d directory: {}", e);
        }
        let fastfetch_sh_path = profile_d.join("fastfetch.sh");
        let script_content = "# Only run fastfetch if stdout is a tty (interactive session)\nif [ -t 1 ] && command -v fastfetch >/dev/null 2>&1; then\n    clear 2>/dev/null || printf \"\\033[H\\033[2J\"\n    fastfetch\nfi\n";
        let should_write = match fs::read_to_string(&fastfetch_sh_path) {
            Ok(content) => content != script_content,
            Err(_) => true,
        };
        if should_write {
            if let Err(e) = fs::write(&fastfetch_sh_path, script_content) {
                eprintln!("Warning: Failed to write fastfetch.sh: {}", e);
            } else {
                // Set executable permissions if on Unix
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&fastfetch_sh_path, fs::Permissions::from_mode(0o755));
                }
            }
        }

        // Auto-install fastfetch if it is missing inside the guest
        let fastfetch_bin = distro_dir.join("merged").join("usr/bin/fastfetch");
        if !fastfetch_bin.exists() {
            println!("{}", "[LSL] fastfetch is not installed in the guest distro. Installing now...".cyan());
            let _ = execute_setup_cmd_in_guest(init_pid, &[
                "bash".to_string(),
                "-c".to_string(),
                "apt-get update >/dev/null 2>&1".to_string(),
            ]);
            match execute_setup_cmd_in_guest(init_pid, &[
                "bash".to_string(),
                "-c".to_string(),
                "apt-get install -y fastfetch >/dev/null 2>&1".to_string(),
            ]) {
                Ok(_) => {
                    if distro_dir.join("merged").join("usr/bin/fastfetch").exists() {
                        println!("{}", "[LSL] fastfetch successfully installed!".green());
                    } else {
                        eprintln!("{}", "Warning: fastfetch installation finished, but binary was not found. Please verify guest internet connection.".yellow());
                    }
                }
                Err(e) => eprintln!("Warning: Failed to install fastfetch inside guest: {}", e),
            }
        }
    }

    // 2. Open all namespace file descriptors first from the host context
    // This avoids swapping Mount namespace before opening the other namespaces.
    let user_file = File::open(format!("/proc/{}/ns/user", init_pid)).ok();
    let mnt_file = File::open(format!("/proc/{}/ns/mnt", init_pid))
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("Mount namespace not found: {}", e)))?;
    let uts_file = File::open(format!("/proc/{}/ns/uts", init_pid))
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("UTS namespace not found: {}", e)))?;
    let ipc_file = File::open(format!("/proc/{}/ns/ipc", init_pid))
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("IPC namespace not found: {}", e)))?;
    let net_file = File::open(format!("/proc/{}/ns/net", init_pid))
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("Network namespace not found: {}", e)))?;
    let pid_file = File::open(format!("/proc/{}/ns/pid", init_pid))
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("PID namespace not found: {}", e)))?;

    // Now setns them in correct order
    if let Some(uf) = user_file {
        let _ = setns(uf, CloneFlags::empty());
    }
    setns(uts_file, CloneFlags::empty())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to setns for uts: {}", e)))?;
    setns(ipc_file, CloneFlags::empty())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to setns for ipc: {}", e)))?;
    setns(net_file, CloneFlags::empty())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to setns for net: {}", e)))?;
    setns(pid_file, CloneFlags::empty())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to setns for pid: {}", e)))?;
    setns(mnt_file, CloneFlags::empty())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to setns for mnt: {}", e)))?;

    // 3. Fork so that the new process is created INSIDE the target PID namespace
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            // Parent: Wait for command to complete and return its exit code
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => Ok(code),
                Ok(_) => Ok(1),
                Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("Failed to wait for child: {}", e))),
            }
        }
        Ok(ForkResult::Child) => {
            // == CHILD (Inside container namespace) ==

            // Sync process root and working directory to the swapped Mount namespace
            let _ = chroot("/");
            let _ = chdir("/");

            let (host_user, host_uid, _host_gid) = get_host_user_details();

            // Drop privileges to the guest user's UID and GID
            if let Some((guid, ggid)) = guest_uid_gid {
                // Load supplementary groups for target_user from guest /etc/group
                if let Ok(c_user) = std::ffi::CString::new(target_user.as_str()) {
                    unsafe {
                        let _ = libc::initgroups(c_user.as_ptr(), ggid as libc::gid_t);
                    }
                } else {
                    let _ = nix::unistd::setgroups(&[Gid::from_raw(ggid)]);
                }
                let _ = nix::unistd::setgid(Gid::from_raw(ggid));
                let _ = nix::unistd::setuid(Uid::from_raw(guid));
            }

            // Copy essential environment variables from host for seamless integration
            let mut envs = std::collections::HashMap::new();
            envs.insert("TERM".to_string(), std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()));
            
            // Silence systemd/journald connection warnings inside container guest namespaces
            envs.insert("SYSTEMD_LOG_LEVEL".to_string(), "emerg".to_string());
            envs.insert("SYSTEMD_LOG_TARGET".to_string(), "console".to_string());
            
            // X11 & GUI Passthrough
            if let Ok(display) = std::env::var("DISPLAY") {
                envs.insert("DISPLAY".to_string(), display);
            }
            if let Ok(xauth) = std::env::var("XAUTHORITY") {
                envs.insert("XAUTHORITY".to_string(), xauth);
            } else {
                // Default fallback to user's .Xauthority
                if host_user != "root" {
                    envs.insert("XAUTHORITY".to_string(), format!("/home/{}/.Xauthority", host_user));
                }
            }

            // Wayland & Audio Passthrough
            if let Ok(wl_display) = std::env::var("WAYLAND_DISPLAY") {
                envs.insert("WAYLAND_DISPLAY".to_string(), wl_display);
            }
            envs.insert("XDG_RUNTIME_DIR".to_string(), format!("/run/user/{}", host_uid));
            envs.insert("PULSE_SERVER".to_string(), format!("unix:/run/user/{}/pulse/native", host_uid));

            // User Identity Env Vars
            let (home_dir, shell_path) = {
                let mut home = "/root".to_string();
                let mut shell = "/bin/bash".to_string();
                if target_user != "root" {
                    if let Ok(passwd_content) = std::fs::read_to_string("/etc/passwd") {
                        for line in passwd_content.lines() {
                            let parts: Vec<&str> = line.split(':').collect();
                            if parts.len() >= 7 && parts[0] == target_user {
                                home = parts[5].to_string();
                                shell = parts[6].to_string();
                                break;
                            }
                        }
                    }
                }
                (home, shell)
            };

            envs.insert("HOME".to_string(), home_dir);
            envs.insert("USER".to_string(), target_user.clone());
            envs.insert("LOGNAME".to_string(), target_user.clone());
            envs.insert("SHELL".to_string(), shell_path.clone());

            // PATH variable inside Kali
            envs.insert("PATH".to_string(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());

            // Clear environment and set our configured variables
            unsafe {
                std::env::vars().for_each(|(k, _)| std::env::remove_var(k));
                for (k, v) in envs {
                    std::env::set_var(k, v);
                }
            }

            // Determine directory: if current directory is inside host user's /home, start in it,
            // otherwise default to user's home directory.
            let cur_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            if cur_dir.starts_with("/home") {
                let _ = chdir(&cur_dir);
            } else {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
                let _ = chdir(Path::new(&home));
            }

            // Execute target command
            let shell_cmd = if command_args.is_empty() {
                // If no command is provided, boot into the default interactive login shell!
                vec![shell_path, "--login".to_string()]
            } else {
                command_args.to_vec()
            };

            let program = &shell_cmd[0];
            let args: Vec<std::ffi::CString> = shell_cmd.iter()
                .map(|s| std::ffi::CString::new(s.as_str()).unwrap())
                .collect();

            let program_c = std::ffi::CString::new(program.as_str()).unwrap();
            
            // execute command
            let ret = nix::unistd::execvp(&program_c, &args);
            
            // If execvp returns, it failed
            eprintln!("Error executing command inside subsystem: {:?}", ret);
            std::process::exit(1);
        }
        Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("Fork failed: {}", e))),
    }
}

fn get_guest_user_ids_from_host(distro_name: &str, username: &str) -> Option<(u32, u32)> {
    let distro_dir = get_distro_dir(distro_name);
    let passwd_path = distro_dir.join("merged").join("etc/passwd");
    let passwd_content = std::fs::read_to_string(&passwd_path).ok()?;
    for line in passwd_content.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 4 && parts[0] == username {
            let uid = parts[2].parse::<u32>().ok()?;
            let gid = parts[3].parse::<u32>().ok()?;
            return Some((uid, gid));
        }
    }
    None
}

fn read_password(prompt: &str) -> io::Result<String> {
    print!("{}", prompt);
    let _ = io::stdout().flush();

    // Disable terminal echo
    let mut termios = unsafe { std::mem::zeroed() };
    let fd = libc::STDIN_FILENO;
    unsafe {
        libc::tcgetattr(fd, &mut termios);
        let mut no_echo = termios;
        no_echo.c_lflag &= !libc::ECHO;
        libc::tcsetattr(fd, libc::TCSANOW, &no_echo);
    }

    let mut password = String::new();
    let res = io::stdin().read_line(&mut password);

    // Restore terminal echo
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &termios);
    }
    println!(); // Print a newline after password entry

    res.map(|_| password.trim().to_string())
}

fn prompt_for_user_setup(distro_name: &str) -> io::Result<(String, String, String, String, String)> {
    println!("\n=== LSL Guest User Configuration ===");
    println!("Please configure the default UNIX user account (like standard OS setup).");

    let mut username = String::new();
    loop {
        print!("Enter new UNIX username: ");
        let _ = io::stdout().flush();
        username.clear();
        io::stdin().read_line(&mut username)?;
        let trimmed = username.trim().to_string();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_') && trimmed != "root" {
            username = trimmed;
            break;
        }
        println!("Invalid username. Use only alphanumeric characters and underscores.");
    }

    let mut hostname = String::new();
    loop {
        print!("Enter guest hostname (default: lsl-{}): ", distro_name);
        let _ = io::stdout().flush();
        hostname.clear();
        io::stdin().read_line(&mut hostname)?;
        let trimmed = hostname.trim().to_string();
        if trimmed.is_empty() {
            hostname = format!("lsl-{}", distro_name);
            break;
        }
        if trimmed.chars().all(|c| c.is_alphanumeric() || c == '-') {
            hostname = trimmed;
            break;
        }
        println!("Invalid hostname. Use only alphanumeric characters and hyphens.");
    }

    let password = loop {
        let p1 = read_password(&format!("Enter new UNIX password for '{}': ", username))?;
        let p2 = read_password("Retype new UNIX password: ")?;
        if p1 == p2 && !p1.is_empty() {
            break p1;
        }
        println!("Passwords do not match or are empty. Please try again.");
    };

    println!("Enter new UNIX password for root (press Enter to use the same password):");
    let root_password = {
        let p1 = read_password("Enter root password: ")?;
        if p1.is_empty() {
            password.clone()
        } else {
            loop {
                let p2 = read_password("Retype root password: ")?;
                if p1 == p2 {
                    break p1;
                }
                println!("Passwords do not match. Please try again.");
            }
        }
    };

    println!("\nChoose Kali Linux package set (like tasksel / archinstall):");
    let package_choice = loop {
        print!("  1) Minimal  - Base OS only (fastest setup)\n  2) Core     - Essential pentesting tools (~1GB)\n  3) Headless - Complete CLI security tools (~3-4GB)\nSelect option [1-3] (default: 2): ");
        let _ = io::stdout().flush();
        let mut ans = String::new();
        io::stdin().read_line(&mut ans)?;
        let trimmed = ans.trim();
        if trimmed.is_empty() || trimmed == "2" {
            break "core".to_string();
        } else if trimmed == "1" {
            break "minimal".to_string();
        } else if trimmed == "3" {
            break "headless".to_string();
        }
        println!("Invalid option. Please enter 1, 2, or 3.");
    };

    Ok((username, hostname, password, root_password, package_choice))
}

fn execute_setup_cmd_in_guest(init_pid: u32, cmd_args: &[String]) -> io::Result<()> {
    // Open all target namespace files
    let user_file = File::open(format!("/proc/{}/ns/user", init_pid)).ok();
    let mnt_file = File::open(format!("/proc/{}/ns/mnt", init_pid))?;
    let uts_file = File::open(format!("/proc/{}/ns/uts", init_pid))?;
    let ipc_file = File::open(format!("/proc/{}/ns/ipc", init_pid))?;
    let net_file = File::open(format!("/proc/{}/ns/net", init_pid))?;
    let pid_file = File::open(format!("/proc/{}/ns/pid", init_pid))?;

    // Fork before setns so that we don't mess up the calling parent process's namespaces!
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            let _ = waitpid(child, None);
            Ok(())
        }
        Ok(ForkResult::Child) => {
            if let Some(uf) = user_file {
                let _ = setns(uf, CloneFlags::empty());
            }
            let _ = setns(uts_file, CloneFlags::empty());
            let _ = setns(ipc_file, CloneFlags::empty());
            let _ = setns(net_file, CloneFlags::empty());
            let _ = setns(pid_file, CloneFlags::empty());
            let _ = setns(mnt_file, CloneFlags::empty());

            // Sync process root and working directory to the swapped Mount namespace
            let _ = chroot("/");
            let _ = chdir("/");

            // Fork again for PID namespace transition
            match unsafe { fork() } {
                Ok(ForkResult::Parent { child }) => {
                    match waitpid(child, None) {
                        Ok(WaitStatus::Exited(_, code)) => std::process::exit(code),
                        _ => std::process::exit(1),
                    }
                }
                Ok(ForkResult::Child) => {
                    let mut cmd = Command::new(&cmd_args[0]);
                    cmd.args(&cmd_args[1..]);
                    cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
                    let status = cmd.status().unwrap_or_else(|_| std::process::exit(1));
                    std::process::exit(if status.success() { 0 } else { 1 });
                }
                Err(_) => std::process::exit(1),
            }
        }
        Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("Fork failed: {}", e))),
    }
}

fn get_host_home_dir() -> PathBuf {
    let sudo_user = std::env::var("SUDO_USER").ok();
    if let Some(user) = sudo_user {
        PathBuf::from(format!("/home/{}", user))
    } else {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home)
        } else {
            PathBuf::from("/root")
        }
    }
}

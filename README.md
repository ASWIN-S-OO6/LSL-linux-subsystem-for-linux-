# Linux Subsystem for Linux (LSL)

LSL (Linux Subsystem for Linux) is a lightweight, low-overhead subsystem manager for Linux hosts. It allows you to run isolated guest Linux distributions (such as Kali Linux) natively on your Linux host using native kernel features.

Unlike heavy virtualization systems, LSL boots in milliseconds and integrates directly with the host's GUI and audio systems.

## Features

- **Isolated Runtime**: Uses Linux kernel namespaces (`uts`, `ipc`, `net`, `pid`, `mnt`, and `user`), `chroot`/`pivot_root` for complete namespace isolation.
- **OverlayFS File Management**: Uses overlay filesystems so that the base rootfs remains untouched while guest changes are persisted in a separate diff directory.
- **Interactive login hook**: Automatically installs and runs `fastfetch` system information when logging into your subsystem.
- **Advanced Network Virtualization**: Supports three networking modes to connect the subsystem to the internet:
  - **Bridged DHCP**: Obtains a real IP address from your local network.
  - **Routed Proxy ARP**: Routes traffic through host interfaces.
  - **NAT Mode**: Creates an isolated subnet and routes traffic via `nftables` masquerading.
- **GUI & Audio Passthrough**: Out-of-the-box support for running graphic applications and playing audio using host sockets (X11, Wayland, and PulseAudio).
- **Host Integration**: Mounts your host `/home` folder to `/mnt/host` inside the guest for seamless file access.

---

## Installation

1. **Clone the Repository**:
   ```bash
   git clone https://github.com/ASWIN-S-OO6/LSL-linux-subsystem-for-linux-.git
   cd LSL-linux-subsystem-for-linux-
   ```

2. **Run the Installer**:
   The installer compiles the Rust application and installs the `lsl` executable to `/usr/local/bin`:
   ```bash
   sudo ./install.sh
   ```

---

## How to Use

> [!NOTE]
> - **SUID Execution & Privilege Escalation**: During installation, LSL is configured with SUID permissions (`chmod 4755`). This allows you to run subcommands (like `lsl list` or `lsl run <cmd>`) directly without typing `sudo`.
> - **Interactive Login Password Prompt**: When launching the interactive login shell (`lsl` without arguments), a separate graphical terminal window is opened. Inside this new window, you will be prompted for your host user password via `sudo` to boot/escalate the subsystem. This also registers your host user context (`SUDO_USER`) so that guest `sudo` commands execute without a password.
> - **Supported Distributions**: For now, **Kali Linux** (`kali`) is the only pre-configured distribution available by default for the `install` command. You can still import other Linux rootfs files using the `import` command.


### 1. Install Guest Distribution
To install the default supported Kali Linux distribution:
```bash
lsl install kali
```
During first setup, LSL will download the rootfs and run an interactive prompt for you to configure your guest username, hostname, passwords, and security package sets (Minimal, Core, or Headless).

### 2. Enter the Subsystem (Interactive Login)
To launch the default login shell inside your guest distribution:
```bash
lsl
```
This boots the container (if stopped) and logs you into Zsh. You will be greeted with the system hardware statistics and ASCII logo from `fastfetch`.

### 3. Run Commands Directly
To execute a command inside the default distro without entering the shell:
```bash
lsl run <command>
```
*Example:*
```bash
lsl run ip address show
```
Or use the shortcut by omitting `run`:
```bash
lsl apt update
```

### 4. Run as Root
By default, commands and login shells run as your configured guest user. To run them as `root`:
```bash
lsl --root
```
Or for direct commands:
```bash
lsl --root apt update
```

### 5. Check Subsystem Status
To list all registered distributions, their boot status, and network IP/MAC addresses:
```bash
lsl list
```

### 6. Stop a Running Subsystem
To stop a running distro daemon and clean up its virtual network interfaces:
```bash
lsl stop <distro-name>
```

### 7. Import Custom Distributions
You can import custom rootfs tarballs (like Arch Linux, Ubuntu, etc.):
```bash
lsl import <subsystem-name> /path/to/rootfs.tar.xz
```

### 8. Unregister/Delete a Subsystem
To completely delete a distribution, its overlay filesystems, and configurations:
```bash
lsl unregister <distro-name>
```

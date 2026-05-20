use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::Command;
use colored::Colorize;

pub fn setup_network(name: &str, init_pid: u32, ip_address: &str, mac_address: &str) -> io::Result<()> {
    println!("{}", format!("Configuring network for distro '{}' (PID: {})...", name, init_pid).blue());

    let netns_name = format!("lsl-{}", name);
    let netns_path = format!("/var/run/netns/{}", netns_name);

    // 1. Ensure /var/run/netns exists
    fs::create_dir_all("/var/run/netns")?;

    // 2. Create netns file for ip netns integration
    if !Path::new(&netns_path).exists() {
        File::create(&netns_path)?;
    }

    // 3. Bind mount the network namespace file
    let status = Command::new("mount")
        .args(&["--bind", &format!("/proc/{}/ns/net", init_pid), &netns_path])
        .status()?;
    if !status.success() {
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to bind-mount network namespace"));
    }

    // Check if user setup is complete
    let distro_dir = Path::new("/var/lib/lsl/distros").join(name);
    let setup_completed_file = distro_dir.join("merged").join("etc/lsl_setup_completed");
    let is_setup_mode = !setup_completed_file.exists();

    if ip_address == "dhcp" && !is_setup_mode {
        println!("{}", "[LSL] Network mode: Bridging to host network (DHCP via ipvlan)...".cyan());
        if let Ok(host_iface) = get_default_host_interface() {
            println!("{}", format!("[LSL] Bridging to host default interface: {}", host_iface).cyan());

            let ipvlan_interface = format!("lsl-ipvl-{}", name);
            let mut success = true;

            // Create ipvlan interface linked to the host's active default interface
            let status = Command::new("ip")
                .args(&["link", "add", &ipvlan_interface, "link", &host_iface, "type", "ipvlan", "mode", "l2"])
                .status();
            if status.is_err() || !status.unwrap().success() {
                success = false;
            }

            if success {
                // Move the ipvlan interface to guest network namespace
                let status = Command::new("ip")
                    .args(&["link", "set", &ipvlan_interface, "netns", &netns_name, "name", "eth0"])
                    .status();
                if status.is_err() || !status.unwrap().success() {
                    success = false;
                }
            }

            if success {
                let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "link", "set", "dev", "eth0", "up"]).status();
                let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "link", "set", "dev", "lo", "up"]).status();

                // Inside guest netns: run dhclient to obtain real-time IP from physical network
                println!("{}", "[LSL] Requesting IP from DHCP server...".cyan());
                let dhcp_status = Command::new("ip")
                    .args(&["netns", "exec", &netns_name, "dhclient", "-1", "-timeout", "5", "eth0"])
                    .status();
                
                if dhcp_status.is_err() || !dhcp_status.unwrap().success() {
                    success = false;
                }
            }

            if success {
                // Verify we received an IPv4 address
                let ip_output = Command::new("ip")
                    .args(&["netns", "exec", &netns_name, "ip", "-4", "addr", "show", "dev", "eth0"])
                    .output();
                if let Ok(out) = ip_output {
                    let ip_str = String::from_utf8_lossy(&out.stdout);
                    if !ip_str.contains("inet ") {
                        success = false;
                    }
                } else {
                    success = false;
                }
            }

            if success {
                println!("{}", "[LSL] Bridged DHCP setup succeeded.".green());
                return Ok(());
            } else {
                println!("{}", "Warning: Bridged DHCP failed (common on wireless/Wi-Fi adapters or if dhclient is missing).".yellow());
                // Clean up in case of failure
                let _ = Command::new("ip").args(&["link", "del", &ipvlan_interface]).status();
                let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "link", "del", "eth0"]).status();
            }
        }
        println!("{}", "[LSL] Falling back to Routed Proxy ARP mode...".yellow());
    }

    if is_setup_mode {
        println!("{}", "[LSL] Interactive setup active: using temporary NAT route for package downloads...".cyan());
    }

    // Get default route details of the host
    let (host_dev, host_src, _host_via) = get_default_route_details()
        .unwrap_or_else(|_| ("wlp46s0".to_string(), "10.0.0.1".to_string(), "10.0.0.1".to_string()));

    let actual_ip = if ip_address == "dhcp" {
        find_free_lan_ip(&host_src)
    } else {
        ip_address.to_string()
    };

    let is_routed_proxy_arp = are_ips_in_same_subnet(&actual_ip, &host_src);

    if is_routed_proxy_arp {
        println!("{}", format!("[LSL] Network mode: Routed Proxy ARP (Assigned LAN IP: {})", actual_ip).cyan());
        
        let veth_host = format!("lsl-v-{}-0", name);
        let veth_guest = "eth0";

        // Create veth pair
        let status = Command::new("ip")
            .args(&["link", "add", &veth_host, "type", "veth", "peer", "name", veth_guest])
            .status()?;
        if !status.success() {
            let _ = cleanup_network(name);
            return Err(io::Error::new(io::ErrorKind::Other, "Failed to create veth pair"));
        }

        // Move guest interface to netns
        let status = Command::new("ip")
            .args(&["link", "set", veth_guest, "netns", &netns_name])
            .status()?;
        if !status.success() {
            let _ = cleanup_network(name);
            return Err(io::Error::new(io::ErrorKind::Other, "Failed to move guest interface to netns"));
        }

        // Inside guest: set MAC address
        let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "link", "set", "dev", veth_guest, "address", mac_address]).status();

        // Inside guest: set IP address
        let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "addr", "add", &format!("{}/24", actual_ip), "dev", veth_guest]).status();

        // Bring guest interfaces UP
        let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "link", "set", "dev", veth_guest, "up"]).status();
        let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "link", "set", "dev", "lo", "up"]).status();

        // Enable IP forwarding and unprivileged ping sockets on host
        let _ = fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        let _ = fs::write("/proc/sys/net/ipv4/ping_group_range", "0 2147483647");

        // Enable proxy ARP on host interfaces
        let _ = fs::write(format!("/proc/sys/net/ipv4/conf/{}/proxy_arp", host_dev), "1");
        let _ = fs::write(format!("/proc/sys/net/ipv4/conf/{}/proxy_arp", veth_host), "1");

        // Bring host end UP
        let _ = Command::new("ip").args(&["link", "set", "dev", &veth_host, "up"]).status();

        // Add host route to guest IP
        let _ = Command::new("ip").args(&["route", "add", &actual_ip, "dev", &veth_host]).status();

        // Inside guest: route to host directly, and set default gateway to host IP
        let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "route", "add", &host_src, "dev", "eth0"]).status();
        let _ = Command::new("ip").args(&["netns", "exec", &netns_name, "ip", "route", "add", "default", "via", &host_src]).status();

        return Ok(());
    }

    // NAT Mode (Fallback/Default)
    let actual_ip = if ip_address == "dhcp" { "10.88.1.2" } else { ip_address };
    let ip_parts: Vec<&str> = actual_ip.split('.').collect();
    if ip_parts.len() != 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Malformed IP address"));
    }
    let subnet_idx = ip_parts[2];
    let host_ip = format!("10.88.{}.1", subnet_idx);
    let subnet_cidr = format!("10.88.{}.0/24", subnet_idx);

    let veth_host = format!("lsl-v-{}-0", name);
    let veth_guest = "eth0";

    // 4. Create veth pair
    let status = Command::new("ip")
        .args(&["link", "add", &veth_host, "type", "veth", "peer", "name", veth_guest])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to create veth pair"));
    }

    // 5. Move guest interface to the network namespace
    let status = Command::new("ip")
        .args(&["link", "set", veth_guest, "netns", &netns_name])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to move guest veth interface to netns"));
    }

    // 6. Inside guest netns: set MAC address of eth0
    let status = Command::new("ip")
        .args(&["netns", "exec", &netns_name, "ip", "link", "set", "dev", veth_guest, "address", mac_address])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to set guest MAC address"));
    }

    // 7. Inside guest netns: set IP address of eth0
    let status = Command::new("ip")
        .args(&["netns", "exec", &netns_name, "ip", "addr", "add", &format!("{}/24", actual_ip), "dev", veth_guest])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to set guest IP address"));
    }

    // 8. Inside guest netns: bring eth0 up
    let status = Command::new("ip")
        .args(&["netns", "exec", &netns_name, "ip", "link", "set", "dev", veth_guest, "up"])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to bring guest interface UP"));
    }

    // 9. Inside guest netns: bring lo up
    let status = Command::new("ip")
        .args(&["netns", "exec", &netns_name, "ip", "link", "set", "dev", "lo", "up"])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to bring loopback interface UP"));
    }

    // 10. Inside guest netns: add default route pointing to host IP
    let status = Command::new("ip")
        .args(&["netns", "exec", &netns_name, "ip", "route", "add", "default", "via", &host_ip])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to add default route inside guest"));
    }

    // 11. On host: assign IP to veth_host
    let status = Command::new("ip")
        .args(&["addr", "add", &format!("{}/24", host_ip), "dev", &veth_host])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to set host IP address on veth"));
    }

    // 12. On host: bring veth_host up
    let status = Command::new("ip")
        .args(&["link", "set", "dev", &veth_host, "up"])
        .status()?;
    if !status.success() {
        let _ = cleanup_network(name);
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to bring host veth interface UP"));
    }

    // 13. Enable IP forwarding and unprivileged ping sockets on the host
    let _ = Command::new("sysctl")
        .args(&["-w", "net.ipv4.ip_forward=1"])
        .status();
    let _ = Command::new("sysctl")
        .args(&["-w", "net.ipv4.ping_group_range=0 2147483647"])
        .status();

    // 14. Configure isolated nftables NAT table on host
    let table_name = format!("lsl-{}", name);
    let _ = Command::new("nft").args(&["delete", "table", "ip", &table_name]).status();
    
    let status1 = Command::new("nft").args(&["add", "table", "ip", &table_name]).status()?;
    let status2 = Command::new("nft").args(&["add", "chain", "ip", &table_name, "nat", "{ type nat hook postrouting priority 100 ; }"]).status()?;
    let status3 = Command::new("nft").args(&["add", "rule", "ip", &table_name, "nat", "ip", "saddr", &subnet_cidr, "masquerade"]).status()?;

    if !status1.success() || !status2.success() || !status3.success() {
        println!("{}", "Warning: nftables NAT configuration failed. Subsystem may not have internet access.".yellow());
    } else {
        println!("{}", "nftables NAT rules configured successfully.".green());
    }

    Ok(())
}

pub fn cleanup_network(name: &str) -> io::Result<()> {
    println!("{}", format!("Cleaning up network configuration for distro '{}'...", name).blue());

    let veth_host = format!("lsl-v-{}-0", name);
    let netns_name = format!("lsl-{}", name);
    let netns_path = format!("/var/run/netns/{}", netns_name);
    let table_name = format!("lsl-{}", name);

    // Kill any dhclient process associated with this network interface inside netns
    let _ = Command::new("pkill").args(&["-f", &format!("dhclient.*{}", netns_name)]).status();

    // 1. Delete nftables NAT table
    let _ = Command::new("nft").args(&["delete", "table", "ip", &table_name]).status();

    // 2. Delete host veth interface (automatically deletes guest end too)
    let _ = Command::new("ip").args(&["link", "del", &veth_host]).status();

    // 3. Unmount network namespace file if mounted
    if Path::new(&netns_path).exists() {
        let _ = Command::new("umount").args(&["-l", &netns_path]).status();
        let _ = fs::remove_file(&netns_path);
    }

    Ok(())
}

fn get_default_host_interface() -> io::Result<String> {
    let route_content = std::fs::read_to_string("/proc/net/route")?;
    for line in route_content.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1] == "00000000" {
            return Ok(parts[0].to_string());
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "No default host network interface found"))
}

pub fn get_default_route_details() -> io::Result<(String, String, String)> {
    let output = Command::new("ip")
        .args(&["route", "get", "1.1.1.1"])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "Failed to get default route")
    })?;

    let parts: Vec<&str> = line.split_whitespace().collect();
    let mut dev = String::new();
    let mut src = String::new();
    let mut via = String::new();

    for i in 0..parts.len() {
        if parts[i] == "dev" && i + 1 < parts.len() {
            dev = parts[i + 1].to_string();
        }
        if parts[i] == "src" && i + 1 < parts.len() {
            src = parts[i + 1].to_string();
        }
        if parts[i] == "via" && i + 1 < parts.len() {
            via = parts[i + 1].to_string();
        }
    }

    if dev.is_empty() || src.is_empty() {
        if let Ok(dev_proc) = get_default_host_interface() {
            return Ok((dev_proc, "10.0.0.1".to_string(), "10.0.0.1".to_string()));
        }
        return Err(io::Error::new(io::ErrorKind::Other, "Failed to parse default route details"));
    }

    Ok((dev, src, via))
}

fn are_ips_in_same_subnet(ip1: &str, ip2: &str) -> bool {
    let parts1: Vec<&str> = ip1.split('.').collect();
    let parts2: Vec<&str> = ip2.split('.').collect();
    if parts1.len() == 4 && parts2.len() == 4 {
        parts1[0..3] == parts2[0..3]
    } else {
        false
    }
}

fn find_free_lan_ip(host_ip: &str) -> String {
    let parts: Vec<&str> = host_ip.split('.').collect();
    if parts.len() == 4 {
        if let Ok(last) = parts[3].parse::<u32>() {
            for offset in &[100, 150, 200, 50, 80] {
                let candidate_last = (last + offset) % 254;
                let candidate_last = if candidate_last == 0 { 253 } else { candidate_last };
                let candidate_ip = format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], candidate_last);
                
                // Ping candidate to check if anyone is using it
                let ping_status = Command::new("ping")
                    .args(&["-c", "1", "-W", "1", &candidate_ip])
                    .status();
                if let Ok(status) = ping_status {
                    if !status.success() {
                        // No response, IP is free!
                        return candidate_ip;
                    }
                }
            }
        }
    }
    // Fallback
    format!("{}.{}.{}.222", parts[0], parts[1], parts[2])
}

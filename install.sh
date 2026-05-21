#!/usr/bin/env bash
#
# Linux Subsystem for Linux (LSL) Installer
# Automates compiling and installing LSL to /usr/local/bin
#

set -euo pipefail

# Color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}===================================================${NC}"
echo -e "${BLUE}    Linux Subsystem for Linux (LSL) Installer         ${NC}"
echo -e "${BLUE}===================================================${NC}"

# Check if Rust and Cargo are installed, install if missing
if ! command -v cargo &> /dev/null; then
    echo -e "${BLUE}[LSL] Rust/Cargo is not installed on this host. Installing now...${NC}"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # Source cargo environment variables to make cargo immediately available
    if [ -f "$HOME/.cargo/env" ]; then
        source "$HOME/.cargo/env"
    elif [ -f "/root/.cargo/env" ]; then
        source "/root/.cargo/env"
    fi
fi

echo -e "${BLUE}[LSL] Building release binary...${NC}"
cargo build --release

echo -e "${BLUE}[LSL] Copying binary to /usr/local/bin/lsl (requires sudo)...${NC}"
sudo rm -f /usr/local/bin/lsl
sudo cp target/release/lsl /usr/local/bin/lsl
# Set SUID permission (4755) so users can run lsl without sudo
sudo chmod 4755 /usr/local/bin/lsl

echo -e "${BLUE}[LSL] Setting up systemd user service for automatic boot...${NC}"
# Determine the real user (even if running under sudo)
REAL_USER="${SUDO_USER:-$(whoami)}"
REAL_HOME=$(eval echo "~$REAL_USER")

# Write systemd user service file
SYSTEMD_USER_DIR="$REAL_HOME/.config/systemd/user"
mkdir -p "$SYSTEMD_USER_DIR"
cat << 'EOF' > "$SYSTEMD_USER_DIR/lsl-autoboot.service"
[Unit]
Description=LSL Subsystem Autoboot and Mount Restoration
After=default.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/lsl boot
RemainAfterExit=yes

[Install]
WantedBy=default.target
EOF

# Fix ownership of user config files if installed with sudo
chown -R "$REAL_USER:" "$REAL_HOME/.config"

# Enable systemd user service and lingering for the real user
sudo -u "$REAL_USER" env XDG_RUNTIME_DIR="/run/user/$(id -u "$REAL_USER")" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$(id -u "$REAL_USER")/bus" systemctl --user daemon-reload
sudo -u "$REAL_USER" env XDG_RUNTIME_DIR="/run/user/$(id -u "$REAL_USER")" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$(id -u "$REAL_USER")/bus" systemctl --user enable lsl-autoboot.service
loginctl enable-linger "$REAL_USER"

echo -e "${GREEN}===================================================${NC}"
echo -e "${GREEN}  LSL successfully installed!                      ${NC}"
echo -e "${GREEN}  Type 'lsl' anywhere to manage subsystems.         ${NC}"
echo -e "${GREEN}===================================================${NC}"

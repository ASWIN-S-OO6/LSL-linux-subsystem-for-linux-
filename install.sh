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

echo -e "${GREEN}===================================================${NC}"
echo -e "${GREEN}  LSL successfully installed!                      ${NC}"
echo -e "${GREEN}  Type 'lsl' anywhere to manage subsystems.         ${NC}"
echo -e "${GREEN}===================================================${NC}"

#!/usr/bin/env bash
#
# Linux Subsystem Layer (LSL) Installer
# Automates compiling and installing LSL to /usr/local/bin
#

set -euo pipefail

# Color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}===================================================${NC}"
echo -e "${BLUE}    Linux Subsystem Layer (LSL) Installer         ${NC}"
echo -e "${BLUE}===================================================${NC}"

# Check if Rust and Cargo are installed
if ! command -v cargo &> /dev/null; then
    echo -e "${RED}Error: Rust/Cargo is not installed on this host.${NC}"
    echo -e "Please install Rust by running:"
    echo -e "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    echo -e "And then restart your terminal and re-run this script."
    exit 1
fi

echo -e "${BLUE}[LSL] Building release binary...${NC}"
cargo build --release

echo -e "${BLUE}[LSL] Copying binary to /usr/local/bin/lsl (requires sudo)...${NC}"
sudo rm -f /usr/local/bin/lsl
sudo cp target/release/lsl /usr/local/bin/lsl
sudo chmod +x /usr/local/bin/lsl

echo -e "${GREEN}===================================================${NC}"
echo -e "${GREEN}  LSL successfully installed!                      ${NC}"
echo -e "${GREEN}  Type 'lsl' anywhere to manage subsystems.         ${NC}"
echo -e "${GREEN}===================================================${NC}"

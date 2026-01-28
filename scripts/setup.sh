#!/usr/bin/env bash

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Function to print section headers
print_section() {
    echo ""
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}$1${NC}"
    echo -e "${BLUE}========================================${NC}"
}

# Function to check if a command exists
check_command() {
    if ! command -v $1 &> /dev/null; then
        echo -e "${RED}✗ $1 is not installed${NC}"
        return 1
    else
        echo -e "${GREEN}✓ $1 is installed${NC}"
        return 0
    fi
}

# Check prerequisites
print_section "Checking Prerequisites"

MISSING_DEPS=0

if ! check_command "cargo"; then
    echo "  Install from: https://rustup.rs/"
    MISSING_DEPS=1
fi

if ! check_command "anvil"; then
    echo "  Install Foundry from: https://book.getfoundry.sh/getting-started/installation"
    MISSING_DEPS=1
fi

if ! check_command "forge"; then
    echo "  Install Foundry from: https://book.getfoundry.sh/getting-started/installation"
    MISSING_DEPS=1
fi

if ! check_command "cast"; then
    echo "  Install Foundry from: https://book.getfoundry.sh/getting-started/installation"
    MISSING_DEPS=1
fi

if ! check_command "node"; then
    echo "  Install from: https://nodejs.org/"
    MISSING_DEPS=1
fi

if ! check_command "yarn"; then
    echo "  Install with: npm install -g yarn"
    MISSING_DEPS=1
fi

if [ $MISSING_DEPS -eq 1 ]; then
    echo ""
    echo -e "${RED}Missing required dependencies. Please install them and run this script again.${NC}"
    exit 1
fi

# Check optional dependencies
echo ""
echo "Optional dependencies:"
if check_command "cargo-nextest"; then
    echo "  (for running tests)"
else
    echo "  Install with: cargo install cargo-nextest"
fi

# Initialize submodules
print_section "Initializing Submodules"

cd "${ROOT_DIR}"

if [ ! -f "zksync-os-server/Cargo.toml" ] || [ ! -f "zksync-era/Cargo.toml" ]; then
    echo "Initializing submodules..."
    git submodule update --init --recursive
    echo -e "${GREEN}✓ Submodules initialized${NC}"
else
    echo -e "${GREEN}✓ Submodules already initialized${NC}"
fi

# Install zkstackup if not already installed
print_section "Installing zkstackup"

if command -v zkstackup &> /dev/null; then
    echo -e "${GREEN}✓ zkstackup already installed${NC}"
else
    echo "Installing zkstackup..."
    cd "${ROOT_DIR}/zksync-era"
    cargo install --path zkstack_cli/crates/zkstackup --force
    echo -e "${GREEN}✓ zkstackup installed${NC}"
fi

# Build zksync-os-server
print_section "Building zksync-os-server"

cd "${ROOT_DIR}/zksync-os-server"

if [ -f "target/release/zksync-os-server" ]; then
    echo -e "${YELLOW}zksync-os-server binary already exists${NC}"
    read -p "Rebuild? (y/N): " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        cargo build --release
        echo -e "${GREEN}✓ zksync-os-server rebuilt${NC}"
    else
        echo "Skipping rebuild"
    fi
else
    echo "Building zksync-os-server (this may take a while)..."
    cargo build --release
    echo -e "${GREEN}✓ zksync-os-server built${NC}"
fi

# Install zkstack
print_section "Installing zkstack"

cd "${ROOT_DIR}/zksync-era"

echo "Running zkstackup --local (this may take a while)..."
zkstackup --local
echo -e "${GREEN}✓ zkstack installed${NC}"

# Install contracts dependencies
print_section "Installing Contract Dependencies"

cd "${ROOT_DIR}/zksync-era/contracts/l1-contracts"

if [ -d "node_modules" ]; then
    echo -e "${YELLOW}node_modules already exists${NC}"
    read -p "Reinstall? (y/N): " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        yarn install
        echo -e "${GREEN}✓ Dependencies reinstalled${NC}"
    else
        echo "Skipping reinstall"
    fi
else
    echo "Installing yarn dependencies..."
    yarn install
    echo -e "${GREEN}✓ Dependencies installed${NC}"
fi

# Summary
print_section "Setup Complete!"

cd "${ROOT_DIR}"

echo ""
echo -e "${GREEN}✓ All components built successfully!${NC}"
echo ""
echo "Binaries created:"
echo "  - zksync-os-server: ${ROOT_DIR}/zksync-os-server/target/release/zksync-os-server"
echo "  - zkstack:          ${ROOT_DIR}/zksync-era/target/release/zkstack"
echo ""
echo "Next steps:"
echo "  1. Run the upgrade test: ${YELLOW}./scripts/run-upgrade-local.sh${NC}"
echo "  2. Or run manually following the README: ${YELLOW}./scripts/README.md${NC}"
echo ""
echo "To add zkstack to your PATH, run:"
echo "  ${YELLOW}export PATH=\"${ROOT_DIR}/zksync-era/target/release:\$PATH\"${NC}"

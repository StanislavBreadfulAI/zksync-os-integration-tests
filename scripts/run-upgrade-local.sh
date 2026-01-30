#!/usr/bin/env bash

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Configuration
UPGRADE_VERSION="v31-interop-b"
UPGRADE_FILE_EXTENSION="v31"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
LOGS_DIR="${ROOT_DIR}/logs"

# Create logs directory
mkdir -p "${LOGS_DIR}"

# Cleanup function
cleanup() {
    printf "${YELLOW}Cleaning up processes...${NC}\n"
    # Stop and remove Docker containers
    docker stop zksync-os-server-v30 2>/dev/null || true
    docker rm zksync-os-server-v30 2>/dev/null || true
    docker stop zksync-os-server-final 2>/dev/null || true
    docker rm zksync-os-server-final 2>/dev/null || true
    # Note: Anvil is managed by the Rust test and automatically cleaned up
    printf "${GREEN}Cleanup complete${NC}\n"
}

# Set up trap for cleanup
trap cleanup EXIT INT TERM

# Function to print section headers
print_section() {
    echo ""
    printf "${GREEN}========================================${NC}\n"
    printf "${GREEN}%s${NC}\n" "$1"
    printf "${GREEN}========================================${NC}\n"
}

# Function to check if a command exists
check_command() {
    if ! command -v $1 &> /dev/null; then
        printf "${RED}Error: %s is not installed${NC}\n" "$1"
        exit 1
    fi
}

# Check prerequisites
print_section "Checking prerequisites"
check_command "anvil"
check_command "forge"
check_command "cast"
check_command "cargo"
check_command "yarn"

# Check if submodules are initialized
echo "Checking submodules..."
echo "Root directory: ${ROOT_DIR}"

if [ ! -d "${ROOT_DIR}/zksync-os-server" ]; then
    printf "${RED}Error: zksync-os-server directory not found${NC}\n"
    echo "Run: git submodule update --init --recursive"
    exit 1
fi

if [ ! -f "${ROOT_DIR}/zksync-os-server/Cargo.toml" ]; then
    printf "${RED}Error: zksync-os-server submodule not initialized${NC}\n"
    echo "Run: git submodule update --init --recursive"
    exit 1
fi

if [ ! -d "${ROOT_DIR}/zksync-era" ]; then
    printf "${RED}Error: zksync-era directory not found${NC}\n"
    echo "Run: git submodule update --init --recursive"
    exit 1
fi

# Check for zkstack_cli/Cargo.toml since there's no root Cargo.toml
if [ ! -f "${ROOT_DIR}/zksync-era/zkstack_cli/Cargo.toml" ]; then
    printf "${RED}Error: zksync-era submodule not initialized${NC}\n"
    echo "Run: git submodule update --init --recursive"
    exit 1
fi

printf "${GREEN}✓ Submodules check passed${NC}\n"

# Pull zksync-os-server Docker image
print_section "Pulling zksync-os-server Docker image"
docker pull ghcr.io/matter-labs/zksync-os-server:latest
echo "Docker image pulled successfully"

# Install/update zkstack
print_section "Installing zkstack"
cd "${ROOT_DIR}/zksync-era"
echo "Running zkstackup install..."
./zkstack_cli/zkstackup/install --path ./zkstack_cli/zkstackup/zkstackup
echo "Running zkstackup --local..."
zkstackup --local || true
echo "zkstack installed"

zkstack --version

# Clean and start zksync-os-server on v30.2
# Note: Anvil L1 is now started automatically by the upgrade test
print_section "Starting zksync-os-server on v30.2"
cd "${ROOT_DIR}/zksync-os-server"

# Clean up any existing database
rm -rf db/*
echo "Database cleaned"

# Start zksync-os-server with v30.2 configuration using Docker
docker run -d \
    --name zksync-os-server-v30 \
    -v "${ROOT_DIR}/zksync-os-server/local-chains/v30.2/default:/config:ro" \
    -v "${ROOT_DIR}/zksync-os-server/db:/db" \
    -p 3050:3050 \
    -p 3312:3312 \
    --network host \
    ghcr.io/matter-labs/zksync-os-server:latest \
    --config /config/config.yaml \
    &> "${LOGS_DIR}/zksync-os-server-v30.log"

echo "ZKsync OS Server started (container: zksync-os-server-v30)"
echo "Logs: ${LOGS_DIR}/zksync-os-server-v30.log"

sleep 10

# Check if server is running
if ! curl -s http://localhost:3050 > /dev/null; then
    printf "${RED}Warning: Server might not be responding on port 3050${NC}\n"
else
    printf "${GREEN}Server is responding${NC}\n"
fi

# Stop v30.2 server before upgrade
print_section "Stopping v30.2 server before upgrade"
docker stop zksync-os-server-v30 || true
docker rm zksync-os-server-v30 || true
sleep 2

# Run the upgrade test (this does all the upgrade steps)
# Note: The test automatically starts and manages the Anvil L1 node
print_section "Running upgrade test"
cd "${ROOT_DIR}"
cargo test --test upgrade-tests test_v30_to_v31_upgrade -- --ignored --nocapture

# Restart zksync-os-server after upgrade
print_section "Restarting zksync-os-server after upgrade"
cd "${ROOT_DIR}/zksync-os-server"

# Restart server - it will detect upgraded protocol from L1 using Docker
docker run -d \
    --name zksync-os-server-final \
    -v "${ROOT_DIR}/zksync-os-server/local-chains/v30.2/default:/config:ro" \
    -v "${ROOT_DIR}/zksync-os-server/db:/db" \
    -p 3050:3050 \
    -p 3312:3312 \
    --network host \
    ghcr.io/matter-labs/zksync-os-server:latest \
    --config /config/config.yaml \
    &> "${LOGS_DIR}/zksync-os-server-final.log"

echo "ZKsync OS Server restarted (container: zksync-os-server-final)"
echo "Logs: ${LOGS_DIR}/zksync-os-server-final.log"

sleep 10

# Check if server is running after upgrade
if ! curl -s http://localhost:3050 > /dev/null; then
    printf "${RED}Warning: Server might not be responding on port 3050 after upgrade${NC}\n"
else
    printf "${GREEN}Server is responding after upgrade${NC}\n"
fi

# Verify upgrade with test transaction
print_section "Verifying upgrade"
cd "${ROOT_DIR}"
cargo test --test upgrade-tests test_post_upgrade_verification -- --ignored --nocapture

# Summary
print_section "Upgrade Complete!"
echo ""
printf "${GREEN}Upgrade process completed successfully!${NC}\n"
echo ""
echo "Services running:"
echo "  - ZKsync OS Server:      http://localhost:3050"
echo ""
echo "Logs directory: ${LOGS_DIR}"
echo "  - anvil.log"
echo "  - zksync-os-server-v30.log"
echo "  - zksync-os-server-final.log"
echo ""
echo "To stop the server:"
echo "  docker stop zksync-os-server-final && docker rm zksync-os-server-final"
echo ""
printf "${YELLOW}Note: Services will continue running. Use Ctrl+C to stop.${NC}\n"

# Keep the script running so services stay up
echo ""
echo "Press Ctrl+C to stop all services and exit..."
wait

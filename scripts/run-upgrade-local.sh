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
    if [ -f /tmp/zksync_os_server.pid ]; then
        kill -9 $(cat /tmp/zksync_os_server.pid) 2>/dev/null || true
        rm /tmp/zksync_os_server.pid
    fi
    if [ -f /tmp/zksync_os_server_final.pid ]; then
        kill -9 $(cat /tmp/zksync_os_server_final.pid) 2>/dev/null || true
        rm /tmp/zksync_os_server_final.pid
    fi
    if [ -f /tmp/anvil.pid ]; then
        kill -9 $(cat /tmp/anvil.pid) 2>/dev/null || true
        rm /tmp/anvil.pid
    fi
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

# Build zksync-os-server if not already built
print_section "Building zksync-os-server"
if [ ! -f "${ROOT_DIR}/zksync-os-server/target/release/zksync-os-server" ]; then
    echo "Building zksync-os-server..."
    cd "${ROOT_DIR}/zksync-os-server"
    cargo build --release
else
    echo "zksync-os-server already built (skipping)"
fi

# Install/update zkstack
print_section "Installing zkstack"
cd "${ROOT_DIR}/zksync-era"
echo "Running zkstackup install..."
./zkstack_cli/zkstackup/install --path ./zkstack_cli/zkstackup/zkstackup
echo "Running zkstackup --local..."
zkstackup --local || true
echo "zkstack installed"

zkstack --version

# Start Anvil with v30.2 L1 state
print_section "Starting Anvil L1 chain with v30.2 state"
anvil --load-state "${ROOT_DIR}/zksync-os-server/local-chains/v30.2/default/zkos-l1-state.json" \
    --port 8545 \
    &> "${LOGS_DIR}/anvil.log" &
echo $! > /tmp/anvil.pid
echo "Anvil started (PID: $(cat /tmp/anvil.pid))"
echo "Logs: ${LOGS_DIR}/anvil.log"
sleep 5

# Clean and start zksync-os-server on v30.2
print_section "Starting zksync-os-server on v30.2"
cd "${ROOT_DIR}/zksync-os-server"

# Clean up any existing database
rm -rf db/*
echo "Database cleaned"

# Start zksync-os-server with v30.2 configuration
./target/release/zksync-os-server \
    --config ./local-chains/v30.2/default/config.yaml \
    &> "${LOGS_DIR}/zksync-os-server-v30.log" &
echo $! > /tmp/zksync_os_server.pid
echo "ZKsync OS Server started (PID: $(cat /tmp/zksync_os_server.pid))"
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
kill -9 $(cat /tmp/zksync_os_server.pid) || true
rm /tmp/zksync_os_server.pid
sleep 2

# Setup zkstack chain configuration in zksync-era
print_section "Setting up zkstack chain configuration"
cd "${ROOT_DIR}/zksync-era"

# Create chains directory and copy v30.2 configs
mkdir -p chains/era/configs

# Copy configs from zksync-os-server v30.2
cp ../zksync-os-server/local-chains/v30.2/default/config.yaml chains/era/ZkStack.yaml
cp ../zksync-os-server/local-chains/v30.2/default/wallets.yaml chains/era/configs/wallets.yaml
cp ../zksync-os-server/local-chains/v30.2/default/contracts.yaml chains/era/configs/contracts.yaml

# Create general.yaml
cat > chains/era/configs/general.yaml <<EOF
api:
  web3_json_rpc:
    http_port: 3050
  prometheus:
    listener_port: 3312
  merkle_tree:
    port: 3053
data_handler:
  http_port: 3124
l2_chain_id: 6565
EOF

echo "zkstack configuration created in ${ROOT_DIR}/zksync-era/chains/era"

# Compile v31 contracts
print_section "Compiling v31 contracts"
zkstack dev contracts

# Update permanent values for upgrade
print_section "Updating permanent values for upgrade"
cd "${ROOT_DIR}/zksync-era"

# Get values from v30.2 config
BRIDGEHUB_ADDR=$(awk '/bridgehub_proxy_addr:/ {print $2; exit}' chains/era/configs/contracts.yaml)
ERA_CHAIN_ID=$(awk '/l2_chain_id:/ {print $2; exit}' chains/era/configs/general.yaml)
CTM_ADDR=$(awk '/state_transition_proxy_addr:/ {print $2; exit}' chains/era/configs/contracts.yaml)
BYTECODES_SUPPLIER=$(awk '/l1_bytecodes_supplier_addr:/ {print $2; exit}' chains/era/configs/contracts.yaml)
CREATE2_FACTORY=$(awk '/create2_factory_addr:/ {print $2; exit}' chains/era/configs/contracts.yaml)
CREATE2_SALT=$(awk '/create2_factory_salt:/ {print $2; exit}' chains/era/configs/contracts.yaml)

echo "Chain ID: ${ERA_CHAIN_ID}"
echo "Bridgehub: ${BRIDGEHUB_ADDR}"
echo "CTM: ${CTM_ADDR}"

# Create permanent-values.toml
mkdir -p contracts/l1-contracts/upgrade-envs/permanent-values

cat > contracts/l1-contracts/script-config/permanent-values.toml <<EOF
era_chain_id = ${ERA_CHAIN_ID}

[core_contracts]
bridgehub_proxy_addr = "${BRIDGEHUB_ADDR}"

[ctm_contracts]
ctm_proxy_addr = "${CTM_ADDR}"
l1_bytecodes_supplier_addr = "${BYTECODES_SUPPLIER}"

[permanent_contracts]
create2_factory_addr = "${CREATE2_FACTORY}"
create2_factory_salt = "${CREATE2_SALT}"
EOF

cp contracts/l1-contracts/script-config/permanent-values.toml \
   contracts/l1-contracts/upgrade-envs/permanent-values/local.toml

echo "Permanent values updated"

# Run ecosystem upgrade stages
print_section "Running ecosystem upgrade - Stage 0 (no-governance-prepare)"
cd "${ROOT_DIR}/zksync-era"
zkstack dev run-ecosystem-upgrade \
    --upgrade-version ${UPGRADE_VERSION} \
    --ecosystem-upgrade-stage no-governance-prepare

print_section "Running ecosystem upgrade - ecosystem-admin"
zkstack dev run-ecosystem-upgrade \
    --upgrade-version ${UPGRADE_VERSION} \
    --ecosystem-upgrade-stage ecosystem-admin

print_section "Running ecosystem upgrade - Stage 0 (governance)"
zkstack dev run-ecosystem-upgrade \
    --upgrade-version ${UPGRADE_VERSION} \
    --ecosystem-upgrade-stage governance-stage0

print_section "Running ecosystem upgrade - Stage 1"
zkstack dev run-ecosystem-upgrade \
    --upgrade-version ${UPGRADE_VERSION} \
    --ecosystem-upgrade-stage governance-stage1

# Generate upgrade YAML output
print_section "Generating upgrade YAML output"
cd "${ROOT_DIR}/zksync-era/contracts/l1-contracts"
UPGRADE_ECOSYSTEM_OUTPUT=script-out/v31-upgrade-ecosystem.toml \
UPGRADE_ECOSYSTEM_OUTPUT_TRANSACTIONS=broadcast/EcosystemUpgrade_v31.s.sol/9/run-latest.json \
YAML_OUTPUT_FILE=script-out/v31-local-output.yaml \
yarn upgrade-yaml-output-generator

# Run chain upgrade
print_section "Running chain upgrade"
cd "${ROOT_DIR}/zksync-era"
zkstack dev run-chain-upgrade \
    --upgrade-version ${UPGRADE_VERSION} \
    --force-display-finalization-params=true \
    --dangerous-local-default-overrides=true \
    --chain era

# Run ecosystem upgrade Stage 2
print_section "Running ecosystem upgrade - Stage 2"
zkstack dev run-ecosystem-upgrade \
    --upgrade-version ${UPGRADE_VERSION} \
    --ecosystem-upgrade-stage governance-stage2

# Run ecosystem upgrade Stage 3 (migrate token balances)
print_section "Running ecosystem upgrade - Stage 3 (migrate token balances)"
cd "${ROOT_DIR}/zksync-era/contracts/l1-contracts"
forge script deploy-scripts/upgrade/v31/EcosystemUpgrade_v31.s.sol:EcosystemUpgrade_v31 \
    --sig "stage3()" \
    --rpc-url http://localhost:8545 \
    --broadcast \
    --private-key 0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110 \
    --legacy \
    --slow \
    --gas-price 50000000000

# Restart zksync-os-server after upgrade
print_section "Restarting zksync-os-server after upgrade"
cd "${ROOT_DIR}/zksync-os-server"

# Clean database to force fresh start with upgraded state
rm -rf db/*
echo "Database cleaned"

# Restart server - it will detect upgraded protocol from L1
./target/release/zksync-os-server \
    --config ./local-chains/v30.2/default/config.yaml \
    &> "${LOGS_DIR}/zksync-os-server-final.log" &
echo $! > /tmp/zksync_os_server_final.pid
echo "ZKsync OS Server restarted (PID: $(cat /tmp/zksync_os_server_final.pid))"
echo "Logs: ${LOGS_DIR}/zksync-os-server-final.log"

sleep 10

# Check if server is running after upgrade
if ! curl -s http://localhost:3050 > /dev/null; then
    printf "${RED}Warning: Server might not be responding on port 3050 after upgrade${NC}\n"
else
    printf "${GREEN}Server is responding after upgrade${NC}\n"
fi

# Test with a transaction
print_section "Testing with a transaction"
RICH_ACCOUNT=0x36615Cf349d7F6344891B1e7CA7C72883F5dc049
PRIVATE_KEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110

echo "Sending test transaction..."
if cast send 0x5A67EE02274D9Ec050d412b96fE810Be4D71e7A0 \
    --value 100 \
    --private-key ${PRIVATE_KEY} \
    --rpc-url http://localhost:3050; then
    printf "${GREEN}Test transaction sent successfully!${NC}\n"
else
    printf "${RED}Test transaction failed${NC}\n"
fi

# Summary
print_section "Upgrade Complete!"
echo ""
printf "${GREEN}Upgrade process completed successfully!${NC}\n"
echo ""
echo "Services running:"
echo "  - Anvil L1:              http://localhost:8545"
echo "  - ZKsync OS Server:      http://localhost:3050"
echo ""
echo "Logs directory: ${LOGS_DIR}"
echo "  - anvil.log"
echo "  - zksync-os-server-v30.log"
echo "  - zksync-os-server-final.log"
echo ""
echo "To stop all services, press Ctrl+C or run:"
echo "  kill \$(cat /tmp/zksync_os_server_final.pid /tmp/anvil.pid 2>/dev/null)"
echo ""
printf "${YELLOW}Note: Services will continue running. Use cleanup() or Ctrl+C to stop.${NC}\n"

# Keep the script running so services stay up
echo ""
echo "Press Ctrl+C to stop all services and exit..."
wait

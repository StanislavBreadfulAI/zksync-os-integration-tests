# Upgrade Testing Scripts

This directory contains scripts for testing ZKsync OS protocol upgrades locally.

## Prerequisites

Before running the upgrade script, ensure you have the following installed:

- **Docker**: For running zksync-os-server container
- **Rust**: `rustup` with the toolchain specified in `rust-toolchain.toml` (for building zkstack)
- **Foundry**: `forge`, `cast`, and `anvil` from [Foundry](https://book.getfoundry.sh/getting-started/installation)
- **Foundry-ZKsync**: [foundry-zksync](https://github.com/matter-labs/foundry-zksync)
- **Node.js**: Version 22 or later
- **Yarn**: `npm install -g yarn`

**Note:** zkstackup will be installed automatically by the setup script. The zksync-os-server Docker image will be pulled automatically.

## Setup

1. **Clone the repository with submodules**:
   ```bash
   git clone --recursive <repository-url>
   cd zksync-os-integration-tests
   ```

   Or if already cloned, initialize submodules:
   ```bash
   git submodule update --init --recursive
   ```

2. **Run the setup script** (optional - the upgrade script will setup if needed):
   ```bash
   ./scripts/setup.sh
   ```

   This will:
   - Pull the zksync-os-server Docker image
   - Install zkstackup and build zkstack
   - Install contract dependencies

## Running the Upgrade

### Quick Start

**Option 1: Run the Rust test directly (recommended for development)**

The test is now fully self-contained and manages the Anvil L1 node automatically:

```bash
# Build the zksync-os-server binary first (required by the workflow)
cd zksync-os-server
cargo build --release
cd ..

# Run the upgrade test (automatically starts and manages Anvil)
cargo test --test upgrade-tests test_v30_to_v31_upgrade -- --ignored --nocapture
```

**Option 2: Run the full Docker-based script**

```bash
# Run the full local upgrade with Docker containers
bash ./scripts/run-upgrade-local.sh
```

**Note:** The upgrade logic is now implemented as a Rust test in `tests/upgrade-tests.rs`. The test automatically:
1. Starts Anvil L1 with v30.2 state
2. Sets up zkstack configuration
3. Compiles v31 contracts
4. Updates permanent values
5. Executes all upgrade stages (v30.2 → v31)
6. Cleans the database for fresh restart
7. Stops Anvil automatically when done

After the test completes, manually restart the server and verify:

```bash
# Restart server with upgraded protocol
cd zksync-os-server
./target/release/zksync-os-server --config ./local-chains/v30.2/default/config.yaml &

# Verify the upgrade
cargo test --test upgrade-tests test_post_upgrade_verification -- --ignored --nocapture
```

### What the Script Does

The script follows these steps:

1. **Environment Check**: Verifies all required tools are installed
2. **Setup Phase**: Pulls zksync-os-server Docker image and builds zkstack if needed
3. **Start v30.2 Chain**:
   - Starts Anvil with pre-configured v30.2 L1 state
   - Starts zksync-os-server Docker container with v30.2 configuration
4. **Prepare Upgrade**:
   - Sets up zkstack chain configuration from v30.2 state
   - Compiles v31 contracts
   - Updates permanent values
5. **Execute Upgrade**:
   - Runs ecosystem upgrade stages (no-governance-prepare, ecosystem-admin, governance stages 0-3)
   - Runs chain upgrade
   - Executes token balance migration
6. **Verify Upgrade**:
   - Restarts zksync-os-server (detects upgraded protocol from L1)
   - Sends test transaction to verify functionality

### Logs

All logs are stored in the `logs/` directory:
- `anvil.log` - L1 chain logs
- `zksync-os-server-v30.log` - Initial v30.2 server logs
- `zksync-os-server-final.log` - Post-upgrade server logs

### Services

After the upgrade completes, the following services will be running:

- **Anvil L1**: `http://localhost:8545`
- **ZKsync OS Server**: `http://localhost:3050`
- **Prometheus Metrics**: `http://localhost:3312`

### Cleanup

The script will automatically clean up when you press `Ctrl+C` or when it exits.

To manually stop services:
```bash
kill $(cat /tmp/zksync_os_server_final.pid /tmp/anvil.pid 2>/dev/null)
```

## Troubleshooting

### Port Already in Use

If ports 8545 or 3050 are already in use:
```bash
# Find processes using the ports
lsof -i :8545
lsof -i :3050

# Kill them (or stop Docker containers)
kill -9 <PID>
docker stop zksync-os-server-v30 zksync-os-server-final
```

### Docker Issues

If the Docker container fails to start:
```bash
# Pull the latest image
docker pull ghcr.io/matter-labs/zksync-os-server:latest

# Clean up old containers
docker stop zksync-os-server-v30 zksync-os-server-final || true
docker rm zksync-os-server-v30 zksync-os-server-final || true
```

### Build Failures (zkstack)

If zkstack fails to build:
```bash
# Rebuild zkstack
cd zksync-era
zkstackup --local
cd ..
```

### Submodule Issues

If submodules are not properly initialized:
```bash
git submodule update --init --recursive --force
```

### Server Not Responding

If the server doesn't respond after startup:
```bash
# Check logs
tail -f logs/zksync-os-server-final.log

# Or check Docker logs
docker logs zksync-os-server-final

# Try restarting manually
docker stop zksync-os-server-final && docker rm zksync-os-server-final
cd zksync-os-server && rm -rf db/*
docker run -d --name zksync-os-server-final \
  -v $(pwd)/local-chains/v30.2/default:/config:ro \
  -v $(pwd)/db:/db \
  -p 3050:3050 -p 3312:3312 --network host \
  ghcr.io/matter-labs/zksync-os-server:latest \
  --config /config/config.yaml
```

## Testing Manually

After the upgrade completes, you can interact with the chain:

```bash
# Check server health
curl http://localhost:3050

# Send a transaction
PRIVATE_KEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110
cast send 0x5A67EE02274D9Ec050d412b96fE810Be4D71e7A0 \
  --value 100 \
  --private-key ${PRIVATE_KEY} \
  --rpc-url http://localhost:3050

# Check balance
cast balance 0x36615Cf349d7F6344891B1e7CA7C72883F5dc049 \
  --rpc-url http://localhost:3050
```

## Running Integration Tests

After the upgrade completes, you can run the integration tests:

```bash
cd zksync-os-server
cargo nextest run --profile ci -p zksync_os_integration_tests
```

## Running in CI

The GitHub Actions workflow (`.github/workflows/upgrade-test.yaml`) automates the entire upgrade process:

1. **Framework Setup**: Installs Rust, Foundry, Node.js, zkstack, etc.
2. **Infrastructure**: Starts Anvil L1 and zksync-os-server on v30.2
3. **Upgrade Test**: Runs `cargo test --test upgrade-tests test_v30_to_v31_upgrade`
4. **Server Restart**: Restarts server with upgraded protocol
5. **Verification**: Runs `cargo test --test upgrade-tests test_post_upgrade_verification`

All upgrade logic is centralized in `tests/upgrade-tests.rs`, eliminating duplication between the workflow and local scripts.

## Advanced Usage

### Running Specific Upgrade Stages

You can modify the script to run only specific upgrade stages by commenting out sections you don't need.

### Changing Upgrade Version

Edit the `UPGRADE_VERSION` variable at the top of the script:
```bash
UPGRADE_VERSION="v31-interop-b"  # Change to your target version
```

### Keeping Services Running

The script will keep services running until you press `Ctrl+C`. This allows you to:
- Manually test the upgraded chain
- Run integration tests
- Inspect logs
- Interact with the RPC

## Related Files

- `.github/workflows/upgrade-test.yaml` - CI workflow (runs similar steps in GitHub Actions)
- `../upgrade-testing/era-cacher/do-upgrade.sh` - Original upgrade script reference

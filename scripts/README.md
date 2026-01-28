# Upgrade Testing Scripts

This directory contains scripts for testing ZKsync OS protocol upgrades locally.

## Prerequisites

Before running the upgrade script, ensure you have the following installed:

- **Rust**: `rustup` with the toolchain specified in `rust-toolchain.toml`
- **Foundry**: `forge`, `cast`, and `anvil` from [Foundry](https://book.getfoundry.sh/getting-started/installation)
- **Foundry-ZKsync**: [foundry-zksync](https://github.com/matter-labs/foundry-zksync)
- **Node.js**: Version 22 or later
- **Yarn**: `npm install -g yarn`
- **cargo-nextest**: `cargo install cargo-nextest` (optional, for running tests)

**Note:** zkstackup will be installed automatically by the setup script.

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

2. **Build the components** (optional - the script will build if needed):
   ```bash
   # Build zksync-os-server
   cd zksync-os-server
   cargo build --release
   cd ..

   # Build zkstack
   cd zksync-era
   cargo build --release --bin zkstack
   cd ..
   ```

## Running the Upgrade

### Quick Start

Simply run the script with bash:

```bash
bash ./scripts/run-upgrade-local.sh
```

Or make sure it runs with bash:

```bash
./scripts/run-upgrade-local.sh
```

**Note:** Do not run with `sh` - the script requires bash features.

The script will:
1. Check prerequisites and build necessary components
2. Start Anvil with v30.2 L1 state
3. Start zksync-os-server on v30.2
4. Set up zkstack configuration
5. Execute all upgrade stages (v30.2 → v31)
6. Restart zksync-os-server with upgraded protocol
7. Run a test transaction to verify the upgrade

### What the Script Does

The script follows these steps:

1. **Environment Check**: Verifies all required tools are installed
2. **Build Phase**: Builds `zksync-os-server` and `zkstack` if not already built
3. **Start v30.2 Chain**:
   - Starts Anvil with pre-configured v30.2 L1 state
   - Starts zksync-os-server with v30.2 configuration
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

# Kill them
kill -9 <PID>
```

### Build Failures

If the script fails during build:
```bash
# Clean and rebuild zksync-os-server
cd zksync-os-server
cargo clean
cargo build --release
cd ..

# Clean and rebuild zkstack
cd zksync-era
cargo clean
cargo build --release --bin zkstack
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

# Try restarting manually
cd zksync-os-server
rm -rf db/*
./target/release/zksync-os-server --config ./local-chains/v30.2/default/config.yaml
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

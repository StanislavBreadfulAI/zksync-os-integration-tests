use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const UPGRADE_VERSION: &str = "v31-interop-b";
const RICH_ACCOUNT_PRIVATE_KEY: &str = "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";

/// Helper to get project root directory
fn get_project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Helper to run a command and return its output
fn run_command(name: &str, cmd: &mut Command) -> Result<()> {
    println!("Running: {}", name);
    let status = cmd
        .status()
        .with_context(|| format!("Failed to run: {}", name))?;

    if !status.success() {
        anyhow::bail!("{} failed with status: {}", name, status);
    }
    Ok(())
}

/// Setup zkstack chain configuration
fn setup_zkstack_configuration(root: &Path) -> Result<()> {
    let era_path = root.join("zksync-era");
    let os_server_path = root.join("zksync-os-server");

    // Create necessary directories
    fs::create_dir_all(era_path.join("chains/era/configs"))?;
    fs::create_dir_all(era_path.join("configs"))?;

    // Create ecosystem secrets.yaml
    fs::write(
        era_path.join("configs/secrets.yaml"),
        "l1:\n  l1_rpc_url: http://localhost:8545\n",
    )?;

    // Copy contracts.yaml from v30.2 to ecosystem level
    fs::copy(
        os_server_path.join("local-chains/v30.2/default/contracts.yaml"),
        era_path.join("configs/contracts.yaml"),
    )?;

    // Copy config files from zksync-os-server v30.2
    fs::copy(
        os_server_path.join("local-chains/v30.2/default/wallets.yaml"),
        era_path.join("chains/era/configs/wallets.yaml"),
    )?;
    fs::copy(
        os_server_path.join("local-chains/v30.2/default/contracts.yaml"),
        era_path.join("chains/era/configs/contracts.yaml"),
    )?;

    // Create general.yaml
    let general_yaml = r#"api:
  web3_json_rpc:
    http_port: 3050
  prometheus:
    listener_port: 3312
  merkle_tree:
    port: 3053
data_handler:
  http_port: 3124
l2_chain_id: 6565
"#;
    fs::write(era_path.join("chains/era/configs/general.yaml"), general_yaml)?;

    // Create ZkStack.yaml
    let zkstack_yaml = r#"id: 1
name: era
chain_id: 6565
prover_version: NoProofs
l1_network: Localhost
link_to_code: ../..
configs: ./configs
rocks_db_path: ../../zksync-os-server/db
l1_batch_commit_data_generator_mode: Rollup
base_token:
  address: "0x0000000000000000000000000000000000000001"
  nominator: 1
  denominator: 1
  kind: Native
wallet_creation: Localhost
evm_emulator: false
tight_ports: false
vm_option: ZKSyncOsVM
"#;
    fs::write(era_path.join("chains/era/ZkStack.yaml"), zkstack_yaml)?;

    println!("✓ zkstack configuration created");
    Ok(())
}

/// Compile v31 contracts
fn compile_contracts(root: &Path) -> Result<()> {
    run_command(
        "Compile v31 contracts",
        Command::new("zkstack")
            .args(["dev", "contracts"])
            .current_dir(root.join("zksync-era")),
    )
}

/// Update permanent values for upgrade
fn update_permanent_values(root: &Path) -> Result<()> {
    let era_path = root.join("zksync-era");

    // Read values from v30.2 config files
    let contracts_yaml = fs::read_to_string(era_path.join("chains/era/configs/contracts.yaml"))?;
    let general_yaml = fs::read_to_string(era_path.join("chains/era/configs/general.yaml"))?;

    // Parse values using simple string matching (could use proper YAML parser)
    let bridgehub_addr = extract_yaml_value(&contracts_yaml, "bridgehub_proxy_addr")?;
    let era_chain_id = extract_yaml_value(&general_yaml, "l2_chain_id")?;
    let ctm_addr = extract_yaml_value(&contracts_yaml, "state_transition_proxy_addr")?;
    let bytecodes_supplier = extract_yaml_value(&contracts_yaml, "l1_bytecodes_supplier_addr")?;
    let create2_factory = extract_yaml_value(&contracts_yaml, "create2_factory_addr")?;
    let create2_salt = extract_yaml_value(&contracts_yaml, "create2_factory_salt")?;

    println!("Chain ID: {}", era_chain_id);
    println!("Bridgehub: {}", bridgehub_addr);
    println!("CTM: {}", ctm_addr);

    // Create permanent-values.toml
    let permanent_values = format!(
        r#"era_chain_id = {}

[core_contracts]
bridgehub_proxy_addr = "{}"

[ctm_contracts]
ctm_proxy_addr = "{}"
l1_bytecodes_supplier_addr = "{}"

[permanent_contracts]
create2_factory_addr = "{}"
create2_factory_salt = "{}"
"#,
        era_chain_id,
        bridgehub_addr,
        ctm_addr,
        bytecodes_supplier,
        create2_factory,
        create2_salt
    );

    let script_config_dir = era_path.join("contracts/l1-contracts/script-config");
    fs::create_dir_all(&script_config_dir)?;
    fs::write(
        script_config_dir.join("permanent-values.toml"),
        &permanent_values,
    )?;

    let upgrade_envs_dir = era_path.join("contracts/l1-contracts/upgrade-envs/permanent-values");
    fs::create_dir_all(&upgrade_envs_dir)?;
    fs::write(upgrade_envs_dir.join("local.toml"), permanent_values)?;

    println!("✓ Permanent values updated");
    Ok(())
}

/// Extract a YAML value (simple parser for key: value format)
fn extract_yaml_value(yaml: &str, key: &str) -> Result<String> {
    for line in yaml.lines() {
        if line.trim_start().starts_with(key) {
            if let Some(value) = line.split(':').nth(1) {
                return Ok(value.trim().to_string());
            }
        }
    }
    anyhow::bail!("Could not find key '{}' in YAML", key)
}

/// Run ecosystem upgrade stages
fn run_ecosystem_upgrades(root: &Path) -> Result<()> {
    let era_path = root.join("zksync-era");

    // Stage 0: no-governance-prepare
    run_command(
        "Ecosystem upgrade - Stage 0 (no-governance-prepare)",
        Command::new("zkstack")
            .args([
                "dev",
                "run-ecosystem-upgrade",
                "--upgrade-version",
                UPGRADE_VERSION,
                "--ecosystem-upgrade-stage",
                "no-governance-prepare",
            ])
            .current_dir(&era_path),
    )?;

    // ecosystem-admin
    run_command(
        "Ecosystem upgrade - ecosystem-admin",
        Command::new("zkstack")
            .args([
                "dev",
                "run-ecosystem-upgrade",
                "--upgrade-version",
                UPGRADE_VERSION,
                "--ecosystem-upgrade-stage",
                "ecosystem-admin",
            ])
            .current_dir(&era_path),
    )?;

    // Stage 0: governance
    run_command(
        "Ecosystem upgrade - Stage 0 (governance)",
        Command::new("zkstack")
            .args([
                "dev",
                "run-ecosystem-upgrade",
                "--upgrade-version",
                UPGRADE_VERSION,
                "--ecosystem-upgrade-stage",
                "governance-stage0",
            ])
            .current_dir(&era_path),
    )?;

    // Stage 1
    run_command(
        "Ecosystem upgrade - Stage 1",
        Command::new("zkstack")
            .args([
                "dev",
                "run-ecosystem-upgrade",
                "--upgrade-version",
                UPGRADE_VERSION,
                "--ecosystem-upgrade-stage",
                "governance-stage1",
            ])
            .current_dir(&era_path),
    )?;

    Ok(())
}

/// Generate upgrade YAML output
fn generate_upgrade_yaml(root: &Path) -> Result<()> {
    let l1_contracts_path = root.join("zksync-era/contracts/l1-contracts");

    run_command(
        "Generate upgrade YAML output",
        Command::new("yarn")
            .arg("upgrade-yaml-output-generator")
            .current_dir(&l1_contracts_path)
            .env("UPGRADE_ECOSYSTEM_OUTPUT", "script-out/v31-upgrade-ecosystem.toml")
            .env(
                "UPGRADE_ECOSYSTEM_OUTPUT_TRANSACTIONS",
                "broadcast/EcosystemUpgrade_v31.s.sol/31337/run-latest.json",
            )
            .env("YAML_OUTPUT_FILE", "script-out/v31-local-output.yaml"),
    )
}

/// Run chain upgrade
fn run_chain_upgrade(root: &Path) -> Result<()> {
    run_command(
        "Chain upgrade",
        Command::new("zkstack")
            .args([
                "dev",
                "run-chain-upgrade",
                "--upgrade-version",
                UPGRADE_VERSION,
                "--force-display-finalization-params=true",
                "--dangerous-local-default-overrides=true",
                "--chain",
                "era",
            ])
            .current_dir(root.join("zksync-era")),
    )
}

/// Run ecosystem upgrade Stage 2 and Stage 3
fn run_final_upgrade_stages(root: &Path) -> Result<()> {
    let era_path = root.join("zksync-era");

    // Stage 2
    run_command(
        "Ecosystem upgrade - Stage 2",
        Command::new("zkstack")
            .args([
                "dev",
                "run-ecosystem-upgrade",
                "--upgrade-version",
                UPGRADE_VERSION,
                "--ecosystem-upgrade-stage",
                "governance-stage2",
            ])
            .current_dir(&era_path),
    )?;

    // Stage 3 (migrate token balances)
    run_command(
        "Ecosystem upgrade - Stage 3 (migrate token balances)",
        Command::new("forge")
            .args([
                "script",
                "deploy-scripts/upgrade/v31/EcosystemUpgrade_v31.s.sol:EcosystemUpgrade_v31",
                "--sig",
                "stage3()",
                "--rpc-url",
                "http://localhost:8545",
                "--broadcast",
                "--private-key",
                RICH_ACCOUNT_PRIVATE_KEY,
                "--legacy",
                "--slow",
                "--gas-price",
                "50000000000",
            ])
            .current_dir(era_path.join("contracts/l1-contracts")),
    )
}

/// Test transaction after upgrade
fn test_transaction() -> Result<()> {
    println!("Sending test transaction...");
    run_command(
        "Test transaction",
        Command::new("cast")
            .args([
                "send",
                "0x5A67EE02274D9Ec050d412b96fE810Be4D71e7A0",
                "--value",
                "100",
                "--private-key",
                RICH_ACCOUNT_PRIVATE_KEY,
                "--rpc-url",
                "http://localhost:3050",
            ]),
    )?;
    println!("✓ Test transaction sent successfully");
    Ok(())
}

#[test]
#[ignore] // This is a long-running integration test, run with --ignored
fn test_v30_to_v31_upgrade() -> Result<()> {
    let root = get_project_root();

    println!("=== Starting v30 to v31 upgrade test ===\n");

    // Setup zkstack configuration
    setup_zkstack_configuration(&root)?;

    // Compile v31 contracts
    compile_contracts(&root)?;

    // Update permanent values for upgrade
    update_permanent_values(&root)?;

    // Run ecosystem upgrade stages
    run_ecosystem_upgrades(&root)?;

    // Generate upgrade YAML output
    generate_upgrade_yaml(&root)?;

    // Run chain upgrade
    run_chain_upgrade(&root)?;

    // Run final upgrade stages
    run_final_upgrade_stages(&root)?;

    // Clean database to force fresh start with upgraded state
    println!("Cleaning database...");
    let db_path = root.join("zksync-os-server/db");
    if db_path.exists() {
        fs::remove_dir_all(&db_path)?;
        fs::create_dir_all(&db_path)?;
    }

    println!("\n=== Upgrade test completed successfully! ===");
    println!("Note: Server needs to be restarted externally to apply the upgrade");
    println!("After restart, run test_post_upgrade_verification to verify");
    Ok(())
}

/// Verification test to run after server has been restarted with upgraded protocol
#[test]
#[ignore] // Run with --ignored after server restart
fn test_post_upgrade_verification() -> Result<()> {
    println!("=== Running post-upgrade verification ===\n");

    // Wait a bit for server to be fully ready
    println!("Waiting for server to be ready...");
    std::thread::sleep(std::time::Duration::from_secs(5));

    // Test transaction
    test_transaction()?;

    println!("\n=== Post-upgrade verification completed successfully! ===");
    Ok(())
}

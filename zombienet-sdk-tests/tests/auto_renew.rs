// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

//! Auto-renewal e2e test.
//!
//! Verifies that `pallet-transaction-storage` auto-renewal works end to end:
//!
//! 1. Shrink `RetentionPeriod` via sudo so expiration fires within test runtime.
//! 2. Authorize Alice for 2× `data.len()` bytes and store the data once — leaves exactly 1×
//!    `data.len()` in authorization, enough for one renewal cycle.
//! 3. Call `TransactionStorage::enable_auto_renew(content_hash)` signed by Alice.
//! 4. Wait for the block where `on_initialize` schedules expiring data for renewal (`store_block +
//!    RETENTION_PERIOD + 1`) and the mandatory `process_auto_renewals` inherent drains
//!    `PendingAutoRenewals`.
//! 5. Assert a `TransactionStorage::DataAutoRenewed` event fires at that block.
//!
//! ## Environment
//!
//! Same env vars as `parachain_sync_storage`:
//! - `POLKADOT_RELAY_BINARY_PATH`, `POLKADOT_PARACHAIN_BINARY_PATH`, `PARACHAIN_CHAIN_SPEC_PATH`,
//!   `RELAY_CHAIN`, `PARACHAIN_ID`, `PARACHAIN_CHAIN_ID`.
//!
//! ## Running
//!
//! ```bash
//! POLKADOT_RELAY_BINARY_PATH=~/local_bulletin_testing/bin/polkadot \
//! POLKADOT_PARACHAIN_BINARY_PATH=~/local_bulletin_testing/bin/polkadot-omni-node \
//! PARACHAIN_CHAIN_SPEC_PATH=./zombienet/bulletin-westend-spec.json \
//!   cargo test -p bulletin-chain-zombienet-sdk-tests \
//!   --features bulletin-chain-zombienet-sdk-tests/auto-renew-tests \
//!   auto_renewal_happy_path_test
//! ```

use crate::{
	test_log,
	utils::{
		authorize_and_store_data, blake2_256, build_parachain_network_config_single_collator,
		content_hash_and_cid, generate_test_data, get_alice_nonce, initialize_network,
		set_retention_period, verify_parachain_binaries, wait_for_in_best_block,
		wait_for_session_change_on_node, NETWORK_READY_TIMEOUT_SECS, NODE_LOG_CONFIG,
		PARACHAIN_TEST_DATA_PATTERN, TEST_DATA_SIZE, TRANSACTION_TIMEOUT_SECS,
	},
};
use anyhow::{anyhow, Context, Result};
use env_logger::Env;
use std::time::Duration;
use subxt::{
	config::substrate::{SubstrateConfig, SubstrateExtrinsicParamsBuilder},
	dynamic::{tx, Value},
	OnlineClient,
};
use subxt_signer::sr25519::dev;

/// Session changes are critical for parachain block production.
const SESSION_CHANGE_TIMEOUT_SECS: u64 = 300;
/// Small retention so the first expiration fires within test runtime.
const RETENTION_PERIOD: u32 = 10;
/// Upper bound on how long to wait for the first `DataAutoRenewed` event after
/// `enable_auto_renew`. Allows for `RETENTION_PERIOD` parachain blocks (~6s each
/// on westend) plus inherent scheduling and margin.
const AUTO_RENEW_WAIT_SECS: u64 = 240;

fn para_node_args() -> Vec<String> {
	vec![
		"--ipfs-server".into(),
		NODE_LOG_CONFIG.into(),
		// Arguments after "--" are passed to the embedded relay chain client.
		"--".into(),
		"--network-backend=libp2p".into(),
	]
}

/// Submit `TransactionStorage::enable_auto_renew(content_hash)` signed by Alice.
async fn enable_auto_renew(
	client: &OnlineClient<SubstrateConfig>,
	content_hash: &[u8; 32],
	nonce: u64,
) -> Result<()> {
	let signer = dev::alice();
	let call = tx(
		"StorageAutoRenewal",
		"enable_auto_renew",
		vec![Value::from_bytes(content_hash.as_slice())],
	);
	let params = SubstrateExtrinsicParamsBuilder::new().nonce(nonce).build();

	log::info!("Submitting enable_auto_renew (nonce={})...", nonce);

	tokio::time::timeout(Duration::from_secs(TRANSACTION_TIMEOUT_SECS), async {
		let progress = client.tx().sign_and_submit_then_watch(&call, &signer, params).await?;
		wait_for_in_best_block(progress).await?;
		Ok::<_, anyhow::Error>(())
	})
	.await
	.map_err(|_| anyhow!("enable_auto_renew transaction timed out"))??;

	log::info!("enable_auto_renew included in block");
	Ok(())
}

/// Subscribe to best blocks and wait for the first
/// `TransactionStorage::DataAutoRenewed` event. Returns the block number the event
/// fired in. Since the test only enables auto-renewal for one content hash, any
/// `DataAutoRenewed` event is necessarily ours.
async fn wait_for_data_auto_renewed(
	client: &OnlineClient<SubstrateConfig>,
	timeout_secs: u64,
) -> Result<u64> {
	let future = async {
		let mut blocks_sub =
			client.blocks().subscribe_best().await.context("subscribe best blocks")?;

		while let Some(block_result) = blocks_sub.next().await {
			let block = block_result.context("block subscription error")?;
			let block_number = block.number() as u64;
			let events = block.events().await.context("fetch block events")?;

			for event in events.iter() {
				let event = event?;
				if event.pallet_name() == "StorageAutoRenewal" &&
					event.variant_name() == "DataAutoRenewed"
				{
					log::info!("DataAutoRenewed event at block {}", block_number);
					return Ok(block_number);
				}
			}
		}
		anyhow::bail!("Block subscription ended before DataAutoRenewed");
	};

	tokio::time::timeout(Duration::from_secs(timeout_secs), future)
		.await
		.map_err(|_| anyhow!("Timeout waiting for DataAutoRenewed after {}s", timeout_secs))?
}

/// Happy path: store data → register auto-renewal → assert `DataAutoRenewed`
/// fires at the expected block.
#[tokio::test(flavor = "multi_thread")]
async fn auto_renewal_happy_path_test() -> Result<()> {
	const TEST: &str = "auto_renew_happy";
	let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();

	test_log!(TEST, "=== Auto-Renewal Happy Path ===");

	verify_parachain_binaries()?;

	let config = build_parachain_network_config_single_collator(para_node_args())?;
	let network = initialize_network(config).await?;
	network.wait_until_is_up(NETWORK_READY_TIMEOUT_SECS).await?;

	let relay_alice = network.get_node("alice").context("Failed to get relay alice node")?;
	log::info!("Waiting for relay chain session change...");
	wait_for_session_change_on_node(relay_alice, SESSION_CHANGE_TIMEOUT_SECS)
		.await
		.context("Failed to detect session change on relay chain")?;

	let collator1 = network.get_node("collator-1").context("Failed to get collator-1 node")?;
	let client: OnlineClient<SubstrateConfig> = collator1.wait_client().await?;

	// 1. Shrink RetentionPeriod so expiration fires within the test runtime.
	let mut nonce = get_alice_nonce(collator1).await?;
	log::info!("Setting RetentionPeriod to {} blocks", RETENTION_PERIOD);
	set_retention_period(&client, RETENTION_PERIOD, nonce).await?;
	nonce += 1;

	// 2. Authorize (2× data.len() bytes) and store. Helper leaves 1× data.len() in authorization —
	//    exactly enough for one successful renewal cycle.
	let data = generate_test_data(TEST_DATA_SIZE, PARACHAIN_TEST_DATA_PATTERN);
	let (hash_hex, cid) = content_hash_and_cid(&data);
	log::info!("Test data ({} bytes), hash={}, CID={}", data.len(), hash_hex, cid);

	let (store_block, next_nonce) = authorize_and_store_data(collator1, &data, nonce).await?;
	nonce = next_nonce;
	log::info!("Data stored at block {}", store_block);

	// 3. Enable auto-renewal.
	let content_hash = blake2_256(&data);
	enable_auto_renew(&client, &content_hash, nonce).await?;

	// 4. `on_initialize(n)` schedules renewal for data stored at `obsolete = n - RETENTION_PERIOD -
	//    1`. So for our store_block the renewal fires at block `store_block + RETENTION_PERIOD +
	//    1`.
	let expected_at_least = store_block + RETENTION_PERIOD as u64 + 1;
	log::info!(
		"Expecting DataAutoRenewed at block >= {} (store_block {} + retention {} + 1)",
		expected_at_least,
		store_block,
		RETENTION_PERIOD
	);

	let renewal_block = wait_for_data_auto_renewed(&client, AUTO_RENEW_WAIT_SECS).await?;
	if renewal_block < expected_at_least {
		anyhow::bail!(
			"DataAutoRenewed fired at block {} but expected >= {}",
			renewal_block,
			expected_at_least
		);
	}
	log::info!("✓ DataAutoRenewed at block {} (expected >= {})", renewal_block, expected_at_least);

	test_log!(TEST, "=== Auto-Renewal Happy Path PASSED ===");
	network.destroy().await?;
	Ok(())
}

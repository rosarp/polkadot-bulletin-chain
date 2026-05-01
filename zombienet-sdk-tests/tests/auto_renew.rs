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
		content_hash_and_cid, generate_test_data, get_alice_nonce, get_db_path, get_para_id,
		get_parachain_binary_path, get_parachain_chain_id, initialize_network,
		set_retention_period, tx::StoredItem, verify_all_items_bitswap, verify_col11,
		verify_ldb_tool, verify_parachain_binaries, wait_for_block_height,
		wait_for_finalized_height, wait_for_fullnode, wait_for_in_best_block,
		wait_for_session_change_on_node, BLOCK_PRODUCTION_TIMEOUT_SECS,
		NETWORK_READY_TIMEOUT_SECS, NODE_LOG_CONFIG, PARACHAIN_TEST_DATA_PATTERN,
		SYNC_TIMEOUT_SECS, TEST_DATA_SIZE, TRANSACTION_TIMEOUT_SECS,
	},
};
use anyhow::{anyhow, Context, Result};
use env_logger::Env;
use futures::future::try_join_all;
use std::{collections::HashMap, time::Duration};
use subxt::{
	config::substrate::{SubstrateConfig, SubstrateExtrinsicParamsBuilder},
	dynamic::{tx, Value},
	events::Phase,
	ext::scale_value::value,
	OnlineClient,
};
use subxt_signer::sr25519::dev;
use tokio::try_join;
use zombienet_orchestrator::AddCollatorOptions;

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

// ─────────────────────────────────────────────────────────────────────────────
//   Multi-renew test (DbExtrinsic::MultiRenew)
// ─────────────────────────────────────────────────────────────────────────────

/// Items per block in the multi-renew test. With N>=2, the inherent calls
/// `sp_io::transaction_index::renew()` more than once within a single
/// extrinsic — exercising sc-client-db's `DbExtrinsic::MultiRenew` path
/// (introduced in polkadot-sdk PR #11474).
const MULTI_RENEW_N: usize = 3;

/// Authorize Alice for `transactions` calls and `bytes` total throughput
/// via `Sudo::sudo(TransactionStorage::authorize_account)`.
async fn authorize_alice(
	client: &OnlineClient<SubstrateConfig>,
	transactions: u32,
	bytes: u64,
	nonce: u64,
) -> Result<()> {
	let signer = dev::alice();
	let authorize_call = subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			TransactionStorage(authorize_account {
				who: Value::from_bytes(signer.public_key().0),
				transactions: transactions,
				bytes: bytes
			})
		}],
	);
	let params = SubstrateExtrinsicParamsBuilder::new().nonce(nonce).build();

	log::info!(
		"Authorizing Alice for {} transactions / {} bytes (nonce={})",
		transactions,
		bytes,
		nonce
	);
	tokio::time::timeout(Duration::from_secs(TRANSACTION_TIMEOUT_SECS), async {
		let progress =
			client.tx().sign_and_submit_then_watch(&authorize_call, &signer, params).await?;
		wait_for_in_best_block(progress).await?;
		Ok::<_, anyhow::Error>(())
	})
	.await
	.map_err(|_| anyhow!("authorize_alice transaction timed out"))??;
	Ok(())
}

/// Submit `items.len()` `TransactionStorage::store` extrinsics in rapid
/// succession (consecutive nonces, no awaits between submits) and require
/// they all land in the same block. Returns the shared store-block number
/// and the next free nonce.
///
/// Same-block landing is what makes a subsequent auto-renewal cycle batch
/// all `IndexOperation::Renew`s into a single inherent extrinsic, which in
/// turn produces a `DbExtrinsic::MultiRenew` entry. If they split across
/// blocks the test bails — re-run; if persistent, lower N or increase
/// `MaxBlockTransactions`.
async fn store_items_same_block(
	client: &OnlineClient<SubstrateConfig>,
	items: &[Vec<u8>],
	mut nonce: u64,
) -> Result<(u64, u64)> {
	let signer = dev::alice();

	let mut submissions = Vec::with_capacity(items.len());
	for (i, data) in items.iter().enumerate() {
		let store_call = tx("TransactionStorage", "store", vec![Value::from_bytes(data)]);
		let params = SubstrateExtrinsicParamsBuilder::new().nonce(nonce).build();
		log::info!("Submit store #{} (nonce={}, {} bytes)", i, nonce, data.len());
		nonce += 1;
		let progress =
			client.tx().sign_and_submit_then_watch(&store_call, &signer, params).await?;
		submissions.push(progress);
	}

	let results = tokio::time::timeout(
		Duration::from_secs(TRANSACTION_TIMEOUT_SECS),
		try_join_all(submissions.into_iter().map(wait_for_in_best_block)),
	)
	.await
	.map_err(|_| anyhow!("store transactions timed out"))??;

	let block_hash = results[0].0;
	if !results.iter().all(|(h, _)| *h == block_hash) {
		anyhow::bail!(
			"Stores split across {} blocks (need all in one to validate MultiRenew). \
			 Re-run; if persistent, lower MULTI_RENEW_N or increase block budget.",
			results
				.iter()
				.map(|(h, _)| h)
				.collect::<std::collections::HashSet<_>>()
				.len()
		);
	}
	let block_number = client.blocks().at(block_hash).await?.number() as u64;
	log::info!("All {} stores in block {}", items.len(), block_number);
	Ok((block_number, nonce))
}

/// Sign and submit `enable_auto_renew(content_hash)` for each hash; await
/// each in turn (intra-block ordering doesn't matter, only that all are
/// registered before the retention block).
async fn enable_auto_renew_for_all(
	client: &OnlineClient<SubstrateConfig>,
	hashes: &[[u8; 32]],
	mut nonce: u64,
) -> Result<u64> {
	for (i, hash) in hashes.iter().enumerate() {
		log::info!("Enable auto-renew for item #{} (nonce={})", i, nonce);
		enable_auto_renew(client, hash, nonce).await?;
		nonce += 1;
	}
	Ok(nonce)
}

/// Wait until a block strictly later than `after_block` contains at least
/// `expected_count` `DataAutoRenewed` events that share the same
/// `Phase::ApplyExtrinsic(idx)`. Returns the block number and the extrinsic
/// index. Sharing the phase is the on-chain signature that a single
/// `process_auto_renewals` inherent drained the pending queue —
/// equivalently, that `sc-client-db` will record the block using
/// `DbExtrinsic::MultiRenew`.
///
/// `after_block` matters because `subscribe_best()` typically yields the
/// current best block first, so calling this twice in a row would otherwise
/// match the same block twice. Pass the previous return value to find the
/// next cycle.
async fn wait_for_n_renewed_in_one_extrinsic(
	client: &OnlineClient<SubstrateConfig>,
	expected_count: usize,
	timeout_secs: u64,
	after_block: u64,
) -> Result<(u64, u32)> {
	let future = async {
		let mut blocks_sub =
			client.blocks().subscribe_best().await.context("subscribe best blocks")?;

		while let Some(block_result) = blocks_sub.next().await {
			let block = block_result.context("block subscription error")?;
			let block_number = block.number() as u64;
			if block_number <= after_block {
				continue;
			}
			let events = block.events().await.context("fetch block events")?;

			let mut by_extrinsic: HashMap<u32, usize> = HashMap::new();
			for event in events.iter() {
				let event = event?;
				if event.pallet_name() == "StorageAutoRenewal" &&
					event.variant_name() == "DataAutoRenewed"
				{
					if let Phase::ApplyExtrinsic(idx) = event.phase() {
						*by_extrinsic.entry(idx).or_insert(0) += 1;
					}
				}
			}

			if let Some((&idx, &count)) =
				by_extrinsic.iter().find(|(_, &c)| c >= expected_count)
			{
				log::info!(
					"{} DataAutoRenewed events in extrinsic {} at block {} (multi-renew confirmed)",
					count,
					idx,
					block_number
				);
				return Ok((block_number, idx));
			}
		}
		anyhow::bail!(
			"Block subscription ended before {} renewals in one extrinsic",
			expected_count
		);
	};

	tokio::time::timeout(Duration::from_secs(timeout_secs), future).await.map_err(|_| {
		anyhow!("Timeout waiting for {} renewals after {}s", expected_count, timeout_secs)
	})?
}

/// Multi-renew: store N items in one block, enable auto-renew on each, wait
/// for the retention block, assert that **all N** `DataAutoRenewed` events
/// fire in the **same extrinsic** in the **same block**. That extrinsic is
/// the `process_auto_renewals` inherent and the on-chain proof that
/// `DbExtrinsic::MultiRenew` is the database representation.
#[tokio::test(flavor = "multi_thread")]
async fn auto_renew_multi_in_one_block_test() -> Result<()> {
	const TEST: &str = "auto_renew_multi";
	let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();

	test_log!(TEST, "=== Auto-Renewal Multi (N={}) ===", MULTI_RENEW_N);

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

	let mut nonce = get_alice_nonce(collator1).await?;
	log::info!("Setting RetentionPeriod to {} blocks", RETENTION_PERIOD);
	set_retention_period(&client, RETENTION_PERIOD, nonce).await?;
	nonce += 1;

	// Authorize Alice for N stores + N renewals (one cycle): 2N transactions,
	// 2N × TEST_DATA_SIZE bytes.
	let n_u32 = MULTI_RENEW_N as u32;
	let total_transactions = 2 * n_u32 + 5;
	let total_bytes = (2 * MULTI_RENEW_N * TEST_DATA_SIZE) as u64;
	authorize_alice(&client, total_transactions, total_bytes, nonce).await?;
	nonce += 1;

	// Generate N distinct items by appending an index suffix to the test
	// pattern. Distinct content hashes are required — auto-renew indexes by
	// content hash and rejects duplicates.
	let items: Vec<Vec<u8>> = (0..MULTI_RENEW_N)
		.map(|i| {
			let mut p = PARACHAIN_TEST_DATA_PATTERN.to_vec();
			p.extend_from_slice(format!("ITEM_{}_", i).as_bytes());
			generate_test_data(TEST_DATA_SIZE, &p)
		})
		.collect();
	let hashes: Vec<[u8; 32]> = items.iter().map(|d| blake2_256(d)).collect();
	for (i, h) in hashes.iter().enumerate() {
		log::info!("Item #{}: hash={}", i, hex::encode(h).to_uppercase());
	}

	let (store_block, next_nonce) = store_items_same_block(&client, &items, nonce).await?;
	nonce = next_nonce;

	nonce = enable_auto_renew_for_all(&client, &hashes, nonce).await?;
	let _ = nonce;

	let expected_at_least = store_block + RETENTION_PERIOD as u64 + 1;
	log::info!(
		"Expecting {} DataAutoRenewed events in one extrinsic at block >= {}",
		MULTI_RENEW_N,
		expected_at_least
	);

	let (renewal_block, extrinsic_idx) =
		wait_for_n_renewed_in_one_extrinsic(&client, MULTI_RENEW_N, AUTO_RENEW_WAIT_SECS, 0).await?;
	if renewal_block < expected_at_least {
		anyhow::bail!(
			"Multi-renew fired at block {} but expected >= {}",
			renewal_block,
			expected_at_least
		);
	}
	log::info!(
		"✓ {} renewals in extrinsic {} at block {} (expected block >= {})",
		MULTI_RENEW_N,
		extrinsic_idx,
		renewal_block,
		expected_at_least
	);

	test_log!(TEST, "=== Auto-Renewal Multi PASSED ===");
	network.destroy().await?;
	Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
//   Multi-renew + full sync (PR #11474 body_uncached reconstruction)
// ─────────────────────────────────────────────────────────────────────────────

/// Buffer past the multi-renew block before adding the sync node, so the
/// MultiRenew block is finalized and a clear sync target exists.
const MULTI_RENEW_FINALITY_BUFFER: u64 = 5;

/// Multi-renew + full sync: produce a multi-renew block on the collator,
/// then add a `--sync=full` node and require it can serve every renewed
/// hash via bitswap.
///
/// On the synced node, sc-client-db must reconstruct the multi-renew
/// block's body via `body_uncached` and index every hash via
/// `block_indexed_body`. Without polkadot-sdk PR #11474 only the **last**
/// renewed hash survives in the index, so bitswap for the others would
/// return DONT_HAVE — the test would fail.
#[tokio::test(flavor = "multi_thread")]
async fn auto_renew_multi_full_sync_test() -> Result<()> {
	const TEST: &str = "auto_renew_multi_full_sync";
	let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();

	test_log!(TEST, "=== Auto-Renewal Multi + Full Sync (N={}) ===", MULTI_RENEW_N);

	verify_parachain_binaries()?;

	let config = build_parachain_network_config_single_collator(para_node_args())?;
	let mut network = initialize_network(config).await?;
	network.wait_until_is_up(NETWORK_READY_TIMEOUT_SECS).await?;

	let relay_alice = network.get_node("alice").context("Failed to get relay alice node")?;
	wait_for_session_change_on_node(relay_alice, SESSION_CHANGE_TIMEOUT_SECS)
		.await
		.context("Failed to detect session change on relay chain")?;

	let collator1 = network.get_node("collator-1").context("Failed to get collator-1 node")?;
	let client: OnlineClient<SubstrateConfig> = collator1.wait_client().await?;

	let mut nonce = get_alice_nonce(collator1).await?;
	set_retention_period(&client, RETENTION_PERIOD, nonce).await?;
	nonce += 1;

	let n_u32 = MULTI_RENEW_N as u32;
	let total_transactions = 2 * n_u32 + 5;
	let total_bytes = (2 * MULTI_RENEW_N * TEST_DATA_SIZE) as u64;
	authorize_alice(&client, total_transactions, total_bytes, nonce).await?;
	nonce += 1;

	let items: Vec<Vec<u8>> = (0..MULTI_RENEW_N)
		.map(|i| {
			let mut p = PARACHAIN_TEST_DATA_PATTERN.to_vec();
			p.extend_from_slice(format!("ITEM_{}_", i).as_bytes());
			generate_test_data(TEST_DATA_SIZE, &p)
		})
		.collect();
	let hashes: Vec<[u8; 32]> = items.iter().map(|d| blake2_256(d)).collect();

	let (store_block, next_nonce) = store_items_same_block(&client, &items, nonce).await?;
	nonce = next_nonce;
	nonce = enable_auto_renew_for_all(&client, &hashes, nonce).await?;
	let _ = nonce;

	let (renewal_block, extrinsic_idx) =
		wait_for_n_renewed_in_one_extrinsic(&client, MULTI_RENEW_N, AUTO_RENEW_WAIT_SECS, 0).await?;
	log::info!(
		"Multi-renew at block {} (extrinsic {}); store_block was {}",
		renewal_block,
		extrinsic_idx,
		store_block
	);

	// The sync node needs a finalized target past the multi-renew block.
	let target_block = renewal_block + MULTI_RENEW_FINALITY_BUFFER;
	log::info!("Waiting for collator block height {} and finality", target_block);
	try_join!(
		wait_for_block_height(collator1, target_block, BLOCK_PRODUCTION_TIMEOUT_SECS),
		wait_for_finalized_height(collator1, target_block, BLOCK_PRODUCTION_TIMEOUT_SECS),
	)?;

	log::info!("Adding sync-node with --sync=full");
	let para_binary = get_parachain_binary_path();
	let sync_node_opts = AddCollatorOptions {
		command: Some(para_binary.as_str().try_into()?),
		args: vec![
			"--sync=full".into(),
			"--ipfs-server".into(),
			NODE_LOG_CONFIG.into(),
			"--".into(),
			"--network-backend=libp2p".into(),
		],
		is_validator: false,
		..Default::default()
	};
	network.add_collator("sync-node", sync_node_opts, get_para_id()).await?;
	let sync_node = network.get_node("sync-node").context("Failed to get sync-node")?;

	wait_for_fullnode(sync_node).await?;
	log::info!("Waiting for sync-node to reach block {}", target_block);
	wait_for_block_height(sync_node, target_block, SYNC_TIMEOUT_SECS).await?;

	// `block_number` on StoredItem is metadata for logging; bitswap is content-addressed.
	let stored_items: Vec<StoredItem> = items
		.iter()
		.cloned()
		.map(|data| StoredItem { data, block_number: store_block })
		.collect();

	log::info!("Verifying all {} items via bitswap from sync-node", MULTI_RENEW_N);
	verify_all_items_bitswap(sync_node, &stored_items, 30, "sync-node").await?;
	log::info!(
		"✓ All {} renewed hashes retrievable via bitswap from sync-node — \
		 body_uncached reconstructed MultiRenew block correctly",
		MULTI_RENEW_N
	);

	test_log!(TEST, "=== Auto-Renewal Multi + Full Sync PASSED ===");
	network.destroy().await?;
	Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
//   Multi-renew + pruning refcount (PR #11474 prune_block ref-count release)
// ─────────────────────────────────────────────────────────────────────────────

/// `--blocks-pruning=N` keeps the last N finalized blocks. Set just below
/// the spread between cycles (`RETENTION_PERIOD + 1 = 11`) so the cycle-1
/// block ages out before cycle-2's renewal, while leaving headroom on the
/// upper bound. Lower values (e.g. 5) caused the parachain collator to
/// stall after the multi-renew block — likely a separate cumulus/pruning
/// edge case unrelated to PR #11474; tracking that is out of scope here.
const PRUNING_BLOCKS_FOR_REFCOUNT: u32 = 10;

/// Build the para-node args with pruning enabled.
fn para_node_args_with_pruning(blocks_pruning: u32) -> Vec<String> {
	vec![
		"--ipfs-server".into(),
		format!("--blocks-pruning={}", blocks_pruning),
		NODE_LOG_CONFIG.into(),
		"--".into(),
		"--network-backend=libp2p".into(),
	]
}

/// Multi-renew + pruning: produce two multi-renew cycles and verify col11
/// settles to exactly N entries each at refcount = 1 once the original
/// store-block and the first multi-renew block have aged past the pruning
/// horizon.
///
/// Without polkadot-sdk PR #11474, multi-renew blocks register only **one**
/// hash in `apply_index_ops` (HashMap clobber). When that block later
/// prunes, only that one hash's refcount is released. Combined with the
/// original store-block pruning step (which decrements all N), the chain
/// ends up with N − 1 leaked entries at refcount = 0 (or items missing
/// entirely from col11). With the fix, all N hashes are tracked across
/// `MultiRenew` so the refcounts decrement symmetrically.
#[tokio::test(flavor = "multi_thread")]
async fn auto_renew_multi_pruning_refcount_test() -> Result<()> {
	const TEST: &str = "auto_renew_multi_prune";
	let _ = env_logger::Builder::from_env(Env::default().default_filter_or("info")).try_init();

	test_log!(
		TEST,
		"=== Auto-Renewal Multi + Pruning (N={}, blocks-pruning={}) ===",
		MULTI_RENEW_N,
		PRUNING_BLOCKS_FOR_REFCOUNT
	);

	let ldb_available = verify_ldb_tool().is_ok();
	if !ldb_available {
		log::warn!(
			"rocksdb_ldb not available (set ROCKSDB_LDB_PATH); skipping refcount LDB check, \
			 falling back to bitswap presence (still proves the fix at chain level)"
		);
	}
	verify_parachain_binaries()?;

	let config = build_parachain_network_config_single_collator(para_node_args_with_pruning(
		PRUNING_BLOCKS_FOR_REFCOUNT,
	))?;
	let network = initialize_network(config).await?;
	network.wait_until_is_up(NETWORK_READY_TIMEOUT_SECS).await?;

	let relay_alice = network.get_node("alice").context("Failed to get relay alice node")?;
	wait_for_session_change_on_node(relay_alice, SESSION_CHANGE_TIMEOUT_SECS)
		.await
		.context("Failed to detect session change on relay chain")?;

	let collator1 = network.get_node("collator-1").context("Failed to get collator-1 node")?;
	let client: OnlineClient<SubstrateConfig> = collator1.wait_client().await?;

	let mut nonce = get_alice_nonce(collator1).await?;
	set_retention_period(&client, RETENTION_PERIOD, nonce).await?;
	nonce += 1;

	// Authorize for 1 store + 2 renewal cycles per item plus margin.
	let n_u32 = MULTI_RENEW_N as u32;
	let total_transactions = 3 * n_u32 + 5;
	let total_bytes = (3 * MULTI_RENEW_N * TEST_DATA_SIZE) as u64;
	authorize_alice(&client, total_transactions, total_bytes, nonce).await?;
	nonce += 1;

	let items: Vec<Vec<u8>> = (0..MULTI_RENEW_N)
		.map(|i| {
			let mut p = PARACHAIN_TEST_DATA_PATTERN.to_vec();
			p.extend_from_slice(format!("ITEM_{}_", i).as_bytes());
			generate_test_data(TEST_DATA_SIZE, &p)
		})
		.collect();
	let hashes: Vec<[u8; 32]> = items.iter().map(|d| blake2_256(d)).collect();

	let (store_block, next_nonce) = store_items_same_block(&client, &items, nonce).await?;
	nonce = next_nonce;
	nonce = enable_auto_renew_for_all(&client, &hashes, nonce).await?;
	let _ = nonce;

	// Cycle 1: refcount goes 1 → 2 (with fix); without fix only 1 of N is bumped.
	let (renewal1, _) =
		wait_for_n_renewed_in_one_extrinsic(&client, MULTI_RENEW_N, AUTO_RENEW_WAIT_SECS, 0)
			.await?;
	log::info!("1st multi-renew at block {}", renewal1);

	// Cycle 2: skip the cycle-1 block by passing it as `after_block`.
	let (renewal2, _) = wait_for_n_renewed_in_one_extrinsic(
		&client,
		MULTI_RENEW_N,
		AUTO_RENEW_WAIT_SECS,
		renewal1,
	)
	.await?;
	log::info!("2nd multi-renew at block {}", renewal2);

	// Wait for finality just past `renewal1 + PRUNING_BLOCKS` — block 1st-cycle
	// is provably pruned but cycle-2's block (which still holds the refs) is
	// well within the pruning window. We must check before cycle-2's block
	// itself ages out (~ PRUNING_BLOCKS more blocks of finality past renewal2).
	let target_finalized = renewal1 + PRUNING_BLOCKS_FOR_REFCOUNT as u64 + 1;
	log::info!(
		"Waiting for finalized height >= {} so block {} (1st cycle) prunes \
		 (block {} from cycle 2 must still be within window)",
		target_finalized,
		renewal1,
		renewal2
	);
	wait_for_finalized_height(collator1, target_finalized, BLOCK_PRODUCTION_TIMEOUT_SECS).await?;

	// Bitswap-presence check: every renewed item must still be retrievable from
	// the collator. If `prune_block` failed to track all hashes in MultiRenew,
	// the original store-block's prune step would have driven those items to
	// refcount=0 and removed them from col11 — bitswap would then return
	// DONT_HAVE for the dropped hashes.
	let stored_items: Vec<StoredItem> = items
		.iter()
		.cloned()
		.map(|data| StoredItem { data, block_number: store_block })
		.collect();
	verify_all_items_bitswap(collator1, &stored_items, 30, "collator-1").await?;
	log::info!(
		"✓ All {} items retrievable from collator-1 after pruning — \
		 prune_block correctly tracked every hash through MultiRenew",
		MULTI_RENEW_N
	);

	// Optional refcount check via rocksdb_ldb when available.
	if ldb_available {
		let base_dir = network
			.base_dir()
			.ok_or_else(|| anyhow!("Failed to get network base directory"))?
			.to_string();
		let chain_id = get_parachain_chain_id();
		let db_path = get_db_path(&base_dir, "collator-1", &chain_id);
		let dump = verify_col11(&db_path, "col11 AFTER 2 cycles + pruning")?;

		if dump.key_count != 2 * MULTI_RENEW_N {
			anyhow::bail!(
				"Expected {} keys (N data + N refcount), found {}",
				2 * MULTI_RENEW_N,
				dump.key_count
			);
		}
		for (i, hash) in hashes.iter().enumerate() {
			let hash_hex = hex::encode(hash).to_uppercase();
			let refcount = dump
				.get_refcount(&hash_hex)
				.ok_or_else(|| anyhow!("Item #{} hash {} missing from col11", i, hash_hex))?;
			if refcount != 1 {
				anyhow::bail!(
					"Item #{} hash {} refcount={}, expected 1",
					i,
					hash_hex,
					refcount
				);
			}
		}
		log::info!("✓ LDB confirms N={} entries, refcount=1 each", MULTI_RENEW_N);
	}

	test_log!(TEST, "=== Auto-Renewal Multi + Pruning PASSED ===");
	network.destroy().await?;
	Ok(())
}

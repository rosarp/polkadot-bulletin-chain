// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Test environment for `pallet-storage-auto-renewal`.
//!
//! Wires together a real `pallet-transaction-storage` plus this pallet so the full
//! expiring → enqueue → process_auto_renewals → finalize cycle can be exercised.

use crate as pallet_storage_auto_renewal;
use pallet_transaction_storage::{DEFAULT_MAX_BLOCK_TRANSACTIONS, DEFAULT_MAX_TRANSACTION_SIZE};
use pallets_common::NoCurrency;
use polkadot_sdk_frame::{prelude::*, runtime::prelude::*, testing_prelude::*};

type Block = MockBlock<Test>;

construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		TransactionStorage: pallet_transaction_storage,
		AutoRenewal: pallet_storage_auto_renewal,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Nonce = u64;
	type Block = Block;
	type BlockHashCount = ConstU64<250>;
}

parameter_types! {
	pub const AuthorizationPeriod: BlockNumberFor<Test> = 10;
	pub const StoreRenewPriority: TransactionPriority = TransactionPriority::MAX;
	pub const StoreRenewLongevity: TransactionLongevity = 10;
	pub const RemoveExpiredAuthorizationPriority: TransactionPriority = TransactionPriority::MAX;
	pub const RemoveExpiredAuthorizationLongevity: TransactionLongevity = 10;
}

impl pallet_transaction_storage::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type Currency = NoCurrency<Self::AccountId, RuntimeHoldReason>;
	type RuntimeHoldReason = RuntimeHoldReason;
	type FeeDestination = ();
	type WeightInfo = ();
	type MaxBlockTransactions = ConstU32<{ DEFAULT_MAX_BLOCK_TRANSACTIONS }>;
	type MaxTransactionSize = ConstU32<{ DEFAULT_MAX_TRANSACTION_SIZE }>;
	type AuthorizationPeriod = AuthorizationPeriod;
	type Authorizer = EnsureRoot<Self::AccountId>;
	type StoreRenewPriority = StoreRenewPriority;
	type StoreRenewLongevity = StoreRenewLongevity;
	type RemoveExpiredAuthorizationPriority = RemoveExpiredAuthorizationPriority;
	type RemoveExpiredAuthorizationLongevity = RemoveExpiredAuthorizationLongevity;
	type OnTransactionExpiring = AutoRenewal;
}

impl pallet_storage_auto_renewal::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = ();
	type MaxBlockTransactions = ConstU32<{ DEFAULT_MAX_BLOCK_TRANSACTIONS }>;
	type StorageRenewer = TransactionStorage;
}

pub fn new_test_ext() -> TestExternalities {
	let t = RuntimeGenesisConfig {
		system: Default::default(),
		transaction_storage: pallet_transaction_storage::GenesisConfig::<Test> {
			retention_period: 10,
			byte_fee: 2,
			entry_fee: 200,
			account_authorizations: vec![],
			preimage_authorizations: vec![],
		},
	}
	.build_storage()
	.unwrap();
	t.into()
}

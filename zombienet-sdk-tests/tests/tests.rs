// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::uninlined_format_args)]

#[cfg(feature = "auto-renew-tests")]
mod auto_renew;
#[cfg(feature = "zombie-sync-tests")]
mod parachain_sync_storage;
#[cfg(any(feature = "zombie-sync-tests", feature = "auto-renew-tests"))]
mod utils;

// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0
use libra_crypto::HashValue;
use libra_crypto_derive::CryptoHasher;
use serde::{Deserialize, Serialize};

#[derive(Debug, Hash, Clone, Eq, PartialEq, Serialize, Deserialize, CryptoHasher)]
pub struct WriteSet {}

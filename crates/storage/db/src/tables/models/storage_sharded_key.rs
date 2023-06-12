//! Storage sharded key

use super::ShardedKey;
use crate::{
    table::{Decode, Encode},
    DatabaseError,
};
use derive_more::AsRef;
use reth_primitives::{BlockNumber, H160, H256};
use serde::{Deserialize, Serialize};

/// Number of indices in one shard.
pub const NUM_OF_INDICES_IN_SHARD: usize = 2_000;

/// Sometimes data can be too big to be saved for a single key. This helps out by dividing the data
/// into different shards. Example:
///
/// `Address | Storagekey | 200` -> data is from transition 0 to 200.
///
/// `Address | StorageKey | 300` -> data is from transition 201 to 300.
#[derive(Debug, Default, Clone, Eq, Ord, PartialOrd, PartialEq, AsRef, Serialize, Deserialize)]
pub struct StorageShardedKey {
    /// Storage account address.
    pub address: H160,
    /// Storage slot with highest transition id.
    #[as_ref]
    pub sharded_key: ShardedKey<H256>,
}

impl StorageShardedKey {
    /// Creates a new `StorageShardedKey`.
    pub fn new(address: H160, storage_key: H256, highest_block_number: BlockNumber) -> Self {
        Self { address, sharded_key: ShardedKey { key: storage_key, highest_block_number } }
    }

    /// Creates a new key with the highest block number set to maximum.
    /// This is useful when we want to search the last value for a given key.
    pub fn last(address: H160, storage_key: H256) -> Self {
        Self {
            address,
            sharded_key: ShardedKey { key: storage_key, highest_block_number: u64::MAX },
        }
    }
}

impl Encode for StorageShardedKey {
    type Encoded = Vec<u8>;

    fn encode(self) -> Self::Encoded {
        let mut buf: Vec<u8> = Encode::encode(self.address).into();
        buf.extend_from_slice(&Encode::encode(self.sharded_key.key));
        buf.extend_from_slice(&self.sharded_key.highest_block_number.to_be_bytes());
        buf
    }
}

impl Decode for StorageShardedKey {
    fn decode<B: AsRef<[u8]>>(value: B) -> Result<Self, DatabaseError> {
        let value = value.as_ref();
        let tx_num_index = value.len() - 8;

        let highest_tx_number = u64::from_be_bytes(
            value[tx_num_index..].try_into().map_err(|_| DatabaseError::DecodeError)?,
        );
        let address = H160::decode(&value[..20])?;
        let storage_key = H256::decode(&value[20..52])?;

        Ok(Self { address, sharded_key: ShardedKey::new(storage_key, highest_tx_number) })
    }
}

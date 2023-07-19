#![cfg(feature = "serde")]

// Parts of this code were adapted from github.com/gakonst/ethers-rs and are distributed under its
// licenses:
// - https://github.com/gakonst/ethers-rs/blob/7e6c3ba98363bdf6131e8284f186cc2c70ff48c3/LICENSE-APACHE
// - https://github.com/gakonst/ethers-rs/blob/7e6c3ba98363bdf6131e8284f186cc2c70ff48c3/LICENSE-MIT
// For the original context, see https://github.com/gakonst/ethers-rs/tree/7e6c3ba98363bdf6131e8284f186cc2c70ff48c3

/// input types for EIP-712 message signing
pub mod eip712;

use std::fmt::Debug;

use crate::{Address, Bloom, Bytes, B256, U256};

use super::{serde_with_helpers::optional_u64_from_hex, withdrawal::Withdrawal};

/// for use as the access_list field in the Transaction struct
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct AccessListEntry {
    address: Address,
    storage_keys: Vec<U256>,
}

/// transaction
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// hash of the transaction
    pub hash: B256,
    /// the number of transactions made by the sender prior to this one
    pub nonce: U256,
    /// hash of the block where this transaction was in
    pub block_hash: Option<B256>,
    /// block number where this transaction was in
    pub block_number: Option<U256>,
    /// integer of the transactions index position in the block. null when its pending
    #[serde(deserialize_with = "optional_u64_from_hex")]
    pub transaction_index: Option<u64>,
    /// address of the sender
    pub from: Address,
    /// address of the receiver. null when its a contract creation transaction.
    pub to: Option<Address>,
    /// value transferred in Wei
    pub value: U256,
    /// gas price provided by the sender in Wei
    pub gas_price: Option<U256>,
    /// gas provided by the sender
    pub gas: U256,
    /// the data sent along with the transaction
    pub input: Bytes,
    /// ECDSA recovery id
    #[serde(deserialize_with = "u64_from_hex")]
    pub v: u64,
    /// ECDSA signature r
    pub r: U256,
    /// ECDSA signature s
    pub s: U256,
    /// chain ID
    #[serde(default, deserialize_with = "optional_u64_from_hex")]
    pub chain_id: Option<u64>,
    /// integer of the transaction type, 0x0 for legacy transactions, 0x1 for access list types, 0x2 for dynamic fees
    #[serde(
        rename = "type",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_u64_from_hex"
    )]
    pub transaction_type: Option<u64>,
    /// access list
    #[serde(default)]
    pub access_list: Option<Vec<AccessListEntry>>,
    /// max fee per gas
    #[serde(default)]
    pub max_fee_per_gas: Option<U256>,
    /// max priority fee per gas
    #[serde(default)]
    pub max_priority_fee_per_gas: Option<U256>,
}

fn u64_from_hex<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: &str = serde::Deserialize::deserialize(deserializer)?;
    Ok(u64::from_str_radix(&s[2..], 16).expect("failed to parse u64"))
}

/// log object used in TransactionReceipt
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct Log {
    /// address
    pub address: Address,
    /// topics
    pub topics: Vec<B256>,
    /// data
    pub data: Bytes,
    /// block hash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<B256>,
    /// block number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number: Option<U256>,
    /// transaction hash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_hash: Option<B256>,
    /// transaction index
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_u64_from_hex"
    )]
    pub transaction_index: Option<u64>,
    /// log index
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_index: Option<U256>,
    /// transaction log index
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_log_index: Option<U256>,
    /// log type
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_type: Option<String>,
    /// removed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
}

/// object returned by eth_getTransactionReceipt
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct TransactionReceipt {
    /// hash of the block where this transaction was in
    pub block_hash: Option<B256>,
    /// block number where this transaction was in
    pub block_number: Option<U256>,
    /// The contract address created, if the transaction was a contract creation, otherwise null.
    pub contract_address: Option<Address>,
    /// The total amount of gas used when this transaction was executed in the block.
    pub cumulative_gas_used: U256,
    /// The sum of the base fee and tip paid per unit of gas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_gas_price: Option<U256>,
    /// address of the sender
    pub from: Address,
    /// The amount of gas used by this specific transaction alone.
    pub gas_used: Option<U256>,
    /// Array of log objects, which this transaction generated.
    pub logs: Vec<Log>,
    /// Bloom filter for light clients to quickly retrieve related logs.
    pub logs_bloom: Bloom,
    /// 32 bytes of post-transaction stateroot (pre Byzantium)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<B256>,
    /// either 1 (success) or 0 (failure)
    #[serde(deserialize_with = "optional_u64_from_hex")]
    pub status: Option<u64>,
    /// address of the receiver. null when its a contract creation transaction.
    pub to: Option<Address>,
    /// hash of the transaction
    pub transaction_hash: B256,
    /// integer of the transactions index position in the block
    #[serde(deserialize_with = "u64_from_hex")]
    pub transaction_index: u64,
    /// integer of the transaction type, 0x0 for legacy transactions, 0x1 for access list types, 0x2 for dynamic fees. It also returns either :
    #[serde(
        rename = "type",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "optional_u64_from_hex"
    )]
    pub transaction_type: Option<u64>,
}

/// block object returned by eth_getBlockBy*
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct Block<TX>
where
    TX: Debug + Default + Clone + PartialEq + Eq,
{
    /// hash of the block. None when its pending block.
    pub hash: Option<B256>,
    /// hash of the parent block.
    pub parent_hash: B256,
    /// SHA3 of the uncles data in the block
    pub sha3_uncles: B256,
    /// author
    pub author: Option<Address>,
    /// the root of the final state trie of the block
    pub state_root: B256,
    /// the root of the transaction trie of the block
    pub transactions_root: B256,
    /// the root of the receipts trie of the block
    pub receipts_root: B256,
    /// the block number. None when its pending block.
    pub number: Option<U256>,
    /// the total used gas by all transactions in this block
    pub gas_used: U256,
    /// the maximum gas allowed in this block
    pub gas_limit: U256,
    /// the "extra data" field of this block
    pub extra_data: Bytes,
    /// the bloom filter for the logs of the block. None when its pending block.
    pub logs_bloom: Option<Bloom>,
    /// the unix timestamp for when the block was collated
    #[serde(default)]
    pub timestamp: U256,
    /// integer of the difficulty for this block
    #[serde(default)]
    pub difficulty: U256,
    /// integer of the total difficulty of the chain until this block
    pub total_difficulty: Option<U256>,
    /// seal fields
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub seal_fields: Vec<Bytes>,
    /// Array of uncle hashes
    #[serde(default)]
    pub uncles: Vec<B256>,
    /// Array of transaction objects, or 32 Bytes transaction hashes depending on the last given parameter
    #[serde(default)]
    pub transactions: Vec<TX>,
    /// integer the size of this block in bytes
    pub size: Option<U256>,
    /// mix hash
    pub mix_hash: Option<B256>,
    /// hash of the generated proof-of-work. null when its pending block.
    pub nonce: Option<U256>,
    /// base fee per gas
    pub base_fee_per_gas: Option<U256>,
    /// the address of the beneficiary to whom the mining rewards were given
    pub miner: Address,
    #[serde(default)]
    /// withdrawals
    pub withdrawals: Vec<Withdrawal>,
    /// withdrawals root
    #[serde(default)]
    pub withdrawals_root: B256,
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

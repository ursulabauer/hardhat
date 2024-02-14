mod account;
mod call;
mod gas;
mod inspector;

use std::{
    cmp,
    cmp::Ordering,
    collections::BTreeMap,
    fmt::Debug,
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use edr_eth::{
    block::{miner_reward, BlobGas},
    log::FilterLog,
    receipt::BlockReceipt,
    remote::{
        client::{HeaderMap, HttpError},
        eth::FeeHistoryResult,
        filter::{FilteredEvents, LogOutput, SubscriptionType},
        BlockSpec, BlockTag, Eip1898BlockSpec, RpcClient, RpcClientError,
    },
    reward_percentile::RewardPercentile,
    signature::{RecoveryMessage, Signature},
    transaction::{SignedTransaction, TransactionRequestAndSender},
    Address, Bytes, SpecId, B256, U256,
};
use edr_evm::{
    blockchain::{
        Blockchain, BlockchainError, ForkedBlockchain, ForkedCreationError, LocalBlockchain,
        LocalCreationError, SyncBlockchain,
    },
    calculate_next_base_fee,
    db::StateRef,
    debug_trace_transaction, execution_result_to_debug_result, mempool, mine_block,
    state::{
        AccountModifierFn, IrregularState, StateDiff, StateError, StateOverride, StateOverrides,
        SyncState,
    },
    trace::{Trace, TraceCollector},
    Account, AccountInfo, BlobExcessGasAndPrice, Block, BlockEnv, Bytecode, CfgEnv,
    DebugTraceConfig, DebugTraceResult, DualInspector, ExecutableTransaction, ExecutionResult,
    HashMap, HashSet, MemPool, OrderedTransaction, RandomHashGenerator, StorageSlot, SyncBlock,
    TracerEip3155, TxEnv, KECCAK_EMPTY,
};
use ethers_core::types::transaction::eip712::{Eip712, TypedData};
use gas::gas_used_ratio;
use indexmap::IndexMap;
use itertools::izip;
use lazy_static::lazy_static;
use lru::LruCache;
use tokio::runtime;

use self::{
    account::{create_accounts, InitialAccounts},
    inspector::EvmInspector,
};
use crate::{
    data::{
        call::{run_call, RunCallArgs},
        gas::{compute_rewards, BinarySearchEstimationArgs, CheckGasLimitArgs},
    },
    debug_mine::{DebugMineBlockResult, DebugMineBlockResultAndState},
    error::{EstimateGasFailure, TransactionFailure},
    filter::{bloom_contains_log_filter, filter_logs, Filter, FilterData, LogFilter},
    logger::SyncLogger,
    pending::BlockchainWithPending,
    requests::hardhat::rpc_types::{ForkConfig, ForkMetadata},
    snapshot::Snapshot,
    MiningConfig, ProviderConfig, ProviderError, SubscriptionEvent, SubscriptionEventData,
    SyncSubscriberCallback,
};

const DEFAULT_INITIAL_BASE_FEE_PER_GAS: u64 = 1_000_000_000;
const MAX_CACHED_STATES: usize = 64;

/// The result of executing an `eth_call`.
#[derive(Clone)]
pub struct CallResult {
    pub console_log_inputs: Vec<Bytes>,
    pub execution_result: ExecutionResult,
    pub trace: Trace,
}

pub struct SendTransactionResult {
    pub transaction_hash: B256,
    /// Present if the transaction was auto-mined.
    pub transaction_result: Option<(ExecutionResult, Trace)>,
    pub mining_results: Vec<DebugMineBlockResult<BlockchainError>>,
}

#[derive(Debug, thiserror::Error)]
pub enum CreationError {
    /// A blockchain error
    #[error(transparent)]
    Blockchain(BlockchainError),
    /// An error that occurred while constructing a forked blockchain.
    #[error(transparent)]
    ForkedBlockchainCreation(#[from] ForkedCreationError),
    #[error("Invalid HTTP header name: {0}")]
    InvalidHttpHeaders(HttpError),
    /// Invalid initial date
    #[error("The initial date configuration value {0:?} is before the UNIX epoch")]
    InvalidInitialDate(SystemTime),
    /// An error that occurred while constructing a local blockchain.
    #[error(transparent)]
    LocalBlockchainCreation(#[from] LocalCreationError),
    /// An error that occured while querying the remote state.
    #[error(transparent)]
    RpcClient(#[from] RpcClientError),
}

pub struct ProviderData<LoggerErrorT: Debug> {
    runtime_handle: runtime::Handle,
    initial_config: ProviderConfig,
    blockchain: Box<dyn SyncBlockchain<BlockchainError, StateError>>,
    pub irregular_state: IrregularState,
    mem_pool: MemPool,
    beneficiary: Address,
    dao_activation_block: Option<u64>,
    min_gas_price: U256,
    prev_randao_generator: RandomHashGenerator,
    block_time_offset_seconds: i64,
    fork_metadata: Option<ForkMetadata>,
    // Must be set if the provider is created with a fork config.
    // Hack to get around the type erasure with the dyn blockchain trait.
    rpc_client: Option<RpcClient>,
    instance_id: B256,
    is_auto_mining: bool,
    next_block_base_fee_per_gas: Option<U256>,
    next_block_timestamp: Option<u64>,
    next_snapshot_id: u64,
    snapshots: BTreeMap<u64, Snapshot>,
    allow_blocks_with_same_timestamp: bool,
    allow_unlimited_contract_size: bool,
    // IndexMap to preserve account order for logging.
    local_accounts: IndexMap<Address, k256::SecretKey>,
    filters: HashMap<U256, Filter>,
    last_filter_id: U256,
    logger: Box<dyn SyncLogger<BlockchainError = BlockchainError, LoggerError = LoggerErrorT>>,
    impersonated_accounts: HashSet<Address>,
    subscriber_callback: Box<dyn SyncSubscriberCallback>,
    // We need the Arc to let us avoid returning references to the cache entries which need &mut
    // self to get.
    block_state_cache: LruCache<StateId, Arc<Box<dyn SyncState<StateError>>>>,
    current_state_id: StateId,
    block_number_to_state_id: BTreeMap<u64, StateId>,
}

impl<LoggerErrorT: Debug> ProviderData<LoggerErrorT> {
    pub fn new(
        runtime_handle: runtime::Handle,
        logger: Box<dyn SyncLogger<BlockchainError = BlockchainError, LoggerError = LoggerErrorT>>,
        subscriber_callback: Box<dyn SyncSubscriberCallback>,
        config: ProviderConfig,
    ) -> Result<Self, CreationError> {
        let InitialAccounts {
            local_accounts,
            genesis_accounts,
        } = create_accounts(&config);

        let BlockchainAndState {
            blockchain,
            fork_metadata,
            rpc_client,
            state,
            irregular_state,
            prev_randao_generator,
            block_time_offset_seconds,
            next_block_base_fee_per_gas,
        } = create_blockchain_and_state(runtime_handle.clone(), &config, genesis_accounts)?;

        let mut block_state_cache =
            LruCache::new(NonZeroUsize::new(MAX_CACHED_STATES).expect("constant is non-zero"));
        let mut block_number_to_state_id = BTreeMap::new();

        let current_state_id = StateId::default();
        block_state_cache.push(current_state_id, Arc::new(state));
        block_number_to_state_id.insert(blockchain.last_block_number(), current_state_id);

        let allow_blocks_with_same_timestamp = config.allow_blocks_with_same_timestamp;
        let allow_unlimited_contract_size = config.allow_unlimited_contract_size;
        let beneficiary = config.coinbase;
        let block_gas_limit = config.block_gas_limit;
        let is_auto_mining = config.mining.auto_mine;
        let min_gas_price = config.min_gas_price;

        let dao_activation_block = config
            .chains
            .get(&config.chain_id)
            .and_then(|config| config.hardfork_activation(SpecId::DAO_FORK));

        Ok(Self {
            runtime_handle,
            initial_config: config,
            blockchain,
            irregular_state,
            mem_pool: MemPool::new(block_gas_limit),
            beneficiary,
            dao_activation_block,
            min_gas_price,
            prev_randao_generator,
            block_time_offset_seconds,
            fork_metadata,
            rpc_client,
            instance_id: B256::random(),
            is_auto_mining,
            next_block_base_fee_per_gas,
            next_block_timestamp: None,
            // Start with 1 to mimic Ganache
            next_snapshot_id: 1,
            snapshots: BTreeMap::new(),
            allow_blocks_with_same_timestamp,
            allow_unlimited_contract_size,
            local_accounts,
            filters: HashMap::default(),
            last_filter_id: U256::ZERO,
            logger,
            impersonated_accounts: HashSet::new(),
            subscriber_callback,
            block_state_cache,
            current_state_id,
            block_number_to_state_id,
        })
    }

    pub fn reset(&mut self, fork_config: Option<ForkConfig>) -> Result<(), CreationError> {
        let mut config = self.initial_config.clone();
        config.fork = fork_config;

        let mut reset_instance = Self::new(
            self.runtime_handle.clone(),
            self.logger.clone(),
            self.subscriber_callback.clone(),
            config,
        )?;

        std::mem::swap(self, &mut reset_instance);

        Ok(())
    }

    /// Retrieves the last pending nonce of the account corresponding to the
    /// provided address, if it exists.
    pub fn account_next_nonce(
        &mut self,
        address: &Address,
    ) -> Result<u64, ProviderError<LoggerErrorT>> {
        let state = self.current_state()?;
        mempool::account_next_nonce(&self.mem_pool, &*state, address).map_err(Into::into)
    }

    pub fn accounts(&self) -> impl Iterator<Item = &Address> {
        self.local_accounts.keys()
    }

    pub fn allow_unlimited_initcode_size(&self) -> bool {
        self.allow_unlimited_contract_size
    }

    /// Returns whether the miner is mining automatically.
    pub fn is_auto_mining(&self) -> bool {
        self.is_auto_mining
    }

    pub fn balance(
        &mut self,
        address: Address,
        block_spec: Option<&BlockSpec>,
    ) -> Result<U256, ProviderError<LoggerErrorT>> {
        self.execute_in_block_context::<Result<U256, ProviderError<LoggerErrorT>>>(
            block_spec,
            move |_blockchain, _block, state| {
                Ok(state
                    .basic(address)?
                    .map_or(U256::ZERO, |account| account.balance))
            },
        )?
    }

    /// Retrieves the gas limit of the next block.
    pub fn block_gas_limit(&self) -> u64 {
        self.mem_pool.block_gas_limit()
    }

    /// Returns the default caller.
    pub fn default_caller(&self) -> Address {
        self.local_accounts
            .keys()
            .next()
            .copied()
            .unwrap_or(Address::ZERO)
    }

    /// Returns the metadata of the forked blockchain, if it exists.
    pub fn fork_metadata(&self) -> Option<&ForkMetadata> {
        self.fork_metadata.as_ref()
    }

    /// Returns the last block in the blockchain.
    pub fn last_block(
        &self,
    ) -> Result<Arc<dyn SyncBlock<Error = BlockchainError>>, BlockchainError> {
        self.blockchain.last_block()
    }

    /// Returns the number of the last block in the blockchain.
    pub fn last_block_number(&self) -> u64 {
        self.blockchain.last_block_number()
    }

    /// Adds a filter for new blocks to the provider.
    pub fn add_block_filter<const IS_SUBSCRIPTION: bool>(
        &mut self,
    ) -> Result<U256, ProviderError<LoggerErrorT>> {
        let block_hash = *self.last_block()?.hash();

        let filter_id = self.next_filter_id();
        self.filters.insert(
            filter_id,
            Filter::new_block_filter(block_hash, IS_SUBSCRIPTION),
        );

        Ok(filter_id)
    }

    /// Adds a filter for new logs to the provider.
    pub fn add_log_filter<const IS_SUBSCRIPTION: bool>(
        &mut self,
        criteria: LogFilter,
    ) -> Result<U256, ProviderError<LoggerErrorT>> {
        let logs = self
            .blockchain
            .logs(
                criteria.from_block,
                criteria
                    .to_block
                    .unwrap_or(self.blockchain.last_block_number()),
                &criteria.addresses,
                &criteria.normalized_topics,
            )?
            .iter()
            .map(LogOutput::from)
            .collect();

        let filter_id = self.next_filter_id();
        self.filters.insert(
            filter_id,
            Filter::new_log_filter(criteria, logs, IS_SUBSCRIPTION),
        );
        Ok(filter_id)
    }

    /// Adds a filter for new pending transactions to the provider.
    pub fn add_pending_transaction_filter<const IS_SUBSCRIPTION: bool>(&mut self) -> U256 {
        let filter_id = self.next_filter_id();
        self.filters.insert(
            filter_id,
            Filter::new_pending_transaction_filter(IS_SUBSCRIPTION),
        );
        filter_id
    }

    /// Whether the provider is configured to bail on call failures.
    pub fn bail_on_call_failure(&self) -> bool {
        self.initial_config.bail_on_call_failure
    }

    /// Whether the provider is configured to bail on transaction failures.
    pub fn bail_on_transaction_failure(&self) -> bool {
        self.initial_config.bail_on_transaction_failure
    }

    /// Fetch a block by block spec.
    /// Returns `None` if the block spec is `pending`.
    /// Returns `ProviderError::InvalidBlockSpec` error if the block spec is a
    /// number or a hash and the block isn't found.
    /// Returns `ProviderError::InvalidBlockTag` error if the block tag is safe
    /// or finalized and block spec is pre-merge.
    pub fn block_by_block_spec(
        &self,
        block_spec: &BlockSpec,
    ) -> Result<Option<Arc<dyn SyncBlock<Error = BlockchainError>>>, ProviderError<LoggerErrorT>>
    {
        let result = match block_spec {
            BlockSpec::Number(block_number) => Some(
                self.blockchain
                    .block_by_number(*block_number)?
                    .ok_or_else(|| ProviderError::InvalidBlockNumberOrHash {
                        block_spec: block_spec.clone(),
                        latest_block_number: self.blockchain.last_block_number(),
                    })?,
            ),
            BlockSpec::Tag(BlockTag::Earliest) => Some(
                self.blockchain
                    .block_by_number(0)?
                    .expect("genesis block should always exist"),
            ),
            // Matching Hardhat behaviour by returning the last block for finalized and safe.
            // https://github.com/NomicFoundation/hardhat/blob/b84baf2d9f5d3ea897c06e0ecd5e7084780d8b6c/packages/hardhat-core/src/internal/hardhat-network/provider/modules/eth.ts#L1395
            BlockSpec::Tag(tag @ (BlockTag::Finalized | BlockTag::Safe)) => {
                if self.spec_id() >= SpecId::MERGE {
                    Some(self.blockchain.last_block()?)
                } else {
                    return Err(ProviderError::InvalidBlockTag {
                        block_tag: *tag,
                        spec: self.spec_id(),
                    });
                }
            }
            BlockSpec::Tag(BlockTag::Latest) => Some(self.blockchain.last_block()?),
            BlockSpec::Tag(BlockTag::Pending) => None,
            BlockSpec::Eip1898(Eip1898BlockSpec::Hash {
                block_hash,
                require_canonical: _,
            }) => Some(self.blockchain.block_by_hash(block_hash)?.ok_or_else(|| {
                ProviderError::InvalidBlockNumberOrHash {
                    block_spec: block_spec.clone(),
                    latest_block_number: self.blockchain.last_block_number(),
                }
            })?),
            BlockSpec::Eip1898(Eip1898BlockSpec::Number { block_number }) => Some(
                self.blockchain
                    .block_by_number(*block_number)?
                    .ok_or_else(|| ProviderError::InvalidBlockNumberOrHash {
                        block_spec: block_spec.clone(),
                        latest_block_number: self.blockchain.last_block_number(),
                    })?,
            ),
        };

        Ok(result)
    }

    /// Retrieves the block number for the provided block spec, if it exists.
    fn block_number_by_block_spec(
        &self,
        block_spec: &BlockSpec,
    ) -> Result<Option<u64>, ProviderError<LoggerErrorT>> {
        let block_number = match block_spec {
            BlockSpec::Number(number) => Some(*number),
            BlockSpec::Tag(BlockTag::Earliest) => Some(0),
            BlockSpec::Tag(tag @ (BlockTag::Finalized | BlockTag::Safe)) => {
                if self.spec_id() >= SpecId::MERGE {
                    Some(self.blockchain.last_block_number())
                } else {
                    return Err(ProviderError::InvalidBlockTag {
                        block_tag: *tag,
                        spec: self.spec_id(),
                    });
                }
            }
            BlockSpec::Tag(BlockTag::Latest) => Some(self.blockchain.last_block_number()),
            BlockSpec::Tag(BlockTag::Pending) => None,
            BlockSpec::Eip1898(Eip1898BlockSpec::Hash { block_hash, .. }) => {
                self.blockchain.block_by_hash(block_hash)?.map_or_else(
                    || {
                        Err(ProviderError::InvalidBlockNumberOrHash {
                            block_spec: block_spec.clone(),
                            latest_block_number: self.blockchain.last_block_number(),
                        })
                    },
                    |block| Ok(Some(block.header().number)),
                )?
            }
            BlockSpec::Eip1898(Eip1898BlockSpec::Number { block_number }) => Some(*block_number),
        };

        Ok(block_number)
    }

    pub fn block_by_hash(
        &self,
        block_hash: &B256,
    ) -> Result<Option<Arc<dyn SyncBlock<Error = BlockchainError>>>, ProviderError<LoggerErrorT>>
    {
        self.blockchain
            .block_by_hash(block_hash)
            .map_err(ProviderError::Blockchain)
    }

    pub fn chain_id(&self) -> u64 {
        self.blockchain.chain_id()
    }

    pub fn coinbase(&self) -> Address {
        self.beneficiary
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn debug_trace_transaction(
        &mut self,
        transaction_hash: &B256,
        trace_config: DebugTraceConfig,
    ) -> Result<DebugTraceResult, ProviderError<LoggerErrorT>> {
        let block = self
            .blockchain
            .block_by_transaction_hash(transaction_hash)?
            .ok_or_else(|| ProviderError::InvalidTransactionHash(*transaction_hash))?;

        let header = block.header();
        let block_spec = Some(BlockSpec::Number(header.number));

        let cfg_env = self.create_evm_config(block_spec.as_ref())?;

        let transactions = block.transactions().to_vec();

        let prev_block_number = block.header().number - 1;
        let prev_block_spec = Some(BlockSpec::Number(prev_block_number));

        self.execute_in_block_context(
            prev_block_spec.as_ref(),
            |blockchain, _prev_block, state| {
                let block_env = BlockEnv {
                    number: U256::from(header.number),
                    coinbase: header.beneficiary,
                    timestamp: U256::from(header.timestamp),
                    gas_limit: U256::from(header.gas_limit),
                    basefee: header.base_fee_per_gas.unwrap_or_default(),
                    difficulty: U256::from(header.difficulty),
                    prevrandao: if cfg_env.spec_id >= SpecId::MERGE {
                        Some(header.mix_hash)
                    } else {
                        None
                    },
                    blob_excess_gas_and_price: header
                        .blob_gas
                        .as_ref()
                        .map(|BlobGas { excess_gas, .. }| BlobExcessGasAndPrice::new(*excess_gas)),
                };

                debug_trace_transaction(
                    blockchain,
                    state.clone(),
                    cfg_env,
                    trace_config,
                    block_env,
                    transactions,
                    transaction_hash,
                )
                .map_err(ProviderError::DebugTrace)
            },
        )?
    }

    pub fn debug_trace_call(
        &mut self,
        transaction: ExecutableTransaction,
        block_spec: Option<&BlockSpec>,
        trace_config: DebugTraceConfig,
    ) -> Result<DebugTraceResult, ProviderError<LoggerErrorT>> {
        let cfg_env = self.create_evm_config(block_spec)?;

        let tx_env: TxEnv = transaction.into();

        let mut tracer = TracerEip3155::new(trace_config);

        self.execute_in_block_context(block_spec, |blockchain, block, state| {
            let result = run_call(RunCallArgs {
                blockchain,
                header: block.header(),
                state,
                state_overrides: &StateOverrides::default(),
                cfg_env: cfg_env.clone(),
                tx_env: tx_env.clone(),
                inspector: Some(&mut tracer),
            })?;

            Ok(execution_result_to_debug_result(result, tracer))
        })?
    }

    /// Estimate the gas cost of a transaction. Matches Hardhat behavior.
    pub fn estimate_gas(
        &mut self,
        transaction: ExecutableTransaction,
        block_spec: &BlockSpec,
    ) -> Result<u64, ProviderError<LoggerErrorT>> {
        let cfg_env = self.create_evm_config(Some(block_spec))?;
        // Minimum gas cost that is required for transaction to be included in
        // a block
        let minimum_cost = transaction.initial_cost(self.spec_id());
        let transaction_hash = *transaction.hash();
        let tx_env: TxEnv = transaction.into();

        let state_overrides = StateOverrides::default();

        self.execute_in_block_context(Some(block_spec), |blockchain, block, state| {
            let mut inspector =
                DualInspector::new(EvmInspector::default(), TraceCollector::default());

            let header = block.header();

            // Measure the gas used by the transaction with optional limit from call request
            // defaulting to block limit. Report errors from initial call as if from
            // `eth_call`.
            let result = call::run_call(RunCallArgs {
                blockchain,
                header,
                state,
                state_overrides: &state_overrides,
                cfg_env: cfg_env.clone(),
                tx_env: tx_env.clone(),
                inspector: Some(&mut inspector),
            })?;

            let (debug_inspector, tracer) = inspector.into_parts();

            let mut initial_estimation = match result {
                ExecutionResult::Success { gas_used, .. } => Ok(gas_used),
                ExecutionResult::Revert { output, .. } => Err(TransactionFailure::revert(
                    output,
                    transaction_hash,
                    tracer.into_trace(),
                )),
                ExecutionResult::Halt { reason, .. } => Err(TransactionFailure::halt(
                    reason,
                    transaction_hash,
                    tracer.into_trace(),
                )),
            }
            .map_err(|transaction_failure| EstimateGasFailure {
                console_log_inputs: debug_inspector.into_console_log_encoded_messages(),
                transaction_failure,
            })?;

            // Ensure that the initial estimation is at least the minimum cost + 1.
            if initial_estimation <= minimum_cost {
                initial_estimation = minimum_cost + 1;
            }

            // Test if the transaction would be successful with the initial estimation
            let result = gas::check_gas_limit(CheckGasLimitArgs {
                blockchain,
                header,
                state,
                state_overrides: &state_overrides,
                cfg_env: cfg_env.clone(),
                tx_env: tx_env.clone(),
                transaction_hash: &transaction_hash,
                gas_limit: initial_estimation,
            })?;

            // Return the initial estimation if it was successful
            if result {
                return Ok(initial_estimation);
            }

            // Correct the initial estimation if the transaction failed with the actually
            // used gas limit. This can happen if the execution logic is based
            // on the available gas.
            let estimation = gas::binary_search_estimation(BinarySearchEstimationArgs {
                blockchain,
                header,
                state,
                state_overrides: &state_overrides,
                cfg_env: cfg_env.clone(),
                tx_env: tx_env.clone(),
                transaction_hash: &transaction_hash,
                lower_bound: initial_estimation,
                upper_bound: header.gas_limit,
            })?;

            Ok(estimation)
        })?
    }

    // Matches Hardhat implementation
    pub fn fee_history(
        &mut self,
        block_count: u64,
        newest_block_spec: &BlockSpec,
        percentiles: Option<Vec<RewardPercentile>>,
    ) -> Result<FeeHistoryResult, ProviderError<LoggerErrorT>> {
        if self.spec_id() < SpecId::LONDON {
            return Err(ProviderError::UnmetHardfork {
                actual: self.spec_id(),
                minimum: SpecId::LONDON,
            });
        }

        let latest_block_number = self.last_block_number();
        let pending_block_number = latest_block_number + 1;
        let newest_block_number = self
            .block_by_block_spec(newest_block_spec)?
            // None if pending block
            .map_or(pending_block_number, |block| block.header().number);
        let oldest_block_number = if newest_block_number < block_count {
            0
        } else {
            newest_block_number - block_count + 1
        };
        let last_block_number = newest_block_number + 1;

        let pending_block = if last_block_number >= pending_block_number {
            let DebugMineBlockResultAndState { block, .. } = self.mine_pending_block()?;
            Some(block)
        } else {
            None
        };

        let mut result = FeeHistoryResult::new(oldest_block_number);

        let mut reward_and_percentile = percentiles.and_then(|percentiles| {
            if percentiles.is_empty() {
                None
            } else {
                Some((Vec::default(), percentiles))
            }
        });

        let range_includes_remote_blocks = self.fork_metadata.as_ref().map_or(false, |metadata| {
            oldest_block_number <= metadata.fork_block_number
        });

        if range_includes_remote_blocks {
            let last_remote_block = cmp::min(
                self.fork_metadata
                    .as_ref()
                    .expect("we checked that there is a fork")
                    .fork_block_number,
                last_block_number,
            );
            let remote_block_count = last_remote_block - oldest_block_number + 1;

            let rpc_client = self
                .rpc_client
                .as_ref()
                .expect("we checked that there is a fork");
            let FeeHistoryResult {
                oldest_block: _,
                base_fee_per_gas,
                gas_used_ratio,
                reward: remote_reward,
            } = tokio::task::block_in_place(|| {
                self.runtime_handle.block_on(
                    rpc_client.fee_history(
                        remote_block_count,
                        newest_block_spec.clone(),
                        reward_and_percentile
                            .as_ref()
                            .map(|(_, percentiles)| percentiles.clone()),
                    ),
                )
            })?;

            result.base_fee_per_gas = base_fee_per_gas;
            result.gas_used_ratio = gas_used_ratio;
            if let Some((ref mut reward, _)) = reward_and_percentile.as_mut() {
                if let Some(remote_reward) = remote_reward {
                    *reward = remote_reward;
                }
            }
        }

        let first_local_block = if range_includes_remote_blocks {
            cmp::min(
                self.fork_metadata
                    .as_ref()
                    .expect("we checked that there is a fork")
                    .fork_block_number,
                last_block_number,
            ) + 1
        } else {
            oldest_block_number
        };

        for block_number in first_local_block..=last_block_number {
            if block_number < pending_block_number {
                let block = self
                    .blockchain
                    .block_by_number(block_number)?
                    .expect("Block must exist as i is at most the last block number");

                let header = block.header();
                result
                    .base_fee_per_gas
                    .push(header.base_fee_per_gas.unwrap_or(U256::ZERO));

                if block_number < last_block_number {
                    result
                        .gas_used_ratio
                        .push(gas_used_ratio(header.gas_used, header.gas_limit));

                    if let Some((ref mut reward, percentiles)) = reward_and_percentile.as_mut() {
                        reward.push(compute_rewards(&block, percentiles)?);
                    }
                }
            } else if block_number == pending_block_number {
                let next_block_base_fee_per_gas = self
                    .next_block_base_fee_per_gas()?
                    .expect("We checked that EIP-1559 is active");
                result.base_fee_per_gas.push(next_block_base_fee_per_gas);

                if block_number < last_block_number {
                    let block = pending_block.as_ref().expect("We mined the pending block");
                    let header = block.header();
                    result
                        .gas_used_ratio
                        .push(gas_used_ratio(header.gas_used, header.gas_limit));

                    if let Some((ref mut reward, percentiles)) = reward_and_percentile.as_mut() {
                        // We don't compute this for the pending block, as there's no
                        // effective miner fee yet.
                        reward.push(percentiles.iter().map(|_| U256::ZERO).collect());
                    }
                }
            } else if block_number == pending_block_number + 1 {
                let block = pending_block.as_ref().expect("We mined the pending block");
                result
                    .base_fee_per_gas
                    .push(calculate_next_base_fee(block.header()));
            }
        }

        if let Some((reward, _)) = reward_and_percentile {
            result.reward = Some(reward);
        }

        Ok(result)
    }

    pub fn gas_price(&self) -> Result<U256, ProviderError<LoggerErrorT>> {
        const PRE_EIP_1559_GAS_PRICE: u64 = 8_000_000_000;
        const SUGGESTED_PRIORITY_FEE_PER_GAS: u64 = 1_000_000_000;

        if let Some(next_block_gas_fee_per_gas) = self.next_block_base_fee_per_gas()? {
            Ok(next_block_gas_fee_per_gas + U256::from(SUGGESTED_PRIORITY_FEE_PER_GAS))
        } else {
            // We return a hardcoded value for networks without EIP-1559
            Ok(U256::from(PRE_EIP_1559_GAS_PRICE))
        }
    }

    pub fn get_code(
        &mut self,
        address: Address,
        block_spec: Option<&BlockSpec>,
    ) -> Result<Bytes, ProviderError<LoggerErrorT>> {
        self.execute_in_block_context(block_spec, move |_blockchain, _block, state| {
            let code = state
                .basic(address)?
                .map_or(Ok(Bytes::new()), |account_info| {
                    state.code_by_hash(account_info.code_hash).map(|bytecode| {
                        // The `Bytecode` REVM struct pad the bytecode with 33 bytes of 0s for the
                        // `Checked` and `Analysed` variants. `Bytecode::original_bytes` returns
                        // unpadded version.
                        bytecode.original_bytes()
                    })
                })?;

            Ok(code)
        })?
    }

    pub fn get_filter_changes(&mut self, filter_id: &U256) -> Option<FilteredEvents> {
        self.filters.get_mut(filter_id).map(Filter::take_events)
    }

    pub fn get_filter_logs(
        &mut self,
        filter_id: &U256,
    ) -> Result<Option<Vec<LogOutput>>, ProviderError<LoggerErrorT>> {
        self.filters
            .get_mut(filter_id)
            .map(|filter| {
                if let Some(events) = filter.take_log_events() {
                    Ok(events)
                } else {
                    Err(ProviderError::InvalidFilterSubscriptionType {
                        filter_id: *filter_id,
                        expected: SubscriptionType::Logs,
                        actual: filter.data.subscription_type(),
                    })
                }
            })
            .transpose()
    }

    pub fn get_storage_at(
        &mut self,
        address: Address,
        index: U256,
        block_spec: Option<&BlockSpec>,
    ) -> Result<U256, ProviderError<LoggerErrorT>> {
        self.execute_in_block_context::<Result<U256, ProviderError<LoggerErrorT>>>(
            block_spec,
            move |_blockchain, _block, state| Ok(state.storage(address, index)?),
        )?
    }

    pub fn get_transaction_count(
        &mut self,
        address: Address,
        block_spec: Option<&BlockSpec>,
    ) -> Result<u64, ProviderError<LoggerErrorT>> {
        self.execute_in_block_context::<Result<u64, ProviderError<LoggerErrorT>>>(
            block_spec,
            move |_blockchain, _block, state| {
                let nonce = state
                    .basic(address)?
                    .map_or(0, |account_info| account_info.nonce);

                Ok(nonce)
            },
        )?
    }

    pub fn impersonate_account(&mut self, address: Address) {
        self.impersonated_accounts.insert(address);
    }

    pub fn increase_block_time(&mut self, increment: u64) -> i64 {
        self.block_time_offset_seconds += i64::try_from(increment).expect("increment too large");
        self.block_time_offset_seconds
    }

    pub fn instance_id(&self) -> &B256 {
        &self.instance_id
    }

    pub fn interval_mine(&mut self) -> Result<bool, ProviderError<LoggerErrorT>> {
        let result = self.mine_and_commit_block(None)?;

        self.logger
            .log_interval_mined(self.spec_id(), &result)
            .map_err(ProviderError::Logger)?;

        Ok(true)
    }

    pub fn logger_mut(
        &mut self,
    ) -> &mut dyn SyncLogger<BlockchainError = BlockchainError, LoggerError = LoggerErrorT> {
        &mut *self.logger
    }

    pub fn logs(&self, filter: LogFilter) -> Result<Vec<FilterLog>, ProviderError<LoggerErrorT>> {
        self.blockchain
            .logs(
                filter.from_block,
                filter
                    .to_block
                    .unwrap_or(self.blockchain.last_block_number()),
                &filter.addresses,
                &filter.normalized_topics,
            )
            .map_err(ProviderError::Blockchain)
    }

    pub fn make_snapshot(&mut self) -> u64 {
        let id = self.next_snapshot_id;
        self.next_snapshot_id += 1;

        let snapshot = Snapshot {
            block_number: self.blockchain.last_block_number(),
            block_number_to_state_id: self.block_number_to_state_id.clone(),
            block_time_offset_seconds: self.block_time_offset_seconds,
            coinbase: self.beneficiary,
            irregular_state: self.irregular_state.clone(),
            mem_pool: self.mem_pool.clone(),
            next_block_base_fee_per_gas: self.next_block_base_fee_per_gas,
            next_block_timestamp: self.next_block_timestamp,
            prev_randao_generator: self.prev_randao_generator.clone(),
            time: Instant::now(),
        };
        self.snapshots.insert(id, snapshot);

        id
    }

    pub fn mine_and_commit_block(
        &mut self,
        timestamp: Option<u64>,
    ) -> Result<DebugMineBlockResult<BlockchainError>, ProviderError<LoggerErrorT>> {
        let (block_timestamp, new_offset) = self.next_block_timestamp(timestamp)?;
        let prevrandao = if self.blockchain.spec_id() >= SpecId::MERGE {
            Some(self.prev_randao_generator.next_value())
        } else {
            None
        };

        let result = self.mine_block(block_timestamp, prevrandao)?;

        if let Some(new_offset) = new_offset {
            self.block_time_offset_seconds = new_offset;
        }

        // Reset the next block base fee per gas upon successful execution
        self.next_block_base_fee_per_gas.take();

        // Reset next block time stamp
        self.next_block_timestamp.take();

        let block_and_total_difficulty = self
            .blockchain
            .insert_block(result.block, result.state_diff)
            .map_err(ProviderError::Blockchain)?;

        self.mem_pool
            .update(&result.state)
            .map_err(ProviderError::MemPoolUpdate)?;

        let block = &block_and_total_difficulty.block;
        for (filter_id, filter) in self.filters.iter_mut() {
            match &mut filter.data {
                FilterData::Logs { criteria, logs } => {
                    let bloom = &block.header().logs_bloom;
                    if bloom_contains_log_filter(bloom, criteria) {
                        let receipts = block.transaction_receipts()?;
                        let new_logs = receipts.iter().flat_map(|receipt| receipt.logs());

                        let mut filtered_logs = filter_logs(new_logs, criteria);
                        if filter.is_subscription {
                            (self.subscriber_callback)(SubscriptionEvent {
                                filter_id: *filter_id,
                                result: SubscriptionEventData::Logs(filtered_logs.clone()),
                            });
                        } else {
                            logs.append(&mut filtered_logs);
                        }
                    }
                }
                FilterData::NewHeads(block_hashes) => {
                    if filter.is_subscription {
                        (self.subscriber_callback)(SubscriptionEvent {
                            filter_id: *filter_id,
                            result: SubscriptionEventData::NewHeads(
                                block_and_total_difficulty.clone(),
                            ),
                        });
                    } else {
                        block_hashes.push(*block.hash());
                    }
                }
                FilterData::NewPendingTransactions(_) => (),
            }
        }

        // Remove outdated filters
        self.filters.retain(|_, filter| !filter.has_expired());

        self.add_state_to_cache(result.state, block.header().number);

        Ok(DebugMineBlockResult {
            block: block_and_total_difficulty.block,
            transaction_results: result.transaction_results,
            transaction_traces: result.transaction_traces,
            console_log_inputs: result.console_log_inputs,
        })
    }

    /// Mines `number_of_blocks` blocks with the provided `interval` between
    /// them.
    pub fn mine_and_commit_blocks(
        &mut self,
        number_of_blocks: u64,
        interval: u64,
    ) -> Result<Vec<DebugMineBlockResult<BlockchainError>>, ProviderError<LoggerErrorT>> {
        // There should be at least 2 blocks left for the reservation to work,
        // because we always mine a block after it. But here we use a bigger
        // number to err on the side of safety.
        const MINIMUM_RESERVABLE_BLOCKS: u64 = 6;

        if number_of_blocks == 0 {
            return Ok(Vec::new());
        }

        let mine_block_with_interval = |data: &mut ProviderData<LoggerErrorT>,
                                        mined_blocks: &mut Vec<
            DebugMineBlockResult<BlockchainError>,
        >|
         -> Result<(), ProviderError<LoggerErrorT>> {
            let previous_timestamp = mined_blocks
                .last()
                .expect("at least one block was mined")
                .block
                .header()
                .timestamp;

            let mined_block = data.mine_and_commit_block(Some(previous_timestamp + interval))?;
            mined_blocks.push(mined_block);

            Ok(())
        };

        // Limit the pre-allocated capacity based on the minimum reservable number of
        // blocks to avoid too large allocations.
        let mut mined_blocks = Vec::with_capacity(
            usize::try_from(number_of_blocks.min(2 * MINIMUM_RESERVABLE_BLOCKS))
                .expect("number of blocks exceeds {u64::MAX}"),
        );

        // we always mine the first block, and we don't apply the interval for it
        mined_blocks.push(self.mine_and_commit_block(None)?);

        while u64::try_from(mined_blocks.len()).expect("usize cannot be larger than u128")
            < number_of_blocks
            && self.mem_pool.has_pending_transactions()
        {
            mine_block_with_interval(self, &mut mined_blocks)?;
        }

        // If there is at least one remaining block, we mine one. This way, we
        // guarantee that there's an empty block immediately before and after the
        // reservation. This makes the logging easier to get right.
        if u64::try_from(mined_blocks.len()).expect("usize cannot be larger than u128")
            < number_of_blocks
        {
            mine_block_with_interval(self, &mut mined_blocks)?;
        }

        let remaining_blocks = number_of_blocks
            - u64::try_from(mined_blocks.len()).expect("usize cannot be larger than u128");

        if remaining_blocks < MINIMUM_RESERVABLE_BLOCKS {
            for _ in 0..remaining_blocks {
                mine_block_with_interval(self, &mut mined_blocks)?;
            }
        } else {
            let current_state = (*self.current_state()?).clone();

            self.blockchain
                .reserve_blocks(remaining_blocks - 1, interval)?;

            // Ensure there is a cache entry for the last reserved block, to avoid
            // recomputation
            self.add_state_to_cache(current_state, self.last_block_number());

            let previous_timestamp = self.blockchain.last_block()?.header().timestamp;

            let mined_block = self.mine_and_commit_block(Some(previous_timestamp + interval))?;
            mined_blocks.push(mined_block);
        }

        mined_blocks.shrink_to_fit();

        Ok(mined_blocks)
    }

    pub fn network_id(&self) -> String {
        self.initial_config.network_id.to_string()
    }

    /// Calculates the next block's base fee per gas.
    pub fn next_block_base_fee_per_gas(&self) -> Result<Option<U256>, BlockchainError> {
        if self.spec_id() < SpecId::LONDON {
            return Ok(None);
        }

        self.next_block_base_fee_per_gas
            .map_or_else(
                || {
                    let last_block = self.last_block()?;

                    let base_fee = calculate_next_base_fee(last_block.header());

                    Ok(base_fee)
                },
                Ok,
            )
            .map(Some)
    }

    /// Calculates the gas price for the next block.
    pub fn next_gas_price(&self) -> Result<U256, BlockchainError> {
        if let Some(next_block_base_fee_per_gas) = self.next_block_base_fee_per_gas()? {
            let suggested_priority_fee_per_gas = U256::from(1_000_000_000u64);
            Ok(next_block_base_fee_per_gas + suggested_priority_fee_per_gas)
        } else {
            // We return a hardcoded value for networks without EIP-1559
            Ok(U256::from(8_000_000_000u64))
        }
    }

    pub fn nonce(
        &mut self,
        address: &Address,
        block_spec: Option<&BlockSpec>,
        state_overrides: &StateOverrides,
    ) -> Result<u64, ProviderError<LoggerErrorT>> {
        state_overrides
            .account_override(address)
            .and_then(|account_override| account_override.nonce)
            .map_or_else(
                || {
                    if matches!(block_spec, Some(BlockSpec::Tag(BlockTag::Pending))) {
                        self.account_next_nonce(address)
                    } else {
                        self.execute_in_block_context(
                            block_spec,
                            move |_blockchain, _block, state| {
                                let nonce =
                                    state.basic(*address)?.map_or(0, |account| account.nonce);

                                Ok(nonce)
                            },
                        )?
                    }
                },
                Ok,
            )
    }

    pub fn pending_transactions(&self) -> impl Iterator<Item = &ExecutableTransaction> {
        self.mem_pool.transactions()
    }

    pub fn remove_filter(&mut self, filter_id: &U256) -> bool {
        self.remove_filter_impl::</* IS_SUBSCRIPTION */ false>(filter_id)
    }

    pub fn remove_subscription(&mut self, filter_id: &U256) -> bool {
        self.remove_filter_impl::</* IS_SUBSCRIPTION */ true>(filter_id)
    }

    /// Removes the transaction with the provided hash from the mem pool, if it
    /// exists.
    pub fn remove_pending_transaction(
        &mut self,
        transaction_hash: &B256,
    ) -> Option<OrderedTransaction> {
        self.mem_pool.remove_transaction(transaction_hash)
    }

    pub fn revert_to_snapshot(&mut self, snapshot_id: u64) -> bool {
        // Ensure that, if the snapshot exists, we also remove all subsequent snapshots,
        // as they can only be used once in Ganache.
        let mut removed_snapshots = self.snapshots.split_off(&snapshot_id);

        if let Some(snapshot) = removed_snapshots.remove(&snapshot_id) {
            let Snapshot {
                block_number,
                block_number_to_state_id,
                block_time_offset_seconds,
                coinbase,
                irregular_state,
                mem_pool,
                next_block_base_fee_per_gas,
                next_block_timestamp,
                prev_randao_generator,
                time,
            } = snapshot;

            self.block_number_to_state_id = block_number_to_state_id;

            // We compute a new offset such that:
            // now + new_offset == snapshot_date + old_offset
            let duration_since_snapshot = Instant::now().duration_since(time);
            self.block_time_offset_seconds = block_time_offset_seconds
                + i64::try_from(duration_since_snapshot.as_secs()).expect("duration too large");

            self.beneficiary = coinbase;
            self.blockchain
                .revert_to_block(block_number)
                .expect("Snapshotted block should exist");

            self.irregular_state = irregular_state;
            self.mem_pool = mem_pool;
            self.next_block_base_fee_per_gas = next_block_base_fee_per_gas;
            self.next_block_timestamp = next_block_timestamp;
            self.prev_randao_generator = prev_randao_generator;

            true
        } else {
            false
        }
    }

    pub fn run_call(
        &mut self,
        transaction: ExecutableTransaction,
        block_spec: Option<&BlockSpec>,
        state_overrides: &StateOverrides,
    ) -> Result<CallResult, ProviderError<LoggerErrorT>> {
        let cfg_env = self.create_evm_config(block_spec)?;
        let tx_env = transaction.into();

        self.execute_in_block_context(block_spec, |blockchain, block, state| {
            let mut inspector =
                DualInspector::new(EvmInspector::default(), TraceCollector::default());

            let execution_result = call::run_call(RunCallArgs {
                blockchain,
                header: block.header(),
                state,
                state_overrides,
                cfg_env,
                tx_env,
                inspector: Some(&mut inspector),
            })?;

            let (debug_inspector, tracer) = inspector.into_parts();

            Ok(CallResult {
                console_log_inputs: debug_inspector.into_console_log_encoded_messages(),
                execution_result,
                trace: tracer.into_trace(),
            })
        })?
    }

    pub fn transaction_receipt(
        &self,
        transaction_hash: &B256,
    ) -> Result<Option<Arc<BlockReceipt>>, ProviderError<LoggerErrorT>> {
        self.blockchain
            .receipt_by_transaction_hash(transaction_hash)
            .map_err(ProviderError::Blockchain)
    }

    pub fn set_min_gas_price(
        &mut self,
        min_gas_price: U256,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        if self.spec_id() >= SpecId::LONDON {
            return Err(ProviderError::SetMinGasPriceUnsupported);
        }

        self.min_gas_price = min_gas_price;

        Ok(())
    }

    pub fn send_transaction(
        &mut self,
        signed_transaction: ExecutableTransaction,
    ) -> Result<SendTransactionResult, ProviderError<LoggerErrorT>> {
        let snapshot_id = if self.is_auto_mining {
            self.validate_auto_mine_transaction(&signed_transaction)?;

            Some(self.make_snapshot())
        } else {
            None
        };

        let transaction_hash =
            self.add_pending_transaction(signed_transaction)
                .map_err(|error| {
                    if let Some(snapshot_id) = snapshot_id {
                        self.revert_to_snapshot(snapshot_id);
                    }

                    error
                })?;

        let mut mining_results = Vec::new();
        let transaction_result = snapshot_id
            .map(
                |snapshot_id| -> Result<(ExecutionResult, Trace), ProviderError<LoggerErrorT>> {
                    let transaction_result = loop {
                        let result = self.mine_and_commit_block(None).map_err(|error| {
                            self.revert_to_snapshot(snapshot_id);

                            error
                        })?;

                        let transaction_result = izip!(
                            result.block.transactions().iter(),
                            result.transaction_results.iter(),
                            result.transaction_traces.iter()
                        )
                        .find_map(|(transaction, result, trace)| {
                            if *transaction.hash() == transaction_hash {
                                Some((result.clone(), trace.clone()))
                            } else {
                                None
                            }
                        });

                        mining_results.push(result);

                        if let Some(transaction_result) = transaction_result {
                            break transaction_result;
                        }
                    };

                    while self.mem_pool.has_pending_transactions() {
                        let result = self.mine_and_commit_block(None).map_err(|error| {
                            self.revert_to_snapshot(snapshot_id);

                            error
                        })?;

                        mining_results.push(result);
                    }

                    self.snapshots.remove(&snapshot_id);

                    Ok(transaction_result)
                },
            )
            .transpose()?;

        Ok(SendTransactionResult {
            transaction_hash,
            transaction_result,
            mining_results,
        })
    }

    /// Sets whether the miner should mine automatically.
    pub fn set_auto_mining(&mut self, enabled: bool) {
        self.is_auto_mining = enabled;
    }

    pub fn set_balance(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        let mut modified_state = (*self.current_state()?).clone();
        let account_info = modified_state.modify_account(
            address,
            AccountModifierFn::new(Box::new(move |account_balance, _, _| {
                *account_balance = balance;
            })),
            &|| {
                Ok(AccountInfo {
                    balance,
                    nonce: 0,
                    code: None,
                    code_hash: KECCAK_EMPTY,
                })
            },
        )?;

        let state_root = modified_state.state_root()?;

        self.mem_pool.update(&modified_state)?;

        let block_number = self.blockchain.last_block_number();
        self.irregular_state
            .state_override_at_block_number(block_number)
            .or_insert_with(|| StateOverride::with_state_root(state_root))
            .diff
            .apply_account_change(address, account_info.clone());

        self.add_state_to_cache(modified_state, block_number);

        Ok(())
    }

    /// Sets the gas limit used for mining new blocks.
    pub fn set_block_gas_limit(
        &mut self,
        gas_limit: u64,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        let state = self.current_state()?;
        self.mem_pool
            .set_block_gas_limit(&*state, gas_limit)
            .map_err(ProviderError::State)
    }

    pub fn set_code(
        &mut self,
        address: Address,
        code: Bytes,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        let code = Bytecode::new_raw(code.clone());
        let default_code = code.clone();
        let irregular_code = code.clone();

        // We clone to automatically revert in case of subsequent errors.
        let mut modified_state = (*self.current_state()?).clone();
        let mut account_info = modified_state.modify_account(
            address,
            AccountModifierFn::new(Box::new(move |_, _, account_code| {
                *account_code = Some(code.clone());
            })),
            &|| {
                Ok(AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code: Some(default_code.clone()),
                    code_hash: KECCAK_EMPTY,
                })
            },
        )?;

        // The code was stripped from the account, so we need to re-add it for the
        // irregular state.
        account_info.code = Some(irregular_code.clone());

        let state_root = modified_state.state_root()?;

        let block_number = self.blockchain.last_block_number();
        self.irregular_state
            .state_override_at_block_number(block_number)
            .or_insert_with(|| StateOverride::with_state_root(state_root))
            .diff
            .apply_account_change(address, account_info.clone());

        self.add_state_to_cache(modified_state, block_number);

        Ok(())
    }

    /// Sets the coinbase.
    pub fn set_coinbase(&mut self, coinbase: Address) {
        self.beneficiary = coinbase;
    }

    /// Sets the next block's base fee per gas.
    pub fn set_next_block_base_fee_per_gas(
        &mut self,
        base_fee_per_gas: U256,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        let spec_id = self.spec_id();
        if spec_id < SpecId::LONDON {
            return Err(ProviderError::SetNextBlockBaseFeePerGasUnsupported { spec_id });
        }

        self.next_block_base_fee_per_gas = Some(base_fee_per_gas);

        Ok(())
    }

    /// Set the next block timestamp.
    pub fn set_next_block_timestamp(
        &mut self,
        timestamp: u64,
    ) -> Result<u64, ProviderError<LoggerErrorT>> {
        let latest_block = self.blockchain.last_block()?;
        let latest_block_header = latest_block.header();

        match timestamp.cmp(&latest_block_header.timestamp) {
            Ordering::Less => Err(ProviderError::TimestampLowerThanPrevious {
                proposed: timestamp,
                previous: latest_block_header.timestamp,
            }),
            Ordering::Equal => Err(ProviderError::TimestampEqualsPrevious {
                proposed: timestamp,
            }),
            Ordering::Greater => {
                self.next_block_timestamp = Some(timestamp);
                Ok(timestamp)
            }
        }
    }

    /// Sets the next block's prevrandao.
    pub fn set_next_prev_randao(
        &mut self,
        prev_randao: B256,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        let spec_id = self.spec_id();
        if spec_id < SpecId::MERGE {
            return Err(ProviderError::SetNextPrevRandaoUnsupported { spec_id });
        }

        self.prev_randao_generator.set_next(prev_randao);

        Ok(())
    }

    pub fn set_nonce(
        &mut self,
        address: Address,
        nonce: u64,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        if mempool::has_transactions(&self.mem_pool) {
            return Err(ProviderError::SetAccountNonceWithPendingTransactions);
        }

        let previous_nonce = self
            .current_state()?
            .basic(address)?
            .map_or(0, |account| account.nonce);

        if nonce < previous_nonce {
            return Err(ProviderError::SetAccountNonceLowerThanCurrent {
                previous: previous_nonce,
                proposed: nonce,
            });
        }

        // We clone to automatically revert in case of subsequent errors.
        let mut modified_state = (*self.current_state()?).clone();
        let account_info = modified_state.modify_account(
            address,
            AccountModifierFn::new(Box::new(move |_, account_nonce, _| *account_nonce = nonce)),
            &|| {
                Ok(AccountInfo {
                    balance: U256::ZERO,
                    nonce,
                    code: None,
                    code_hash: KECCAK_EMPTY,
                })
            },
        )?;

        let state_root = modified_state.state_root()?;

        self.mem_pool.update(&modified_state)?;

        let block_number = self.last_block_number();
        self.irregular_state
            .state_override_at_block_number(block_number)
            .or_insert_with(|| StateOverride::with_state_root(state_root))
            .diff
            .apply_account_change(address, account_info.clone());

        self.add_state_to_cache(modified_state, block_number);

        Ok(())
    }

    pub fn set_account_storage_slot(
        &mut self,
        address: Address,
        index: U256,
        value: U256,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        // We clone to automatically revert in case of subsequent errors.
        let mut modified_state = (*self.current_state()?).clone();
        modified_state.set_account_storage_slot(address, index, value)?;

        let old_value = modified_state.set_account_storage_slot(address, index, value)?;

        let slot = StorageSlot::new_changed(old_value, value);
        let account_info = modified_state.basic(address).and_then(|mut account_info| {
            // Retrieve the code if it's not empty. This is needed for the irregular state.
            if let Some(account_info) = &mut account_info {
                if account_info.code_hash != KECCAK_EMPTY {
                    account_info.code = Some(modified_state.code_by_hash(account_info.code_hash)?);
                }
            }

            Ok(account_info)
        })?;

        let state_root = modified_state.state_root()?;

        let block_number = self.blockchain.last_block_number();
        self.irregular_state
            .state_override_at_block_number(block_number)
            .or_insert_with(|| StateOverride::with_state_root(state_root))
            .diff
            .apply_storage_change(address, index, slot, account_info);

        self.add_state_to_cache(modified_state, block_number);

        Ok(())
    }

    pub fn sign(
        &self,
        address: &Address,
        message: Bytes,
    ) -> Result<Signature, ProviderError<LoggerErrorT>> {
        match self.local_accounts.get(address) {
            Some(secret_key) => Ok(Signature::new(&message[..], secret_key)?),
            None => Err(ProviderError::UnknownAddress { address: *address }),
        }
    }

    pub fn sign_typed_data_v4(
        &self,
        address: &Address,
        message: &TypedData,
    ) -> Result<Signature, ProviderError<LoggerErrorT>> {
        match self.local_accounts.get(address) {
            Some(secret_key) => {
                let hash: B256 = message.encode_eip712()?.into();
                Ok(Signature::new(RecoveryMessage::Hash(hash), secret_key)?)
            }
            None => Err(ProviderError::UnknownAddress { address: *address }),
        }
    }

    pub fn spec_id(&self) -> SpecId {
        self.blockchain.spec_id()
    }

    pub fn stop_impersonating_account(&mut self, address: Address) -> bool {
        self.impersonated_accounts.remove(&address)
    }

    pub fn total_difficulty_by_hash(
        &self,
        hash: &B256,
    ) -> Result<Option<U256>, ProviderError<LoggerErrorT>> {
        self.blockchain
            .total_difficulty_by_hash(hash)
            .map_err(ProviderError::Blockchain)
    }

    /// Get a transaction by hash from the blockchain or from the mempool if
    /// it's not mined yet.
    pub fn transaction_by_hash(
        &self,
        hash: &B256,
    ) -> Result<Option<TransactionAndBlock>, ProviderError<LoggerErrorT>> {
        let transaction = if let Some(tx) = self.mem_pool.transaction_by_hash(hash) {
            let signed_transaction = tx.pending().as_inner().clone();

            Some(TransactionAndBlock {
                signed_transaction,
                block_data: None,
                is_pending: true,
            })
        } else if let Some(block) = self.blockchain.block_by_transaction_hash(hash)? {
            let tx_index_u64 = self
                .blockchain
                .receipt_by_transaction_hash(hash)?
                .expect("If the transaction was inserted in a block, it must have a receipt")
                .transaction_index;
            let tx_index =
                usize::try_from(tx_index_u64).expect("Indices cannot be larger than usize::MAX");

            let signed_transaction = block
                .transactions()
                .get(tx_index)
                .expect("Transaction index must be valid, since it's from the receipt.")
                .as_inner()
                .clone();

            Some(TransactionAndBlock {
                signed_transaction,
                block_data: Some(BlockDataForTransaction {
                    block,
                    transaction_index: tx_index_u64,
                }),
                is_pending: false,
            })
        } else {
            None
        };

        Ok(transaction)
    }

    fn add_pending_transaction(
        &mut self,
        transaction: ExecutableTransaction,
    ) -> Result<B256, ProviderError<LoggerErrorT>> {
        let transaction_hash = *transaction.hash();

        let state = self.current_state()?;
        // Handles validation
        self.mem_pool.add_transaction(&*state, transaction)?;

        for (filter_id, filter) in self.filters.iter_mut() {
            if let FilterData::NewPendingTransactions(events) = &mut filter.data {
                if filter.is_subscription {
                    (self.subscriber_callback)(SubscriptionEvent {
                        filter_id: *filter_id,
                        result: SubscriptionEventData::NewPendingTransactions(transaction_hash),
                    });
                } else {
                    events.push(transaction_hash);
                }
            }
        }

        Ok(transaction_hash)
    }

    fn create_evm_config(
        &self,
        block_spec: Option<&BlockSpec>,
    ) -> Result<CfgEnv, ProviderError<LoggerErrorT>> {
        let block_number = block_spec
            .map(|block_spec| self.block_number_by_block_spec(block_spec))
            .transpose()?
            .flatten();

        let spec_id = if let Some(block_number) = block_number {
            self.blockchain.spec_at_block_number(block_number)?
        } else {
            self.blockchain.spec_id()
        };

        let mut evm_config = CfgEnv::default();
        evm_config.chain_id = self.blockchain.chain_id();
        evm_config.spec_id = spec_id;
        evm_config.limit_contract_code_size = if self.allow_unlimited_contract_size {
            Some(usize::MAX)
        } else {
            None
        };
        evm_config.disable_eip3607 = true;

        Ok(evm_config)
    }

    fn execute_in_block_context<T>(
        &mut self,
        block_spec: Option<&BlockSpec>,
        function: impl FnOnce(
            &dyn SyncBlockchain<BlockchainError, StateError>,
            &Arc<dyn SyncBlock<Error = BlockchainError>>,
            &Box<dyn SyncState<StateError>>,
        ) -> T,
    ) -> Result<T, ProviderError<LoggerErrorT>> {
        let block = if let Some(block_spec) = block_spec {
            self.block_by_block_spec(block_spec)?
        } else {
            Some(self.blockchain.last_block()?)
        };

        if let Some(block) = block {
            let block_header = block.header();
            let block_number = block_header.number;

            let contextual_state = self.get_or_compute_state(block_number)?;

            Ok(function(&*self.blockchain, &block, &contextual_state))
        } else {
            // Block spec is pending
            let result = self.mine_pending_block()?;

            let blockchain =
                BlockchainWithPending::new(&*self.blockchain, result.block, result.state_diff);

            let block = blockchain
                .last_block()
                .expect("The pending block is the last block");

            Ok(function(&blockchain, &block, &result.state))
        }
    }

    /// Mine a block at a specific timestamp
    fn mine_block(
        &mut self,
        timestamp: u64,
        prevrandao: Option<B256>,
    ) -> Result<DebugMineBlockResultAndState<StateError>, ProviderError<LoggerErrorT>> {
        let evm_config = self.create_evm_config(None)?;

        let mut inspector = EvmInspector::default();

        let state_to_be_modified = (*self.current_state()?).clone();

        let result = mine_block(
            &*self.blockchain,
            state_to_be_modified,
            &self.mem_pool,
            &evm_config,
            timestamp,
            self.beneficiary,
            self.min_gas_price,
            self.initial_config.mining.mem_pool.order,
            miner_reward(evm_config.spec_id).unwrap_or(U256::ZERO),
            self.next_block_base_fee_per_gas,
            prevrandao,
            self.dao_activation_block,
            Some(&mut inspector),
        )?;

        Ok(DebugMineBlockResultAndState::new(
            result,
            inspector.into_console_log_encoded_messages(),
        ))
    }

    /// Mines a pending block, without modifying any values.
    pub fn mine_pending_block(
        &mut self,
    ) -> Result<DebugMineBlockResultAndState<StateError>, ProviderError<LoggerErrorT>> {
        let (block_timestamp, _new_offset) = self.next_block_timestamp(None)?;

        // Mining a pending block shouldn't affect the mix hash.
        let prevrandao = None;

        self.mine_block(block_timestamp, prevrandao)
    }

    pub fn mining_config(&self) -> &MiningConfig {
        &self.initial_config.mining
    }

    /// Get the timestamp for the next block.
    /// Ported from <https://github.com/NomicFoundation/hardhat/blob/b84baf2d9f5d3ea897c06e0ecd5e7084780d8b6c/packages/hardhat-core/src/internal/hardhat-network/provider/node.ts#L1942>
    fn next_block_timestamp(
        &self,
        timestamp: Option<u64>,
    ) -> Result<(u64, Option<i64>), ProviderError<LoggerErrorT>> {
        let latest_block = self.blockchain.last_block()?;
        let latest_block_header = latest_block.header();

        let current_timestamp =
            i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
                .expect("timestamp too large");

        let (mut block_timestamp, mut new_offset) = if let Some(timestamp) = timestamp {
            timestamp.checked_sub(latest_block_header.timestamp).ok_or(
                ProviderError::TimestampLowerThanPrevious {
                    proposed: timestamp,
                    previous: latest_block_header.timestamp,
                },
            )?;

            let offset = i64::try_from(timestamp).expect("timestamp too large") - current_timestamp;
            (timestamp, Some(offset))
        } else if let Some(next_block_timestamp) = self.next_block_timestamp {
            let offset = i64::try_from(next_block_timestamp).expect("timestamp too large")
                - current_timestamp;

            (next_block_timestamp, Some(offset))
        } else {
            let next_timestamp = u64::try_from(current_timestamp + self.block_time_offset_seconds)
                .expect("timestamp must be positive");

            (next_timestamp, None)
        };

        let timestamp_needs_increase = block_timestamp == latest_block_header.timestamp
            && !self.allow_blocks_with_same_timestamp;
        if timestamp_needs_increase {
            block_timestamp += 1;
            if new_offset.is_none() {
                new_offset = Some(self.block_time_offset_seconds + 1);
            }
        }

        Ok((block_timestamp, new_offset))
    }

    fn next_filter_id(&mut self) -> U256 {
        self.last_filter_id = self
            .last_filter_id
            .checked_add(U256::from(1))
            .expect("filter id starts at zero, so it'll never overflow for U256");
        self.last_filter_id
    }

    fn remove_filter_impl<const IS_SUBSCRIPTION: bool>(&mut self, filter_id: &U256) -> bool {
        if let Some(filter) = self.filters.get(filter_id) {
            filter.is_subscription == IS_SUBSCRIPTION && self.filters.remove(filter_id).is_some()
        } else {
            false
        }
    }

    pub fn sign_transaction_request(
        &self,
        transaction_request: TransactionRequestAndSender,
    ) -> Result<ExecutableTransaction, ProviderError<LoggerErrorT>> {
        let TransactionRequestAndSender { request, sender } = transaction_request;

        if self.impersonated_accounts.contains(&sender) {
            let signed_transaction = request.fake_sign(&sender);

            Ok(ExecutableTransaction::with_caller(
                self.blockchain.spec_id(),
                signed_transaction,
                sender,
            )?)
        } else {
            let secret_key = self
                .local_accounts
                .get(&sender)
                .ok_or(ProviderError::UnknownAddress { address: sender })?;

            let signed_transaction = request.sign(secret_key)?;
            Ok(ExecutableTransaction::new(
                self.blockchain.spec_id(),
                signed_transaction,
            )?)
        }
    }

    fn validate_auto_mine_transaction(
        &mut self,
        transaction: &ExecutableTransaction,
    ) -> Result<(), ProviderError<LoggerErrorT>> {
        let next_nonce = { self.account_next_nonce(transaction.caller())? };

        match transaction.nonce().cmp(&next_nonce) {
            Ordering::Less => {
                return Err(ProviderError::AutoMineNonceTooLow {
                    expected: next_nonce,
                    actual: transaction.nonce(),
                })
            }
            Ordering::Equal => (),
            Ordering::Greater => {
                return Err(ProviderError::AutoMineNonceTooHigh {
                    expected: next_nonce,
                    actual: transaction.nonce(),
                })
            }
        }

        // Question: Why do we use the max priority fee per gas as gas price?
        let max_priority_fee_per_gas = transaction
            .max_priority_fee_per_gas()
            .unwrap_or_else(|| transaction.gas_price());

        if max_priority_fee_per_gas < self.min_gas_price {
            return Err(ProviderError::AutoMinePriorityFeeTooLow {
                expected: self.min_gas_price,
                actual: max_priority_fee_per_gas,
            });
        }

        if let Some(next_block_base_fee) = self.next_block_base_fee_per_gas()? {
            if let Some(max_fee_per_gas) = transaction.max_fee_per_gas() {
                if max_fee_per_gas < next_block_base_fee {
                    return Err(ProviderError::AutoMineMaxFeeTooLow {
                        expected: next_block_base_fee,
                        actual: max_fee_per_gas,
                    });
                }
            } else {
                let gas_price = transaction.gas_price();
                if gas_price < next_block_base_fee {
                    return Err(ProviderError::AutoMineGasPriceTooLow {
                        expected: next_block_base_fee,
                        actual: gas_price,
                    });
                }
            }
        }

        Ok(())
    }

    fn current_state(
        &mut self,
    ) -> Result<Arc<Box<dyn SyncState<StateError>>>, ProviderError<LoggerErrorT>> {
        self.get_or_compute_state(self.last_block_number())
    }

    fn get_or_compute_state(
        &mut self,
        block_number: u64,
    ) -> Result<Arc<Box<dyn SyncState<StateError>>>, ProviderError<LoggerErrorT>> {
        if let Some(state_id) = self.block_number_to_state_id.get(&block_number) {
            // We cannot use `LruCache::try_get_or_insert`, because it needs &mut self, but
            // we would need &self in the callback to reference the blockchain.
            if let Some(state) = self.block_state_cache.get(state_id) {
                return Ok(state.clone());
            }
        };

        let state = self
            .blockchain
            .state_at_block_number(block_number, self.irregular_state.state_overrides())?;
        let state_id = self.add_state_to_cache(state, block_number);
        Ok(self
            .block_state_cache
            .get(&state_id)
            // State must exist, since we just inserted it, and we have exclusive access to
            // the cache due to &mut self.
            .expect("State must exist")
            .clone())
    }

    fn add_state_to_cache(
        &mut self,
        state: Box<dyn SyncState<StateError>>,
        block_number: u64,
    ) -> StateId {
        let state_id = self.current_state_id.increment();
        self.block_state_cache.push(state_id, Arc::new(state));
        self.block_number_to_state_id.insert(block_number, state_id);
        state_id
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub(crate) struct StateId(u64);

impl StateId {
    /// Increment the current state id and return the incremented id.
    fn increment(&mut self) -> Self {
        self.0 += 1;
        *self
    }
}

fn block_time_offset_seconds(config: &ProviderConfig) -> Result<i64, CreationError> {
    config.initial_date.map_or(Ok(0), |initial_date| {
        let initial_timestamp = i64::try_from(
            initial_date
                .duration_since(UNIX_EPOCH)
                .map_err(|_e| CreationError::InvalidInitialDate(initial_date))?
                .as_secs(),
        )
        .expect("initial date must be representable as i64");

        let current_timestamp = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("current time must be after UNIX epoch")
                .as_secs(),
        )
        .expect("Current timestamp must be representable as i64");

        Ok(initial_timestamp - current_timestamp)
    })
}

struct BlockchainAndState {
    blockchain: Box<dyn SyncBlockchain<BlockchainError, StateError>>,
    fork_metadata: Option<ForkMetadata>,
    rpc_client: Option<RpcClient>,
    state: Box<dyn SyncState<StateError>>,
    irregular_state: IrregularState,
    prev_randao_generator: RandomHashGenerator,
    block_time_offset_seconds: i64,
    next_block_base_fee_per_gas: Option<U256>,
}

fn create_blockchain_and_state(
    runtime: runtime::Handle,
    config: &ProviderConfig,
    mut genesis_accounts: HashMap<Address, Account>,
) -> Result<BlockchainAndState, CreationError> {
    let mut prev_randao_generator = RandomHashGenerator::with_seed(edr_defaults::MIX_HASH_SEED);

    if let Some(fork_config) = &config.fork {
        let state_root_generator = Arc::new(parking_lot::Mutex::new(
            RandomHashGenerator::with_seed(edr_defaults::STATE_ROOT_HASH_SEED),
        ));

        let http_headers = fork_config
            .http_headers
            .as_ref()
            .map(|headers| HeaderMap::try_from(headers).map_err(CreationError::InvalidHttpHeaders))
            .transpose()?;

        let blockchain = tokio::task::block_in_place(|| {
            runtime.block_on(ForkedBlockchain::new(
                runtime.clone(),
                Some(config.chain_id),
                config.hardfork,
                RpcClient::new(
                    &fork_config.json_rpc_url,
                    config.cache_dir.clone(),
                    http_headers.clone(),
                )
                .expect("url ok"),
                fork_config.block_number,
                state_root_generator.clone(),
                &config.chains,
            ))
        })?;

        let fork_block_number = blockchain.last_block_number();

        let rpc_client = RpcClient::new(
            &fork_config.json_rpc_url,
            config.cache_dir.clone(),
            http_headers,
        )
        .expect("url ok");

        let mut irregular_state = IrregularState::default();
        if !genesis_accounts.is_empty() {
            let genesis_addresses = genesis_accounts.keys().cloned().collect::<Vec<_>>();
            let genesis_account_infos = tokio::task::block_in_place(|| {
                runtime.block_on(rpc_client.get_account_infos(
                    &genesis_addresses,
                    Some(BlockSpec::Number(fork_block_number)),
                ))
            })?;

            // Make sure that the nonce and the code of genesis accounts matches the fork
            // state as we only want to overwrite the balance.
            for (address, account_info) in genesis_addresses.into_iter().zip(genesis_account_infos)
            {
                genesis_accounts.entry(address).and_modify(|account| {
                    let AccountInfo {
                        balance: _,
                        nonce,
                        code,
                        code_hash,
                    } = &mut account.info;

                    *nonce = account_info.nonce;
                    *code = account_info.code;
                    *code_hash = account_info.code_hash;
                });
            }

            let state_root = state_root_generator.lock().next_value();

            irregular_state
                .state_override_at_block_number(fork_block_number)
                .or_insert(StateOverride {
                    diff: StateDiff::from(genesis_accounts),
                    state_root,
                });
        }

        let state = blockchain
            .state_at_block_number(fork_block_number, irregular_state.state_overrides())
            .expect("Fork state must exist");

        let block_time_offset_seconds = {
            let fork_block_timestamp = UNIX_EPOCH
                + Duration::from_secs(
                    blockchain
                        .last_block()
                        .map_err(CreationError::Blockchain)?
                        .header()
                        .timestamp,
                );

            let elapsed_time = SystemTime::now()
                .duration_since(fork_block_timestamp)
                .expect("current time must be after fork block")
                .as_secs();

            -i64::try_from(elapsed_time)
                .expect("Elapsed time since fork block must be representable as i64")
        };

        let next_block_base_fee_per_gas = if config.hardfork >= SpecId::LONDON {
            if let Some(base_fee) = config.initial_base_fee_per_gas {
                Some(base_fee)
            } else {
                let previous_base_fee = blockchain
                    .last_block()
                    .map_err(CreationError::Blockchain)?
                    .header()
                    .base_fee_per_gas;

                if previous_base_fee.is_none() {
                    Some(U256::from(DEFAULT_INITIAL_BASE_FEE_PER_GAS))
                } else {
                    None
                }
            }
        } else {
            None
        };

        Ok(BlockchainAndState {
            fork_metadata: Some(ForkMetadata {
                chain_id: blockchain.chain_id(),
                fork_block_number,
                fork_block_hash: *blockchain
                    .block_by_number(fork_block_number)
                    .map_err(CreationError::Blockchain)?
                    .expect("Fork block must exist")
                    .hash(),
            }),
            rpc_client: Some(rpc_client),
            blockchain: Box::new(blockchain),
            state: Box::new(state),
            irregular_state,
            prev_randao_generator,
            block_time_offset_seconds,
            next_block_base_fee_per_gas,
        })
    } else {
        let blockchain = LocalBlockchain::new(
            StateDiff::from(genesis_accounts),
            config.chain_id,
            config.hardfork,
            config.block_gas_limit,
            config.initial_date.map(|d| {
                d.duration_since(UNIX_EPOCH)
                    .expect("initial date must be after UNIX epoch")
                    .as_secs()
            }),
            Some(prev_randao_generator.next_value()),
            config.initial_base_fee_per_gas,
            config.initial_blob_gas.clone(),
            config.initial_parent_beacon_block_root,
        )?;

        let irregular_state = IrregularState::default();
        let state = blockchain
            .state_at_block_number(0, irregular_state.state_overrides())
            .expect("Genesis state must exist");

        let block_time_offset_seconds = block_time_offset_seconds(config)?;

        Ok(BlockchainAndState {
            fork_metadata: None,
            rpc_client: None,
            blockchain: Box::new(blockchain),
            state,
            irregular_state,
            block_time_offset_seconds,
            prev_randao_generator,
            // For local blockchain the initial base fee per gas config option is incorporated as
            // part of the genesis block.
            next_block_base_fee_per_gas: None,
        })
    }
}

/// The result returned by requesting a transaction.
#[derive(Debug, Clone)]
pub struct TransactionAndBlock {
    /// The signed transaction.
    pub signed_transaction: SignedTransaction,
    /// Block data in which the transaction is found if it has been mined.
    pub block_data: Option<BlockDataForTransaction>,
    /// Whether the transaction is pending
    pub is_pending: bool,
}

/// Block metadata for a transaction.
#[derive(Debug, Clone)]
pub struct BlockDataForTransaction {
    pub block: Arc<dyn SyncBlock<Error = BlockchainError>>,
    pub transaction_index: u64,
}

lazy_static! {
    static ref CONSOLE_ADDRESS: Address = "0x000000000000000000636F6e736F6c652e6c6f67"
        .parse()
        .expect("static ok");
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use anyhow::Context;
    use edr_eth::transaction::{Eip155TransactionRequest, TransactionKind, TransactionRequest};
    use edr_evm::hex;
    use edr_test_utils::env::get_alchemy_url;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        data::inspector::tests::{deploy_console_log_contract, ConsoleLogTransaction},
        test_utils::{
            create_test_config_with_impersonated_accounts_and_fork, one_ether, FORK_BLOCK_NUMBER,
        },
        Logger, ProviderConfig,
    };

    #[derive(Clone, Default)]
    struct NoopLogger;

    impl Logger for NoopLogger {
        type BlockchainError = BlockchainError;

        type LoggerError = Infallible;

        fn is_enabled(&self) -> bool {
            true
        }

        fn set_is_enabled(&mut self, _is_enabled: bool) {}

        fn print_method_logs(
            &mut self,
            _method: &str,
            _error: Option<&ProviderError<Infallible>>,
        ) -> Result<(), Infallible> {
            Ok(())
        }
    }

    struct ProviderTestFixture {
        // We need to keep the tempdir and runtime alive for the duration of the test
        _cache_dir: TempDir,
        _runtime: runtime::Runtime,
        config: ProviderConfig,
        provider_data: ProviderData<Infallible>,
        impersonated_account: Address,
    }

    impl ProviderTestFixture {
        pub(crate) fn new() -> anyhow::Result<Self> {
            Self::new_with_config(false)
        }

        pub(crate) fn new_forked() -> anyhow::Result<Self> {
            Self::new_with_config(true)
        }

        fn new_with_config(forked: bool) -> anyhow::Result<Self> {
            let cache_dir = TempDir::new()?;

            let impersonated_account = Address::random();
            let config = create_test_config_with_impersonated_accounts_and_fork(
                cache_dir.path().to_path_buf(),
                vec![impersonated_account],
                forked,
            );

            let logger = Box::<NoopLogger>::default();
            let subscription_callback = Box::new(|_| ());

            let runtime = runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .thread_name("provider-data-test")
                .build()?;

            let mut provider_data = ProviderData::new(
                runtime.handle().clone(),
                logger,
                subscription_callback,
                config.clone(),
            )?;
            provider_data
                .impersonated_accounts
                .insert(impersonated_account);

            Ok(Self {
                _cache_dir: cache_dir,
                _runtime: runtime,
                config,
                provider_data,
                impersonated_account,
            })
        }

        fn dummy_transaction_request(&self, nonce: Option<u64>) -> TransactionRequestAndSender {
            let request = TransactionRequest::Eip155(Eip155TransactionRequest {
                kind: TransactionKind::Call(Address::ZERO),
                gas_limit: 100_000,
                gas_price: U256::from(42_000_000_000_u64),
                value: U256::from(1),
                input: Bytes::default(),
                nonce: nonce.unwrap_or(0),
                chain_id: self.config.chain_id,
            });

            TransactionRequestAndSender {
                request,
                sender: self.first_local_account(),
            }
        }

        fn first_local_account(&self) -> Address {
            *self
                .provider_data
                .local_accounts
                .keys()
                .next()
                .expect("there are local accounts")
        }

        fn impersonated_dummy_transaction(&self) -> anyhow::Result<ExecutableTransaction> {
            let mut transaction = self.dummy_transaction_request(None);
            transaction.sender = self.impersonated_account;

            Ok(self.provider_data.sign_transaction_request(transaction)?)
        }

        fn signed_dummy_transaction(&self) -> anyhow::Result<ExecutableTransaction> {
            let transaction = self.dummy_transaction_request(None);
            Ok(self.provider_data.sign_transaction_request(transaction)?)
        }
    }

    #[test]
    fn test_local_account_balance() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        let account = *fixture
            .provider_data
            .local_accounts
            .keys()
            .next()
            .expect("there are local accounts");

        let last_block_number = fixture.provider_data.last_block_number();
        let block_spec = BlockSpec::Number(last_block_number);

        let balance = fixture.provider_data.balance(account, Some(&block_spec))?;

        assert_eq!(balance, one_ether());

        Ok(())
    }

    #[test]
    fn test_local_account_balance_forked() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new_forked()?;

        let account = *fixture
            .provider_data
            .local_accounts
            .keys()
            .next()
            .expect("there are local accounts");

        let last_block_number = fixture.provider_data.last_block_number();
        let block_spec = BlockSpec::Number(last_block_number);

        let balance = fixture.provider_data.balance(account, Some(&block_spec))?;

        assert_eq!(balance, one_ether());

        Ok(())
    }

    #[test]
    fn test_sign_transaction_request() -> anyhow::Result<()> {
        let fixture = ProviderTestFixture::new()?;

        let transaction = fixture.signed_dummy_transaction()?;
        let recovered_address = transaction.recover()?;

        assert!(fixture
            .provider_data
            .local_accounts
            .contains_key(&recovered_address));

        Ok(())
    }

    #[test]
    fn test_sign_transaction_request_impersonated_account() -> anyhow::Result<()> {
        let fixture = ProviderTestFixture::new()?;

        let transaction = fixture.impersonated_dummy_transaction()?;

        assert_eq!(transaction.caller(), &fixture.impersonated_account);

        Ok(())
    }

    fn test_add_pending_transaction(
        fixture: &mut ProviderTestFixture,
        transaction: ExecutableTransaction,
    ) -> anyhow::Result<()> {
        let filter_id = fixture
            .provider_data
            .add_pending_transaction_filter::<false>();

        let transaction_hash = fixture.provider_data.add_pending_transaction(transaction)?;

        assert!(fixture
            .provider_data
            .mem_pool
            .transaction_by_hash(&transaction_hash)
            .is_some());

        match fixture
            .provider_data
            .get_filter_changes(&filter_id)
            .unwrap()
        {
            FilteredEvents::NewPendingTransactions(hashes) => {
                assert!(hashes.contains(&transaction_hash));
            }
            _ => panic!("expected pending transaction"),
        };

        assert!(fixture.provider_data.mem_pool.has_pending_transactions());

        Ok(())
    }

    #[test]
    fn add_pending_transaction() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;
        let transaction = fixture.signed_dummy_transaction()?;

        test_add_pending_transaction(&mut fixture, transaction)
    }

    #[test]
    fn add_pending_transaction_from_impersonated_account() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;
        let transaction = fixture.impersonated_dummy_transaction()?;

        test_add_pending_transaction(&mut fixture, transaction)
    }

    #[test]
    fn block_by_block_spec_earliest() -> anyhow::Result<()> {
        let fixture = ProviderTestFixture::new()?;

        let block_spec = BlockSpec::Tag(BlockTag::Earliest);

        let block = fixture
            .provider_data
            .block_by_block_spec(&block_spec)?
            .context("block should exist")?;

        assert_eq!(block.header().number, 0);

        Ok(())
    }

    #[test]
    fn block_by_block_spec_finalized_safe_latest() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        // Mine a block to make sure we're not getting the genesis block
        fixture.provider_data.mine_and_commit_block(None)?;
        let last_block_number = fixture.provider_data.last_block_number();
        // Sanity check
        assert!(last_block_number > 0);

        let block_tags = vec![BlockTag::Finalized, BlockTag::Safe, BlockTag::Latest];
        for tag in block_tags {
            let block_spec = BlockSpec::Tag(tag);

            let block = fixture
                .provider_data
                .block_by_block_spec(&block_spec)?
                .context("block should exist")?;

            assert_eq!(block.header().number, last_block_number);
        }

        Ok(())
    }

    #[test]
    fn block_by_block_spec_pending() -> anyhow::Result<()> {
        let fixture = ProviderTestFixture::new()?;

        let block_spec = BlockSpec::Tag(BlockTag::Pending);

        let block = fixture.provider_data.block_by_block_spec(&block_spec)?;

        assert!(block.is_none());

        Ok(())
    }

    // Make sure executing a transaction in a pending block context doesn't panic.
    #[test]
    fn execute_in_block_context_pending() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        let block_spec = Some(BlockSpec::Tag(BlockTag::Pending));

        let mut value = 0;
        let _ =
            fixture
                .provider_data
                .execute_in_block_context(block_spec.as_ref(), |_, _, _| {
                    value += 1;
                    Ok::<(), ProviderError<Infallible>>(())
                })?;

        assert_eq!(value, 1);

        Ok(())
    }

    #[test]
    fn chain_id() -> anyhow::Result<()> {
        let fixture = ProviderTestFixture::new()?;

        let chain_id = fixture.provider_data.chain_id();
        assert_eq!(chain_id, fixture.config.chain_id);

        Ok(())
    }

    #[test]
    fn chain_id_fork_mode() -> anyhow::Result<()> {
        let fixture = ProviderTestFixture::new_forked()?;

        let chain_id = fixture.provider_data.chain_id();
        assert_eq!(chain_id, fixture.config.chain_id);

        Ok(())
    }

    #[test]
    fn console_log_mine_block() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;
        let ConsoleLogTransaction {
            transaction,
            expected_call_data,
        } = deploy_console_log_contract(&mut fixture.provider_data)?;

        let signed_transaction = fixture
            .provider_data
            .sign_transaction_request(transaction)?;

        fixture.provider_data.set_auto_mining(false);
        fixture.provider_data.send_transaction(signed_transaction)?;
        let (block_timestamp, _) = fixture.provider_data.next_block_timestamp(None)?;
        let prevrandao = fixture.provider_data.prev_randao_generator.next_value();
        let result = fixture
            .provider_data
            .mine_block(block_timestamp, Some(prevrandao))?;

        let console_log_inputs = result.console_log_inputs;
        assert_eq!(console_log_inputs.len(), 1);
        assert_eq!(console_log_inputs[0], expected_call_data);

        Ok(())
    }

    #[test]
    fn console_log_run_call() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;
        let ConsoleLogTransaction {
            transaction,
            expected_call_data,
        } = deploy_console_log_contract(&mut fixture.provider_data)?;

        let pending_transaction = fixture
            .provider_data
            .sign_transaction_request(transaction)?;

        let result = fixture.provider_data.run_call(
            pending_transaction,
            None,
            &StateOverrides::default(),
        )?;

        let console_log_inputs = result.console_log_inputs;
        assert_eq!(console_log_inputs.len(), 1);
        assert_eq!(console_log_inputs[0], expected_call_data);

        Ok(())
    }

    #[test]
    fn mine_and_commit_block_empty() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        let result = fixture.provider_data.mine_and_commit_block(None)?;

        let cached_state = fixture
            .provider_data
            .get_or_compute_state(result.block.header().number)?;

        let calculated_state = fixture.provider_data.blockchain.state_at_block_number(
            fixture.provider_data.last_block_number(),
            fixture.provider_data.irregular_state.state_overrides(),
        )?;

        assert_eq!(cached_state.state_root()?, calculated_state.state_root()?);

        Ok(())
    }

    #[test]
    fn mine_and_commit_blocks_empty() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        fixture
            .provider_data
            .mine_and_commit_blocks(1_000_000_000, 1)?;

        let cached_state = fixture
            .provider_data
            .get_or_compute_state(fixture.provider_data.last_block_number())?;

        let calculated_state = fixture.provider_data.blockchain.state_at_block_number(
            fixture.provider_data.last_block_number(),
            fixture.provider_data.irregular_state.state_overrides(),
        )?;

        assert_eq!(cached_state.state_root()?, calculated_state.state_root()?);

        Ok(())
    }

    #[test]
    fn next_filter_id() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        let mut prev_filter_id = fixture.provider_data.last_filter_id;
        for _ in 0..10 {
            let filter_id = fixture.provider_data.next_filter_id();
            assert!(prev_filter_id < filter_id);
            prev_filter_id = filter_id;
        }

        Ok(())
    }

    #[test]
    fn set_balance_updates_mem_pool() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        let transaction = {
            let mut request = fixture.dummy_transaction_request(None);
            request.sender = fixture.impersonated_account;

            fixture.provider_data.sign_transaction_request(request)?
        };

        let transaction_hash = fixture.provider_data.add_pending_transaction(transaction)?;

        assert!(fixture
            .provider_data
            .mem_pool
            .transaction_by_hash(&transaction_hash)
            .is_some());

        fixture
            .provider_data
            .set_balance(fixture.impersonated_account, U256::from(100))?;

        assert!(fixture
            .provider_data
            .mem_pool
            .transaction_by_hash(&transaction_hash)
            .is_none());

        Ok(())
    }

    #[test]
    fn transaction_by_invalid_hash() -> anyhow::Result<()> {
        let fixture = ProviderTestFixture::new()?;

        let non_existing_tx = fixture.provider_data.transaction_by_hash(&B256::ZERO)?;

        assert!(non_existing_tx.is_none());

        Ok(())
    }

    #[test]
    fn pending_transaction_by_hash() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        let transaction_request = fixture.signed_dummy_transaction()?;
        let transaction_hash = fixture
            .provider_data
            .add_pending_transaction(transaction_request)?;

        let transaction_result = fixture
            .provider_data
            .transaction_by_hash(&transaction_hash)?
            .context("transaction not found")?;

        assert_eq!(
            transaction_result.signed_transaction.hash(),
            &transaction_hash
        );

        Ok(())
    }

    #[test]
    fn transaction_by_hash() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        let transaction_request = fixture.signed_dummy_transaction()?;
        let transaction_hash = fixture
            .provider_data
            .add_pending_transaction(transaction_request)?;

        let results = fixture.provider_data.mine_and_commit_block(None)?;

        // Make sure transaction was mined successfully.
        assert!(results
            .transaction_results
            .first()
            .context("failed to mine transaction")?
            .is_success());
        // Sanity check that the mempool is empty.
        assert_eq!(fixture.provider_data.mem_pool.transactions().count(), 0);

        let transaction_result = fixture
            .provider_data
            .transaction_by_hash(&transaction_hash)?
            .context("transaction not found")?;

        assert_eq!(
            transaction_result.signed_transaction.hash(),
            &transaction_hash
        );

        Ok(())
    }

    #[test]
    fn reset_local_to_forking() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new()?;

        let fork_config = Some(ForkConfig {
            json_rpc_url: get_alchemy_url(),
            // Random recent block for better cache consistency
            block_number: Some(FORK_BLOCK_NUMBER),
            http_headers: None,
        });

        let block_spec = BlockSpec::Number(FORK_BLOCK_NUMBER);

        assert_eq!(fixture.provider_data.last_block_number(), 0);

        fixture.provider_data.reset(fork_config)?;

        // We're fetching a specific block instead of the last block number for the
        // forked blockchain, because the last block number query cannot be
        // cached.
        assert!(fixture
            .provider_data
            .block_by_block_spec(&block_spec)?
            .is_some());

        Ok(())
    }

    #[test]
    fn reset_forking_to_local() -> anyhow::Result<()> {
        let mut fixture = ProviderTestFixture::new_forked()?;

        // We're fetching a specific block instead of the last block number for the
        // forked blockchain, because the last block number query cannot be
        // cached.
        assert!(fixture
            .provider_data
            .block_by_block_spec(&BlockSpec::Number(FORK_BLOCK_NUMBER))?
            .is_some());

        fixture.provider_data.reset(None)?;

        assert_eq!(fixture.provider_data.last_block_number(), 0);

        Ok(())
    }

    #[test]
    fn sign_typed_data_v4() -> anyhow::Result<()> {
        let fixture = ProviderTestFixture::new()?;

        // This test was taken from the `eth_signTypedData` example from the
        // EIP-712 specification via Hardhat.
        // <https://eips.ethereum.org/EIPS/eip-712#eth_signtypeddata>

        let address: Address = "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826".parse()?;
        let message = json!({
          "types": {
            "EIP712Domain": [
              { "name": "name", "type": "string" },
              { "name": "version", "type": "string" },
              { "name": "chainId", "type": "uint256" },
              { "name": "verifyingContract", "type": "address" },
            ],
            "Person": [
              { "name": "name", "type": "string" },
              { "name": "wallet", "type": "address" },
            ],
            "Mail": [
              { "name": "from", "type": "Person" },
              { "name": "to", "type": "Person" },
              { "name": "contents", "type": "string" },
            ],
          },
          "primaryType": "Mail",
          "domain": {
            "name": "Ether Mail",
            "version": "1",
            "chainId": 1,
            "verifyingContract": "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC",
          },
          "message": {
            "from": {
              "name": "Cow",
              "wallet": "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826",
            },
            "to": {
              "name": "Bob",
              "wallet": "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB",
            },
            "contents": "Hello, Bob!",
          },
        });
        let message: TypedData = serde_json::from_value(message)?;

        let signature = fixture
            .provider_data
            .sign_typed_data_v4(&address, &message)?;

        let expected_signature = "0x4355c47d63924e8a72e509b65029052eb6c299d53a04e167c5775fd466751c9d07299936d304c153f6443dfa05f40ff007d72911b6f72307f996231605b915621c";

        assert_eq!(hex::decode(expected_signature)?, signature.to_vec(),);

        Ok(())
    }
}

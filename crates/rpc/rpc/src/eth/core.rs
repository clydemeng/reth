//! Implementation of the [`jsonrpsee`] generated [`EthApiServer`](crate::EthApi) trait
//! Handles RPC requests for the `eth_` namespace.

use std::sync::Arc;

use crate::{eth::helpers::types::EthRpcConverter, EthApiBuilder};
use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_network::Ethereum;
use alloy_primitives::{Bytes, U256};
use derive_more::Deref;
use reth_chainspec::{ChainSpec, ChainSpecProvider};
use reth_evm_ethereum::EthEvmConfig;
use reth_network_api::noop::NoopNetwork;
use reth_node_api::{FullNodeComponents, FullNodeTypes};
use reth_rpc_convert::{RpcConvert, RpcConverter};
use reth_rpc_eth_api::{
    helpers::{pending_block::PendingEnvBuilder, spec::SignersForRpc, SpawnBlocking},
    node::{RpcNodeCoreAdapter, RpcNodeCoreExt},
    EthApiTypes, RpcNodeCore,
};
use reth_rpc_eth_types::{
    receipt::EthReceiptConverter, EthApiError, EthStateCache, FeeHistoryCache, GasCap,
    GasPriceOracle, PendingBlock,
};
use reth_storage_api::{noop::NoopProvider, BlockReaderIdExt, ProviderHeader};
use reth_tasks::{
    pool::{BlockingTaskGuard, BlockingTaskPool},
    TaskSpawner, TokioTaskExecutor,
};
use reth_transaction_pool::noop::NoopTransactionPool;
use tokio::sync::{broadcast, Mutex};

const DEFAULT_BROADCAST_CAPACITY: usize = 2000;

/// Helper type alias for [`RpcConverter`] with components from the given [`FullNodeComponents`].
pub type EthRpcConverterFor<N> = RpcConverter<
    Ethereum,
    <N as FullNodeComponents>::Evm,
    EthReceiptConverter<<<N as FullNodeTypes>::Provider as ChainSpecProvider>::ChainSpec>,
>;

/// Helper type alias for [`EthApi`] with components from the given [`FullNodeComponents`].
pub type EthApiFor<N> = EthApi<N, EthRpcConverterFor<N>>;

/// Helper type alias for [`EthApi`] with components from the given [`FullNodeComponents`].
pub type EthApiBuilderFor<N> = EthApiBuilder<N, EthRpcConverterFor<N>>;

/// `Eth` API implementation.
///
/// This type provides the functionality for handling `eth_` related requests.
/// These are implemented two-fold: Core functionality is implemented as
/// [`EthApiSpec`](reth_rpc_eth_api::helpers::EthApiSpec) trait. Additionally, the required server
/// implementations (e.g. [`EthApiServer`](reth_rpc_eth_api::EthApiServer)) are implemented
/// separately in submodules. The rpc handler implementation can then delegate to the main impls.
/// This way [`EthApi`] is not limited to [`jsonrpsee`] and can be used standalone or in other
/// network handlers (for example ipc).
///
/// ## Trait requirements
///
/// While this type requires various unrestricted generic components, trait bounds are enforced when
/// additional traits are implemented for this type.
#[derive(Deref)]
pub struct EthApi<N: RpcNodeCore, Rpc: RpcConvert> {
    /// All nested fields bundled together.
    #[deref]
    pub(super) inner: Arc<EthApiInner<N, Rpc>>,
}

impl<N, Rpc> Clone for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl
    EthApi<
        RpcNodeCoreAdapter<NoopProvider, NoopTransactionPool, NoopNetwork, EthEvmConfig>,
        EthRpcConverter<ChainSpec>,
    >
{
    /// Convenience fn to obtain a new [`EthApiBuilder`] instance with mandatory components.
    ///
    /// Creating an [`EthApi`] requires a few mandatory components:
    ///  - provider: The type responsible for fetching requested data from disk.
    ///  - transaction pool: To interact with the pool, submitting new transactions (e.g.
    ///    `eth_sendRawTransactions`).
    ///  - network: required to handle requests related to network state (e.g. `eth_syncing`).
    ///  - evm config: Knows how create a new EVM instance to transact,estimate,call,trace.
    ///
    /// # Create an instance with noop ethereum implementations
    ///
    /// ```no_run
    /// use alloy_network::Ethereum;
    /// use reth_evm_ethereum::EthEvmConfig;
    /// use reth_network_api::noop::NoopNetwork;
    /// use reth_provider::noop::NoopProvider;
    /// use reth_rpc::EthApi;
    /// use reth_transaction_pool::noop::NoopTransactionPool;
    /// let eth_api = EthApi::builder(
    ///     NoopProvider::default(),
    ///     NoopTransactionPool::default(),
    ///     NoopNetwork::default(),
    ///     EthEvmConfig::mainnet(),
    /// )
    /// .build();
    /// ```
    #[expect(clippy::type_complexity)]
    pub fn builder<Provider, Pool, Network, EvmConfig, ChainSpec>(
        provider: Provider,
        pool: Pool,
        network: Network,
        evm_config: EvmConfig,
    ) -> EthApiBuilder<
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>,
        RpcConverter<Ethereum, EvmConfig, EthReceiptConverter<ChainSpec>>,
    >
    where
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>:
            RpcNodeCore<Provider: ChainSpecProvider<ChainSpec = ChainSpec>, Evm = EvmConfig>,
    {
        EthApiBuilder::new(provider, pool, network, evm_config)
    }
}

impl<N, Rpc> EthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
    (): PendingEnvBuilder<N::Evm>,
{
    /// Creates a new, shareable instance using the default tokio task spawner.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        components: N,
        eth_cache: EthStateCache<N::Primitives>,
        gas_oracle: GasPriceOracle<N::Provider>,
        gas_cap: impl Into<GasCap>,
        max_simulate_blocks: u64,
        eth_proof_window: u64,
        blocking_task_pool: BlockingTaskPool,
        fee_history_cache: FeeHistoryCache<ProviderHeader<N::Provider>>,
        proof_permits: usize,
        rpc_converter: Rpc,
    ) -> Self {
        let inner = EthApiInner::new(
            components,
            eth_cache,
            gas_oracle,
            gas_cap,
            max_simulate_blocks,
            eth_proof_window,
            blocking_task_pool,
            fee_history_cache,
            TokioTaskExecutor::default().boxed(),
            proof_permits,
            rpc_converter,
            (),
        );

        Self { inner: Arc::new(inner) }
    }
}

impl<N, Rpc> EthApiTypes for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    type Error = EthApiError;
    type NetworkTypes = Rpc::Network;
    type RpcConvert = Rpc;

    fn tx_resp_builder(&self) -> &Self::RpcConvert {
        &self.tx_resp_builder
    }
}

impl<N, Rpc> RpcNodeCore for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    type Primitives = N::Primitives;
    type Provider = N::Provider;
    type Pool = N::Pool;
    type Evm = N::Evm;
    type Network = N::Network;

    fn pool(&self) -> &Self::Pool {
        self.inner.pool()
    }

    fn evm_config(&self) -> &Self::Evm {
        self.inner.evm_config()
    }

    fn network(&self) -> &Self::Network {
        self.inner.network()
    }

    fn provider(&self) -> &Self::Provider {
        self.inner.provider()
    }
}

impl<N, Rpc> RpcNodeCoreExt for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    #[inline]
    fn cache(&self) -> &EthStateCache<N::Primitives> {
        self.inner.cache()
    }
}

impl<N, Rpc> std::fmt::Debug for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EthApi").finish_non_exhaustive()
    }
}

impl<N, Rpc> SpawnBlocking for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    #[inline]
    fn io_task_spawner(&self) -> impl TaskSpawner {
        self.inner.task_spawner()
    }

    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.blocking_task_pool()
    }

    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.blocking_task_guard()
    }
}

/// Container type `EthApi`
#[expect(missing_debug_implementations)]
pub struct EthApiInner<N: RpcNodeCore, Rpc: RpcConvert> {
    /// The components of the node.
    components: N,
    /// All configured Signers
    signers: SignersForRpc<N::Provider, Rpc::Network>,
    /// The async cache frontend for eth related data
    eth_cache: EthStateCache<N::Primitives>,
    /// The async gas oracle frontend for gas price suggestions
    gas_oracle: GasPriceOracle<N::Provider>,
    /// Maximum gas limit for `eth_call` and call tracing RPC methods.
    gas_cap: u64,
    /// Maximum number of blocks for `eth_simulateV1`.
    max_simulate_blocks: u64,
    /// The maximum number of blocks into the past for generating state proofs.
    eth_proof_window: u64,
    /// The block number at which the node started
    starting_block: U256,
    /// The type that can spawn tasks which would otherwise block.
    task_spawner: Box<dyn TaskSpawner>,
    /// Cached pending block if any
    pending_block: Mutex<Option<PendingBlock<N::Primitives>>>,
    /// A pool dedicated to CPU heavy blocking tasks.
    blocking_task_pool: BlockingTaskPool,
    /// Cache for block fees history
    fee_history_cache: FeeHistoryCache<ProviderHeader<N::Provider>>,

    /// Guard for getproof calls
    blocking_task_guard: BlockingTaskGuard,

    /// Transaction broadcast channel
    raw_tx_sender: broadcast::Sender<Bytes>,

    /// Converter for RPC types.
    tx_resp_builder: Rpc,

    /// Builder for pending block environment.
    next_env_builder: Box<dyn PendingEnvBuilder<N::Evm>>,
}

impl<N, Rpc> EthApiInner<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    /// Creates a new, shareable instance using the default tokio task spawner.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        components: N,
        eth_cache: EthStateCache<N::Primitives>,
        gas_oracle: GasPriceOracle<N::Provider>,
        gas_cap: impl Into<GasCap>,
        max_simulate_blocks: u64,
        eth_proof_window: u64,
        blocking_task_pool: BlockingTaskPool,
        fee_history_cache: FeeHistoryCache<ProviderHeader<N::Provider>>,
        task_spawner: Box<dyn TaskSpawner + 'static>,
        proof_permits: usize,
        tx_resp_builder: Rpc,
        next_env: impl PendingEnvBuilder<N::Evm>,
    ) -> Self {
        let signers = parking_lot::RwLock::new(Default::default());
        // get the block number of the latest block
        let starting_block = U256::from(
            components
                .provider()
                .header_by_number_or_tag(BlockNumberOrTag::Latest)
                .ok()
                .flatten()
                .map(|header| header.number())
                .unwrap_or_default(),
        );

        let (raw_tx_sender, _) = broadcast::channel(DEFAULT_BROADCAST_CAPACITY);

        Self {
            components,
            signers,
            eth_cache,
            gas_oracle,
            gas_cap: gas_cap.into().into(),
            max_simulate_blocks,
            eth_proof_window,
            starting_block,
            task_spawner,
            pending_block: Default::default(),
            blocking_task_pool,
            fee_history_cache,
            blocking_task_guard: BlockingTaskGuard::new(proof_permits),
            raw_tx_sender,
            tx_resp_builder,
            next_env_builder: Box::new(next_env),
        }
    }
}

impl<N, Rpc> EthApiInner<N, Rpc>
where
    N: RpcNodeCore,
    Rpc: RpcConvert,
{
    /// Returns a handle to data on disk.
    #[inline]
    pub fn provider(&self) -> &N::Provider {
        self.components.provider()
    }

    /// Returns a handle to the transaction response builder.
    #[inline]
    pub const fn tx_resp_builder(&self) -> &Rpc {
        &self.tx_resp_builder
    }

    /// Returns a handle to data in memory.
    #[inline]
    pub const fn cache(&self) -> &EthStateCache<N::Primitives> {
        &self.eth_cache
    }

    /// Returns a handle to the pending block.
    #[inline]
    pub const fn pending_block(&self) -> &Mutex<Option<PendingBlock<N::Primitives>>> {
        &self.pending_block
    }

    /// Returns a type that knows how to build a [`reth_evm::ConfigureEvm::NextBlockEnvCtx`] for a
    /// pending block.
    #[inline]
    pub const fn pending_env_builder(&self) -> &dyn PendingEnvBuilder<N::Evm> {
        &*self.next_env_builder
    }

    /// Returns a handle to the task spawner.
    #[inline]
    pub const fn task_spawner(&self) -> &dyn TaskSpawner {
        &*self.task_spawner
    }

    /// Returns a handle to the blocking thread pool.
    #[inline]
    pub const fn blocking_task_pool(&self) -> &BlockingTaskPool {
        &self.blocking_task_pool
    }

    /// Returns a handle to the EVM config.
    #[inline]
    pub fn evm_config(&self) -> &N::Evm {
        self.components.evm_config()
    }

    /// Returns a handle to the transaction pool.
    #[inline]
    pub fn pool(&self) -> &N::Pool {
        self.components.pool()
    }

    /// Returns the gas cap.
    #[inline]
    pub const fn gas_cap(&self) -> u64 {
        self.gas_cap
    }

    /// Returns the `max_simulate_blocks`.
    #[inline]
    pub const fn max_simulate_blocks(&self) -> u64 {
        self.max_simulate_blocks
    }

    /// Returns a handle to the gas oracle.
    #[inline]
    pub const fn gas_oracle(&self) -> &GasPriceOracle<N::Provider> {
        &self.gas_oracle
    }

    /// Returns a handle to the fee history cache.
    #[inline]
    pub const fn fee_history_cache(&self) -> &FeeHistoryCache<ProviderHeader<N::Provider>> {
        &self.fee_history_cache
    }

    /// Returns a handle to the signers.
    #[inline]
    pub const fn signers(&self) -> &SignersForRpc<N::Provider, Rpc::Network> {
        &self.signers
    }

    /// Returns the starting block.
    #[inline]
    pub const fn starting_block(&self) -> U256 {
        self.starting_block
    }

    /// Returns the inner `Network`
    #[inline]
    pub fn network(&self) -> &N::Network {
        self.components.network()
    }

    /// The maximum number of blocks into the past for generating state proofs.
    #[inline]
    pub const fn eth_proof_window(&self) -> u64 {
        self.eth_proof_window
    }

    /// Returns reference to [`BlockingTaskGuard`].
    #[inline]
    pub const fn blocking_task_guard(&self) -> &BlockingTaskGuard {
        &self.blocking_task_guard
    }

    /// Returns [`broadcast::Receiver`] of new raw transactions
    #[inline]
    pub fn subscribe_to_raw_transactions(&self) -> broadcast::Receiver<Bytes> {
        self.raw_tx_sender.subscribe()
    }

    /// Broadcasts raw transaction if there are active subscribers.
    #[inline]
    pub fn broadcast_raw_transaction(&self, raw_tx: Bytes) {
        let _ = self.raw_tx_sender.send(raw_tx);
    }
}

#[cfg(test)]
mod tests {
    use crate::{eth::helpers::types::EthRpcConverter, EthApi, EthApiBuilder};
    use alloy_consensus::{Block, BlockBody, Header};
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::{Signature, B256, U64};
    use alloy_rpc_types::FeeHistory;
    use jsonrpsee_types::error::INVALID_PARAMS_CODE;
    use rand::Rng;
    use reth_chain_state::CanonStateSubscriptions;
    use reth_chainspec::{ChainSpec, ChainSpecProvider, EthChainSpec};
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm_ethereum::EthEvmConfig;
    use reth_network_api::noop::NoopNetwork;
    use reth_provider::{
        test_utils::{MockEthProvider, NoopProvider},
        StageCheckpointReader,
    };
    use reth_rpc_eth_api::{node::RpcNodeCoreAdapter, EthApiServer};
    use reth_storage_api::{BlockReader, BlockReaderIdExt, StateProviderFactory};
    use reth_testing_utils::generators;
    use reth_transaction_pool::test_utils::{testing_pool, TestPool};

    type FakeEthApi<P = MockEthProvider> = EthApi<
        RpcNodeCoreAdapter<P, TestPool, NoopNetwork, EthEvmConfig>,
        EthRpcConverter<ChainSpec>,
    >;

    fn build_test_eth_api<
        P: BlockReaderIdExt<
                Block = reth_ethereum_primitives::Block,
                Receipt = reth_ethereum_primitives::Receipt,
                Header = alloy_consensus::Header,
                Transaction = reth_ethereum_primitives::TransactionSigned,
            > + BlockReader
            + ChainSpecProvider<ChainSpec = ChainSpec>
            + StateProviderFactory
            + CanonStateSubscriptions<Primitives = reth_ethereum_primitives::EthPrimitives>
            + StageCheckpointReader
            + Unpin
            + Clone
            + 'static,
    >(
        provider: P,
    ) -> FakeEthApi<P> {
        EthApiBuilder::new(
            provider.clone(),
            testing_pool(),
            NoopNetwork::default(),
            EthEvmConfig::new(provider.chain_spec()),
        )
        .build()
    }

    // Function to prepare the EthApi with mock data
    fn prepare_eth_api(
        newest_block: u64,
        mut oldest_block: Option<B256>,
        block_count: u64,
        mock_provider: MockEthProvider,
    ) -> (FakeEthApi, Vec<u128>, Vec<f64>) {
        let mut rng = generators::rng();

        // Build mock data
        let mut gas_used_ratios = Vec::with_capacity(block_count as usize);
        let mut base_fees_per_gas = Vec::with_capacity(block_count as usize);
        let mut last_header = None;
        let mut parent_hash = B256::default();

        for i in (0..block_count).rev() {
            let hash = rng.random();
            // Note: Generates saner values to avoid invalid overflows later
            let gas_limit = rng.random::<u32>() as u64;
            let base_fee_per_gas: Option<u64> =
                rng.random::<bool>().then(|| rng.random::<u32>() as u64);
            let gas_used = rng.random::<u32>() as u64;

            let header = Header {
                number: newest_block - i,
                gas_limit,
                gas_used,
                base_fee_per_gas,
                parent_hash,
                ..Default::default()
            };
            last_header = Some(header.clone());
            parent_hash = hash;

            const TOTAL_TRANSACTIONS: usize = 100;
            let mut transactions = Vec::with_capacity(TOTAL_TRANSACTIONS);
            for _ in 0..TOTAL_TRANSACTIONS {
                let random_fee: u128 = rng.random();

                if let Some(base_fee_per_gas) = header.base_fee_per_gas {
                    let transaction = TransactionSigned::new_unhashed(
                        reth_ethereum_primitives::Transaction::Eip1559(
                            alloy_consensus::TxEip1559 {
                                max_priority_fee_per_gas: random_fee,
                                max_fee_per_gas: random_fee + base_fee_per_gas as u128,
                                ..Default::default()
                            },
                        ),
                        Signature::test_signature(),
                    );

                    transactions.push(transaction);
                } else {
                    let transaction = TransactionSigned::new_unhashed(
                        reth_ethereum_primitives::Transaction::Legacy(Default::default()),
                        Signature::test_signature(),
                    );

                    transactions.push(transaction);
                }
            }

            mock_provider.add_block(
                hash,
                Block {
                    header: header.clone(),
                    body: BlockBody { transactions, ..Default::default() },
                },
            );
            mock_provider.add_header(hash, header);

            oldest_block.get_or_insert(hash);
            gas_used_ratios.push(gas_used as f64 / gas_limit as f64);
            base_fees_per_gas.push(base_fee_per_gas.map(|fee| fee as u128).unwrap_or_default());
        }

        // Add final base fee (for the next block outside of the request)
        let last_header = last_header.unwrap();
        let spec = mock_provider.chain_spec();
        base_fees_per_gas.push(
            spec.next_block_base_fee(&last_header, last_header.timestamp).unwrap_or_default()
                as u128,
        );

        let eth_api = build_test_eth_api(mock_provider);

        (eth_api, base_fees_per_gas, gas_used_ratios)
    }

    /// Invalid block range
    #[tokio::test]
    async fn test_fee_history_empty() {
        let response = <EthApi<_, _> as EthApiServer<_, _, _, _, _>>::fee_history(
            &build_test_eth_api(NoopProvider::default()),
            U64::from(1),
            BlockNumberOrTag::Latest,
            None,
        )
        .await;
        assert!(response.is_err());
        let error_object = response.unwrap_err();
        assert_eq!(error_object.code(), INVALID_PARAMS_CODE);
    }

    #[tokio::test]
    /// Invalid block range (request is before genesis)
    async fn test_fee_history_invalid_block_range_before_genesis() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, _, _) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let response = <EthApi<_, _> as EthApiServer<_, _, _, _, _>>::fee_history(
            &eth_api,
            U64::from(newest_block + 1),
            newest_block.into(),
            Some(vec![10.0]),
        )
        .await;

        assert!(response.is_err());
        let error_object = response.unwrap_err();
        assert_eq!(error_object.code(), INVALID_PARAMS_CODE);
    }

    #[tokio::test]
    /// Invalid block range (request is in the future)
    async fn test_fee_history_invalid_block_range_in_future() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, _, _) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let response = <EthApi<_, _> as EthApiServer<_, _, _, _, _>>::fee_history(
            &eth_api,
            U64::from(1),
            (newest_block + 1000).into(),
            Some(vec![10.0]),
        )
        .await;

        assert!(response.is_err());
        let error_object = response.unwrap_err();
        assert_eq!(error_object.code(), INVALID_PARAMS_CODE);
    }

    #[tokio::test]
    /// Requesting no block should result in a default response
    async fn test_fee_history_no_block_requested() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, _, _) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let response = <EthApi<_, _> as EthApiServer<_, _, _, _, _>>::fee_history(
            &eth_api,
            U64::from(0),
            newest_block.into(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            response,
            FeeHistory::default(),
            "none: requesting no block should yield a default response"
        );
    }

    #[tokio::test]
    /// Requesting a single block should return 1 block (+ base fee for the next block over)
    async fn test_fee_history_single_block() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, base_fees_per_gas, gas_used_ratios) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let fee_history =
            eth_api.fee_history(U64::from(1), newest_block.into(), None).await.unwrap();
        assert_eq!(
            fee_history.base_fee_per_gas,
            &base_fees_per_gas[base_fees_per_gas.len() - 2..],
            "one: base fee per gas is incorrect"
        );
        assert_eq!(
            fee_history.base_fee_per_gas.len(),
            2,
            "one: should return base fee of the next block as well"
        );
        assert_eq!(
            &fee_history.gas_used_ratio,
            &gas_used_ratios[gas_used_ratios.len() - 1..],
            "one: gas used ratio is incorrect"
        );
        assert_eq!(fee_history.oldest_block, newest_block, "one: oldest block is incorrect");
        assert!(
            fee_history.reward.is_none(),
            "one: no percentiles were requested, so there should be no rewards result"
        );
    }

    /// Requesting all blocks should be ok
    #[tokio::test]
    async fn test_fee_history_all_blocks() {
        let block_count = 10;
        let newest_block = 1337;
        let oldest_block = None;

        let (eth_api, base_fees_per_gas, gas_used_ratios) =
            prepare_eth_api(newest_block, oldest_block, block_count, MockEthProvider::default());

        let fee_history =
            eth_api.fee_history(U64::from(block_count), newest_block.into(), None).await.unwrap();

        assert_eq!(
            &fee_history.base_fee_per_gas, &base_fees_per_gas,
            "all: base fee per gas is incorrect"
        );
        assert_eq!(
            fee_history.base_fee_per_gas.len() as u64,
            block_count + 1,
            "all: should return base fee of the next block as well"
        );
        assert_eq!(
            &fee_history.gas_used_ratio, &gas_used_ratios,
            "all: gas used ratio is incorrect"
        );
        assert_eq!(
            fee_history.oldest_block,
            newest_block - block_count + 1,
            "all: oldest block is incorrect"
        );
        assert!(
            fee_history.reward.is_none(),
            "all: no percentiles were requested, so there should be no rewards result"
        );
    }
}

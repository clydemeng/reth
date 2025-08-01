//! Configure reth RPC.
//!
//! This crate contains several builder and config types that allow to configure the selection of
//! [`RethRpcModule`] specific to transports (ws, http, ipc).
//!
//! The [`RpcModuleBuilder`] is the main entrypoint for configuring all reth modules. It takes
//! instances of components required to start the servers, such as provider impls, network and
//! transaction pool. [`RpcModuleBuilder::build`] returns a [`TransportRpcModules`] which contains
//! the transport specific config (what APIs are available via this transport).
//!
//! The [`RpcServerConfig`] is used to assemble and start the http server, ws server, ipc servers,
//! it requires the [`TransportRpcModules`] so it can start the servers with the configured modules.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use crate::{auth::AuthRpcModule, error::WsHttpSamePortError, metrics::RpcRequestMetrics};
use alloy_network::Ethereum;
use alloy_provider::{fillers::RecommendedFillers, Provider, ProviderBuilder};
use core::marker::PhantomData;
use error::{ConflictingModules, RpcError, ServerKind};
use http::{header::AUTHORIZATION, HeaderMap};
use jsonrpsee::{
    core::RegisterMethodError,
    server::{middleware::rpc::RpcServiceBuilder, AlreadyStoppedError, IdProvider, ServerHandle},
    Methods, RpcModule,
};
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_consensus::{ConsensusError, FullConsensus};
use reth_evm::ConfigureEvm;
use reth_network_api::{noop::NoopNetwork, NetworkInfo, Peers};
use reth_primitives_traits::NodePrimitives;
use reth_rpc::{
    AdminApi, DebugApi, EngineEthApi, EthApi, EthApiBuilder, EthBundle, MinerApi, NetApi,
    OtterscanApi, RPCApi, RethApi, TraceApi, TxPoolApi, ValidationApiConfig, Web3Api,
};
use reth_rpc_api::servers::*;
use reth_rpc_eth_api::{
    helpers::{
        pending_block::PendingEnvBuilder, Call, EthApiSpec, EthTransactions, LoadPendingBlock,
        TraceExt,
    },
    node::RpcNodeCoreAdapter,
    EthApiServer, EthApiTypes, FullEthApiServer, RpcBlock, RpcConvert, RpcConverter, RpcHeader,
    RpcNodeCore, RpcReceipt, RpcTransaction, RpcTxReq,
};
use reth_rpc_eth_types::{receipt::EthReceiptConverter, EthConfig, EthSubscriptionIdProvider};
use reth_rpc_layer::{AuthLayer, Claims, CompressionLayer, JwtAuthValidator, JwtSecret};
use reth_storage_api::{
    AccountReader, BlockReader, ChangeSetReader, FullRpcProvider, ProviderBlock,
    StateProviderFactory,
};
use reth_tasks::{pool::BlockingTaskGuard, TaskSpawner, TokioTaskExecutor};
use reth_transaction_pool::{noop::NoopTransactionPool, TransactionPool};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Debug,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tower_http::cors::CorsLayer;

pub use cors::CorsDomainError;

// re-export for convenience
pub use jsonrpsee::server::ServerBuilder;
use jsonrpsee::server::ServerConfigBuilder;
pub use reth_ipc::server::{
    Builder as IpcServerBuilder, RpcServiceBuilder as IpcRpcServiceBuilder,
};
pub use reth_rpc_server_types::{constants, RethRpcModule, RpcModuleSelection};
pub use tower::layer::util::{Identity, Stack};

/// Auth server utilities.
pub mod auth;

/// RPC server utilities.
pub mod config;

/// Utils for installing Rpc middleware
pub mod middleware;

/// Cors utilities.
mod cors;

/// Rpc error utilities.
pub mod error;

/// Eth utils
pub mod eth;
pub use eth::EthHandlers;

// Rpc server metrics
mod metrics;
use crate::middleware::RethRpcMiddleware;
pub use metrics::{MeteredRequestFuture, RpcRequestMetricsService};
use reth_chain_state::CanonStateSubscriptions;
use reth_rpc::eth::sim_bundle::EthSimBundle;

// Rpc rate limiter
pub mod rate_limiter;

/// A builder type to configure the RPC module: See [`RpcModule`]
///
/// This is the main entrypoint and the easiest way to configure an RPC server.
#[derive(Debug, Clone)]
pub struct RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, Consensus> {
    /// The Provider type to when creating all rpc handlers
    provider: Provider,
    /// The Pool type to when creating all rpc handlers
    pool: Pool,
    /// The Network type to when creating all rpc handlers
    network: Network,
    /// How additional tasks are spawned, for example in the eth pubsub namespace
    executor: Box<dyn TaskSpawner + 'static>,
    /// Defines how the EVM should be configured before execution.
    evm_config: EvmConfig,
    /// The consensus implementation.
    consensus: Consensus,
    /// Node data primitives.
    _primitives: PhantomData<N>,
}

// === impl RpcBuilder ===

impl<N, Provider, Pool, Network, EvmConfig, Consensus>
    RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, Consensus>
{
    /// Create a new instance of the builder
    pub const fn new(
        provider: Provider,
        pool: Pool,
        network: Network,
        executor: Box<dyn TaskSpawner + 'static>,
        evm_config: EvmConfig,
        consensus: Consensus,
    ) -> Self {
        Self { provider, pool, network, executor, evm_config, consensus, _primitives: PhantomData }
    }

    /// Configure the provider instance.
    pub fn with_provider<P>(
        self,
        provider: P,
    ) -> RpcModuleBuilder<N, P, Pool, Network, EvmConfig, Consensus> {
        let Self { pool, network, executor, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure the transaction pool instance.
    pub fn with_pool<P>(
        self,
        pool: P,
    ) -> RpcModuleBuilder<N, Provider, P, Network, EvmConfig, Consensus> {
        let Self { provider, network, executor, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure a [`NoopTransactionPool`] instance.
    ///
    /// Caution: This will configure a pool API that does absolutely nothing.
    /// This is only intended for allow easier setup of namespaces that depend on the
    /// [`EthApi`] which requires a [`TransactionPool`] implementation.
    pub fn with_noop_pool(
        self,
    ) -> RpcModuleBuilder<N, Provider, NoopTransactionPool, Network, EvmConfig, Consensus> {
        let Self { provider, executor, network, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder {
            provider,
            executor,
            network,
            evm_config,
            pool: NoopTransactionPool::default(),
            consensus,
            _primitives,
        }
    }

    /// Configure the network instance.
    pub fn with_network<Net>(
        self,
        network: Net,
    ) -> RpcModuleBuilder<N, Provider, Pool, Net, EvmConfig, Consensus> {
        let Self { provider, pool, executor, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure a [`NoopNetwork`] instance.
    ///
    /// Caution: This will configure a network API that does absolutely nothing.
    /// This is only intended for allow easier setup of namespaces that depend on the
    /// [`EthApi`] which requires a [`NetworkInfo`] implementation.
    pub fn with_noop_network(
        self,
    ) -> RpcModuleBuilder<N, Provider, Pool, NoopNetwork, EvmConfig, Consensus> {
        let Self { provider, pool, executor, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder {
            provider,
            pool,
            executor,
            network: NoopNetwork::default(),
            evm_config,
            consensus,
            _primitives,
        }
    }

    /// Configure the task executor to use for additional tasks.
    pub fn with_executor(self, executor: Box<dyn TaskSpawner + 'static>) -> Self {
        let Self { pool, network, provider, evm_config, consensus, _primitives, .. } = self;
        Self { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure [`TokioTaskExecutor`] as the task executor to use for additional tasks.
    ///
    /// This will spawn additional tasks directly via `tokio::task::spawn`, See
    /// [`TokioTaskExecutor`].
    pub fn with_tokio_executor(self) -> Self {
        let Self { pool, network, provider, evm_config, consensus, _primitives, .. } = self;
        Self {
            provider,
            network,
            pool,
            executor: Box::new(TokioTaskExecutor::default()),
            evm_config,
            consensus,
            _primitives,
        }
    }

    /// Configure the evm configuration type
    pub fn with_evm_config<E>(
        self,
        evm_config: E,
    ) -> RpcModuleBuilder<N, Provider, Pool, Network, E, Consensus> {
        let Self { provider, pool, executor, network, consensus, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure the consensus implementation.
    pub fn with_consensus<C>(
        self,
        consensus: C,
    ) -> RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, C> {
        let Self { provider, network, pool, executor, evm_config, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Instantiates a new [`EthApiBuilder`] from the configured components.
    #[expect(clippy::type_complexity)]
    pub fn eth_api_builder<ChainSpec>(
        &self,
    ) -> EthApiBuilder<
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>,
        RpcConverter<Ethereum, EvmConfig, EthReceiptConverter<ChainSpec>>,
    >
    where
        Provider: Clone,
        Pool: Clone,
        Network: Clone,
        EvmConfig: Clone,
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>:
            RpcNodeCore<Provider: ChainSpecProvider<ChainSpec = ChainSpec>, Evm = EvmConfig>,
    {
        EthApiBuilder::new(
            self.provider.clone(),
            self.pool.clone(),
            self.network.clone(),
            self.evm_config.clone(),
        )
    }

    /// Initializes a new [`EthApiServer`] with the configured components and default settings.
    ///
    /// Note: This spawns all necessary tasks.
    ///
    /// See also [`EthApiBuilder`].
    #[expect(clippy::type_complexity)]
    pub fn bootstrap_eth_api<ChainSpec>(
        &self,
    ) -> EthApi<
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>,
        RpcConverter<Ethereum, EvmConfig, EthReceiptConverter<ChainSpec>>,
    >
    where
        Provider: Clone,
        Pool: Clone,
        Network: Clone,
        EvmConfig: ConfigureEvm + Clone,
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>:
            RpcNodeCore<Provider: ChainSpecProvider<ChainSpec = ChainSpec>, Evm = EvmConfig>,
        RpcConverter<Ethereum, EvmConfig, EthReceiptConverter<ChainSpec>>: RpcConvert,
        (): PendingEnvBuilder<EvmConfig>,
    {
        self.eth_api_builder().build()
    }
}

impl<N, Provider, Pool, Network, EvmConfig, Consensus>
    RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: FullRpcProvider<Block = N::Block, Receipt = N::Receipt, Header = N::BlockHeader>
        + CanonStateSubscriptions<Primitives = N>
        + AccountReader
        + ChangeSetReader,
    Pool: TransactionPool + 'static,
    Network: NetworkInfo + Peers + Clone + 'static,
    EvmConfig: ConfigureEvm<Primitives = N> + 'static,
    Consensus: FullConsensus<N, Error = ConsensusError> + Clone + 'static,
{
    /// Configures all [`RpcModule`]s specific to the given [`TransportRpcModuleConfig`] which can
    /// be used to start the transport server(s).
    ///
    /// This behaves exactly as [`RpcModuleBuilder::build`] for the [`TransportRpcModules`], but
    /// also configures the auth (engine api) server, which exposes a subset of the `eth_`
    /// namespace.
    pub fn build_with_auth_server<EthApi>(
        self,
        module_config: TransportRpcModuleConfig,
        engine: impl IntoEngineApiRpcModule,
        eth: EthApi,
    ) -> (
        TransportRpcModules,
        AuthRpcModule,
        RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>,
    )
    where
        EthApi: FullEthApiServer<Provider = Provider, Pool = Pool>,
    {
        let Self { provider, pool, network, executor, consensus, evm_config, .. } = self;

        let config = module_config.config.clone().unwrap_or_default();

        let mut registry = RpcRegistryInner::new(
            provider, pool, network, executor, consensus, config, evm_config, eth,
        );

        let modules = registry.create_transport_rpc_modules(module_config);

        let auth_module = registry.create_auth_module(engine);

        (modules, auth_module, registry)
    }

    /// Converts the builder into a [`RpcRegistryInner`] which can be used to create all
    /// components.
    ///
    /// This is useful for getting access to API handlers directly
    pub fn into_registry<EthApi>(
        self,
        config: RpcModuleConfig,
        eth: EthApi,
    ) -> RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    where
        EthApi: EthApiTypes + 'static,
    {
        let Self { provider, pool, network, executor, consensus, evm_config, .. } = self;
        RpcRegistryInner::new(provider, pool, network, executor, consensus, config, evm_config, eth)
    }

    /// Configures all [`RpcModule`]s specific to the given [`TransportRpcModuleConfig`] which can
    /// be used to start the transport server(s).
    pub fn build<EthApi>(
        self,
        module_config: TransportRpcModuleConfig,
        eth: EthApi,
    ) -> TransportRpcModules<()>
    where
        EthApi: FullEthApiServer<Provider = Provider, Pool = Pool>,
    {
        let mut modules = TransportRpcModules::default();

        let Self { provider, pool, network, executor, consensus, evm_config, .. } = self;

        if !module_config.is_empty() {
            let TransportRpcModuleConfig { http, ws, ipc, config } = module_config.clone();

            let mut registry = RpcRegistryInner::new(
                provider,
                pool,
                network,
                executor,
                consensus,
                config.unwrap_or_default(),
                evm_config,
                eth,
            );

            modules.config = module_config;
            modules.http = registry.maybe_module(http.as_ref());
            modules.ws = registry.maybe_module(ws.as_ref());
            modules.ipc = registry.maybe_module(ipc.as_ref());
        }

        modules
    }
}

impl<N: NodePrimitives> Default for RpcModuleBuilder<N, (), (), (), (), ()> {
    fn default() -> Self {
        Self::new((), (), (), Box::new(TokioTaskExecutor::default()), (), ())
    }
}

/// Bundles settings for modules
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RpcModuleConfig {
    /// `eth` namespace settings
    eth: EthConfig,
    /// `flashbots` namespace settings
    flashbots: ValidationApiConfig,
}

// === impl RpcModuleConfig ===

impl RpcModuleConfig {
    /// Convenience method to create a new [`RpcModuleConfigBuilder`]
    pub fn builder() -> RpcModuleConfigBuilder {
        RpcModuleConfigBuilder::default()
    }

    /// Returns a new RPC module config given the eth namespace config
    pub const fn new(eth: EthConfig, flashbots: ValidationApiConfig) -> Self {
        Self { eth, flashbots }
    }

    /// Get a reference to the eth namespace config
    pub const fn eth(&self) -> &EthConfig {
        &self.eth
    }

    /// Get a mutable reference to the eth namespace config
    pub const fn eth_mut(&mut self) -> &mut EthConfig {
        &mut self.eth
    }
}

/// Configures [`RpcModuleConfig`]
#[derive(Clone, Debug, Default)]
pub struct RpcModuleConfigBuilder {
    eth: Option<EthConfig>,
    flashbots: Option<ValidationApiConfig>,
}

// === impl RpcModuleConfigBuilder ===

impl RpcModuleConfigBuilder {
    /// Configures a custom eth namespace config
    pub const fn eth(mut self, eth: EthConfig) -> Self {
        self.eth = Some(eth);
        self
    }

    /// Configures a custom flashbots namespace config
    pub fn flashbots(mut self, flashbots: ValidationApiConfig) -> Self {
        self.flashbots = Some(flashbots);
        self
    }

    /// Consumes the type and creates the [`RpcModuleConfig`]
    pub fn build(self) -> RpcModuleConfig {
        let Self { eth, flashbots } = self;
        RpcModuleConfig { eth: eth.unwrap_or_default(), flashbots: flashbots.unwrap_or_default() }
    }

    /// Get a reference to the eth namespace config, if any
    pub const fn get_eth(&self) -> Option<&EthConfig> {
        self.eth.as_ref()
    }

    /// Get a mutable reference to the eth namespace config, if any
    pub const fn eth_mut(&mut self) -> &mut Option<EthConfig> {
        &mut self.eth
    }

    /// Get the eth namespace config, creating a default if none is set
    pub fn eth_mut_or_default(&mut self) -> &mut EthConfig {
        self.eth.get_or_insert_with(EthConfig::default)
    }
}

/// A Helper type the holds instances of the configured modules.
#[derive(Debug, Clone)]
#[expect(dead_code)] // Consensus generic, might be useful in the future
pub struct RpcRegistryInner<
    Provider: BlockReader,
    Pool,
    Network,
    EthApi: EthApiTypes,
    EvmConfig,
    Consensus,
> {
    provider: Provider,
    pool: Pool,
    network: Network,
    executor: Box<dyn TaskSpawner + 'static>,
    evm_config: EvmConfig,
    consensus: Consensus,
    /// Holds a all `eth_` namespace handlers
    eth: EthHandlers<EthApi>,
    /// to put trace calls behind semaphore
    blocking_pool_guard: BlockingTaskGuard,
    /// Contains the [Methods] of a module
    modules: HashMap<RethRpcModule, Methods>,
    /// eth config settings
    eth_config: EthConfig,
}

// === impl RpcRegistryInner ===

impl<N, Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: StateProviderFactory
        + CanonStateSubscriptions<Primitives = N>
        + BlockReader<Block = N::Block, Receipt = N::Receipt>
        + Clone
        + Unpin
        + 'static,
    Pool: Send + Sync + Clone + 'static,
    Network: Clone + 'static,
    EthApi: EthApiTypes + 'static,
    EvmConfig: ConfigureEvm<Primitives = N>,
{
    /// Creates a new, empty instance.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        provider: Provider,
        pool: Pool,
        network: Network,
        executor: Box<dyn TaskSpawner + 'static>,
        consensus: Consensus,
        config: RpcModuleConfig,
        evm_config: EvmConfig,
        eth_api: EthApi,
    ) -> Self
    where
        EvmConfig: ConfigureEvm<Primitives = N>,
    {
        let blocking_pool_guard = BlockingTaskGuard::new(config.eth.max_tracing_requests);

        let eth = EthHandlers::bootstrap(config.eth, executor.clone(), eth_api);

        Self {
            provider,
            pool,
            network,
            eth,
            executor,
            consensus,
            modules: Default::default(),
            blocking_pool_guard,
            eth_config: config.eth,
            evm_config,
        }
    }
}

impl<Provider, Pool, Network, EthApi, BlockExecutor, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, BlockExecutor, Consensus>
where
    Provider: BlockReader,
    EthApi: EthApiTypes,
{
    /// Returns a reference to the installed [`EthApi`].
    pub const fn eth_api(&self) -> &EthApi {
        &self.eth.api
    }

    /// Returns a reference to the installed [`EthHandlers`].
    pub const fn eth_handlers(&self) -> &EthHandlers<EthApi> {
        &self.eth
    }

    /// Returns a reference to the pool
    pub const fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Returns a reference to the tasks type
    pub const fn tasks(&self) -> &(dyn TaskSpawner + 'static) {
        &*self.executor
    }

    /// Returns a reference to the provider
    pub const fn provider(&self) -> &Provider {
        &self.provider
    }

    /// Returns all installed methods
    pub fn methods(&self) -> Vec<Methods> {
        self.modules.values().cloned().collect()
    }

    /// Returns a merged `RpcModule`
    pub fn module(&self) -> RpcModule<()> {
        let mut module = RpcModule::new(());
        for methods in self.modules.values().cloned() {
            module.merge(methods).expect("No conflicts");
        }
        module
    }
}

impl<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    Network: NetworkInfo + Clone + 'static,
    EthApi: EthApiTypes,
    Provider: BlockReader + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    EvmConfig: ConfigureEvm,
{
    /// Instantiates `AdminApi`
    pub fn admin_api(&self) -> AdminApi<Network, Provider::ChainSpec>
    where
        Network: Peers,
    {
        AdminApi::new(self.network.clone(), self.provider.chain_spec())
    }

    /// Instantiates `Web3Api`
    pub fn web3_api(&self) -> Web3Api<Network> {
        Web3Api::new(self.network.clone())
    }

    /// Register Admin Namespace
    pub fn register_admin(&mut self) -> &mut Self
    where
        Network: Peers,
    {
        let adminapi = self.admin_api();
        self.modules.insert(RethRpcModule::Admin, adminapi.into_rpc().into());
        self
    }

    /// Register Web3 Namespace
    pub fn register_web3(&mut self) -> &mut Self {
        let web3api = self.web3_api();
        self.modules.insert(RethRpcModule::Web3, web3api.into_rpc().into());
        self
    }
}

impl<N, Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: FullRpcProvider<
            Header = N::BlockHeader,
            Block = N::Block,
            Receipt = N::Receipt,
            Transaction = N::SignedTx,
        > + AccountReader
        + ChangeSetReader
        + CanonStateSubscriptions,
    Network: NetworkInfo + Peers + Clone + 'static,
    EthApi: EthApiServer<
            RpcTxReq<EthApi::NetworkTypes>,
            RpcTransaction<EthApi::NetworkTypes>,
            RpcBlock<EthApi::NetworkTypes>,
            RpcReceipt<EthApi::NetworkTypes>,
            RpcHeader<EthApi::NetworkTypes>,
        > + EthApiTypes,
    EvmConfig: ConfigureEvm<Primitives = N> + 'static,
{
    /// Register Eth Namespace
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn register_eth(&mut self) -> &mut Self {
        let eth_api = self.eth_api().clone();
        self.modules.insert(RethRpcModule::Eth, eth_api.into_rpc().into());
        self
    }

    /// Register Otterscan Namespace
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn register_ots(&mut self) -> &mut Self
    where
        EthApi: TraceExt + EthTransactions,
    {
        let otterscan_api = self.otterscan_api();
        self.modules.insert(RethRpcModule::Ots, otterscan_api.into_rpc().into());
        self
    }

    /// Register Debug Namespace
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn register_debug(&mut self) -> &mut Self
    where
        EthApi: EthApiSpec + EthTransactions + TraceExt,
        EvmConfig::Primitives: NodePrimitives<Block = ProviderBlock<EthApi::Provider>>,
    {
        let debug_api = self.debug_api();
        self.modules.insert(RethRpcModule::Debug, debug_api.into_rpc().into());
        self
    }

    /// Register Trace Namespace
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn register_trace(&mut self) -> &mut Self
    where
        EthApi: TraceExt,
    {
        let trace_api = self.trace_api();
        self.modules.insert(RethRpcModule::Trace, trace_api.into_rpc().into());
        self
    }

    /// Register Net Namespace
    ///
    /// See also [`Self::eth_api`]
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime.
    pub fn register_net(&mut self) -> &mut Self
    where
        EthApi: EthApiSpec + 'static,
    {
        let netapi = self.net_api();
        self.modules.insert(RethRpcModule::Net, netapi.into_rpc().into());
        self
    }

    /// Register Reth namespace
    ///
    /// See also [`Self::eth_api`]
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime.
    pub fn register_reth(&mut self) -> &mut Self {
        let rethapi = self.reth_api();
        self.modules.insert(RethRpcModule::Reth, rethapi.into_rpc().into());
        self
    }

    /// Instantiates `OtterscanApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn otterscan_api(&self) -> OtterscanApi<EthApi> {
        let eth_api = self.eth_api().clone();
        OtterscanApi::new(eth_api)
    }
}

impl<N, Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: FullRpcProvider<
            Block = N::Block,
            Header = N::BlockHeader,
            Transaction = N::SignedTx,
            Receipt = N::Receipt,
        > + AccountReader
        + ChangeSetReader,
    Network: NetworkInfo + Peers + Clone + 'static,
    EthApi: EthApiTypes,
    EvmConfig: ConfigureEvm<Primitives = N>,
{
    /// Instantiates `TraceApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn trace_api(&self) -> TraceApi<EthApi> {
        TraceApi::new(self.eth_api().clone(), self.blocking_pool_guard.clone(), self.eth_config)
    }

    /// Instantiates [`EthBundle`] Api
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn bundle_api(&self) -> EthBundle<EthApi>
    where
        EthApi: EthTransactions + LoadPendingBlock + Call,
    {
        let eth_api = self.eth_api().clone();
        EthBundle::new(eth_api, self.blocking_pool_guard.clone())
    }

    /// Instantiates `DebugApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn debug_api(&self) -> DebugApi<EthApi> {
        DebugApi::new(self.eth_api().clone(), self.blocking_pool_guard.clone())
    }

    /// Instantiates `NetApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn net_api(&self) -> NetApi<Network, EthApi>
    where
        EthApi: EthApiSpec + 'static,
    {
        let eth_api = self.eth_api().clone();
        NetApi::new(self.network.clone(), eth_api)
    }

    /// Instantiates `RethApi`
    pub fn reth_api(&self) -> RethApi<Provider> {
        RethApi::new(self.provider.clone(), self.executor.clone())
    }
}

impl<N, Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: FullRpcProvider<Block = N::Block>
        + CanonStateSubscriptions<Primitives = N>
        + AccountReader
        + ChangeSetReader,
    Pool: TransactionPool + 'static,
    Network: NetworkInfo + Peers + Clone + 'static,
    EthApi: FullEthApiServer,
    EvmConfig: ConfigureEvm<Primitives = N> + 'static,
    Consensus: FullConsensus<N, Error = ConsensusError> + Clone + 'static,
{
    /// Configures the auth module that includes the
    ///   * `engine_` namespace
    ///   * `api_` namespace
    ///
    /// Note: This does _not_ register the `engine_` in this registry.
    pub fn create_auth_module(&self, engine_api: impl IntoEngineApiRpcModule) -> AuthRpcModule {
        let mut module = engine_api.into_rpc_module();

        // also merge a subset of `eth_` handlers
        let eth_handlers = self.eth_handlers();
        let engine_eth = EngineEthApi::new(eth_handlers.api.clone(), eth_handlers.filter.clone());

        module.merge(engine_eth.into_rpc()).expect("No conflicting methods");

        AuthRpcModule { inner: module }
    }

    /// Helper function to create a [`RpcModule`] if it's not `None`
    fn maybe_module(&mut self, config: Option<&RpcModuleSelection>) -> Option<RpcModule<()>> {
        config.map(|config| self.module_for(config))
    }

    /// Configure a [`TransportRpcModules`] using the current registry. This
    /// creates [`RpcModule`] instances for the modules selected by the
    /// `config`.
    pub fn create_transport_rpc_modules(
        &mut self,
        config: TransportRpcModuleConfig,
    ) -> TransportRpcModules<()> {
        let mut modules = TransportRpcModules::default();
        let http = self.maybe_module(config.http.as_ref());
        let ws = self.maybe_module(config.ws.as_ref());
        let ipc = self.maybe_module(config.ipc.as_ref());

        modules.config = config;
        modules.http = http;
        modules.ws = ws;
        modules.ipc = ipc;
        modules
    }

    /// Populates a new [`RpcModule`] based on the selected [`RethRpcModule`]s in the given
    /// [`RpcModuleSelection`]
    pub fn module_for(&mut self, config: &RpcModuleSelection) -> RpcModule<()> {
        let mut module = RpcModule::new(());
        let all_methods = self.reth_methods(config.iter_selection());
        for methods in all_methods {
            module.merge(methods).expect("No conflicts");
        }
        module
    }

    /// Returns the [Methods] for the given [`RethRpcModule`]
    ///
    /// If this is the first time the namespace is requested, a new instance of API implementation
    /// will be created.
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn reth_methods(
        &mut self,
        namespaces: impl Iterator<Item = RethRpcModule>,
    ) -> Vec<Methods> {
        let EthHandlers { api: eth_api, filter: eth_filter, pubsub: eth_pubsub, .. } =
            self.eth_handlers().clone();

        // Create a copy, so we can list out all the methods for rpc_ api
        let namespaces: Vec<_> = namespaces.collect();
        namespaces
            .iter()
            .copied()
            .map(|namespace| {
                self.modules
                    .entry(namespace)
                    .or_insert_with(|| match namespace {
                        RethRpcModule::Admin => {
                            AdminApi::new(self.network.clone(), self.provider.chain_spec())
                                .into_rpc()
                                .into()
                        }
                        RethRpcModule::Debug => {
                            DebugApi::new(eth_api.clone(), self.blocking_pool_guard.clone())
                                .into_rpc()
                                .into()
                        }
                        RethRpcModule::Eth => {
                            // merge all eth handlers
                            let mut module = eth_api.clone().into_rpc();
                            module.merge(eth_filter.clone().into_rpc()).expect("No conflicts");
                            module.merge(eth_pubsub.clone().into_rpc()).expect("No conflicts");
                            module
                                .merge(
                                    EthBundle::new(
                                        eth_api.clone(),
                                        self.blocking_pool_guard.clone(),
                                    )
                                    .into_rpc(),
                                )
                                .expect("No conflicts");

                            module.into()
                        }
                        RethRpcModule::Net => {
                            NetApi::new(self.network.clone(), eth_api.clone()).into_rpc().into()
                        }
                        RethRpcModule::Trace => TraceApi::new(
                            eth_api.clone(),
                            self.blocking_pool_guard.clone(),
                            self.eth_config,
                        )
                        .into_rpc()
                        .into(),
                        RethRpcModule::Web3 => Web3Api::new(self.network.clone()).into_rpc().into(),
                        RethRpcModule::Txpool => TxPoolApi::new(
                            self.eth.api.pool().clone(),
                            self.eth.api.tx_resp_builder().clone(),
                        )
                        .into_rpc()
                        .into(),
                        RethRpcModule::Rpc => RPCApi::new(
                            namespaces
                                .iter()
                                .map(|module| (module.to_string(), "1.0".to_string()))
                                .collect(),
                        )
                        .into_rpc()
                        .into(),
                        RethRpcModule::Ots => OtterscanApi::new(eth_api.clone()).into_rpc().into(),
                        RethRpcModule::Reth => {
                            RethApi::new(self.provider.clone(), self.executor.clone())
                                .into_rpc()
                                .into()
                        }
                        // only relevant for Ethereum and configured in `EthereumAddOns`
                        // implementation
                        // TODO: can we get rid of this here?
                        RethRpcModule::Flashbots => Default::default(),
                        RethRpcModule::Miner => MinerApi::default().into_rpc().into(),
                        RethRpcModule::Mev => {
                            EthSimBundle::new(eth_api.clone(), self.blocking_pool_guard.clone())
                                .into_rpc()
                                .into()
                        }
                    })
                    .clone()
            })
            .collect::<Vec<_>>()
    }
}

/// A builder type for configuring and launching the servers that will handle RPC requests.
///
/// Supported server transports are:
///    - http
///    - ws
///    - ipc
///
/// Http and WS share the same settings: [`ServerBuilder`].
///
/// Once the [`RpcModule`] is built via [`RpcModuleBuilder`] the servers can be started, See also
/// [`ServerBuilder::build`] and [`Server::start`](jsonrpsee::server::Server::start).
#[derive(Debug)]
pub struct RpcServerConfig<RpcMiddleware = Identity> {
    /// Configs for JSON-RPC Http.
    http_server_config: Option<ServerConfigBuilder>,
    /// Allowed CORS Domains for http
    http_cors_domains: Option<String>,
    /// Address where to bind the http server to
    http_addr: Option<SocketAddr>,
    /// Control whether http responses should be compressed
    http_disable_compression: bool,
    /// Configs for WS server
    ws_server_config: Option<ServerConfigBuilder>,
    /// Allowed CORS Domains for ws.
    ws_cors_domains: Option<String>,
    /// Address where to bind the ws server to
    ws_addr: Option<SocketAddr>,
    /// Configs for JSON-RPC IPC server
    ipc_server_config: Option<IpcServerBuilder<Identity, Identity>>,
    /// The Endpoint where to launch the ipc server
    ipc_endpoint: Option<String>,
    /// JWT secret for authentication
    jwt_secret: Option<JwtSecret>,
    /// Configurable RPC middleware
    rpc_middleware: RpcMiddleware,
}

// === impl RpcServerConfig ===

impl Default for RpcServerConfig<Identity> {
    /// Create a new config instance
    fn default() -> Self {
        Self {
            http_server_config: None,
            http_cors_domains: None,
            http_addr: None,
            http_disable_compression: false,
            ws_server_config: None,
            ws_cors_domains: None,
            ws_addr: None,
            ipc_server_config: None,
            ipc_endpoint: None,
            jwt_secret: None,
            rpc_middleware: Default::default(),
        }
    }
}

impl RpcServerConfig {
    /// Creates a new config with only http set
    pub fn http(config: ServerConfigBuilder) -> Self {
        Self::default().with_http(config)
    }

    /// Creates a new config with only ws set
    pub fn ws(config: ServerConfigBuilder) -> Self {
        Self::default().with_ws(config)
    }

    /// Creates a new config with only ipc set
    pub fn ipc(config: IpcServerBuilder<Identity, Identity>) -> Self {
        Self::default().with_ipc(config)
    }

    /// Configures the http server
    ///
    /// Note: this always configures an [`EthSubscriptionIdProvider`] [`IdProvider`] for
    /// convenience. To set a custom [`IdProvider`], please use [`Self::with_id_provider`].
    pub fn with_http(mut self, config: ServerConfigBuilder) -> Self {
        self.http_server_config =
            Some(config.set_id_provider(EthSubscriptionIdProvider::default()));
        self
    }

    /// Configures the ws server
    ///
    /// Note: this always configures an [`EthSubscriptionIdProvider`] [`IdProvider`] for
    /// convenience. To set a custom [`IdProvider`], please use [`Self::with_id_provider`].
    pub fn with_ws(mut self, config: ServerConfigBuilder) -> Self {
        self.ws_server_config = Some(config.set_id_provider(EthSubscriptionIdProvider::default()));
        self
    }

    /// Configures the ipc server
    ///
    /// Note: this always configures an [`EthSubscriptionIdProvider`] [`IdProvider`] for
    /// convenience. To set a custom [`IdProvider`], please use [`Self::with_id_provider`].
    pub fn with_ipc(mut self, config: IpcServerBuilder<Identity, Identity>) -> Self {
        self.ipc_server_config = Some(config.set_id_provider(EthSubscriptionIdProvider::default()));
        self
    }
}

impl<RpcMiddleware> RpcServerConfig<RpcMiddleware> {
    /// Configure rpc middleware
    pub fn set_rpc_middleware<T>(self, rpc_middleware: T) -> RpcServerConfig<T> {
        RpcServerConfig {
            http_server_config: self.http_server_config,
            http_cors_domains: self.http_cors_domains,
            http_addr: self.http_addr,
            http_disable_compression: self.http_disable_compression,
            ws_server_config: self.ws_server_config,
            ws_cors_domains: self.ws_cors_domains,
            ws_addr: self.ws_addr,
            ipc_server_config: self.ipc_server_config,
            ipc_endpoint: self.ipc_endpoint,
            jwt_secret: self.jwt_secret,
            rpc_middleware,
        }
    }

    /// Configure the cors domains for http _and_ ws
    pub fn with_cors(self, cors_domain: Option<String>) -> Self {
        self.with_http_cors(cors_domain.clone()).with_ws_cors(cors_domain)
    }

    /// Configure the cors domains for WS
    pub fn with_ws_cors(mut self, cors_domain: Option<String>) -> Self {
        self.ws_cors_domains = cors_domain;
        self
    }

    /// Configure whether HTTP responses should be compressed
    pub const fn with_http_disable_compression(mut self, http_disable_compression: bool) -> Self {
        self.http_disable_compression = http_disable_compression;
        self
    }

    /// Configure the cors domains for HTTP
    pub fn with_http_cors(mut self, cors_domain: Option<String>) -> Self {
        self.http_cors_domains = cors_domain;
        self
    }

    /// Configures the [`SocketAddr`] of the http server
    ///
    /// Default is [`Ipv4Addr::LOCALHOST`] and
    /// [`reth_rpc_server_types::constants::DEFAULT_HTTP_RPC_PORT`]
    pub const fn with_http_address(mut self, addr: SocketAddr) -> Self {
        self.http_addr = Some(addr);
        self
    }

    /// Configures the [`SocketAddr`] of the ws server
    ///
    /// Default is [`Ipv4Addr::LOCALHOST`] and
    /// [`reth_rpc_server_types::constants::DEFAULT_WS_RPC_PORT`]
    pub const fn with_ws_address(mut self, addr: SocketAddr) -> Self {
        self.ws_addr = Some(addr);
        self
    }

    /// Sets a custom [`IdProvider`] for all configured transports.
    ///
    /// By default all transports use [`EthSubscriptionIdProvider`]
    pub fn with_id_provider<I>(mut self, id_provider: I) -> Self
    where
        I: IdProvider + Clone + 'static,
    {
        if let Some(config) = self.http_server_config {
            self.http_server_config = Some(config.set_id_provider(id_provider.clone()));
        }
        if let Some(config) = self.ws_server_config {
            self.ws_server_config = Some(config.set_id_provider(id_provider.clone()));
        }
        if let Some(ipc) = self.ipc_server_config {
            self.ipc_server_config = Some(ipc.set_id_provider(id_provider));
        }

        self
    }

    /// Configures the endpoint of the ipc server
    ///
    /// Default is [`reth_rpc_server_types::constants::DEFAULT_IPC_ENDPOINT`]
    pub fn with_ipc_endpoint(mut self, path: impl Into<String>) -> Self {
        self.ipc_endpoint = Some(path.into());
        self
    }

    /// Configures the JWT secret for authentication.
    pub const fn with_jwt_secret(mut self, secret: Option<JwtSecret>) -> Self {
        self.jwt_secret = secret;
        self
    }

    /// Returns true if any server is configured.
    ///
    /// If no server is configured, no server will be launched on [`RpcServerConfig::start`].
    pub const fn has_server(&self) -> bool {
        self.http_server_config.is_some() ||
            self.ws_server_config.is_some() ||
            self.ipc_server_config.is_some()
    }

    /// Returns the [`SocketAddr`] of the http server
    pub const fn http_address(&self) -> Option<SocketAddr> {
        self.http_addr
    }

    /// Returns the [`SocketAddr`] of the ws server
    pub const fn ws_address(&self) -> Option<SocketAddr> {
        self.ws_addr
    }

    /// Returns the endpoint of the ipc server
    pub fn ipc_endpoint(&self) -> Option<String> {
        self.ipc_endpoint.clone()
    }

    /// Creates the [`CorsLayer`] if any
    fn maybe_cors_layer(cors: Option<String>) -> Result<Option<CorsLayer>, CorsDomainError> {
        cors.as_deref().map(cors::create_cors_layer).transpose()
    }

    /// Creates the [`AuthLayer`] if any
    fn maybe_jwt_layer(jwt_secret: Option<JwtSecret>) -> Option<AuthLayer<JwtAuthValidator>> {
        jwt_secret.map(|secret| AuthLayer::new(JwtAuthValidator::new(secret)))
    }

    /// Returns a [`CompressionLayer`] that adds compression support (gzip, deflate, brotli, zstd)
    /// based on the client's `Accept-Encoding` header
    fn maybe_compression_layer(disable_compression: bool) -> Option<CompressionLayer> {
        if disable_compression {
            None
        } else {
            Some(CompressionLayer::new())
        }
    }

    /// Builds and starts the configured server(s): http, ws, ipc.
    ///
    /// If both http and ws are on the same port, they are combined into one server.
    ///
    /// Returns the [`RpcServerHandle`] with the handle to the started servers.
    pub async fn start(self, modules: &TransportRpcModules) -> Result<RpcServerHandle, RpcError>
    where
        RpcMiddleware: RethRpcMiddleware,
    {
        let mut http_handle = None;
        let mut ws_handle = None;
        let mut ipc_handle = None;

        let http_socket_addr = self.http_addr.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            constants::DEFAULT_HTTP_RPC_PORT,
        )));

        let ws_socket_addr = self.ws_addr.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            constants::DEFAULT_WS_RPC_PORT,
        )));

        let metrics = modules.ipc.as_ref().map(RpcRequestMetrics::ipc).unwrap_or_default();
        let ipc_path =
            self.ipc_endpoint.clone().unwrap_or_else(|| constants::DEFAULT_IPC_ENDPOINT.into());

        if let Some(builder) = self.ipc_server_config {
            let ipc = builder
                .set_rpc_middleware(IpcRpcServiceBuilder::new().layer(metrics))
                .build(ipc_path);
            ipc_handle = Some(ipc.start(modules.ipc.clone().expect("ipc server error")).await?);
        }

        // If both are configured on the same port, we combine them into one server.
        if self.http_addr == self.ws_addr &&
            self.http_server_config.is_some() &&
            self.ws_server_config.is_some()
        {
            let cors = match (self.ws_cors_domains.as_ref(), self.http_cors_domains.as_ref()) {
                (Some(ws_cors), Some(http_cors)) => {
                    if ws_cors.trim() != http_cors.trim() {
                        return Err(WsHttpSamePortError::ConflictingCorsDomains {
                            http_cors_domains: Some(http_cors.clone()),
                            ws_cors_domains: Some(ws_cors.clone()),
                        }
                        .into());
                    }
                    Some(ws_cors)
                }
                (a, b) => a.or(b),
            }
            .cloned();

            // we merge this into one server using the http setup
            modules.config.ensure_ws_http_identical()?;

            if let Some(config) = self.http_server_config {
                let server = ServerBuilder::new()
                    .set_http_middleware(
                        tower::ServiceBuilder::new()
                            .option_layer(Self::maybe_cors_layer(cors)?)
                            .option_layer(Self::maybe_jwt_layer(self.jwt_secret))
                            .option_layer(Self::maybe_compression_layer(
                                self.http_disable_compression,
                            )),
                    )
                    .set_rpc_middleware(
                        RpcServiceBuilder::default()
                            .layer(
                                modules
                                    .http
                                    .as_ref()
                                    .or(modules.ws.as_ref())
                                    .map(RpcRequestMetrics::same_port)
                                    .unwrap_or_default(),
                            )
                            .layer(self.rpc_middleware.clone()),
                    )
                    .set_config(config.build())
                    .build(http_socket_addr)
                    .await
                    .map_err(|err| {
                        RpcError::server_error(err, ServerKind::WsHttp(http_socket_addr))
                    })?;
                let addr = server.local_addr().map_err(|err| {
                    RpcError::server_error(err, ServerKind::WsHttp(http_socket_addr))
                })?;
                if let Some(module) = modules.http.as_ref().or(modules.ws.as_ref()) {
                    let handle = server.start(module.clone());
                    http_handle = Some(handle.clone());
                    ws_handle = Some(handle);
                }
                return Ok(RpcServerHandle {
                    http_local_addr: Some(addr),
                    ws_local_addr: Some(addr),
                    http: http_handle,
                    ws: ws_handle,
                    ipc_endpoint: self.ipc_endpoint.clone(),
                    ipc: ipc_handle,
                    jwt_secret: self.jwt_secret,
                });
            }
        }

        let mut ws_local_addr = None;
        let mut ws_server = None;
        let mut http_local_addr = None;
        let mut http_server = None;

        if let Some(config) = self.ws_server_config {
            let server = ServerBuilder::new()
                .set_config(config.ws_only().build())
                .set_http_middleware(
                    tower::ServiceBuilder::new()
                        .option_layer(Self::maybe_cors_layer(self.ws_cors_domains.clone())?)
                        .option_layer(Self::maybe_jwt_layer(self.jwt_secret)),
                )
                .set_rpc_middleware(
                    RpcServiceBuilder::default()
                        .layer(modules.ws.as_ref().map(RpcRequestMetrics::ws).unwrap_or_default())
                        .layer(self.rpc_middleware.clone()),
                )
                .build(ws_socket_addr)
                .await
                .map_err(|err| RpcError::server_error(err, ServerKind::WS(ws_socket_addr)))?;

            let addr = server
                .local_addr()
                .map_err(|err| RpcError::server_error(err, ServerKind::WS(ws_socket_addr)))?;

            ws_local_addr = Some(addr);
            ws_server = Some(server);
        }

        if let Some(config) = self.http_server_config {
            let server = ServerBuilder::new()
                .set_config(config.http_only().build())
                .set_http_middleware(
                    tower::ServiceBuilder::new()
                        .option_layer(Self::maybe_cors_layer(self.http_cors_domains.clone())?)
                        .option_layer(Self::maybe_jwt_layer(self.jwt_secret))
                        .option_layer(Self::maybe_compression_layer(self.http_disable_compression)),
                )
                .set_rpc_middleware(
                    RpcServiceBuilder::default()
                        .layer(
                            modules.http.as_ref().map(RpcRequestMetrics::http).unwrap_or_default(),
                        )
                        .layer(self.rpc_middleware.clone()),
                )
                .build(http_socket_addr)
                .await
                .map_err(|err| RpcError::server_error(err, ServerKind::Http(http_socket_addr)))?;
            let local_addr = server
                .local_addr()
                .map_err(|err| RpcError::server_error(err, ServerKind::Http(http_socket_addr)))?;
            http_local_addr = Some(local_addr);
            http_server = Some(server);
        }

        http_handle = http_server
            .map(|http_server| http_server.start(modules.http.clone().expect("http server error")));
        ws_handle = ws_server
            .map(|ws_server| ws_server.start(modules.ws.clone().expect("ws server error")));
        Ok(RpcServerHandle {
            http_local_addr,
            ws_local_addr,
            http: http_handle,
            ws: ws_handle,
            ipc_endpoint: self.ipc_endpoint.clone(),
            ipc: ipc_handle,
            jwt_secret: self.jwt_secret,
        })
    }
}

/// Holds modules to be installed per transport type
///
/// # Example
///
/// Configure a http transport only
///
/// ```
/// use reth_rpc_builder::{RethRpcModule, TransportRpcModuleConfig};
/// let config =
///     TransportRpcModuleConfig::default().with_http([RethRpcModule::Eth, RethRpcModule::Admin]);
/// ```
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct TransportRpcModuleConfig {
    /// http module configuration
    http: Option<RpcModuleSelection>,
    /// ws module configuration
    ws: Option<RpcModuleSelection>,
    /// ipc module configuration
    ipc: Option<RpcModuleSelection>,
    /// Config for the modules
    config: Option<RpcModuleConfig>,
}

// === impl TransportRpcModuleConfig ===

impl TransportRpcModuleConfig {
    /// Creates a new config with only http set
    pub fn set_http(http: impl Into<RpcModuleSelection>) -> Self {
        Self::default().with_http(http)
    }

    /// Creates a new config with only ws set
    pub fn set_ws(ws: impl Into<RpcModuleSelection>) -> Self {
        Self::default().with_ws(ws)
    }

    /// Creates a new config with only ipc set
    pub fn set_ipc(ipc: impl Into<RpcModuleSelection>) -> Self {
        Self::default().with_ipc(ipc)
    }

    /// Sets the [`RpcModuleSelection`] for the http transport.
    pub fn with_http(mut self, http: impl Into<RpcModuleSelection>) -> Self {
        self.http = Some(http.into());
        self
    }

    /// Sets the [`RpcModuleSelection`] for the ws transport.
    pub fn with_ws(mut self, ws: impl Into<RpcModuleSelection>) -> Self {
        self.ws = Some(ws.into());
        self
    }

    /// Sets the [`RpcModuleSelection`] for the http transport.
    pub fn with_ipc(mut self, ipc: impl Into<RpcModuleSelection>) -> Self {
        self.ipc = Some(ipc.into());
        self
    }

    /// Sets a custom [`RpcModuleConfig`] for the configured modules.
    pub fn with_config(mut self, config: RpcModuleConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Get a mutable reference to the
    pub const fn http_mut(&mut self) -> &mut Option<RpcModuleSelection> {
        &mut self.http
    }

    /// Get a mutable reference to the
    pub const fn ws_mut(&mut self) -> &mut Option<RpcModuleSelection> {
        &mut self.ws
    }

    /// Get a mutable reference to the
    pub const fn ipc_mut(&mut self) -> &mut Option<RpcModuleSelection> {
        &mut self.ipc
    }

    /// Get a mutable reference to the
    pub const fn config_mut(&mut self) -> &mut Option<RpcModuleConfig> {
        &mut self.config
    }

    /// Returns true if no transports are configured
    pub const fn is_empty(&self) -> bool {
        self.http.is_none() && self.ws.is_none() && self.ipc.is_none()
    }

    /// Returns the [`RpcModuleSelection`] for the http transport
    pub const fn http(&self) -> Option<&RpcModuleSelection> {
        self.http.as_ref()
    }

    /// Returns the [`RpcModuleSelection`] for the ws transport
    pub const fn ws(&self) -> Option<&RpcModuleSelection> {
        self.ws.as_ref()
    }

    /// Returns the [`RpcModuleSelection`] for the ipc transport
    pub const fn ipc(&self) -> Option<&RpcModuleSelection> {
        self.ipc.as_ref()
    }

    /// Returns the [`RpcModuleConfig`] for the configured modules
    pub const fn config(&self) -> Option<&RpcModuleConfig> {
        self.config.as_ref()
    }

    /// Returns true if the given module is configured for any transport.
    pub fn contains_any(&self, module: &RethRpcModule) -> bool {
        self.contains_http(module) || self.contains_ws(module) || self.contains_ipc(module)
    }

    /// Returns true if the given module is configured for the http transport.
    pub fn contains_http(&self, module: &RethRpcModule) -> bool {
        self.http.as_ref().is_some_and(|http| http.contains(module))
    }

    /// Returns true if the given module is configured for the ws transport.
    pub fn contains_ws(&self, module: &RethRpcModule) -> bool {
        self.ws.as_ref().is_some_and(|ws| ws.contains(module))
    }

    /// Returns true if the given module is configured for the ipc transport.
    pub fn contains_ipc(&self, module: &RethRpcModule) -> bool {
        self.ipc.as_ref().is_some_and(|ipc| ipc.contains(module))
    }

    /// Ensures that both http and ws are configured and that they are configured to use the same
    /// port.
    fn ensure_ws_http_identical(&self) -> Result<(), WsHttpSamePortError> {
        if RpcModuleSelection::are_identical(self.http.as_ref(), self.ws.as_ref()) {
            Ok(())
        } else {
            let http_modules =
                self.http.as_ref().map(RpcModuleSelection::to_selection).unwrap_or_default();
            let ws_modules =
                self.ws.as_ref().map(RpcModuleSelection::to_selection).unwrap_or_default();

            let http_not_ws = http_modules.difference(&ws_modules).copied().collect();
            let ws_not_http = ws_modules.difference(&http_modules).copied().collect();
            let overlap = http_modules.intersection(&ws_modules).copied().collect();

            Err(WsHttpSamePortError::ConflictingModules(Box::new(ConflictingModules {
                overlap,
                http_not_ws,
                ws_not_http,
            })))
        }
    }
}

/// Holds installed modules per transport type.
#[derive(Debug, Clone, Default)]
pub struct TransportRpcModules<Context = ()> {
    /// The original config
    config: TransportRpcModuleConfig,
    /// rpcs module for http
    http: Option<RpcModule<Context>>,
    /// rpcs module for ws
    ws: Option<RpcModule<Context>>,
    /// rpcs module for ipc
    ipc: Option<RpcModule<Context>>,
}

// === impl TransportRpcModules ===

impl TransportRpcModules {
    /// Sets a custom [`TransportRpcModuleConfig`] for the configured modules.
    /// This will overwrite current configuration, if any.
    pub fn with_config(mut self, config: TransportRpcModuleConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets the [`RpcModule`] for the http transport.
    /// This will overwrite current module, if any.
    pub fn with_http(mut self, http: RpcModule<()>) -> Self {
        self.http = Some(http);
        self
    }

    /// Sets the [`RpcModule`] for the ws transport.
    /// This will overwrite current module, if any.
    pub fn with_ws(mut self, ws: RpcModule<()>) -> Self {
        self.ws = Some(ws);
        self
    }

    /// Sets the [`RpcModule`] for the http transport.
    /// This will overwrite current module, if any.
    pub fn with_ipc(mut self, ipc: RpcModule<()>) -> Self {
        self.ipc = Some(ipc);
        self
    }

    /// Returns the [`TransportRpcModuleConfig`] used to configure this instance.
    pub const fn module_config(&self) -> &TransportRpcModuleConfig {
        &self.config
    }

    /// Merge the given [`Methods`] in all configured transport modules if the given
    /// [`RethRpcModule`] is configured for the transport.
    ///
    /// Fails if any of the methods in other is present already.
    pub fn merge_if_module_configured(
        &mut self,
        module: RethRpcModule,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        if self.module_config().contains_http(&module) {
            self.merge_http(other.clone())?;
        }
        if self.module_config().contains_ws(&module) {
            self.merge_ws(other.clone())?;
        }
        if self.module_config().contains_ipc(&module) {
            self.merge_ipc(other)?;
        }

        Ok(())
    }

    /// Merge the given [Methods] in the configured http methods.
    ///
    /// Fails if any of the methods in other is present already.
    ///
    /// Returns [Ok(false)] if no http transport is configured.
    pub fn merge_http(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        if let Some(ref mut http) = self.http {
            return http.merge(other.into()).map(|_| true)
        }
        Ok(false)
    }

    /// Merge the given [Methods] in the configured ws methods.
    ///
    /// Fails if any of the methods in other is present already.
    ///
    /// Returns [Ok(false)] if no ws transport is configured.
    pub fn merge_ws(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        if let Some(ref mut ws) = self.ws {
            return ws.merge(other.into()).map(|_| true)
        }
        Ok(false)
    }

    /// Merge the given [Methods] in the configured ipc methods.
    ///
    /// Fails if any of the methods in other is present already.
    ///
    /// Returns [Ok(false)] if no ipc transport is configured.
    pub fn merge_ipc(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        if let Some(ref mut ipc) = self.ipc {
            return ipc.merge(other.into()).map(|_| true)
        }
        Ok(false)
    }

    /// Merge the given [`Methods`] in all configured methods.
    ///
    /// Fails if any of the methods in other is present already.
    pub fn merge_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        self.merge_http(other.clone())?;
        self.merge_ws(other.clone())?;
        self.merge_ipc(other)?;
        Ok(())
    }

    /// Returns all unique endpoints installed for the given module.
    ///
    /// Note: In case of duplicate method names this only record the first occurrence.
    pub fn methods_by_module<F>(&self, module: RethRpcModule) -> Methods {
        self.methods_by(|name| name.starts_with(module.as_str()))
    }

    /// Returns all unique endpoints installed in any of the configured modules.
    ///
    /// Note: In case of duplicate method names this only record the first occurrence.
    pub fn methods_by<F>(&self, mut filter: F) -> Methods
    where
        F: FnMut(&str) -> bool,
    {
        let mut methods = Methods::new();

        // filter that matches the given filter and also removes duplicates we already have
        let mut f =
            |name: &str, mm: &Methods| filter(name) && !mm.method_names().any(|m| m == name);

        if let Some(m) = self.http_methods(|name| f(name, &methods)) {
            let _ = methods.merge(m);
        }
        if let Some(m) = self.ws_methods(|name| f(name, &methods)) {
            let _ = methods.merge(m);
        }
        if let Some(m) = self.ipc_methods(|name| f(name, &methods)) {
            let _ = methods.merge(m);
        }
        methods
    }

    /// Returns all [`Methods`] installed for the http server based in the given closure.
    ///
    /// Returns `None` if no http support is configured.
    pub fn http_methods<F>(&self, filter: F) -> Option<Methods>
    where
        F: FnMut(&str) -> bool,
    {
        self.http.as_ref().map(|module| methods_by(module, filter))
    }

    /// Returns all [`Methods`] installed for the ws server based in the given closure.
    ///
    /// Returns `None` if no ws support is configured.
    pub fn ws_methods<F>(&self, filter: F) -> Option<Methods>
    where
        F: FnMut(&str) -> bool,
    {
        self.ws.as_ref().map(|module| methods_by(module, filter))
    }

    /// Returns all [`Methods`] installed for the ipc server based in the given closure.
    ///
    /// Returns `None` if no ipc support is configured.
    pub fn ipc_methods<F>(&self, filter: F) -> Option<Methods>
    where
        F: FnMut(&str) -> bool,
    {
        self.ipc.as_ref().map(|module| methods_by(module, filter))
    }

    /// Removes the method with the given name from the configured http methods.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    ///
    /// Be aware that a subscription consist of two methods, `subscribe` and `unsubscribe` and
    /// it's the caller responsibility to remove both `subscribe` and `unsubscribe` methods for
    /// subscriptions.
    pub fn remove_http_method(&mut self, method_name: &'static str) -> bool {
        if let Some(http_module) = &mut self.http {
            http_module.remove_method(method_name).is_some()
        } else {
            false
        }
    }

    /// Removes the given methods from the configured http methods.
    pub fn remove_http_methods(&mut self, methods: impl IntoIterator<Item = &'static str>) {
        for name in methods {
            self.remove_http_method(name);
        }
    }

    /// Removes the method with the given name from the configured ws methods.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    ///
    /// Be aware that a subscription consist of two methods, `subscribe` and `unsubscribe` and
    /// it's the caller responsibility to remove both `subscribe` and `unsubscribe` methods for
    /// subscriptions.
    pub fn remove_ws_method(&mut self, method_name: &'static str) -> bool {
        if let Some(ws_module) = &mut self.ws {
            ws_module.remove_method(method_name).is_some()
        } else {
            false
        }
    }

    /// Removes the given methods from the configured ws methods.
    pub fn remove_ws_methods(&mut self, methods: impl IntoIterator<Item = &'static str>) {
        for name in methods {
            self.remove_ws_method(name);
        }
    }

    /// Removes the method with the given name from the configured ipc methods.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    ///
    /// Be aware that a subscription consist of two methods, `subscribe` and `unsubscribe` and
    /// it's the caller responsibility to remove both `subscribe` and `unsubscribe` methods for
    /// subscriptions.
    pub fn remove_ipc_method(&mut self, method_name: &'static str) -> bool {
        if let Some(ipc_module) = &mut self.ipc {
            ipc_module.remove_method(method_name).is_some()
        } else {
            false
        }
    }

    /// Removes the given methods from the configured ipc methods.
    pub fn remove_ipc_methods(&mut self, methods: impl IntoIterator<Item = &'static str>) {
        for name in methods {
            self.remove_ipc_method(name);
        }
    }

    /// Removes the method with the given name from all configured transports.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    pub fn remove_method_from_configured(&mut self, method_name: &'static str) -> bool {
        let http_removed = self.remove_http_method(method_name);
        let ws_removed = self.remove_ws_method(method_name);
        let ipc_removed = self.remove_ipc_method(method_name);

        http_removed || ws_removed || ipc_removed
    }

    /// Renames a method in all configured transports by:
    /// 1. Removing the old method name.
    /// 2. Adding the new method.
    pub fn rename(
        &mut self,
        old_name: &'static str,
        new_method: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        // Remove the old method from all configured transports
        self.remove_method_from_configured(old_name);

        // Merge the new method into the configured transports
        self.merge_configured(new_method)
    }

    /// Replace the given [`Methods`] in the configured http methods.
    ///
    /// Fails if any of the methods in other is present already or if the method being removed is
    /// not present
    ///
    /// Returns [Ok(false)] if no http transport is configured.
    pub fn replace_http(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_http_methods(other.method_names());
        self.merge_http(other)
    }

    /// Replace the given [Methods] in the configured ipc methods.
    ///
    /// Fails if any of the methods in other is present already or if the method being removed is
    /// not present
    ///
    /// Returns [Ok(false)] if no ipc transport is configured.
    pub fn replace_ipc(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_ipc_methods(other.method_names());
        self.merge_ipc(other)
    }

    /// Replace the given [Methods] in the configured ws methods.
    ///
    /// Fails if any of the methods in other is present already or if the method being removed is
    /// not present
    ///
    /// Returns [Ok(false)] if no ws transport is configured.
    pub fn replace_ws(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_ws_methods(other.method_names());
        self.merge_ws(other)
    }

    /// Replaces the method with the given name from all configured transports.
    ///
    /// Returns `true` if the method was found and replaced, `false` otherwise
    pub fn replace_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.replace_http(other.clone())?;
        self.replace_ws(other.clone())?;
        self.replace_ipc(other)?;
        Ok(true)
    }

    /// Adds or replaces given [`Methods`] in http module.
    ///
    /// Returns `true` if the methods were replaced or added, `false` otherwise.
    pub fn add_or_replace_http(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_http_methods(other.method_names());
        self.merge_http(other)
    }

    /// Adds or replaces given [`Methods`] in ws module.
    ///
    /// Returns `true` if the methods were replaced or added, `false` otherwise.
    pub fn add_or_replace_ws(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_ws_methods(other.method_names());
        self.merge_ws(other)
    }

    /// Adds or replaces given [`Methods`] in ipc module.
    ///
    /// Returns `true` if the methods were replaced or added, `false` otherwise.
    pub fn add_or_replace_ipc(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_ipc_methods(other.method_names());
        self.merge_ipc(other)
    }

    /// Adds or replaces given [`Methods`] in all configured network modules.
    pub fn add_or_replace_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        self.add_or_replace_http(other.clone())?;
        self.add_or_replace_ws(other.clone())?;
        self.add_or_replace_ipc(other)?;
        Ok(())
    }
}

/// Returns the methods installed in the given module that match the given filter.
fn methods_by<T, F>(module: &RpcModule<T>, mut filter: F) -> Methods
where
    F: FnMut(&str) -> bool,
{
    let mut methods = Methods::new();
    let method_names = module.method_names().filter(|name| filter(name));

    for name in method_names {
        if let Some(matched_method) = module.method(name).cloned() {
            let _ = methods.verify_and_insert(name, matched_method);
        }
    }

    methods
}

/// A handle to the spawned servers.
///
/// When this type is dropped or [`RpcServerHandle::stop`] has been called the server will be
/// stopped.
#[derive(Clone, Debug)]
#[must_use = "Server stops if dropped"]
pub struct RpcServerHandle {
    /// The address of the http/ws server
    http_local_addr: Option<SocketAddr>,
    ws_local_addr: Option<SocketAddr>,
    http: Option<ServerHandle>,
    ws: Option<ServerHandle>,
    ipc_endpoint: Option<String>,
    ipc: Option<jsonrpsee::server::ServerHandle>,
    jwt_secret: Option<JwtSecret>,
}

// === impl RpcServerHandle ===

impl RpcServerHandle {
    /// Configures the JWT secret for authentication.
    fn bearer_token(&self) -> Option<String> {
        self.jwt_secret.as_ref().map(|secret| {
            format!(
                "Bearer {}",
                secret
                    .encode(&Claims {
                        iat: (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() +
                            Duration::from_secs(60))
                        .as_secs(),
                        exp: None,
                    })
                    .unwrap()
            )
        })
    }
    /// Returns the [`SocketAddr`] of the http server if started.
    pub const fn http_local_addr(&self) -> Option<SocketAddr> {
        self.http_local_addr
    }

    /// Returns the [`SocketAddr`] of the ws server if started.
    pub const fn ws_local_addr(&self) -> Option<SocketAddr> {
        self.ws_local_addr
    }

    /// Tell the server to stop without waiting for the server to stop.
    pub fn stop(self) -> Result<(), AlreadyStoppedError> {
        if let Some(handle) = self.http {
            handle.stop()?
        }

        if let Some(handle) = self.ws {
            handle.stop()?
        }

        if let Some(handle) = self.ipc {
            handle.stop()?
        }

        Ok(())
    }

    /// Returns the endpoint of the launched IPC server, if any
    pub fn ipc_endpoint(&self) -> Option<String> {
        self.ipc_endpoint.clone()
    }

    /// Returns the url to the http server
    pub fn http_url(&self) -> Option<String> {
        self.http_local_addr.map(|addr| format!("http://{addr}"))
    }

    /// Returns the url to the ws server
    pub fn ws_url(&self) -> Option<String> {
        self.ws_local_addr.map(|addr| format!("ws://{addr}"))
    }

    /// Returns a http client connected to the server.
    pub fn http_client(&self) -> Option<jsonrpsee::http_client::HttpClient> {
        let url = self.http_url()?;

        let client = if let Some(token) = self.bearer_token() {
            jsonrpsee::http_client::HttpClientBuilder::default()
                .set_headers(HeaderMap::from_iter([(AUTHORIZATION, token.parse().unwrap())]))
                .build(url)
        } else {
            jsonrpsee::http_client::HttpClientBuilder::default().build(url)
        };

        client.expect("failed to create http client").into()
    }

    /// Returns a ws client connected to the server.
    pub async fn ws_client(&self) -> Option<jsonrpsee::ws_client::WsClient> {
        let url = self.ws_url()?;
        let mut builder = jsonrpsee::ws_client::WsClientBuilder::default();

        if let Some(token) = self.bearer_token() {
            let headers = HeaderMap::from_iter([(AUTHORIZATION, token.parse().unwrap())]);
            builder = builder.set_headers(headers);
        }

        let client = builder.build(url).await.expect("failed to create ws client");
        Some(client)
    }

    /// Returns a new [`alloy_network::Ethereum`] http provider with its recommended fillers.
    pub fn eth_http_provider(
        &self,
    ) -> Option<impl Provider<alloy_network::Ethereum> + Clone + Unpin + 'static> {
        self.new_http_provider_for()
    }

    /// Returns an http provider from the rpc server handle for the
    /// specified [`alloy_network::Network`].
    ///
    /// This installs the recommended fillers: [`RecommendedFillers`]
    pub fn new_http_provider_for<N>(&self) -> Option<impl Provider<N> + Clone + Unpin + 'static>
    where
        N: RecommendedFillers<RecommendedFillers: Unpin>,
    {
        let rpc_url = self.http_url()?;
        let provider = ProviderBuilder::default()
            .with_recommended_fillers()
            .connect_http(rpc_url.parse().expect("valid url"));
        Some(provider)
    }

    /// Returns a new [`alloy_network::Ethereum`] websocket provider with its recommended fillers.
    pub async fn eth_ws_provider(
        &self,
    ) -> Option<impl Provider<alloy_network::Ethereum> + Clone + Unpin + 'static> {
        self.new_ws_provider_for().await
    }

    /// Returns an ws provider from the rpc server handle for the
    /// specified [`alloy_network::Network`].
    ///
    /// This installs the recommended fillers: [`RecommendedFillers`]
    pub async fn new_ws_provider_for<N>(&self) -> Option<impl Provider<N> + Clone + Unpin + 'static>
    where
        N: RecommendedFillers<RecommendedFillers: Unpin>,
    {
        let rpc_url = self.ws_url()?;
        let provider = ProviderBuilder::default()
            .with_recommended_fillers()
            .connect(&rpc_url)
            .await
            .expect("failed to create ws client");
        Some(provider)
    }

    /// Returns a new [`alloy_network::Ethereum`] ipc provider with its recommended fillers.
    pub async fn eth_ipc_provider(
        &self,
    ) -> Option<impl Provider<alloy_network::Ethereum> + Clone + Unpin + 'static> {
        self.new_ipc_provider_for().await
    }

    /// Returns an ipc provider from the rpc server handle for the
    /// specified [`alloy_network::Network`].
    ///
    /// This installs the recommended fillers: [`RecommendedFillers`]
    pub async fn new_ipc_provider_for<N>(
        &self,
    ) -> Option<impl Provider<N> + Clone + Unpin + 'static>
    where
        N: RecommendedFillers<RecommendedFillers: Unpin>,
    {
        let rpc_url = self.ipc_endpoint()?;
        let provider = ProviderBuilder::default()
            .with_recommended_fillers()
            .connect(&rpc_url)
            .await
            .expect("failed to create ipc client");
        Some(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_eth_call_bundle_selection() {
        let selection = "eth,admin,debug".parse::<RpcModuleSelection>().unwrap();
        assert_eq!(
            selection,
            RpcModuleSelection::Selection(
                [RethRpcModule::Eth, RethRpcModule::Admin, RethRpcModule::Debug,].into()
            )
        );
    }

    #[test]
    fn parse_rpc_module_selection() {
        let selection = "all".parse::<RpcModuleSelection>().unwrap();
        assert_eq!(selection, RpcModuleSelection::All);
    }

    #[test]
    fn parse_rpc_module_selection_none() {
        let selection = "none".parse::<RpcModuleSelection>().unwrap();
        assert_eq!(selection, RpcModuleSelection::Selection(Default::default()));
    }

    #[test]
    fn parse_rpc_unique_module_selection() {
        let selection = "eth,admin,eth,net".parse::<RpcModuleSelection>().unwrap();
        assert_eq!(
            selection,
            RpcModuleSelection::Selection(
                [RethRpcModule::Eth, RethRpcModule::Admin, RethRpcModule::Net,].into()
            )
        );
    }

    #[test]
    fn identical_selection() {
        assert!(RpcModuleSelection::are_identical(
            Some(&RpcModuleSelection::All),
            Some(&RpcModuleSelection::All),
        ));
        assert!(!RpcModuleSelection::are_identical(
            Some(&RpcModuleSelection::All),
            Some(&RpcModuleSelection::Standard),
        ));
        assert!(RpcModuleSelection::are_identical(
            Some(&RpcModuleSelection::Selection(RpcModuleSelection::Standard.to_selection())),
            Some(&RpcModuleSelection::Standard),
        ));
        assert!(RpcModuleSelection::are_identical(
            Some(&RpcModuleSelection::Selection([RethRpcModule::Eth].into())),
            Some(&RpcModuleSelection::Selection([RethRpcModule::Eth].into())),
        ));
        assert!(RpcModuleSelection::are_identical(
            None,
            Some(&RpcModuleSelection::Selection(Default::default())),
        ));
        assert!(RpcModuleSelection::are_identical(
            Some(&RpcModuleSelection::Selection(Default::default())),
            None,
        ));
        assert!(RpcModuleSelection::are_identical(None, None));
    }

    #[test]
    fn test_rpc_module_str() {
        macro_rules! assert_rpc_module {
            ($($s:expr => $v:expr,)*) => {
                $(
                    let val: RethRpcModule  = $s.parse().unwrap();
                    assert_eq!(val, $v);
                    assert_eq!(val.to_string().as_str(), $s);
                )*
            };
        }
        assert_rpc_module!
        (
                "admin" =>  RethRpcModule::Admin,
                "debug" =>  RethRpcModule::Debug,
                "eth" =>  RethRpcModule::Eth,
                "net" =>  RethRpcModule::Net,
                "trace" =>  RethRpcModule::Trace,
                "web3" =>  RethRpcModule::Web3,
                "rpc" => RethRpcModule::Rpc,
                "ots" => RethRpcModule::Ots,
                "reth" => RethRpcModule::Reth,
            );
    }

    #[test]
    fn test_default_selection() {
        let selection = RpcModuleSelection::Standard.to_selection();
        assert_eq!(selection, [RethRpcModule::Eth, RethRpcModule::Net, RethRpcModule::Web3].into())
    }

    #[test]
    fn test_create_rpc_module_config() {
        let selection = vec!["eth", "admin"];
        let config = RpcModuleSelection::try_from_selection(selection).unwrap();
        assert_eq!(
            config,
            RpcModuleSelection::Selection([RethRpcModule::Eth, RethRpcModule::Admin].into())
        );
    }

    #[test]
    fn test_configure_transport_config() {
        let config = TransportRpcModuleConfig::default()
            .with_http([RethRpcModule::Eth, RethRpcModule::Admin]);
        assert_eq!(
            config,
            TransportRpcModuleConfig {
                http: Some(RpcModuleSelection::Selection(
                    [RethRpcModule::Eth, RethRpcModule::Admin].into()
                )),
                ws: None,
                ipc: None,
                config: None,
            }
        )
    }

    #[test]
    fn test_configure_transport_config_none() {
        let config = TransportRpcModuleConfig::default().with_http(Vec::<RethRpcModule>::new());
        assert_eq!(
            config,
            TransportRpcModuleConfig {
                http: Some(RpcModuleSelection::Selection(Default::default())),
                ws: None,
                ipc: None,
                config: None,
            }
        )
    }

    fn create_test_module() -> RpcModule<()> {
        let mut module = RpcModule::new(());
        module.register_method("anything", |_, _, _| "succeed").unwrap();
        module
    }

    #[test]
    fn test_remove_http_method() {
        let mut modules =
            TransportRpcModules { http: Some(create_test_module()), ..Default::default() };
        // Remove a method that exists
        assert!(modules.remove_http_method("anything"));

        // Remove a method that does not exist
        assert!(!modules.remove_http_method("non_existent_method"));

        // Verify that the method was removed
        assert!(modules.http.as_ref().unwrap().method("anything").is_none());
    }

    #[test]
    fn test_remove_ws_method() {
        let mut modules =
            TransportRpcModules { ws: Some(create_test_module()), ..Default::default() };

        // Remove a method that exists
        assert!(modules.remove_ws_method("anything"));

        // Remove a method that does not exist
        assert!(!modules.remove_ws_method("non_existent_method"));

        // Verify that the method was removed
        assert!(modules.ws.as_ref().unwrap().method("anything").is_none());
    }

    #[test]
    fn test_remove_ipc_method() {
        let mut modules =
            TransportRpcModules { ipc: Some(create_test_module()), ..Default::default() };

        // Remove a method that exists
        assert!(modules.remove_ipc_method("anything"));

        // Remove a method that does not exist
        assert!(!modules.remove_ipc_method("non_existent_method"));

        // Verify that the method was removed
        assert!(modules.ipc.as_ref().unwrap().method("anything").is_none());
    }

    #[test]
    fn test_remove_method_from_configured() {
        let mut modules = TransportRpcModules {
            http: Some(create_test_module()),
            ws: Some(create_test_module()),
            ipc: Some(create_test_module()),
            ..Default::default()
        };

        // Remove a method that exists
        assert!(modules.remove_method_from_configured("anything"));

        // Remove a method that was just removed (it does not exist anymore)
        assert!(!modules.remove_method_from_configured("anything"));

        // Remove a method that does not exist
        assert!(!modules.remove_method_from_configured("non_existent_method"));

        // Verify that the method was removed from all transports
        assert!(modules.http.as_ref().unwrap().method("anything").is_none());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_none());
        assert!(modules.ipc.as_ref().unwrap().method("anything").is_none());
    }

    #[test]
    fn test_transport_rpc_module_rename() {
        let mut modules = TransportRpcModules {
            http: Some(create_test_module()),
            ws: Some(create_test_module()),
            ipc: Some(create_test_module()),
            ..Default::default()
        };

        // Verify that the old we want to rename exists at the start
        assert!(modules.http.as_ref().unwrap().method("anything").is_some());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_some());
        assert!(modules.ipc.as_ref().unwrap().method("anything").is_some());

        // Verify that the new method does not exist at the start
        assert!(modules.http.as_ref().unwrap().method("something").is_none());
        assert!(modules.ws.as_ref().unwrap().method("something").is_none());
        assert!(modules.ipc.as_ref().unwrap().method("something").is_none());

        // Create another module
        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        // Rename the method
        modules.rename("anything", other_module).expect("rename failed");

        // Verify that the old method was removed from all transports
        assert!(modules.http.as_ref().unwrap().method("anything").is_none());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_none());
        assert!(modules.ipc.as_ref().unwrap().method("anything").is_none());

        // Verify that the new method was added to all transports
        assert!(modules.http.as_ref().unwrap().method("something").is_some());
        assert!(modules.ws.as_ref().unwrap().method("something").is_some());
        assert!(modules.ipc.as_ref().unwrap().method("something").is_some());
    }

    #[test]
    fn test_replace_http_method() {
        let mut modules =
            TransportRpcModules { http: Some(create_test_module()), ..Default::default() };

        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        assert!(modules.replace_http(other_module.clone()).unwrap());

        assert!(modules.http.as_ref().unwrap().method("something").is_some());

        other_module.register_method("anything", |_, _, _| "fails").unwrap();
        assert!(modules.replace_http(other_module.clone()).unwrap());

        assert!(modules.http.as_ref().unwrap().method("anything").is_some());
    }
    #[test]
    fn test_replace_ipc_method() {
        let mut modules =
            TransportRpcModules { ipc: Some(create_test_module()), ..Default::default() };

        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        assert!(modules.replace_ipc(other_module.clone()).unwrap());

        assert!(modules.ipc.as_ref().unwrap().method("something").is_some());

        other_module.register_method("anything", |_, _, _| "fails").unwrap();
        assert!(modules.replace_ipc(other_module.clone()).unwrap());

        assert!(modules.ipc.as_ref().unwrap().method("anything").is_some());
    }
    #[test]
    fn test_replace_ws_method() {
        let mut modules =
            TransportRpcModules { ws: Some(create_test_module()), ..Default::default() };

        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        assert!(modules.replace_ws(other_module.clone()).unwrap());

        assert!(modules.ws.as_ref().unwrap().method("something").is_some());

        other_module.register_method("anything", |_, _, _| "fails").unwrap();
        assert!(modules.replace_ws(other_module.clone()).unwrap());

        assert!(modules.ws.as_ref().unwrap().method("anything").is_some());
    }

    #[test]
    fn test_replace_configured() {
        let mut modules = TransportRpcModules {
            http: Some(create_test_module()),
            ws: Some(create_test_module()),
            ipc: Some(create_test_module()),
            ..Default::default()
        };
        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        assert!(modules.replace_configured(other_module).unwrap());

        // Verify that the other_method was added
        assert!(modules.http.as_ref().unwrap().method("something").is_some());
        assert!(modules.ipc.as_ref().unwrap().method("something").is_some());
        assert!(modules.ws.as_ref().unwrap().method("something").is_some());

        assert!(modules.http.as_ref().unwrap().method("anything").is_some());
        assert!(modules.ipc.as_ref().unwrap().method("anything").is_some());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_some());
    }
}

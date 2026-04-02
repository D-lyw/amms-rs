//! SKY Protocol Factory (Placeholder)
//!
//! SKY converters are NOT created via a factory contract - they are manually deployed.
//! This module provides a placeholder factory implementation for API consistency
//! with other AMM types in the framework.

use crate::amms::{
    amm::{AutomatedMarketMaker, SyncAction, AMM},
    error::AMMError,
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    sky::SkyConverter,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, B256},
    providers::Provider,
    rpc::types::eth::Log,
};
use eyre::Result;
use serde::{Deserialize, Serialize};

/// Placeholder factory for SKY converters.
///
/// Since SKY converters are manually deployed (not via a factory contract),
/// this implementation always returns an error when attempting to create pools.
/// Use `SkyConverter::new()` directly to create converter instances.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SkyFactory;

impl AutomatedMarketMakerFactory for SkyFactory {
    type PoolVariant = SkyConverter;

    fn address(&self) -> Address {
        Address::ZERO
    }

    fn creation_block(&self) -> u64 {
        0
    }

    fn pool_creation_event(&self) -> B256 {
        B256::ZERO
    }

    fn create_pool(&self, _log: Log) -> Result<AMM, AMMError> {
        Err(AMMError::Msg(
            "SKY converters are not created via factory. Use SkyConverter::new() directly.".into(),
        ))
    }
}

impl DiscoverySync for SkyFactory {
    async fn discover<N, P>(
        &self,
        _to_block: BlockId,
        _provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // SKY converters cannot be discovered via events
        // They must be manually registered
        Ok(vec![])
    }

    async fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        _to_block: BlockId,
        _provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // SKY converters are stateless, no sync needed
        Ok(amms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sky_factory_address() {
        let factory = SkyFactory;
        assert!(factory.address().is_zero());
    }

    #[test]
    fn test_sky_factory_creation_block() {
        let factory = SkyFactory;
        assert_eq!(factory.creation_block(), 0);
    }

    #[test]
    fn test_sky_factory_pool_creation_event() {
        let factory = SkyFactory;
        assert!(factory.pool_creation_event().is_zero());
    }
}

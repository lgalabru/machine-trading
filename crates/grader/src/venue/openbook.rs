//! OpenBook v2 as a [`Venue`] (`specs/venues/openbook.md`): the spot books
//! its profile names ([`super::books`]).

use std::sync::Arc;

use async_trait::async_trait;
use mtx_core::{
    VenueId,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    chain::{ChainClient, RpcChainClient},
    protocols::openbook::v2 as openbook_v2,
};

use super::{Market, Venue, books::BookMarket, not_offered};
use crate::{Result, session::INTERNAL_RPC_TIMEOUT};

/// OpenBook, read and traded through one endpoint.
pub struct OpenBook {
    id: VenueId,
    rpc_url: String,
}

impl OpenBook {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            id: VenueId::new("openbook"),
            rpc_url: rpc_url.into(),
        }
    }
}

#[async_trait]
impl Venue for OpenBook {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        openbook_v2::MARKETS
            .iter()
            .map(|m| InstrumentRef {
                venue: self.id.0.clone(),
                kind: Kind::Spot,
                symbol: m.symbol.into(),
            })
            .collect()
    }

    async fn market(&self, instrument: &InstrumentRef) -> Result<Box<dyn Market>> {
        if instrument.venue != self.id.0 || instrument.kind != Kind::Spot {
            return Err(not_offered(&self.id, instrument));
        }
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
            &self.rpc_url,
            INTERNAL_RPC_TIMEOUT,
        ));
        Ok(Box::new(
            BookMarket::connect(&self.id.0, &instrument.symbol, chain).await?,
        ))
    }
}

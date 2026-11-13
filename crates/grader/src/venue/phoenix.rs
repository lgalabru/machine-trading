//! Phoenix as a [`Venue`] (`specs/venues/phoenix.md`): the Eternal
//! perpetuals of the snapshot's exchange ([`super::perps`]), where the
//! snapshot holds one, and the v1 spot books its profile names
//! ([`super::books`]).

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use mtx_core::{
    VenueId,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    chain::{ChainClient, RpcChainClient},
    phoenix_rise::types::exchange::ExchangeResponse,
    protocols::phoenix::v1 as phoenix_v1,
};
use solana_pubkey::Pubkey;

use super::{Market, Venue, books::BookMarket, not_offered, perps::PerpMarket};
use crate::{Result, session::INTERNAL_RPC_TIMEOUT};

/// Phoenix, read and traded through one endpoint.
pub struct Phoenix {
    id: VenueId,
    rpc_url: String,
    /// The Eternal exchange's metadata and the file it was read from, and
    /// an account that exists to pay for Hawkeye's reads.
    exchange: Option<(ExchangeResponse, PathBuf, Pubkey)>,
}

impl Phoenix {
    /// Phoenix's spot books, read through `rpc_url`.
    pub fn new(rpc_url: &str) -> Self {
        Self {
            id: VenueId::new(crate::scenario::VENUE),
            rpc_url: rpc_url.into(),
            exchange: None,
        }
    }

    /// Phoenix with the Eternal exchange a snapshot holds (its metadata,
    /// and the file it was read from): its perpetuals too, read with
    /// `reader`, an account that exists, as Hawkeye's fee payer.
    pub fn with_exchange(
        rpc_url: &str,
        exchange: ExchangeResponse,
        exchange_path: PathBuf,
        reader: Pubkey,
    ) -> Self {
        Self {
            exchange: Some((exchange, exchange_path, reader)),
            ..Self::new(rpc_url)
        }
    }

    /// `symbol`'s perpetual.
    pub fn perp(&self, symbol: &str) -> Result<PerpMarket> {
        let Some((exchange, path, reader)) = &self.exchange else {
            return Err(not_offered(
                &self.id,
                &InstrumentRef {
                    venue: self.id.0.clone(),
                    kind: Kind::Perp,
                    symbol: symbol.into(),
                },
            ));
        };
        PerpMarket::new(
            exchange.clone(),
            path.clone(),
            symbol,
            &self.rpc_url,
            *reader,
        )
    }

    /// `symbol`'s v1 spot book.
    pub async fn book(&self, symbol: &str) -> Result<BookMarket> {
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
            &self.rpc_url,
            INTERNAL_RPC_TIMEOUT,
        ));
        BookMarket::connect(&self.id.0, symbol, chain).await
    }
}

#[async_trait]
impl Venue for Phoenix {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        let at = |kind, symbol: &str| InstrumentRef {
            venue: self.id.0.clone(),
            kind,
            symbol: symbol.into(),
        };
        let perps = self.exchange.iter().flat_map(|(exchange, _, _)| {
            exchange
                .markets
                .iter()
                .map(move |m| at(Kind::Perp, &m.symbol))
        });
        perps
            .chain(phoenix_v1::MARKETS.iter().map(|m| at(Kind::Spot, m.symbol)))
            .collect()
    }

    async fn market(&self, instrument: &InstrumentRef) -> Result<Box<dyn Market>> {
        if instrument.venue != self.id.0 {
            return Err(not_offered(&self.id, instrument));
        }
        Ok(match instrument.kind {
            Kind::Perp => Box::new(self.perp(&instrument.symbol)?),
            Kind::Spot => Box::new(self.book(&instrument.symbol).await?),
            Kind::StakePool => return Err(not_offered(&self.id, instrument)),
        })
    }
}

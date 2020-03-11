// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

mod chain;

pub use chain::BlockChain;

pub mod chain_service;
pub mod mem_chain;
pub mod message;

use crate::chain_service::ChainServiceImpl;
use crate::message::ChainResponse;
use actix::dev::ToEnvelope;
use actix::fut::wrap_future;
use actix::prelude::*;
use anyhow::{bail, Error, Result};
use bus::{BusActor, Subscription};
use config::NodeConfig;
use consensus::dummy::DummyConsensus;
use crypto::{hash::CryptoHash, HashValue};
use executor::mock_executor::MockExecutor;
use futures::compat::Future01CompatExt;
use futures_locks::RwLock;
use logger::prelude::*;
use message::ChainRequest;
use network::network::NetworkAsyncService;
use starcoin_accumulator::{Accumulator, AccumulatorNodeStore, MerkleAccumulator};
use state_tree::StateNodeStore;
use std::sync::Arc;
use storage::{BlockStorageOp, StarcoinStorage};
use traits::{AsyncChain, ChainAsyncService, ChainReader, ChainService, ChainWriter};
use txpool::TxPoolRef;
use types::{
    block::{Block, BlockHeader, BlockNumber, BlockTemplate},
    startup_info::{ChainInfo, StartupInfo},
    system_events::SystemEvents,
};

/// actor for block chain.
pub struct ChainActor {
    //TODO use Generic Parameter for Executor and Consensus.
    service: ChainServiceImpl<MockExecutor, DummyConsensus, TxPoolRef, StarcoinStorage>,
    bus: Addr<BusActor>,
}

impl ChainActor {
    pub fn launch(
        config: Arc<NodeConfig>,
        startup_info: StartupInfo,
        storage: Arc<StarcoinStorage>,
        network: Option<NetworkAsyncService<TxPoolRef>>,
        bus: Addr<BusActor>,
        txpool: TxPoolRef,
    ) -> Result<ChainActorRef<ChainActor>> {
        let actor = ChainActor {
            service: ChainServiceImpl::new(config, startup_info, storage, network, txpool)?,
            bus,
        }
        .start();
        Ok(actor.into())
    }
}

impl Actor for ChainActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let recipient = ctx.address().recipient::<SystemEvents>();
        self.bus
            .send(Subscription { recipient })
            .into_actor(self)
            .then(|_res, act, _ctx| async {}.into_actor(act))
            .wait(ctx);
        info!("ChainActor actor started");
    }
}

impl Handler<ChainRequest> for ChainActor {
    type Result = Result<ChainResponse>;

    fn handle(&mut self, msg: ChainRequest, ctx: &mut Self::Context) -> Self::Result {
        match msg {
            ChainRequest::CreateBlock(times) => {
                let head_block = self.service.head_block();
                let mut parent_block_hash = head_block.crypto_hash();
                for i in 0..times {
                    debug!("parent_block_hash: {:?}", parent_block_hash);
                    let current_block_header =
                        BlockHeader::new_block_header_for_test(parent_block_hash, i);
                    let current_block = Block::new_nil_block_for_test(current_block_header);
                    parent_block_hash = current_block.crypto_hash();
                    self.service.try_connect(current_block)?;
                }
                Ok(ChainResponse::None)
            }
            ChainRequest::CurrentHeader() => {
                Ok(ChainResponse::BlockHeader(self.service.current_header()))
            }
            ChainRequest::GetHeaderByHash(hash) => Ok(ChainResponse::BlockHeader(
                self.service.get_header(hash).unwrap().unwrap(),
            )),
            ChainRequest::HeadBlock() => Ok(ChainResponse::Block(self.service.head_block())),
            ChainRequest::GetHeaderByNumber(number) => Ok(ChainResponse::BlockHeader(
                self.service.get_header_by_number(number)?.unwrap(),
            )),
            ChainRequest::GetBlockByNumber(number) => Ok(ChainResponse::Block(
                self.service.get_block_by_number(number)?.unwrap(),
            )),
            ChainRequest::CreateBlockTemplate() => Ok(ChainResponse::BlockTemplate(
                //TODO get txn from txpool.
                self.service.create_block_template(vec![]).unwrap(),
            )),
            ChainRequest::GetBlockByHash(hash) => Ok(ChainResponse::OptionBlock(
                self.service.get_block(hash).unwrap(),
            )),
            ChainRequest::ConnectBlock(block) => {
                self.service.try_connect(block).unwrap();
                Ok(ChainResponse::None)
            }
            ChainRequest::GetHeadBranch() => {
                let hash = self.service.get_head_branch();
                Ok(ChainResponse::HashValue(hash))
            }
            ChainRequest::GetChainInfo() => {
                Ok(ChainResponse::ChainInfo(self.service.get_chain_info()))
            }
            ChainRequest::GenTx() => {
                self.service.gen_tx().unwrap();
                Ok(ChainResponse::None)
            }
        }
    }
}

impl Handler<SystemEvents> for ChainActor {
    type Result = ();

    fn handle(&mut self, msg: SystemEvents, ctx: &mut Self::Context) -> Self::Result {
        debug!("try connect mined block.");
        match msg {
            SystemEvents::MinedBlock(new_block) => match self.service.try_connect(new_block) {
                Ok(_) => debug!("Process mined block success."),
                Err(e) => {
                    warn!("Process mined block fail, error: {:?}", e);
                }
            },
            _ => {}
        }
    }
}

pub struct ChainActorRef<A>
where
    A: Actor + Handler<ChainRequest>,
    A::Context: ToEnvelope<A, ChainRequest>,
{
    pub address: Addr<A>,
}

impl<A> Clone for ChainActorRef<A>
where
    A: Actor + Handler<ChainRequest>,
    A::Context: ToEnvelope<A, ChainRequest>,
{
    fn clone(&self) -> ChainActorRef<A> {
        ChainActorRef {
            address: self.address.clone(),
        }
    }
}

impl<A> Into<Addr<A>> for ChainActorRef<A>
where
    A: Actor + Handler<ChainRequest>,
    A::Context: ToEnvelope<A, ChainRequest>,
{
    fn into(self) -> Addr<A> {
        self.address
    }
}

impl<A> Into<ChainActorRef<A>> for Addr<A>
where
    A: Actor + Handler<ChainRequest>,
    A::Context: ToEnvelope<A, ChainRequest>,
{
    fn into(self) -> ChainActorRef<A> {
        ChainActorRef { address: self }
    }
}

#[async_trait::async_trait(? Send)]
impl<A> AsyncChain for ChainActorRef<A>
where
    A: Actor + Handler<ChainRequest>,
    A::Context: ToEnvelope<A, ChainRequest>,
{
    async fn current_header(self) -> Option<BlockHeader> {
        if let ChainResponse::BlockHeader(header) = self
            .address
            .send(ChainRequest::CurrentHeader())
            .await
            .unwrap()
            .unwrap()
        {
            Some(header)
        } else {
            None
        }
    }

    async fn get_header_by_hash(self, hash: &HashValue) -> Option<BlockHeader> {
        if let ChainResponse::BlockHeader(header) = self
            .address
            .send(ChainRequest::GetHeaderByHash(hash.clone()))
            .await
            .unwrap()
            .unwrap()
        {
            Some(header)
        } else {
            None
        }
    }

    async fn head_block(self) -> Option<Block> {
        if let ChainResponse::Block(block) = self
            .address
            .send(ChainRequest::HeadBlock())
            .await
            .unwrap()
            .unwrap()
        {
            Some(block)
        } else {
            None
        }
    }

    async fn get_header_by_number(self, number: BlockNumber) -> Option<BlockHeader> {
        if let ChainResponse::BlockHeader(header) = self
            .address
            .send(ChainRequest::GetHeaderByNumber(number))
            .await
            .unwrap()
            .unwrap()
        {
            Some(header)
        } else {
            None
        }
    }

    async fn get_block_by_number(self, number: BlockNumber) -> Option<Block> {
        if let ChainResponse::Block(block) = self
            .address
            .send(ChainRequest::GetBlockByNumber(number))
            .await
            .unwrap()
            .unwrap()
        {
            Some(block)
        } else {
            None
        }
    }

    async fn create_block_template(self) -> Option<BlockTemplate> {
        let address = self.address.clone();
        drop(self);
        if let ChainResponse::BlockTemplate(block_template) = address
            .send(ChainRequest::CreateBlockTemplate())
            .await
            .unwrap()
            .unwrap()
        {
            Some(block_template)
        } else {
            None
        }
    }

    async fn get_block_by_hash(self, hash: &HashValue) -> Option<Block> {
        debug!("hash: {:?}", hash);
        if let ChainResponse::OptionBlock(block) = self
            .address
            .send(ChainRequest::GetBlockByHash(hash.clone()))
            .await
            .unwrap()
            .unwrap()
        {
            match block {
                Some(b) => Some(b),
                _ => None,
            }
        } else {
            None
        }
    }
}

#[async_trait::async_trait(? Send)]
impl<A> ChainAsyncService for ChainActorRef<A>
where
    A: Actor + Handler<ChainRequest>,
    A::Context: ToEnvelope<A, ChainRequest>,
{
    async fn try_connect(self, block: Block) -> Result<()> {
        self.address
            .send(ChainRequest::ConnectBlock(block))
            .await
            .map_err(|e| Into::<Error>::into(e))?;
        Ok(())
    }

    async fn get_head_branch(self) -> Result<HashValue> {
        if let ChainResponse::HashValue(hash) =
            self.address.send(ChainRequest::GetHeadBranch()).await??
        {
            Ok(hash)
        } else {
            panic!("Chain response type error.")
        }
    }

    async fn get_chain_info(self) -> Result<ChainInfo> {
        let response = self
            .address
            .send(ChainRequest::GetChainInfo())
            .await
            .map_err(|e| Into::<Error>::into(e))??;
        if let ChainResponse::ChainInfo(chain_info) = response {
            Ok(chain_info)
        } else {
            bail!("Get chain info response error.")
        }
    }

    async fn gen_tx(&self) -> Result<()> {
        self.address
            .send(ChainRequest::GenTx())
            .await
            .map_err(|e| Into::<Error>::into(e))?;
        Ok(())
    }
}

mod tests;

// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::config::Config;
use crate::metrics::BridgeIndexerMetrics;
use crate::postgres_manager::{
    read_eth_progress_store, update_earliest_block_synced, update_latest_block_synced, write,
    PgPool,
};
use crate::{BridgeDataSource, TokenTransfer, TokenTransferData, TokenTransferStatus};
use anyhow::Result;
use core::panic;
use ethers::providers::Provider;
use ethers::providers::{Http, Middleware};
use ethers::types::Address as EthAddress;
use mysten_metrics::spawn_logged_monitored_task;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use sui_bridge::abi::{EthBridgeEvent, EthSuiBridgeEvents};
use sui_bridge::error::BridgeError;
use sui_bridge::metrics::BridgeMetrics;
use sui_bridge::retry_with_max_elapsed_time;
use sui_bridge::types::{EthEvent, EthLog, RawEthLog};
use sui_bridge::{eth_client::EthClient, eth_syncer::EthSyncer};
use tokio::task::JoinHandle;
use tracing::info;
use tracing::log::error;

const MAX_BLOCK_RANGE: u64 = 1000;
const MIN_BLOCK_RANGE: u64 = 2;

#[derive(Clone)]
pub struct EthBridgeWorker {
    provider: Arc<Provider<Http>>,
    pg_pool: PgPool,
    bridge_metrics: Arc<BridgeMetrics>,
    metrics: BridgeIndexerMetrics,
    pub bridge_address: EthAddress,
    config: Config,
}

impl EthBridgeWorker {
    pub fn new(
        pg_pool: PgPool,
        bridge_metrics: Arc<BridgeMetrics>,
        metrics: BridgeIndexerMetrics,
        config: Config,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let bridge_address = EthAddress::from_str(&config.eth_sui_bridge_contract_address)?;

        let provider = Arc::new(
            Provider::<Http>::try_from(&config.eth_rpc_url)?
                .interval(std::time::Duration::from_millis(2000)),
        );

        Ok(Self {
            provider,
            pg_pool,
            bridge_metrics,
            metrics,
            bridge_address,
            config,
        })
    }

    pub async fn sync_events(
        &self,
        client: Arc<EthClient<ethers::providers::Http>>,
        genesis_block: u64,
    ) {
        println!("Syncing event history");

        // get current block number
        let current_block = self.provider.get_block_number().await.unwrap().as_u64();

        // get eth sync progress store
        let (mut earliest_block_synced, latest_block_synced) =
            read_eth_progress_store(&self.pg_pool).unwrap();

        if earliest_block_synced == 0 {
            earliest_block_synced = current_block;
        }

        // start range from current block to MAX_BLOCK_RANGE blocks back
        let mut end_block = current_block;
        let mut start_block = end_block - MAX_BLOCK_RANGE;

        loop {
            // if next range is before genesis block, break
            if end_block <= genesis_block {
                break;
            }

            // if range overlaps with latest block synced and the end block is still after the latest block synced
            if start_block < latest_block_synced && end_block > latest_block_synced {
                start_block = latest_block_synced;
            }

            let events = retry_with_max_elapsed_time!(
                client.get_raw_events_in_range(self.bridge_address, start_block, end_block),
                Duration::from_secs(30000)
            )
            .and_then(|events_result| events_result)
            .unwrap();

            if events.len() > 0 {
                println!(
                    "⏳ Processing {} events in range {} to {}",
                    events.len(),
                    start_block,
                    end_block
                );
                retry_with_max_elapsed_time!(
                    process_eth_events(
                        self.provider.clone(),
                        client.clone(),
                        self.pg_pool.clone(),
                        self.metrics.clone(),
                        events.clone(),
                    ),
                    Duration::from_millis(30000)
                );
                println!(
                    "✅ Processed {} events in range {} to {}",
                    events.len(),
                    start_block,
                    end_block
                );
            }

            // if range connects with latest block synced or first sync, update latest block synced to end block
            if start_block == latest_block_synced
                || (latest_block_synced == 0 && end_block == current_block)
            {
                // TODO: handle error
                println!(
                    "🚀 Caught up to previous sync... skipping to {}",
                    earliest_block_synced
                );
                let _ = update_latest_block_synced(&self.pg_pool.clone(), current_block);
                end_block = earliest_block_synced;
            } else {
                // if range does not connect with latest block synced, grab next range
                end_block = start_block;
            }

            start_block = end_block - MAX_BLOCK_RANGE;

            if start_block <= genesis_block {
                start_block = genesis_block;
            }

            // if range overlaps with earliest block synced update sync progress to include latest block to current block
            if start_block < earliest_block_synced {
                // TODO: handle error
                let _ = update_earliest_block_synced(&self.pg_pool.clone(), start_block);
            }
            // TODO: update metrics
            // progress_gauge.set(block_number as i64);
        }
        println!("Finished syncing event history");
    }

    pub async fn subscribe_to_latest_events(&self) {
        println!("Subscribing to latest events");
    }

    pub async fn subscribe_to_finalized_events(&self) {
        println!("Subscribing to finalized events");
    }
}

async fn process_eth_events<E: EthEvent>(
    provider: Arc<Provider<Http>>,
    client: Arc<EthClient<ethers::providers::Http>>,
    pg_pool: PgPool,
    metrics: BridgeIndexerMetrics,
    events: Vec<E>,
) -> Result<()> {
    let last_finalized_block = client.get_last_finalized_block_id().await.unwrap();
    let mut transfers: Vec<TokenTransfer> = Vec::new();
    let mut block_timestamps: HashMap<u64, u64> = HashMap::new();

    for event in events.iter() {
        let eth_bridge_event = EthBridgeEvent::try_from_log(event.log());
        if eth_bridge_event.is_none() {
            continue;
        }
        metrics.total_eth_bridge_transactions.inc();
        let bridge_event = eth_bridge_event.unwrap();
        let block_number = event.block_number();
        let finalized = block_number <= last_finalized_block;

        // Check if the block timestamp is already in the cache
        let timestamp = if let Some(&cached_timestamp) = block_timestamps.get(&block_number) {
            cached_timestamp
        } else {
            // TODO: handle error
            let block = retry_with_max_elapsed_time!(
                provider.get_block(block_number),
                Duration::from_millis(30000)
            )
            .unwrap()
            .unwrap()
            .unwrap();
            let timestamp = block.timestamp.as_u64() * 1000;
            block_timestamps.insert(block_number, timestamp);
            timestamp
        };

        let tx_hash = event.tx_hash();
        // TODO: handle error
        let transaction = retry_with_max_elapsed_time!(
            provider.get_transaction(tx_hash),
            Duration::from_millis(30000)
        )
        .unwrap()
        .unwrap()
        .unwrap();
        let gas = transaction.gas;

        let transfer: TokenTransfer = match bridge_event {
            EthBridgeEvent::EthSuiBridgeEvents(bridge_event) => match bridge_event {
                EthSuiBridgeEvents::TokensDepositedFilter(bridge_event) => {
                    metrics.total_eth_token_deposited.inc();
                    TokenTransfer {
                        chain_id: bridge_event.source_chain_id,
                        nonce: bridge_event.nonce,
                        block_height: block_number,
                        timestamp_ms: timestamp,
                        txn_hash: tx_hash.as_bytes().to_vec(),
                        txn_sender: bridge_event.sender_address.as_bytes().to_vec(),
                        status: TokenTransferStatus::Deposited,
                        gas_usage: gas.as_u64() as i64,
                        data_source: BridgeDataSource::Eth,
                        data: Some(TokenTransferData {
                            sender_address: bridge_event.sender_address.as_bytes().to_vec(),
                            destination_chain: bridge_event.destination_chain_id,
                            recipient_address: bridge_event.recipient_address.to_vec(),
                            token_id: bridge_event.token_id,
                            amount: bridge_event.sui_adjusted_amount,
                        }),
                    }
                }
                EthSuiBridgeEvents::TokensClaimedFilter(bridge_event) => {
                    metrics.total_eth_token_transfer_claimed.inc();
                    TokenTransfer {
                        chain_id: bridge_event.source_chain_id,
                        nonce: bridge_event.nonce,
                        block_height: block_number,
                        timestamp_ms: timestamp,
                        txn_hash: tx_hash.as_bytes().to_vec(),
                        txn_sender: bridge_event.sender_address.to_vec(),
                        status: TokenTransferStatus::Claimed,
                        gas_usage: gas.as_u64() as i64,
                        data_source: BridgeDataSource::Eth,
                        data: None,
                    }
                }
                EthSuiBridgeEvents::PausedFilter(_)
                | EthSuiBridgeEvents::UnpausedFilter(_)
                | EthSuiBridgeEvents::UpgradedFilter(_)
                | EthSuiBridgeEvents::InitializedFilter(_) => {
                    metrics.total_eth_bridge_txn_other.inc();
                    continue;
                }
            },
            EthBridgeEvent::EthBridgeCommitteeEvents(_)
            | EthBridgeEvent::EthBridgeLimiterEvents(_)
            | EthBridgeEvent::EthBridgeConfigEvents(_)
            | EthBridgeEvent::EthCommitteeUpgradeableContractEvents(_) => {
                metrics.total_eth_bridge_txn_other.inc();
                continue;
            }
        };

        // if event is in finalized block and is a deposit event, include unfinalized deposit event
        if finalized && matches!(transfer.status, TokenTransferStatus::Deposited) {
            let unfinalized_transfer = TokenTransfer {
                status: TokenTransferStatus::DepositedUnfinalized,
                ..transfer.clone()
            };
            transfers.push(unfinalized_transfer);
        }

        transfers.push(transfer.clone());

        // Batch write all transfers
        if let Err(e) = write(&pg_pool, transfers.clone()) {
            error!("Error writing token transfers to database: {:?}", e);
        }
    }

    Ok(())
}

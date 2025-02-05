use std::{fs::File, time::Duration};

use anyhow::Result;
use cairo_vm::types::layout_name::LayoutName;
use log::{debug, error};
use starknet::{core::types::BlockId, providers::{jsonrpc::HttpTransport, JsonRpcClient, Provider}};
use tokio::sync::mpsc::Sender;
use url::Url;

use crate::{
    block_ingestor::{BlockIngestor, BlockIngestorBuilder, NewBlock},
    service::{Daemon, FinishHandle, ShutdownHandle},
};

const PROVE_BLOCK_FAILURE_BACKOFF: Duration = Duration::from_secs(5);

/// A block ingestor which collects new blocks by polling a Starknet RPC endpoint.
#[derive(Debug)]
pub struct PollingBlockIngestor<S> {
    rpc_url: Url,
    snos: S,
    current_block: u64,
    channel: Sender<NewBlock>,
    finish_handle: FinishHandle,
}

#[derive(Debug)]
pub struct PollingBlockIngestorBuilder<S> {
    rpc_url: Url,
    snos: S,
    start_block: Option<u64>,
    channel: Option<Sender<NewBlock>>,
}

impl<S> PollingBlockIngestor<S>
where
    S: AsRef<[u8]>,
{
    async fn run(mut self) {
        let url = self.rpc_url.clone();

        loop {
            let pie = match prove_block::prove_block(
                self.snos.as_ref(),
                self.current_block,
                // This is because `snos` expects a base URL to be able to derive `pathfinder` RPC path.
                url.clone().as_str().trim_end_matches("/rpc/v0_7"),
                LayoutName::all_cairo,
                true,
            )
            .await
            // Need to do this as `ProveBlockError::ReExecutionError` is not `Send`
            .map_err(|err| format!("{}", err))
            {
                Ok((pie, _)) => pie,
                Err(err) => {
                    if !err.contains("BlockNotFound") {
                        error!("Failed to prove block #{}: {}", self.current_block, err);
                    }

                    tokio::select! {
                        _ = self.finish_handle.shutdown_requested() => break,
                        _ = tokio::time::sleep(PROVE_BLOCK_FAILURE_BACKOFF) => continue,
                    }
                }
            };

            // For testing, let's gather some into of the block.
            let provider = JsonRpcClient::new(HttpTransport::new(url.clone()));
            let block = provider.get_block_with_tx_hashes(BlockId::Number(self.current_block)).await.unwrap();
            let n_txs = block.transactions().len() as u64;

            debug!("PIE generated for block #{} ({} steps)", self.current_block, pie.execution_resources.n_steps);

            // Write the PIE to a file to debug (using json serde)
            let mut file = File::create(format!("pie_{}_{}.json", self.current_block, n_txs)).unwrap();
            serde_json::to_writer(&mut file, &pie).unwrap();

            // No way to hook into `prove_block` for cancellation. The next best thing we can do is
            // to check cancellation immediately after PIE generation.
            if self.finish_handle.is_shutdown_requested() {
                break;
            }

            let new_block = NewBlock {
                number: self.current_block,
                pie,
                n_txs,
            };

            // Since the channel is bounded, it's possible
            tokio::select! {
                _ = self.finish_handle.shutdown_requested() => break,
                _ = self.channel.send(new_block) => {},
            }

            self.current_block += 1;
        }

        debug!("Graceful shutdown finished");
        self.finish_handle.finish();
    }
}

impl<S> PollingBlockIngestorBuilder<S> {
    pub fn new(rpc_url: Url, snos: S) -> Self {
        Self {
            rpc_url,
            snos,
            start_block: None,
            channel: None,
        }
    }
}

impl<S> BlockIngestorBuilder for PollingBlockIngestorBuilder<S>
where
    S: AsRef<[u8]> + Send + 'static,
{
    type Ingestor = PollingBlockIngestor<S>;

    fn build(self) -> Result<Self::Ingestor> {
        Ok(PollingBlockIngestor {
            rpc_url: self.rpc_url,
            snos: self.snos,
            current_block: self
                .start_block
                .ok_or_else(|| anyhow::anyhow!("`start_block` not set"))?,
            channel: self
                .channel
                .ok_or_else(|| anyhow::anyhow!("`channel` not set"))?,
            finish_handle: FinishHandle::new(),
        })
    }

    fn start_block(mut self, start_block: u64) -> Self {
        self.start_block = Some(start_block);
        self
    }

    fn channel(mut self, channel: Sender<NewBlock>) -> Self {
        self.channel = Some(channel);
        self
    }
}

impl<S> BlockIngestor for PollingBlockIngestor<S> where S: AsRef<[u8]> + Send + 'static {}

impl<S> Daemon for PollingBlockIngestor<S>
where
    S: AsRef<[u8]> + Send + 'static,
{
    fn shutdown_handle(&self) -> ShutdownHandle {
        self.finish_handle.shutdown_handle()
    }

    fn start(self) {
        tokio::spawn(self.run());
    }
}

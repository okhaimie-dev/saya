use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use integrity::{split_proof, VerifierConfiguration};
use log::{debug, info};
use starknet::{
    accounts::{Account, ConnectedAccount, SingleOwnerAccount},
    core::{
        codec::{Decode, Encode},
        types::{BlockId, BlockTag, Call, FunctionCall, TransactionReceipt, U256},
    },
    macros::{selector, short_string},
    providers::{jsonrpc::HttpTransport, JsonRpcClient, Provider},
    signers::{LocalWallet, SigningKey},
};
use starknet_types_core::felt::Felt;
use tokio::sync::mpsc::{Receiver, Sender};
use url::Url;

use crate::{
    data_availability::DataAvailabilityCursor,
    prover::RecursiveProof,
    service::{Daemon, FinishHandle},
    settlement::{SettlementBackend, SettlementBackendBuilder, SettlementCursor},
    utils::{calculate_output, felt_to_bigdecimal, split_calls, watch_tx},
};

const POLLING_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct PiltoverSettlementBackend {
    provider: Arc<JsonRpcClient<HttpTransport>>,
    account: SingleOwnerAccount<Arc<JsonRpcClient<HttpTransport>>, LocalWallet>,
    integrity_address: Felt,
    piltover_address: Felt,
    da_channel: Receiver<DataAvailabilityCursor<RecursiveProof>>,
    cursor_channel: Sender<SettlementCursor>,
    finish_handle: FinishHandle,
    use_mock_layout_bridge: bool,
}

#[derive(Debug)]
pub struct PiltoverSettlementBackendBuilder {
    rpc_url: Url,
    integrity_address: Felt,
    piltover_address: Felt,
    account_address: Felt,
    account_private_key: Felt,
    da_channel: Option<Receiver<DataAvailabilityCursor<RecursiveProof>>>,
    cursor_channel: Option<Sender<SettlementCursor>>,
    use_mock_layout_bridge: bool,
}

#[derive(Debug, Decode)]
struct AppchainState {
    #[allow(unused)]
    state_root: Felt,
    block_number: u64,
    #[allow(unused)]
    block_hash: Felt,
}

#[derive(Debug, Encode)]
struct UpdateStateCalldata {
    snos_output: Vec<Felt>,
    program_output: Vec<Felt>,
    onchain_data_hash: Felt,
    onchain_data_size: U256,
}

impl PiltoverSettlementBackend {
    async fn get_state(&self) -> Result<AppchainState> {
        let raw_result = self
            .provider
            .call(
                FunctionCall {
                    contract_address: self.piltover_address,
                    entry_point_selector: selector!("get_state"),
                    calldata: vec![],
                },
                BlockId::Tag(BlockTag::Pending),
            )
            .await?;

        Ok(AppchainState::decode(&raw_result)?)
    }

    async fn run(mut self) {
        loop {
            let new_da = tokio::select! {
                _ = self.finish_handle.shutdown_requested() => break,
                new_da = self.da_channel.recv() => new_da,
            };

            // This should be fine for now as DA backends wouldn't drop senders. This might change
            // in the future.
            let new_da = new_da.unwrap();
            debug!("Received new DA cursor");

            if !self.use_mock_layout_bridge {
                // TODO: error handling
                let split_proof = split_proof::<
                    swiftness_air::layout::recursive_with_poseidon::Layout,
                >(new_da.full_payload.layout_bridge_proof.clone())
                .unwrap();
                let integrity_job_id = SigningKey::from_random().secret_scalar();
                let integrity_calls = split_proof
                    .into_calls(
                        integrity_job_id,
                        VerifierConfiguration {
                            layout: short_string!("recursive_with_poseidon"),
                            hasher: short_string!("keccak_160_lsb"),
                            stone_version: short_string!("stone6"),
                            memory_verification: short_string!("relaxed"),
                        },
                    )
                    .collect_calls(self.integrity_address);
                let integrity_call_chunks = split_calls(integrity_calls);
                debug!(
                    "{} transactions to integrity verifier generated (job id: {:#064x})",
                    integrity_call_chunks.len(),
                    integrity_job_id
                );

                // TODO: error handling
                let mut nonce = self.account.get_nonce().await.unwrap();
                let mut total_fee = Felt::ZERO;

                let proof_start = Instant::now();

                for (ind, chunk) in integrity_call_chunks.iter().enumerate() {
                    let tx = self
                        .account
                        .execute_v3(chunk.to_owned())
                        .nonce(nonce)
                        .send()
                        .await
                        .unwrap();
                    debug!(
                        "[{} / {}] Integrity verification transaction sent: {:#064x}",
                        ind + 1,
                        integrity_call_chunks.len(),
                        tx.transaction_hash
                    );

                    // TODO: error handling
                    let receipt = watch_tx(&self.provider, tx.transaction_hash, POLLING_INTERVAL)
                        .await
                        .unwrap();

                    let fee = match &receipt.receipt {
                        TransactionReceipt::Invoke(receipt) => &receipt.actual_fee,
                        TransactionReceipt::L1Handler(receipt) => &receipt.actual_fee,
                        TransactionReceipt::Declare(receipt) => &receipt.actual_fee,
                        TransactionReceipt::Deploy(receipt) => &receipt.actual_fee,
                        TransactionReceipt::DeployAccount(receipt) => &receipt.actual_fee,
                    };

                    debug!(
                        "[{} / {}] Integrity verification transaction confirmed: {:#064x}",
                        ind + 1,
                        integrity_call_chunks.len(),
                        tx.transaction_hash
                    );

                    nonce += Felt::ONE;
                    total_fee += fee.amount;
                }

                let proof_end = Instant::now();
                info!(
                "Proof successfully verified on integrity in {:.2} seconds. Total cost: {} STRK",
                proof_end.duration_since(proof_start).as_secs_f32(),
                felt_to_bigdecimal(total_fee, 18));
            }

            let program_output = if self.use_mock_layout_bridge {
                // The SNOS output hash is the only value required to be correct when layout bridge proof is mocked.
                // The fact registry will always return true, so the size of the program output only matter for SNOS output hash.
                // It is located at the index `4` in the `program_output` array.
                vec![
                    Felt::ZERO,
                    Felt::ZERO,
                    Felt::ZERO,
                    Felt::ZERO,
                    starknet_crypto::poseidon_hash_many(&new_da.full_payload.snos_output),
                ]
            } else {
                calculate_output(&new_da.full_payload.layout_bridge_proof)
            };

            let update_state_call = Call {
                to: self.piltover_address,
                selector: selector!("update_state"),
                calldata: {
                    let calldata = UpdateStateCalldata {
                        snos_output: new_da.full_payload.snos_output,
                        program_output,
                        onchain_data_hash: Felt::ZERO,
                        onchain_data_size: U256::from_words(0, 0),
                    };
                    let mut raw_calldata = vec![];

                    // Encoding `UpdateStateCalldata` never fails
                    calldata.encode(&mut raw_calldata).unwrap();

                    raw_calldata
                },
            };

            dbg!(&update_state_call);
            let execution = self.account.execute_v3(vec![update_state_call]);

            // TODO: error handling
            let fees = execution.estimate_fee().await.unwrap();
            debug!(
                "Estimated settlement transaction cost for block #{}: {} STRK",
                new_da.block_number,
                felt_to_bigdecimal(fees.overall_fee, 18)
            );

            // TODO: wait for transaction to confirm
            // TODO: error handling
            let transaction = execution.send().await.unwrap();
            info!(
                "Piltover statement transaction sent for block #{}: {:#064x}",
                new_da.block_number, transaction.transaction_hash
            );

            // TODO: timeout
            // TODO: error handling
            watch_tx(
                &self.provider,
                transaction.transaction_hash,
                POLLING_INTERVAL,
            )
            .await
            .unwrap();
            info!(
                "Piltover statement transaction block #{} confirmed: {:#064x}",
                new_da.block_number, transaction.transaction_hash
            );

            let new_cursor = SettlementCursor {
                block_number: new_da.block_number,
                transaction_hash: transaction.transaction_hash,
            };

            // Since the channel is bounded, it's possible
            tokio::select! {
                _ = self.finish_handle.shutdown_requested() => break,
                _ = self.cursor_channel.send(new_cursor) => {},
            }
        }

        debug!("Graceful shutdown finished");
        self.finish_handle.finish();
    }
}

impl PiltoverSettlementBackendBuilder {
    pub fn new(
        rpc_url: Url,
        integrity_address: Felt,
        piltover_address: Felt,
        account_address: Felt,
        account_private_key: Felt,
        use_mock_layout_bridge: bool,
    ) -> Self {
        Self {
            rpc_url,
            integrity_address,
            piltover_address,
            account_address,
            account_private_key,
            da_channel: None,
            cursor_channel: None,
            use_mock_layout_bridge,
        }
    }
}

impl SettlementBackendBuilder for PiltoverSettlementBackendBuilder {
    type Backend = PiltoverSettlementBackend;

    async fn build(self) -> Result<Self::Backend> {
        let provider = Arc::new(JsonRpcClient::new(HttpTransport::new(self.rpc_url)));
        let chain_id = provider.chain_id().await?;

        let mut account = SingleOwnerAccount::new(
            provider.clone(),
            LocalWallet::from_signing_key(SigningKey::from_secret_scalar(self.account_private_key)),
            self.account_address,
            chain_id,
            starknet::accounts::ExecutionEncoding::New,
        );
        account.set_block_id(BlockId::Tag(BlockTag::Pending));

        Ok(PiltoverSettlementBackend {
            provider,
            account,
            integrity_address: self.integrity_address,
            piltover_address: self.piltover_address,
            da_channel: self
                .da_channel
                .ok_or_else(|| anyhow::anyhow!("`da_channel` not set"))?,
            cursor_channel: self
                .cursor_channel
                .ok_or_else(|| anyhow::anyhow!("`cursor_channel` not set"))?,
            finish_handle: FinishHandle::new(),
            use_mock_layout_bridge: self.use_mock_layout_bridge,
        })
    }

    fn da_channel(mut self, da_channel: Receiver<DataAvailabilityCursor<RecursiveProof>>) -> Self {
        self.da_channel = Some(da_channel);
        self
    }

    fn cursor_channel(mut self, cursor_channel: Sender<SettlementCursor>) -> Self {
        self.cursor_channel = Some(cursor_channel);
        self
    }
}

impl SettlementBackend for PiltoverSettlementBackend {
    async fn get_block_number(&self) -> Result<u64> {
        let appchain_state = self.get_state().await?;
        Ok(appchain_state.block_number)
    }
}

impl Daemon for PiltoverSettlementBackend {
    fn shutdown_handle(&self) -> crate::service::ShutdownHandle {
        self.finish_handle.shutdown_handle()
    }

    fn start(self) {
        tokio::spawn(self.run());
    }
}

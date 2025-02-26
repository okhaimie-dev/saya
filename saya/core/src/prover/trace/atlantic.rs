use std::time::Duration;

use anyhow::Result;
use cairo_vm::vm::runners::cairo_pie::CairoPie;
use log::info;

use crate::{
    prover::atlantic::{AtlanticClient, AtlanticJobStatus},
    storage::PersistantStorage,
};

const PROOF_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(10);
const TRACE_GENERATION_JOB_NAME: &str = "TRACE_GENERATION";

#[derive(Debug, Clone)]
pub struct AtlanticTraceGenerator {
    pub atlantic_client: AtlanticClient,
}
impl AtlanticTraceGenerator {
    pub fn new(atlantic_client: AtlanticClient) -> Self {
        Self { atlantic_client }
    }
}

impl AtlanticTraceGenerator {
    pub async fn generate_trace(
        &self,
        program: Vec<u8>,
        block_number: u32,
        label: &str,
        input: Vec<u8>,
        db: impl PersistantStorage,
    ) -> Result<CairoPie> {
        let atlantic_query_id = match db
            .get_query_id(block_number, crate::storage::Query::BridgeTrace)
            .await
        {
            Ok(query_id) => query_id,
            Err(_) => {
                let atlantic_query_id = self
                    .atlantic_client
                    .submit_trace_generation(label, program, input)
                    .await?;

                crate::utils::retry_with_backoff(
                    || {
                        db.add_query_id(
                            block_number,
                            atlantic_query_id.clone(),
                            crate::storage::Query::BridgeTrace,
                        )
                    },
                    "add_query_id",
                    3,
                    Duration::from_secs(2),
                )
                .await?;

                atlantic_query_id
            }
        };
        info!(
            "Atlantic trace generation submitted with query id: {}",
            atlantic_query_id
        );

        loop {
            tokio::time::sleep(PROOF_STATUS_POLL_INTERVAL).await;

            // TODO: error handling
            if let Ok(jobs) = self
                .atlantic_client
                .get_query_jobs(&atlantic_query_id)
                .await
            {
                if let Some(proof_generation_job) = jobs
                    .iter()
                    .find(|job| job.job_name == TRACE_GENERATION_JOB_NAME)
                {
                    match proof_generation_job.status {
                        AtlanticJobStatus::Completed => break,
                        AtlanticJobStatus::Failed => {
                            // TODO: error handling
                            panic!("Atlantic proof generation {} failed", atlantic_query_id);
                        }
                        AtlanticJobStatus::InProgress => {}
                    }
                }
            }
        }
        let pie_bytes = self.atlantic_client.get_trace(&atlantic_query_id).await?;
        let pie = CairoPie::from_bytes(&pie_bytes)?;
        info!("Trace generated for query: {}", atlantic_query_id);
        Ok(pie)
    }
}

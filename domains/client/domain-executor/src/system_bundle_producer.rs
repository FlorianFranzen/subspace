use crate::bundle_election_solver::BundleElectionSolver;
use crate::utils::ExecutorSlotInfo;
use crate::{BundleSender, ExecutionReceiptFor};
use codec::{Decode, Encode};
use domain_runtime_primitives::{AccountId, DomainCoreApi};
use futures::{select, FutureExt};
use sc_client_api::{AuxStore, BlockBackend, ProofProvider};
use sc_transaction_pool_api::InPoolTransaction;
use sp_api::{NumberFor, ProvideRuntimeApi};
use sp_block_builder::BlockBuilder;
use sp_blockchain::HeaderBackend;
use sp_consensus_slots::Slot;
use sp_domains::{
    Bundle, BundleHeader, DomainId, ExecutorApi, ExecutorPublicKey, ExecutorSignature,
    SignedBundle, SignedOpaqueBundle,
};
use sp_keystore::{SyncCryptoStore, SyncCryptoStorePtr};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::{BlakeTwo256, Block as BlockT, Hash as HashT, Zero};
use sp_runtime::RuntimeAppPublic;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time;
use subspace_core_primitives::BlockNumber;
use system_runtime_primitives::SystemDomainApi;

const LOG_TARGET: &str = "bundle-producer";

pub(super) struct SystemBundleProducer<Block, PBlock, Client, PClient, TransactionPool>
where
    Block: BlockT,
    PBlock: BlockT,
{
    domain_id: DomainId,
    primary_chain_client: Arc<PClient>,
    client: Arc<Client>,
    transaction_pool: Arc<TransactionPool>,
    bundle_sender: Arc<BundleSender<Block, PBlock>>,
    is_authority: bool,
    keystore: SyncCryptoStorePtr,
    bundle_election_solver: BundleElectionSolver<Block, PBlock, Client>,
    _phantom_data: PhantomData<PBlock>,
}

impl<Block, PBlock, Client, PClient, TransactionPool> Clone
    for SystemBundleProducer<Block, PBlock, Client, PClient, TransactionPool>
where
    Block: BlockT,
    PBlock: BlockT,
{
    fn clone(&self) -> Self {
        Self {
            domain_id: self.domain_id,
            primary_chain_client: self.primary_chain_client.clone(),
            client: self.client.clone(),
            transaction_pool: self.transaction_pool.clone(),
            bundle_sender: self.bundle_sender.clone(),
            is_authority: self.is_authority,
            keystore: self.keystore.clone(),
            bundle_election_solver: self.bundle_election_solver.clone(),
            _phantom_data: self._phantom_data,
        }
    }
}

impl<Block, PBlock, Client, PClient, TransactionPool>
    SystemBundleProducer<Block, PBlock, Client, PClient, TransactionPool>
where
    Block: BlockT,
    PBlock: BlockT,
    Client: HeaderBackend<Block>
        + BlockBackend<Block>
        + AuxStore
        + ProvideRuntimeApi<Block>
        + ProofProvider<Block>,
    Client::Api: DomainCoreApi<Block, AccountId>
        + SystemDomainApi<Block, NumberFor<PBlock>, PBlock::Hash>
        + BlockBuilder<Block>,
    PClient: HeaderBackend<PBlock> + ProvideRuntimeApi<PBlock>,
    PClient::Api: ExecutorApi<PBlock, Block::Hash>,
    TransactionPool: sc_transaction_pool_api::TransactionPool<Block = Block>,
{
    pub(super) fn new(
        domain_id: DomainId,
        primary_chain_client: Arc<PClient>,
        client: Arc<Client>,
        transaction_pool: Arc<TransactionPool>,
        bundle_sender: Arc<BundleSender<Block, PBlock>>,
        is_authority: bool,
        keystore: SyncCryptoStorePtr,
    ) -> Self {
        let bundle_election_solver = BundleElectionSolver::new(client.clone(), keystore.clone());
        Self {
            domain_id,
            primary_chain_client,
            client,
            transaction_pool,
            bundle_sender,
            is_authority,
            keystore,
            bundle_election_solver,
            _phantom_data: PhantomData::default(),
        }
    }

    pub(super) async fn produce_bundle(
        self,
        primary_info: (PBlock::Hash, NumberFor<PBlock>),
        slot_info: ExecutorSlotInfo,
    ) -> Result<
        Option<SignedOpaqueBundle<NumberFor<PBlock>, PBlock::Hash, Block::Hash>>,
        sp_blockchain::Error,
    > {
        let ExecutorSlotInfo {
            slot,
            global_challenge,
        } = slot_info;

        let best_hash = self.client.info().best_hash;
        let best_number = self.client.info().best_number;

        if let Some(proof_of_election) = self
            .bundle_election_solver
            .solve_bundle_election_challenge(
                best_hash,
                best_number,
                self.domain_id,
                global_challenge,
            )?
        {
            tracing::info!(target: LOG_TARGET, "📦 Claimed bundle at slot {slot}");

            let bundle = self.propose_bundle_at(slot, primary_info).await?;

            let to_sign = bundle.hash();

            match SyncCryptoStore::sign_with(
                &*self.keystore,
                ExecutorPublicKey::ID,
                &proof_of_election.executor_public_key.clone().into(),
                to_sign.as_ref(),
            ) {
                Ok(Some(signature)) => {
                    let signed_bundle = SignedBundle {
                        bundle,
                        proof_of_election,
                        signature: ExecutorSignature::decode(&mut signature.as_slice()).map_err(
                            |err| {
                                sp_blockchain::Error::Application(Box::from(format!(
                                    "Failed to decode the signature of bundle: {err}"
                                )))
                            },
                        )?,
                    };

                    // TODO: Re-enable the bundle gossip over X-Net when the compact bundle is supported.
                    // if let Err(e) = self.bundle_sender.unbounded_send(signed_bundle.clone()) {
                    // tracing::error!(target: LOG_TARGET, error = ?e, "Failed to send transaction bundle");
                    // }

                    Ok(Some(signed_bundle.into_signed_opaque_bundle()))
                }
                Ok(None) => Err(sp_blockchain::Error::Application(Box::from(
                    "This should not happen as the existence of key was just checked",
                ))),
                Err(error) => Err(sp_blockchain::Error::Application(Box::from(format!(
                    "Error occurred when signing the bundle: {error}"
                )))),
            }
        } else {
            Ok(None)
        }
    }

    async fn propose_bundle_at(
        &self,
        slot: Slot,
        primary_info: (PBlock::Hash, NumberFor<PBlock>),
    ) -> sp_blockchain::Result<Bundle<Block::Extrinsic, NumberFor<PBlock>, PBlock::Hash, Block::Hash>>
    {
        let parent_number = self.client.info().best_number;

        let mut t1 = self.transaction_pool.ready_at(parent_number).fuse();
        // TODO: proper timeout
        let mut t2 = futures_timer::Delay::new(time::Duration::from_micros(100)).fuse();

        let pending_iterator = select! {
            res = t1 => res,
            _ = t2 => {
                tracing::warn!(
                    target: LOG_TARGET,
                    "Timeout fired waiting for transaction pool at #{}, proceeding with production.",
                    parent_number,
                );
                self.transaction_pool.ready()
            }
        };

        // TODO: proper deadline
        let pushing_duration = time::Duration::from_micros(500);

        let start = time::Instant::now();

        // TODO: Select transactions properly from the transaction pool
        //
        // Selection policy:
        // - minimize the transaction equivocation.
        // - maximize the executor computation power.
        let mut extrinsics = Vec::new();

        for pending_tx in pending_iterator {
            if start.elapsed() >= pushing_duration {
                break;
            }
            let pending_tx_data = pending_tx.data().clone();
            extrinsics.push(pending_tx_data);
        }

        let extrinsics_root = BlakeTwo256::ordered_trie_root(
            extrinsics.iter().map(|xt| xt.encode()).collect(),
            sp_core::storage::StateVersion::V1,
        );

        let (primary_hash, primary_number) = primary_info;

        let receipts = if primary_number.is_zero() {
            Vec::new()
        } else {
            self.collect_system_bundle_receipts(primary_hash, parent_number)?
        };

        let bundle = Bundle {
            header: BundleHeader {
                primary_hash,
                slot_number: slot.into(),
                extrinsics_root,
            },
            receipts,
            extrinsics,
        };

        Ok(bundle)
    }

    /// Returns the receipts in the next system domain bundle.
    fn collect_system_bundle_receipts(
        &self,
        primary_hash: PBlock::Hash,
        header_number: NumberFor<Block>,
    ) -> sp_blockchain::Result<Vec<ExecutionReceiptFor<PBlock, Block::Hash>>> {
        let best_execution_chain_number = self
            .primary_chain_client
            .runtime_api()
            .best_execution_chain_number(&BlockId::Hash(primary_hash))?;

        let best_execution_chain_number: BlockNumber = best_execution_chain_number
            .try_into()
            .unwrap_or_else(|_| panic!("Primary number must fit into u32; qed"));

        let load_receipt = |block_hash| {
            crate::aux_schema::load_execution_receipt::<
                _,
                Block::Hash,
                NumberFor<PBlock>,
                PBlock::Hash,
            >(&*self.client, block_hash)?
            .ok_or_else(|| {
                sp_blockchain::Error::Backend(format!("Receipt not found for {block_hash}"))
            })
        };

        let header_number: BlockNumber = header_number
            .try_into()
            .unwrap_or_else(|_| panic!("Secondary number must fit into u32; qed"));

        // Ideally, the receipt of current block will be included in the next block, i.e., no
        // missing receipts.
        let receipts = if header_number == best_execution_chain_number + 1 {
            let block_hash = self.client.hash(header_number.into())?.ok_or_else(|| {
                sp_blockchain::Error::Backend(format!(
                    "Hash for Block {:?} not found",
                    header_number
                ))
            })?;
            vec![load_receipt(block_hash)?]
        } else {
            // Receipts for some previous blocks are missing.
            let max_drift = self
                .primary_chain_client
                .runtime_api()
                .maximum_receipt_drift(&BlockId::Hash(primary_hash))?;

            let max_drift: BlockNumber = max_drift
                .try_into()
                .unwrap_or_else(|_| panic!("Primary number must fit into u32; qed"));

            let max_allowed = (best_execution_chain_number + max_drift).min(header_number);

            let mut to_send = best_execution_chain_number + 1;
            let mut receipts = Vec::with_capacity((max_allowed - to_send + 1) as usize);
            while to_send <= max_allowed {
                let block_hash = self.client.hash(to_send.into())?.ok_or_else(|| {
                    sp_blockchain::Error::Backend(format!("Hash for Block {:?} not found", to_send))
                })?;
                receipts.push(load_receipt(block_hash)?);
                to_send += 1;
            }
            receipts
        };

        Ok(receipts)
    }
}

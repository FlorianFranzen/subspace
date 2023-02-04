use domain_runtime_primitives::Hash;
use futures::StreamExt;
use parity_scale_codec::{Decode, Encode};
use parking_lot::Mutex;
use sc_client_api::{BlockBackend, BlockImportNotification, BlockchainEvents, HeaderBackend};
use sp_api::{HeaderT, ProvideRuntimeApi};
use sp_blockchain::HeaderMetadata;
use sp_domains::{Bundle, ExecutorApi};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::{Block as BlockT, NumberFor};
use sp_runtime::OpaqueExtrinsic;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

/// The `BlockBundles` containing the hashs of all the bundles within a block
#[derive(Clone)]
struct BlockBundles<Block: BlockT> {
    // The block that containing the bundles
    block_hash: Block::Hash,
    // The hashs of all the bundles within a block
    bundle_hashs: HashSet<Hash>,
}

/// `BundleCollector` collect all the bundle from the last K (confirm depth) blocks
/// of the best chain
pub struct BundleCollector<Block: BlockT, Client> {
    client: Arc<Client>,
    // `BundleSyncer` used to sync `bundle_stored_in_last_k` to other thread
    bundle_syncer: BundleSyncer<Block>,
    // The `BlockBundles` of the last K blocks, sorted from newer block to
    // older block
    bundle_stored_in_last_k: VecDeque<BlockBundles<Block>>,
    confirm_depth_k: usize,
}

impl<Block, Client> BundleCollector<Block, Client>
where
    Block: BlockT,
    Hash: Encode + Decode,
    Client: BlockchainEvents<Block>
        + HeaderBackend<Block>
        + BlockBackend<Block>
        + HeaderMetadata<Block, Error = sp_blockchain::Error>
        + ProvideRuntimeApi<Block>,
    Client::Api: ExecutorApi<Block, Hash>,
{
    pub fn new(client: Arc<Client>, confirm_depth_k: usize) -> Self {
        BundleCollector {
            client,
            bundle_syncer: BundleSyncer::new(),
            bundle_stored_in_last_k: VecDeque::new(),
            confirm_depth_k,
        }
    }

    pub fn get_syncer(&self) -> BundleSyncer<Block> {
        self.bundle_syncer.clone()
    }

    /// Get the hash of the current best block, return `None` if it have not process
    /// any block.
    fn best_hash(&self) -> Option<Block::Hash> {
        self.bundle_stored_in_last_k.front().map(|bb| bb.block_hash)
    }

    fn collect_bundle_from_block(
        &self,
        block_hash: Block::Hash,
    ) -> Result<BlockBundles<Block>, sp_blockchain::Error> {
        let extrinsics = self.client.block_body(block_hash)?.ok_or_else(|| {
            sp_blockchain::Error::Backend(format!("BlockBody of {block_hash:?} unavailable"))
        })?;
        let bundle_hashs: HashSet<_> = self
            .client
            .runtime_api()
            .extract_bundle_hashs(&BlockId::Hash(block_hash), extrinsics)?
            .into_iter()
            .collect();
        Ok(BlockBundles {
            block_hash,
            bundle_hashs,
        })
    }

    fn collect_bundle_from_last_k(
        &mut self,
        mut hash: Block::Hash,
    ) -> Result<(), sp_blockchain::Error> {
        assert!(self.bundle_stored_in_last_k.is_empty());
        let mut block_hashs = VecDeque::new();
        for _ in 0..self.confirm_depth_k {
            // Push new hash to the back end because visting from newer block to older block
            block_hashs.push_back(hash);
            match self.client.header(hash)? {
                Some(header) => hash = *header.parent_hash(),
                // `None` means the chain currently don't have `confirm_depth_k` number of block
                None => break,
            }
        }
        for h in block_hashs {
            let block_bundles = self.collect_bundle_from_block(h)?;
            self.bundle_stored_in_last_k.push_front(block_bundles);
        }
        Ok(())
    }

    /// Collect bundles within the new blocks of the best chain, blocks are handled from
    /// older blcok to newer blcok, an `Err` may return in the middle and left some blocks
    /// unhandled, these blocks will be handled when processing the next new best block.
    fn on_block_import(
        &mut self,
        incoming_block: BlockImportNotification<Block>,
    ) -> Result<(), sp_blockchain::Error> {
        let incoming_best_hash = {
            if !incoming_block.is_new_best {
                return Ok(());
            }
            incoming_block.hash
        };
        let current_best_hash = match self.best_hash() {
            Some(h) => h,
            // Collect bundle from the last K blocks if it have not process any block
            None => {
                self.collect_bundle_from_last_k(incoming_best_hash)?;
                return Ok(());
            }
        };

        let route =
            sp_blockchain::tree_route(&*self.client, current_best_hash, incoming_best_hash)?;
        let (retracted, enacted) = (route.retracted(), route.enacted());

        // Remove bundles from the stale fork
        for retracted_block in retracted {
            match self.bundle_stored_in_last_k.front() {
                Some(bb) if bb.block_hash == retracted_block.hash => {
                    self.bundle_stored_in_last_k.pop_front();
                }
                bb => {
                    return Err(sp_blockchain::Error::Application(Box::from(
                        format!(
                            "Got wrong block from the bundle-collector, expect {:?}, got {:?}, this should not happen",
                            retracted_block,
                            bb.map(|bb| bb.block_hash),
                        ),
                    )));
                }
            }
        }

        // Add bundles from the new block of the best fork
        for enacted_block in enacted {
            let block_bundles = self.collect_bundle_from_block(enacted_block.hash)?;
            self.bundle_stored_in_last_k.push_front(block_bundles);
        }

        // Remove blocks from the back end to keep at most the bundle of the last K blocks
        self.bundle_stored_in_last_k.truncate(self.confirm_depth_k);

        Ok(())
    }

    pub async fn run(mut self) {
        let mut domain_block_import = self.client.import_notification_stream();
        while let Some(incoming_block) = domain_block_import.next().await {
            let pre_best_hash = self.best_hash();
            if let Err(err) = self.on_block_import(incoming_block) {
                tracing::error!(
                    %err,
                    "Failed to handle import block for bundle-collector"
                );
            }
            if pre_best_hash != self.best_hash() {
                self.bundle_syncer
                    .update(self.bundle_stored_in_last_k.clone());
            }
        }
    }
}

/// `BundleSyncer` used to share the bundles of the last K blocks with different thread
#[derive(Clone)]
pub struct BundleSyncer<Block: BlockT> {
    bundle_stored_in_last_k: Arc<Mutex<VecDeque<BlockBundles<Block>>>>,
}

impl<Block: BlockT> BundleSyncer<Block> {
    fn new() -> Self {
        BundleSyncer {
            bundle_stored_in_last_k: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn update(&self, incoming: VecDeque<BlockBundles<Block>>) {
        *self.bundle_stored_in_last_k.lock() = incoming;
    }
}

pub trait CheckBundle<Block: BlockT, DomainHash> {
    fn check_duplicate_bundle(
        &self,
        bundle: &Bundle<OpaqueExtrinsic, NumberFor<Block>, Block::Hash, DomainHash>,
    ) -> Result<(), sp_blockchain::Error>;
}

impl<Block: BlockT, DomainHash> CheckBundle<Block, DomainHash> for () {
    fn check_duplicate_bundle(
        &self,
        _bundle: &Bundle<OpaqueExtrinsic, NumberFor<Block>, Block::Hash, DomainHash>,
    ) -> Result<(), sp_blockchain::Error> {
        Ok(())
    }
}

impl<Block: BlockT, DomainHash> CheckBundle<Block, DomainHash> for BundleSyncer<Block> {
    // This implement will never return false negative result (i.e return `Err` for a new bundle)
    // but it may return false positive result (i.e return `Ok` for a duplicated bundle) if
    // `BundleHandler::on_block_import` return error and left some blocks unhandled, and it
    // will be recovered after successfully handling the next best block.
    fn check_duplicate_bundle(
        &self,
        bundle: &Bundle<OpaqueExtrinsic, NumberFor<Block>, Block::Hash, DomainHash>,
    ) -> Result<(), sp_blockchain::Error> {
        let incoming_bundle = bundle.hash();
        let block_bundles = self.bundle_stored_in_last_k.lock();
        for bb in block_bundles.iter() {
            if bb.bundle_hashs.contains(&incoming_bundle) {
                return Err(sp_blockchain::Error::Application(Box::from(format!(
                    "Duplicated bundle {incoming_bundle}, bundle already included in block {}",
                    bb.block_hash
                ))));
            }
        }
        Ok(())
    }
}

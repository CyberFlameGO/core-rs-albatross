use std::collections::HashMap;
use std::collections::btree_map::BTreeMap;
use std::sync::{Arc, Weak};
use std::fmt;

use failure::Fail;
use parking_lot::{RwLock, RwLockUpgradableReadGuard};

use block_albatross::{
    BlockHeader,
    ForkProof, PbftProof, PbftProposal,
    PbftPrepareMessage, PbftCommitMessage,
    SignedPbftCommitMessage, SignedPbftPrepareMessage, SignedPbftProposal,
    SignedViewChange, ViewChange, ViewChangeProof
};
use block_albatross::signed::AggregateProof;
use blockchain_albatross::Blockchain;
use bls::bls12_381::CompressedPublicKey;
use collections::grouped_list::Group;
use hash::{Blake2bHash, Hash};
use messages::Message;
use network::{Network, NetworkEvent, Peer};
use network_primitives::validator_info::{SignedValidatorInfo};
use network_primitives::address::PeerId;
use primitives::policy::{SLOTS, TWO_THIRD_SLOTS, is_macro_block_at};
use primitives::validators::{Validators, IndexedSlot};
use utils::mutable_once::MutableOnce;
use utils::observer::{PassThroughNotifier, weak_listener, weak_passthru_listener};
use handel::aggregation::AggregationEvent;
use handel::update::LevelUpdateMessage;

use crate::validator_agent::{ValidatorAgent, ValidatorAgentEvent};
use crate::signature_aggregation::view_change::ViewChangeAggregation;
use crate::signature_aggregation::pbft::PbftAggregation;


#[derive(Clone, Debug, Fail)]
pub enum ValidatorNetworkError {
    #[fail(display = "View change already in progress: {:?}", _0)]
    ViewChangeAlreadyExists(ViewChange),

    #[fail(display = "Already got another pBFT proposal at this view")]
    ProposalCollision,
    #[fail(display = "Unknown pBFT proposal")]
    UnknownProposal,
    #[fail(display = "Invalid pBFT proposal")]
    InvalidProposal,
}

#[derive(Clone, Debug)]
pub enum ValidatorNetworkEvent {
    /// When a fork proof was given
    ForkProof(ForkProof),

    /// When a valid view change was completed
    ViewChangeComplete(ViewChange, ViewChangeProof),

    /// When a valid macro block is proposed by the correct pBFT-leader. This can happen multiple
    /// times during an epoch - i.e. when a proposal with a higher view number is received.
    PbftProposal(Blake2bHash, PbftProposal),

    /// When enough prepare signatures are collected for a proposed macro block
    PbftPrepareComplete(Blake2bHash, PbftProposal),

    /// When the pBFT proof is complete
    PbftComplete(Blake2bHash, PbftProposal, PbftProof),
}


/// State of current pBFT phase
#[derive(Clone)]
struct PbftState {
    /// The proposed macro block with justification
    proposal: SignedPbftProposal,

    /// The hash of the header of the proposed macro block
    block_hash: Blake2bHash,

    /// The state of the signature aggregation for pBFT prepare and commit
    aggregation: Arc<RwLock<PbftAggregation>>,

    /// The pBFT prepare proof, once it's complete
    prepare_proof: Option<AggregateProof<PbftPrepareMessage>>,
}

impl PbftState {
    pub fn new(block_hash: Blake2bHash, proposal: SignedPbftProposal, node_id: usize, validators: Validators, peers: Arc<HashMap<usize, Arc<ValidatorAgent>>>) -> Self {
        let aggregation = Arc::new(RwLock::new(PbftAggregation::new(block_hash.clone(), node_id, validators, peers, None)));
        Self {
            proposal,
            block_hash,
            aggregation,
            prepare_proof: None,
        }
    }

    // Only works on non-buffered proposals
    fn check_verified(&self, chain: &Blockchain) -> bool {
        let block_number = self.proposal.message.header.block_number;
        let view_number = self.proposal.message.header.view_number;

        // Can we verify validity of macro block?
        let IndexedSlot { slot, .. } = chain.get_block_producer_at(block_number, view_number, None)
            .expect("check_verified() called without enough micro blocks");

        // Check the signer index
        if let Some(ref validator) = chain.get_current_validator_by_idx(self.proposal.signer_idx) {
            let Group(_, validator_key) = validator;
            // Does the key own the current slot?
            if validator_key != &slot.public_key {
                return false;
            }
        } else {
            // No validator at this index
            return false;
        }

        // Check the validity of the block
        // TODO: We check the view change proof the second time here if previously buffered
        if let Err(e) = chain.verify_block_header(
            &BlockHeader::Macro(self.proposal.message.header.clone()),
            self.proposal.message.view_change.as_ref().into(),
            &slot.public_key.uncompress().unwrap(),
            None // TODO Would it make sense to pass a Read transaction?
        ) {
            debug!("[PBFT-PROPOSAL] Invalid macro block header: {:?}", e);
            return false;
        }

        // Check the signature of the proposal
        let public_key = &slot.public_key.uncompress_unchecked();
        if !self.proposal.verify(&public_key) {
            debug!("[PBFT-PROPOSAL] Invalid signature");
            return false;
        }

        true
    }
}

impl fmt::Debug for PbftState {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let (prepare_votes, commit_votes) = self.aggregation.read().votes();
        write!(f, "PbftState {{ proposal: {}, prepare: {}, commit: {}", self.block_hash, prepare_votes, commit_votes)
    }
}

#[derive(Default)]
struct ValidatorNetworkState {
    /// The peers that are connected that have the validator service flag set. So this is not
    /// exactly the set of validators. Potential validators should set this flag and then broadcast
    /// a `ValidatorInfo`.
    ///
    /// NOTE: The mapping from the `PeerId` is needed to efficiently remove agents from this set.
    /// NOTE: This becomes obsolete once we can actively connect to validators
    agents: HashMap<PeerId, Arc<ValidatorAgent>>,

    /// Peers for which we received a `ValidatorInfo` and thus have a BLS public key.
    potential_validators: BTreeMap<CompressedPublicKey, Arc<ValidatorAgent>>,

    /// Subset of validators that only includes validators that are active in the current epoch.
    /// NOTE: This is Arc'd, such that we can pass it to Handel without cloning.
    active_validators: Arc<HashMap<usize, Arc<ValidatorAgent>>>,

    /// Maps (view-change-number, block-number) to the proof that is being aggregated
    /// and a flag whether it's finalized. clear after macro block
    view_changes: HashMap<ViewChange, ViewChangeAggregation>,

    /// If we're in pBFT phase, this is the current state of it
    pbft_states: Vec<PbftState>,

    /// If we're an active validator, set our validator ID here
    validator_id: Option<usize>,
}

impl ValidatorNetworkState {
    pub(crate) fn get_pbft_state(&self, hash: &Blake2bHash) -> Option<&PbftState> {
        self.pbft_states.iter().find(|state| &state.block_hash == hash)
    }

    pub(crate) fn get_pbft_state_mut(&mut self, hash: &Blake2bHash) -> Option<&mut PbftState> {
        self.pbft_states.iter_mut().find(|state| &state.block_hash == hash)
    }
}

pub struct ValidatorNetwork {
    blockchain: Arc<Blockchain<'static>>,

    /// The signed validator info for this node
    info: SignedValidatorInfo,

    /// The validator network state
    state: RwLock<ValidatorNetworkState>,

    self_weak: MutableOnce<Weak<ValidatorNetwork>>,
    pub notifier: RwLock<PassThroughNotifier<'static, ValidatorNetworkEvent>>,
}

impl ValidatorNetwork {
    const MAX_VALIDATOR_INFOS: usize = 64;

    pub fn new(network: Arc<Network<Blockchain<'static>>>, blockchain: Arc<Blockchain<'static>>, info: SignedValidatorInfo) -> Arc<Self> {
        let this = Arc::new(ValidatorNetwork {
            blockchain,
            info,
            state: RwLock::new(ValidatorNetworkState::default()),
            self_weak: MutableOnce::new(Weak::new()),
            notifier: RwLock::new(PassThroughNotifier::new()),
        });

        Self::init_listeners(&this, network);

        this
    }

    fn init_listeners(this: &Arc<Self>, network: Arc<Network<Blockchain<'static>>>) {
        unsafe { this.self_weak.replace(Arc::downgrade(this)) };

        // Register for peers joining and leaving
        network.notifier.write().register(weak_listener(Arc::downgrade(this), |this, event| {
            match event {
                NetworkEvent::PeerJoined(peer) => this.on_peer_joined(&peer),
                NetworkEvent::PeerLeft(peer) => this.on_peer_left(&peer),
                _ => {}
            }
        }));
    }

    fn on_peer_joined(&self, peer: &Arc<Peer>) {
        if peer.peer_address().services.is_validator() {
            let agent = ValidatorAgent::new(Arc::clone(peer), Arc::clone(&self.blockchain));

            // Insert into set of all agents that have the validator service flag
            self.state.write().agents.insert(agent.peer_id(), Arc::clone(&agent));

            // Register for messages received by agent
            agent.notifier.write().register(weak_passthru_listener(Weak::clone(&self.self_weak), |this, event| {
                match event {
                    ValidatorAgentEvent::ValidatorInfo(info) => {
                        this.on_validator_info(info);
                    },
                    ValidatorAgentEvent::ForkProof(fork_proof) => {
                        this.on_fork_proof(fork_proof);
                    }
                    ValidatorAgentEvent::ViewChange(update_message) => {
                        this.on_view_change_level_update(update_message);
                    },
                    ValidatorAgentEvent::PbftProposal(proposal) => {
                        this.on_pbft_proposal(proposal)
                            .unwrap_or_else(|e| debug!("Rejecting pBFT proposal: {}", e));
                    },
                    ValidatorAgentEvent::PbftPrepare(level_update) => {
                        this.on_pbft_prepare_level_update(level_update);
                    },
                    ValidatorAgentEvent::PbftCommit(level_update) => {
                        this.on_pbft_commit_level_update(level_update);
                    },
                }
            }));

            // Send known validator infos to peer
            let mut infos = self.state.read().agents.iter()
                .filter_map(|(_, agent)| {
                    agent.state.read().validator_info.clone()
                })
                .take(Self::MAX_VALIDATOR_INFOS) // limit the number of validator infos
                .collect::<Vec<SignedValidatorInfo>>();
            infos.push(self.info.clone()); // add our infos
            peer.channel.send_or_close(Message::ValidatorInfo(infos));
        }
    }

    fn on_peer_left(&self, peer: &Arc<Peer>) {
        let mut state = self.state.write();

        if let Some(agent) = state.agents.remove(&peer.peer_address().peer_id) {
            info!("Validator left: {}", agent.peer_id());
        }
    }

    /// NOTE: assumes that the signature of the validator info was checked
    fn on_validator_info(&self, info: SignedValidatorInfo) {
        let mut state = self.state.write();

        trace!("Validator info: {:?}", info.message);

        if let Some(agent) = state.agents.get(&info.message.peer_address.peer_id) {
            let agent = Arc::clone(&agent);
            let agent_state = agent.state.upgradable_read();

            if let Some(current_info) = &agent_state.validator_info {
                if current_info.message.public_key == info.message.public_key {
                    // Didn't change, do nothing
                    return;
                }
            }

            // Insert into potential validators, indexed by compressed public key
            state.potential_validators.insert(info.message.public_key.clone(), Arc::clone(&agent));

            // Check if active validator and put into `active` list
            // TODO: Use a HashMap to map PublicKeys to validator ID
            /*for (id, Group(_, public_key)) in self.blockchain.current_validators().groups().iter().enumerate() {
                if *public_key.compressed() == info.message.public_key {
                    trace!("Validator is active");
                    state.active_validators.insert(id, agent);
                    break;
                }
            }*/

            // Put validator info into agent
            RwLockUpgradableReadGuard::upgrade(agent_state).validator_info = Some(info);
        }
        else {
            warn!("ValidatorInfo for unknown peer: {:?}", info);
        }
    }

    fn on_fork_proof(&self, fork_proof: ForkProof) {
        self.notifier.read().notify(ValidatorNetworkEvent::ForkProof(fork_proof.clone()));
        self.broadcast_fork_proof(fork_proof);
    }

    /// Called when we reach finality - i.e. when a macro block was produced. This must be called be the
    /// validator.
    ///
    /// `validator_id`: The index of the validator (a.k.a `pk_idx`), if we're active
    pub fn on_finality(&self, validator_id: Option<usize>) {
        trace!("Clearing view change and pBFT proof");
        let mut state = self.state.write();

        // Clear view changes
        state.view_changes.clear();

        // Clear pBFT states
        state.pbft_states.clear();

        // Set validator ID
        state.validator_id = validator_id;

        // Create mapping from validator ID to agent/peer
        let validators = self.blockchain.current_validators();
        let mut active_validators = HashMap::new();
        for (id, Group(_, public_key)) in validators.iter_groups().enumerate() {
            if let Some(agent) = state.potential_validators.get(public_key.compressed()) {
                trace!("Validator {}: {}", id, agent.validator_info().unwrap().peer_address.as_uri());
                active_validators.insert(id, Arc::clone(agent));
            }
            else {
                error!("Unreachable validator: {}: {:?}", id, public_key);
            }
        }
        state.active_validators = Arc::new(active_validators);
    }

    /// Called when a new block is added
    pub fn on_blockchain_extended(&self) {
        // Check if next block will be a macro block
        let new_height = self.blockchain.block_number();
        if !is_macro_block_at(new_height + 1) {
            return;
        }

        // The rest of this function switches the state from buffered to complete.
        // Only the proposal with the highest view number will remain.
        let mut state = self.state.write();

        // Remove invalid proposals
        state.pbft_states.retain(|pbft| {
            let verified = pbft.check_verified(&self.blockchain);
            if !verified {
                debug!("Buffered pBFT proposal confirmed invalid: {}", &pbft.block_hash);
            } else {
                debug!("Verified pBFT proposal: {}", pbft.block_hash);
            }
            verified
        });

        if state.pbft_states.is_empty() {
            return;
        }

        // Remove proposals that collide on the same views
        state.pbft_states.dedup_by_key(|pbft| {
            let header = &pbft.proposal.message.header;
            (header.block_number, header.view_number)
        });

        // Choose proposal with the highest view number
        let best_pbft = state.pbft_states.iter()
            .max_by_key(|pbft| pbft.proposal.message.header.view_number)
            .unwrap().clone();
        state.pbft_states = vec![best_pbft.clone()];

        // We need to drop the state before notifying and relaying
        drop(state);

        // Notify Validator (and send prepare message)
        let block_hash = best_pbft.proposal.message.header.hash::<Blake2bHash>();
        self.notifier.read().notify(ValidatorNetworkEvent::PbftProposal(block_hash, best_pbft.proposal.message));
    }

    /// Pushes the update to the signature aggregation for this view-change
    fn on_view_change_level_update(&self, update_message: LevelUpdateMessage<ViewChange>) {
        let state = self.state.read();
        if let Some(aggregation) = state.view_changes.get(&update_message.tag) {
            aggregation.push_update(update_message);
            debug!("View change: {}", fmt_vote_progress(aggregation.votes()));
        }
    }

    /// Start pBFT with the given proposal.
    /// Either we generated that proposal, or we received it
    /// Proposal yet to be verified
    pub fn on_pbft_proposal(&self, signed_proposal: SignedPbftProposal) -> Result<(), ValidatorNetworkError> {
        let mut state = self.state.write();
        let block_hash = signed_proposal.message.header.hash::<Blake2bHash>();

        if state.get_pbft_state(&block_hash).is_some() {
            // Proposal already known, ignore
            trace!("Ignoring known pBFT proposal: {}", &block_hash);
            return Ok(());
        }

        let active_validators = Arc::clone(&state.active_validators);
        let validator_id = state.validator_id.expect("Not an active validator");

        debug!("pBFT proposal by validator {}: {:#?}", validator_id, signed_proposal);

        let pbft = PbftState::new(
            block_hash.clone(),
            signed_proposal.clone(),
            validator_id,
            self.blockchain.current_validators().clone(),
            active_validators,
        );

        let chain_height = self.blockchain.height();
        let buffered = !is_macro_block_at(chain_height + 1);

        // Check validity if proposal not buffered
        if !buffered {
            let verified = pbft.check_verified(&self.blockchain);
            if !verified {
                return Err(ValidatorNetworkError::InvalidProposal);
            }

            // Check if another proposal has same or greater view number
            let header = &pbft.proposal.message.header;
            let other_header = state.pbft_states.iter()
                .map(|pbft| &pbft.proposal.message.header)
                .find(|other_header| header.view_number <= other_header.view_number);
            if let Some(other_header) = other_header {
                if header.view_number == other_header.view_number {
                    return Err(ValidatorNetworkError::ProposalCollision);
                } else {
                    return Ok(());
                }
            }
        }

        // The prepare handler. This will store the finished prepare proof in the pBFT state
        let key = block_hash.clone();
        pbft.aggregation.read().prepare_aggregation.notifier.write()
            .register(weak_passthru_listener(Weak::clone(&self.self_weak), move |this, event| {
                match event {
                    AggregationEvent::Complete { best } => {
                        let event = if let Some(pbft) = this.state.write().get_pbft_state_mut(&key) {
                            if pbft.prepare_proof.is_none() {
                                // Build prepare proof
                                let prepare_proof = AggregateProof::new(best.signature, best.signers);
                                trace!("Prepare complete: {:?}", prepare_proof);
                                pbft.prepare_proof = Some(prepare_proof);

                                // Return the event
                                Some(ValidatorNetworkEvent::PbftPrepareComplete(pbft.block_hash.clone(), pbft.proposal.message.clone()))
                            } else {
                                warn!("Prepare proof already exists");
                                None
                            }
                        } else {
                            error!("No pBFT state");
                            None
                        };
                        // If we generated a prepare complete event, notify the validator
                        event.map(move |event| this.notifier.read().notify(event));
                    }
                }
            }));

        // The commit handler. This will store the finished commit proof and construct the
        // pBFT proof.
        let key = block_hash.clone();
        pbft.aggregation.read().commit_aggregation.notifier.write()
            .register(weak_passthru_listener(Weak::clone(&self.self_weak), move |this, event| {
                match event {
                    AggregationEvent::Complete { best } => {
                        let event = if let Some(pbft) = this.state.write().get_pbft_state_mut(&key) {
                            // Build commit proof
                            let commit_proof = AggregateProof::new(best.signature, best.signers);
                            trace!("Commit complete: {:?}", commit_proof);

                            // NOTE: The commit evaluator will only mark the signature as final, if there are enough commit signatures from validators that also
                            //       signed prepare messages. Thus a complete prepare proof must exist at this point.
                            // NOTE: Either `take()` it, or `clone()` it. Really doesn't matter I guess
                            let prepare_proof = pbft.prepare_proof.take().expect("No pBFT prepare proof");
                            let pbft_proof = PbftProof { prepare: prepare_proof, commit: commit_proof };

                            // Return the event
                            Some(ValidatorNetworkEvent::PbftComplete(pbft.block_hash.clone(), pbft.proposal.message.clone(), pbft_proof))
                        } else {
                            error!("No pBFT state");
                            None
                        };
                        // If we generated a prepare complete event, notify the validatir
                        event.map(move |event| this.notifier.read().notify(event));
                    }
                }
            }));

        if !buffered {
            // Replace pBFT state
            state.pbft_states = vec![pbft];
        } else {
            // Add pBFT state
            state.pbft_states.push(pbft);
        }

        // We need to drop the state before notifying and relaying
        drop(state);

        // Notify Validator (and send prepare message)
        if !buffered {
            self.notifier.read().notify(ValidatorNetworkEvent::PbftProposal(block_hash.clone(), signed_proposal.message.clone()));
        }

        // Broadcast to other validators
        self.broadcast_pbft_proposal(signed_proposal);

        Ok(())
    }

    pub fn on_pbft_prepare_level_update(&self, level_update: LevelUpdateMessage<PbftPrepareMessage>) {
        let state = self.state.read();

        if let Some(pbft) = state.get_pbft_state(&level_update.tag.block_hash) {
            let aggregation = Arc::clone(&pbft.aggregation);
            let aggregation = aggregation.read();
            drop(state);
            aggregation.push_prepare_level_update(level_update);
            let (prepare_votes, commit_votes) = aggregation.votes();
            debug!("pBFT: Prepare: {}, Commit: {}", fmt_vote_progress(prepare_votes), fmt_vote_progress(commit_votes));
        }
    }

    pub fn on_pbft_commit_level_update(&self, level_update: LevelUpdateMessage<PbftCommitMessage>) {
        // TODO: This is almost identical to the prepare one, maybe we can make the method generic over it?
        let state = self.state.read();

        if let Some(pbft) = state.get_pbft_state(&level_update.tag.block_hash) {
            let aggregation = Arc::clone(&pbft.aggregation);
            let aggregation = aggregation.read();
            drop(state);
            aggregation.push_commit_level_update(level_update);
            let (prepare_votes, commit_votes) = aggregation.votes();
            debug!("pBFT: Prepare: {}, Commit: {}", fmt_vote_progress(prepare_votes), fmt_vote_progress(commit_votes));
        }
    }

    // Public interface: start view changes, pBFT phase, push contributions

    /// Starts a new view-change
    pub fn start_view_change(&self, signed_view_change: SignedViewChange) -> Result<(), ValidatorNetworkError> {
        let view_change = signed_view_change.message.clone();
        let mut state = self.state.write();

        if let Some(aggregation) = state.view_changes.get(&view_change) {
            // Do nothing, but return an error. At some point the validator should increase the view number
            warn!("{:?} already exists with {} votes", signed_view_change.message, aggregation.votes());
            Err(ValidatorNetworkError::ViewChangeAlreadyExists(view_change))
        }
        else {
            let validators = self.blockchain.current_validators().clone();

            let node_id = state.validator_id.expect("Validator ID not set");
            assert_eq!(signed_view_change.signer_idx as usize, node_id);

            // Create view change aggregation
            let aggregation = ViewChangeAggregation::new(
                view_change.clone(),
                node_id,
                validators,
                Arc::clone(&state.active_validators),
                None
            );

            // Register handler for when done and start (or use Future)
            {
                let view_change = view_change.clone();
                aggregation.inner.notifier.write().register(weak_passthru_listener(Weak::clone(&self.self_weak), move |this, event| {
                    match event {
                        AggregationEvent::Complete { best } => {
                            info!("Complete: {:?}", view_change);
                            let proof = ViewChangeProof::new(best.signature, best.signers);
                            this.notifier.read()
                                .notify(ValidatorNetworkEvent::ViewChangeComplete(view_change.clone(), proof))
                        }
                    }
                }));
            }

            // Push our contribution
            aggregation.push_contribution(signed_view_change);

            state.view_changes.insert(view_change, aggregation);

            Ok(())
        }
    }

    /// Start pBFT phase with our proposal
    pub fn start_pbft(&self, signed_proposal: SignedPbftProposal) -> Result<(), ValidatorNetworkError> {
        //info!("Starting pBFT with proposal: {:?}", signed_proposal.message);
        self.on_pbft_proposal(signed_proposal)
    }

    pub fn push_prepare(&self, signed_prepare: SignedPbftPrepareMessage) -> Result<(), ValidatorNetworkError> {
        let state = self.state.read();
        if let Some(pbft) = state.get_pbft_state(&signed_prepare.message.block_hash) {
            let aggregation = Arc::clone(&pbft.aggregation);
            let aggregation = aggregation.read();
            drop(state);
            aggregation.push_signed_prepare(signed_prepare);
            Ok(())
        }
        else {
            Err(ValidatorNetworkError::UnknownProposal)
        }
    }

    pub fn push_commit(&self, signed_commit: SignedPbftCommitMessage) -> Result<(), ValidatorNetworkError> {
        let state = self.state.read();
        if let Some(pbft) = state.get_pbft_state(&signed_commit.message.block_hash) {
            let aggregation = Arc::clone(&pbft.aggregation);
            let aggregation = aggregation.read();
            drop(state);
            aggregation.push_signed_commit(signed_commit);
            Ok(())
        }
        else {
            Err(ValidatorNetworkError::UnknownProposal)
        }
    }

    // Legacy broadcast methods -------------------------
    //
    // These are still used to relay `ValidatorInfo` and `PbftProposal`

    /// Broadcast to all known active validators
    fn broadcast_active(&self, msg: Message) {
        // FIXME: Active validators don't actively connect to other active validators right now.
        /*trace!("Broadcast to active validators: {:#?}", msg);
        for (_, agent) in self.state.read().active.iter() {
            agent.read().peer.channel.send_or_close(msg.clone())
        }*/
        self.broadcast_all(msg);
    }

    /// Broadcast to all known validators
    fn broadcast_all(&self, msg: Message) {
        trace!("Broadcast to all validators: {}", msg.ty());
        for (_, agent) in self.state.read().potential_validators.iter() {
            trace!("Sending to {}", agent.peer.peer_address());
            agent.peer.channel.send_or_close(msg.clone());
        }
    }

    /// Broadcast pBFT proposal
    fn broadcast_pbft_proposal(&self, proposal: SignedPbftProposal) {
        self.broadcast_active(Message::PbftProposal(Box::new(proposal)));
    }

    /// Broadcast fork-proof
    fn broadcast_fork_proof(&self, fork_proof: ForkProof) {
        self.broadcast_active(Message::ForkProof(Box::new(fork_proof)));
    }
}

/// Pretty-print voting progress
fn fmt_vote_progress(slots: usize) -> String {
    let done = slots >= (TWO_THIRD_SLOTS as usize);
    format!("votes={: >3} / {}, done={}", slots, SLOTS, done)
}

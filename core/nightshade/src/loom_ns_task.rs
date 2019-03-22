use crate::nightshade::{BlockProposal, Nightshade, State};
use log::*;
use primitives::aggregate_signature::BlsPublicKey;
use primitives::hash::{hash_struct, CryptoHash};
use primitives::signature::{verify, PublicKey, Signature};
use primitives::signer::BlockSigner;
use primitives::types::{AuthorityId, BlockIndex};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use loom::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct Message {
    pub sender_id: AuthorityId,
    pub receiver_id: AuthorityId,
    pub state: State,
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
pub enum GossipBody {
    /// Use box because large size difference between variants
    NightshadeStateUpdate(Box<Message>),
    PayloadRequest(Vec<AuthorityId>),
    PayloadReply(Vec<SignedBlockProposal>),
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct Gossip {
    pub sender_id: AuthorityId,
    pub receiver_id: AuthorityId,
    pub body: GossipBody,
    pub block_index: BlockIndex,
    pub signature: Signature,
}

impl Gossip {
    fn new(
        sender_id: AuthorityId,
        receiver_id: AuthorityId,
        body: GossipBody,
        signer: Arc<BlockSigner>,
        block_index: u64,
    ) -> Self {
        let hash = hash_struct(&(sender_id, receiver_id, &body, block_index));

        Self { sender_id, receiver_id, body, signature: signer.sign(hash.as_ref()), block_index }
    }

    fn get_hash(&self) -> CryptoHash {
        hash_struct(&(self.sender_id, self.receiver_id, &self.body, self.block_index))
    }

    fn verify(&self, pk: &PublicKey) -> bool {
        verify(self.get_hash().as_ref(), &self.signature, &pk)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedBlockProposal {
    block_proposal: BlockProposal,
    signature: Signature,
}

impl SignedBlockProposal {
    fn new(author: AuthorityId, hash: CryptoHash, signer: Arc<BlockSigner>) -> Self {
        let block_proposal = BlockProposal { author, hash };
        let signature = signer.sign(block_proposal.hash.as_ref());

        Self { block_proposal, signature }
    }

    fn verify(&self, public_key: &PublicKey) -> bool {
        verify(self.block_proposal.hash.as_ref(), &self.signature, &public_key)
    }
}



pub struct NightshadeTask {
    /// Signer.
    signer: Arc<BlockSigner>,
    /// Blocks from other authorities containing payloads. At the beginning of the consensus
    /// authorities only have their own block. It is required for an authority to endorse a block
    /// from other authority to have its block.
    proposals: Vec<Option<SignedBlockProposal>>,
    /// Nightshade main data structure.
    nightshade: Option<Nightshade>,
    /// Block index that we are currently working on.
    block_index: Option<u64>,
    /// Standard public/secret keys are used to sign payloads and gossips
    public_keys: Vec<PublicKey>,
    /// Receiver -> Gossips map
    gossips: Arc<Mutex<HashMap<AuthorityId, Vec<Gossip>>>>,
    /// AuthorityId -> BlockProposal.
    commitments: Arc<Mutex<HashMap<AuthorityId, BlockProposal>>>,
    /// Number of messages this task is allowed to send.
    messages_quota: RefCell<i64>,
    consensus_reported: bool,
}

impl NightshadeTask {
    pub fn new(
        owner_uid: AuthorityId,
        block_index: BlockIndex,
        hash: CryptoHash,
        public_keys: Vec<PublicKey>,
        bls_public_keys: Vec<BlsPublicKey>,
        signer: Arc<BlockSigner>,
        gossips: Arc<Mutex<HashMap<AuthorityId, Vec<Gossip>>>>,
        commitments: Arc<Mutex<HashMap<AuthorityId, BlockProposal>>>,
        messages_quota: i64,
    ) -> Self {
        let mut res = Self {
            signer,
            proposals: vec![],
            nightshade: None,
            block_index: None,
            public_keys: vec![],
            gossips,
            commitments,
            messages_quota: RefCell::new(messages_quota),
            consensus_reported: false,
        };
        res.init_nightshade(owner_uid, block_index, hash, public_keys, bls_public_keys);
        res
    }

    fn nightshade_as_ref(&self) -> &Nightshade {
        self.nightshade.as_ref().expect("Nightshade should be initialized")
    }

    fn nightshade_as_mut_ref(&mut self) -> &mut Nightshade {
        self.nightshade.as_mut().expect("Nightshade should be initialized")
    }

    fn init_nightshade(
        &mut self,
        owner_uid: AuthorityId,
        block_index: BlockIndex,
        hash: CryptoHash,
        public_keys: Vec<PublicKey>,
        bls_public_keys: Vec<BlsPublicKey>,
    ) {
        let num_authorities = public_keys.len();
        info!(target: "nightshade", "Init nightshade for authority {}/{}, block {}, proposal {}", owner_uid, num_authorities, block_index, hash);
        assert!(
            self.block_index.is_none()
                || self.block_index.unwrap() < block_index
                || self.proposals[owner_uid as usize].as_ref().unwrap().block_proposal.hash == hash,
            "Reset without increasing block index: adversarial behavior"
        );

        self.proposals = vec![None; num_authorities];
        self.proposals[owner_uid] =
            Some(SignedBlockProposal::new(owner_uid, hash, self.signer.clone()));
        self.block_index = Some(block_index);
        self.public_keys = public_keys;
        self.nightshade = Some(Nightshade::new(
            owner_uid,
            num_authorities,
            self.proposals[owner_uid].clone().unwrap().block_proposal,
            bls_public_keys,
            self.signer.clone(),
        ));
        self.consensus_reported = false;

        // Announce proposal to every other node in the beginning of the consensus
        for a in 0..num_authorities {
            if a == owner_uid {
                continue;
            }
            self.send_payloads(a, vec![owner_uid]);
        }
    }

    fn send_state(&self, message: Message) {
        self.send_gossip(Gossip::new(
            self.nightshade.as_ref().unwrap().owner_id,
            message.receiver_id,
            GossipBody::NightshadeStateUpdate(Box::new(message)),
            self.signer.clone(),
            self.block_index.unwrap(),
        ));
    }

    fn send_gossip(&self, message: Gossip) {
        self.gossips
            .lock()
            .unwrap()
            .entry(message.receiver_id)
            .or_insert_with(Vec::new)
            .push(message);
        *self.messages_quota.borrow_mut() -= 1;
    }

    #[allow(dead_code)]
    fn is_final(&self) -> bool {
        self.nightshade_as_ref().is_final()
    }

    fn process_message(&mut self, message: Message) {
        // Get author of the payload inside this message
        let author = message.state.bare_state.endorses.author;

        // Check if we already receive this proposal.
        if let Some(signed_proposal) = &self.proposals[author] {
            if signed_proposal.block_proposal.hash == message.state.block_hash() {
                if let Err(e) =
                    self.nightshade_as_mut_ref().update_state(message.sender_id, message.state)
                {
                    warn!(target: "nightshade", "{}", e);
                }
            } else {
                // There is at least one malicious actor between the sender of this message
                // and the original author of the payload. But we can't determine which is the bad actor.
            }
        } else {
            // TODO: This message is discarded if we haven't received the proposal yet.
            let gossip = Gossip::new(
                self.owner_id(),
                author,
                GossipBody::PayloadRequest(vec![author]),
                self.signer.clone(),
                self.block_index.unwrap(),
            );
            self.send_gossip(gossip);
        }
    }

    fn process_gossip(&mut self, gossip: Gossip) {
        debug!(target: "nightshade", "Node: {} Processing gossip on block index {}", self.owner_id(), gossip.block_index);

        if Some(gossip.block_index) != self.block_index {
            return;
        }
        if !gossip.verify(&self.public_keys[gossip.sender_id]) {
            return;
        }

        match gossip.body {
            GossipBody::NightshadeStateUpdate(message) => self.process_message(*message),
            GossipBody::PayloadRequest(authorities) => {
                self.send_payloads(gossip.sender_id, authorities)
            }
            GossipBody::PayloadReply(payloads) => self.receive_payloads(gossip.sender_id, payloads),
        }
    }

    fn send_payloads(&self, receiver_id: AuthorityId, authorities: Vec<AuthorityId>) {
        let mut payloads = Vec::new();
        for a in authorities {
            if let Some(ref p) = self.proposals[a] {
                payloads.push(p.clone());
            }
        }
        let gossip = Gossip::new(
            self.nightshade.as_ref().unwrap().owner_id,
            receiver_id,
            GossipBody::PayloadReply(payloads),
            self.signer.clone(),
            self.block_index.unwrap(),
        );
        self.send_gossip(gossip);
    }

    fn receive_payloads(&mut self, sender_id: AuthorityId, payloads: Vec<SignedBlockProposal>) {
        for signed_payload in payloads {
            let authority_id = signed_payload.block_proposal.author;

            // If the signed block is not properly signed by its author, we mark the sender as adversary.
            if !signed_payload.verify(&self.public_keys[authority_id]) {
                self.nightshade_as_mut_ref().set_adversary(sender_id);
                continue;
            }

            if let Some(ref p) = self.proposals[authority_id] {
                // If received proposal differs from our current proposal flag him as an adversary.
                if p.block_proposal.hash != signed_payload.block_proposal.hash {
                    self.nightshade_as_mut_ref().set_adversary(authority_id);
                    self.proposals[authority_id] = None;
                    panic!("the case of adversaries creating forks is not properly handled yet");
                }
            } else {
                self.proposals[authority_id] = Some(signed_payload);
            }
        }
    }

    /// Sends gossip to random authority peers.
    fn gossip_state(&self) {
        let my_state = self.state();

        for i in 0..self.nightshade.as_ref().unwrap().num_authorities {
            if i != self.nightshade.as_ref().unwrap().owner_id {
                let message = Message {
                    sender_id: self.nightshade.as_ref().unwrap().owner_id,
                    receiver_id: i,
                    state: my_state.clone(),
                };
                self.send_state(message);
            }
        }
    }

    /// Helper functions to access values in `nightshade`. Used mostly to debug.

    fn state(&self) -> &State {
        self.nightshade_as_ref().state()
    }

    fn owner_id(&self) -> AuthorityId {
        self.nightshade_as_ref().owner_id
    }

    pub fn run(&mut self) {
        loop {
            let gossips: Vec<_> = self.gossips.lock().unwrap().entry(self.owner_id()).or_insert_with(Vec::new).drain(..).collect();
            let mut prev_gossip_sig = None;
            for gossip in gossips {
                if prev_gossip_sig.is_some() && prev_gossip_sig.as_ref().unwrap() == &gossip.signature {
                    continue;
                }
                prev_gossip_sig = Some(gossip.signature.clone());
                self.process_gossip(gossip);

                // Report as soon as possible when an authority reach consensus on some outcome
                if !self.consensus_reported {
                    if let Some(outcome) = self.nightshade_as_ref().committed.clone() {
                        self.consensus_reported = true;
                        self.commitments.lock().unwrap().insert(self.owner_id(), outcome);
                    }
                }
            }
            self.gossip_state();
            if *self.messages_quota.borrow() <= 0 {
                break;
            }
        }
    }
}

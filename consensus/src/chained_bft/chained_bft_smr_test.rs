// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    chained_bft::{
        block_storage::BlockReader,
        chained_bft_smr::{ChainedBftSMR, ChainedBftSMRConfig},
        common::Author,
        consensus_types::{
            proposal_msg::{ProposalMsg, ProposalUncheckedSignatures},
            vote_msg::VoteMsg,
        },
        network::ConsensusNetworkImpl,
        network_tests::NetworkPlayground,
        test_utils::{MockStateComputer, MockStorage, MockTransactionManager, TestPayload},
    },
    state_replication::StateMachineReplication,
};
use channel;
use crypto::hash::CryptoHash;
use futures::{channel::mpsc, executor::block_on, prelude::*};
use network::validator_network::{ConsensusNetworkEvents, ConsensusNetworkSender};
use proto_conv::FromProto;
use std::sync::Arc;

use crate::chained_bft::{
    consensus_types::timeout_msg::TimeoutMsg,
    epoch_manager::EpochManager,
    persistent_storage::RecoveryData,
    test_utils::{consensus_runtime, with_smr_id},
};
use config::config::ConsensusProposerType::{
    self, FixedProposer, MultipleOrderedProposers, RotatingProposer,
};
use std::{collections::HashMap, time::Duration};
use tokio::runtime;
use types::crypto_proxies::{LedgerInfoWithSignatures, ValidatorSigner, ValidatorVerifier};

/// Auxiliary struct that is preparing SMR for the test
struct SMRNode {
    author: Author,
    signer: ValidatorSigner,
    epoch_mgr: Arc<EpochManager>,
    proposer: Vec<Author>,
    proposer_type: ConsensusProposerType,
    smr_id: usize,
    smr: ChainedBftSMR<TestPayload>,
    commit_cb_receiver: mpsc::UnboundedReceiver<LedgerInfoWithSignatures>,
    mempool: Arc<MockTransactionManager>,
    mempool_notif_receiver: mpsc::Receiver<usize>,
    storage: Arc<MockStorage<TestPayload>>,
}

impl SMRNode {
    fn start(
        playground: &mut NetworkPlayground,
        signer: ValidatorSigner,
        epoch_mgr: Arc<EpochManager>,
        proposer: Vec<Author>,
        smr_id: usize,
        storage: Arc<MockStorage<TestPayload>>,
        initial_data: RecoveryData<TestPayload>,
        proposer_type: ConsensusProposerType,
    ) -> Self {
        let author = signer.author();

        let (network_reqs_tx, network_reqs_rx) = channel::new_test(8);
        let (consensus_tx, consensus_rx) = channel::new_test(8);
        let network_sender = ConsensusNetworkSender::new(network_reqs_tx);
        let network_events = ConsensusNetworkEvents::new(consensus_rx);

        playground.add_node(author, consensus_tx, network_reqs_rx);
        let runtime = runtime::Builder::new()
            .after_start(with_smr_id(signer.author().short_str()))
            .build()
            .expect("Failed to create Tokio runtime!");
        let network = ConsensusNetworkImpl::new(
            author,
            network_sender,
            network_events,
            Arc::clone(&epoch_mgr),
        );

        let config = ChainedBftSMRConfig {
            max_pruned_blocks_in_mem: 10000,
            pacemaker_initial_timeout: Duration::from_secs(3),
            proposer_type,
            contiguous_rounds: 2,
            max_block_size: 50,
        };
        let mut smr = ChainedBftSMR::new(
            author,
            signer.clone(),
            proposer.clone(),
            network,
            runtime,
            config,
            storage.clone(),
            initial_data,
            Arc::clone(&epoch_mgr),
        );
        let (commit_cb_sender, commit_cb_receiver) = mpsc::unbounded::<LedgerInfoWithSignatures>();
        let mut mp = MockTransactionManager::new();
        let commit_receiver = mp.take_commit_receiver();
        let mempool = Arc::new(mp);
        smr.start(
            mempool.clone(),
            Arc::new(MockStateComputer::new(
                commit_cb_sender.clone(),
                Arc::clone(&storage),
            )),
        )
        .expect("Failed to start SMR!");
        Self {
            author,
            signer,
            epoch_mgr,
            proposer,
            proposer_type,
            smr_id,
            smr,
            commit_cb_receiver,
            mempool,
            mempool_notif_receiver: commit_receiver,
            storage,
        }
    }

    fn restart(mut self, playground: &mut NetworkPlayground) -> Self {
        self.smr.stop();
        let recover_data = self
            .storage
            .get_recovery_data()
            .unwrap_or_else(|e| panic!("fail to restart due to: {}", e));
        Self::start(
            playground,
            self.signer,
            self.epoch_mgr,
            self.proposer,
            self.smr_id + 10,
            self.storage,
            recover_data,
            self.proposer_type,
        )
    }

    fn start_num_nodes(
        num_nodes: usize,
        quorum_size: usize,
        playground: &mut NetworkPlayground,
        proposer_type: ConsensusProposerType,
    ) -> Vec<Self> {
        let mut signers = vec![];
        let mut author_to_public_keys = HashMap::new();
        for smr_id in 0..num_nodes {
            // 0 -> [0000], 1 -> [1000] in the logs
            let random_validator_signer = ValidatorSigner::from_int(smr_id as u8);
            author_to_public_keys.insert(
                random_validator_signer.author(),
                random_validator_signer.public_key(),
            );
            signers.push(random_validator_signer);
        }
        let validator_verifier =
            ValidatorVerifier::new_with_quorum_size(author_to_public_keys, quorum_size)
                .expect("Invalid quorum_size.");
        let epoch_mgr = Arc::new(EpochManager::new(0, validator_verifier));
        let peers = epoch_mgr.validators().get_ordered_account_addresses();
        let proposer = {
            match proposer_type {
                FixedProposer => vec![peers[0]],
                RotatingProposer | MultipleOrderedProposers => peers,
            }
        };
        let mut nodes = vec![];
        for smr_id in 0..num_nodes {
            let (storage, initial_data) = MockStorage::start_for_testing();
            nodes.push(Self::start(
                playground,
                signers.remove(0),
                Arc::clone(&epoch_mgr),
                proposer.clone(),
                smr_id,
                storage,
                initial_data,
                proposer_type,
            ));
        }
        nodes
    }
}

fn verify_finality_proof(node: &SMRNode, ledger_info_with_sig: &LedgerInfoWithSignatures) {
    let ledger_info_hash = ledger_info_with_sig.ledger_info().hash();
    for (author, signature) in ledger_info_with_sig.signatures() {
        assert_eq!(
            Ok(()),
            node.epoch_mgr
                .validators()
                .verify_signature(*author, ledger_info_hash, &signature)
        );
    }
}

#[test]
/// Should receive a new proposal upon start
fn basic_start_test() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    let nodes = SMRNode::start_num_nodes(2, 2, &mut playground, RotatingProposer);
    let genesis = nodes[0]
        .smr
        .block_store()
        .expect("No valid block store!")
        .root();
    block_on(async move {
        let mut msg = playground
            .wait_for_messages(1, NetworkPlayground::proposals_only)
            .await;
        let first_proposal: ProposalMsg<Vec<u64>> =
            ProposalUncheckedSignatures::<Vec<u64>>::from_proto(msg[0].1.take_proposal())
                .unwrap()
                .into();
        assert_eq!(first_proposal.proposal().height(), 1);
        assert_eq!(first_proposal.proposal().parent_id(), genesis.id());
        assert_eq!(
            first_proposal.proposal().quorum_cert().certified_block_id(),
            genesis.id()
        );
    });
}

#[test]
/// Upon startup, the first proposal is sent, delivered and voted by all the participants.
fn start_with_proposal_test() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    let nodes = SMRNode::start_num_nodes(2, 2, &mut playground, RotatingProposer);

    block_on(async move {
        let _proposals = playground
            .wait_for_messages(1, NetworkPlayground::proposals_only)
            .await;
        // Need to wait for 2 votes for the 2 replicas
        let votes: Vec<VoteMsg> = playground
            .wait_for_messages(2, NetworkPlayground::votes_only)
            .await
            .into_iter()
            .map(|(_, mut msg)| VoteMsg::from_proto(msg.take_vote()).unwrap())
            .collect();
        let proposed_block_id = votes[0].vote_data().block_id();

        // Verify that the proposed block id is indeed present in the block store.
        assert!(nodes[0]
            .smr
            .block_store()
            .unwrap()
            .get_block(proposed_block_id)
            .is_some());
        assert!(nodes[1]
            .smr
            .block_store()
            .unwrap()
            .get_block(proposed_block_id)
            .is_some());
    });
}

fn basic_full_round(num_nodes: usize, quorum_size: usize, proposer_type: ConsensusProposerType) {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    let _nodes = SMRNode::start_num_nodes(num_nodes, quorum_size, &mut playground, proposer_type);

    // In case we're using multi-proposer, every proposal and vote is sent to two participants.
    let num_messages_to_send = if proposer_type == MultipleOrderedProposers {
        2 * (num_nodes - 1)
    } else {
        num_nodes - 1
    };
    block_on(async move {
        let _broadcast_proposals_1 = playground
            .wait_for_messages(num_messages_to_send, NetworkPlayground::proposals_only)
            .await;
        let _votes_1 = playground
            .wait_for_messages(num_messages_to_send, NetworkPlayground::votes_only)
            .await;
        let mut broadcast_proposals_2 = playground
            .wait_for_messages(num_messages_to_send, NetworkPlayground::proposals_only)
            .await;
        let next_proposal: ProposalMsg<Vec<u64>> =
            ProposalUncheckedSignatures::<Vec<u64>>::from_proto(
                broadcast_proposals_2[0].1.take_proposal(),
            )
            .unwrap()
            .into();
        assert!(next_proposal.proposal().round() >= 2);
        assert!(next_proposal.proposal().height() >= 2);
    });
}

#[test]
/// Upon startup, the first proposal is sent, voted by all the participants, QC is formed and
/// then the next proposal is sent.
fn basic_full_round_test() {
    basic_full_round(2, 2, FixedProposer);
}

#[test]
/// Basic happy path with multiple proposers
fn happy_path_with_multi_proposer() {
    basic_full_round(2, 2, MultipleOrderedProposers);
}

/// Verify the basic e2e flow: blocks are committed, txn manager is notified, block tree is
/// pruned, restart the node and we can still continue.
#[test]
fn basic_commit_and_restart() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    let mut nodes = SMRNode::start_num_nodes(2, 2, &mut playground, RotatingProposer);
    let mut block_ids = vec![];

    block_on(async {
        let num_rounds = 10;

        for round in 0..num_rounds {
            let _proposals = playground
                .wait_for_messages(1, NetworkPlayground::exclude_timeout_msg)
                .await;

            // A proposal is carrying a QC that commits a block of round - 3.
            if round >= 3 {
                let block_id_to_commit = block_ids[round - 3];
                let commit_v1 = nodes[0].commit_cb_receiver.next().await.unwrap();
                let commit_v2 = nodes[1].commit_cb_receiver.next().await.unwrap();
                assert_eq!(
                    commit_v1.ledger_info().consensus_block_id(),
                    block_id_to_commit
                );
                verify_finality_proof(&nodes[0], &commit_v1);
                assert_eq!(
                    commit_v2.ledger_info().consensus_block_id(),
                    block_id_to_commit
                );
                verify_finality_proof(&nodes[1], &commit_v2);
            }

            // v1 and v2 send votes
            let mut votes = playground
                .wait_for_messages(1, NetworkPlayground::votes_only)
                .await;
            let vote_msg = VoteMsg::from_proto(votes[0].1.take_vote()).unwrap();
            block_ids.push(vote_msg.vote_data().block_id());
        }
        assert!(
            nodes[0].smr.block_store().unwrap().root().height() >= 6,
            "height of node 0 is {}",
            nodes[0].smr.block_store().unwrap().root().height()
        );
        assert!(
            nodes[1].smr.block_store().unwrap().root().height() >= 6,
            "height of node 1 is {}",
            nodes[1].smr.block_store().unwrap().root().height()
        );
        // This message is for proposal with round 11 to delivery the QC, but not gather the QC
        // so after restart, proposer will propose round 11 again.
        playground
            .wait_for_messages(1, NetworkPlayground::exclude_timeout_msg)
            .await;
    });
    // create a new playground to avoid polling potential vote messages in previous one.
    playground = NetworkPlayground::new(runtime.executor());
    nodes = nodes
        .into_iter()
        .map(|node| node.restart(&mut playground))
        .collect();

    block_on(async {
        let mut round = 0;

        while round < 10 {
            // The loop is to ensure that we collect a network vote(enough for QC with 2 nodes) then
            // move the round forward because there's a race that node1 may or may not
            // reject round 11 depends on whether it voted for before restart.
            loop {
                let msg = playground
                    .wait_for_messages(1, NetworkPlayground::exclude_timeout_msg)
                    .await;
                if msg[0].1.has_vote() {
                    round += 1;
                    break;
                }
            }
        }
        // Because of the race, we can't assert the commit reliably, instead we assert
        // both nodes commit to at least round 17.
        // We cannot reliable wait for the event of "commit & prune": the only thing that we know is
        // that after receiving the vote for round 20, the root should be at least height 16.
        assert!(
            nodes[0].smr.block_store().unwrap().root().height() >= 16,
            "height of node 0 is {}",
            nodes[0].smr.block_store().unwrap().root().height()
        );
        assert!(
            nodes[1].smr.block_store().unwrap().root().height() >= 16,
            "height of node 1 is {}",
            nodes[1].smr.block_store().unwrap().root().height()
        );
    });
}

#[test]
fn basic_block_retrieval() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    // This test depends on the fixed proposer on nodes[0]
    let mut nodes = SMRNode::start_num_nodes(3, 2, &mut playground, FixedProposer);
    block_on(async move {
        let mut first_proposals = vec![];
        // First three proposals are delivered just to nodes[0[ and nodes[1].
        playground.drop_message_for(&nodes[0].author, nodes[2].author);
        for _ in 0..2 {
            playground
                .wait_for_messages(1, NetworkPlayground::proposals_only)
                .await;
            let mut votes = playground
                .wait_for_messages(1, NetworkPlayground::votes_only)
                .await;
            let vote_msg = VoteMsg::from_proto(votes[0].1.take_vote()).unwrap();
            let proposal_id = vote_msg.vote_data().block_id();
            first_proposals.push(proposal_id);
        }
        // The next proposal is delivered to all: as a result nodes[2] should retrieve the missing
        // blocks from nodes[0] and vote for the 3th proposal.
        playground.stop_drop_message_for(&nodes[0].author, &nodes[2].author);

        playground
            .wait_for_messages(2, NetworkPlayground::proposals_only)
            .await;
        // Wait until nodes[2] sent out a vote, drop the vote from nodes[1] so that nodes[0]
        // won't move too far and prune the requested block
        playground.drop_message_for(&nodes[1].author, nodes[0].author);
        playground
            .wait_for_messages(1, NetworkPlayground::votes_only)
            .await;
        playground.stop_drop_message_for(&nodes[1].author, &nodes[0].author);
        // the first two proposals should be present at nodes[2]
        for block_id in &first_proposals {
            assert!(nodes[2]
                .smr
                .block_store()
                .unwrap()
                .get_block(*block_id)
                .is_some());
        }

        // Both nodes[1] and nodes[2] are going to vote for 4th proposal and commit the 1th one.

        // Verify that nodes[2] commits the first proposal.
        playground
            .wait_for_messages(2, NetworkPlayground::votes_only)
            .await;
        if let Some(commit_v3) = nodes[2].commit_cb_receiver.next().await {
            assert_eq!(
                commit_v3.ledger_info().consensus_block_id(),
                first_proposals[0],
            );
        }
    });
}

#[test]
fn block_retrieval_with_timeout() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    let nodes = SMRNode::start_num_nodes(3, 2, &mut playground, FixedProposer);
    block_on(async move {
        let mut first_proposals = vec![];
        // First three proposals are delivered just to nodes[0] and nodes[1].
        playground.drop_message_for(&nodes[0].author, nodes[2].author);
        for _ in 0..2 {
            playground
                .wait_for_messages(1, NetworkPlayground::proposals_only)
                .await;
            let mut votes = playground
                .wait_for_messages(1, NetworkPlayground::votes_only)
                .await;
            let vote_msg = VoteMsg::from_proto(votes[0].1.take_vote()).unwrap();
            let proposal_id = vote_msg.vote_data().block_id();
            first_proposals.push(proposal_id);
        }
        // The next proposal is delivered to all: as a result nodes[2] should retrieve the missing
        // blocks from v1 and vote for the 4th proposal.
        playground.stop_drop_message_for(&nodes[0].author, &nodes[2].author);

        playground
            .wait_for_messages(2, NetworkPlayground::proposals_only)
            .await;
        playground.drop_message_for(&nodes[1].author, nodes[0].author);
        // Block RPC and wait until timeout for current round
        playground.drop_message_for(&nodes[2].author, nodes[0].author);
        playground
            .wait_for_messages(1, NetworkPlayground::timeout_msg_only)
            .await;
        // Unblock RPC
        playground.stop_drop_message_for(&nodes[2].author, &nodes[0].author);
        // Wait until v3 sent out a vote, drop the vote from v2 so that v1 won't move too far
        // and prune the requested block
        playground
            .wait_for_messages(1, NetworkPlayground::votes_only)
            .await;
        playground.stop_drop_message_for(&nodes[1].author, &nodes[0].author);
        // the first two proposals should be present at v3
        for block_id in &first_proposals {
            assert!(nodes[2]
                .smr
                .block_store()
                .unwrap()
                .get_block(*block_id)
                .is_some());
        }
    });
}

#[test]
/// Verify that a node that is lagging behind can catch up by state sync some blocks
/// have been pruned by the others.
fn basic_state_sync() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    // This test depends on the fixed proposer on nodes[0]
    let mut nodes = SMRNode::start_num_nodes(3, 2, &mut playground, FixedProposer);
    block_on(async move {
        let mut proposals = vec![];
        // The first ten proposals are delivered just to nodes[0] and nodes[1], which should commit
        // the first seven blocks.
        playground.drop_message_for(&nodes[0].author, nodes[2].author);
        for _ in 0..10 {
            playground
                .wait_for_messages(1, NetworkPlayground::proposals_only)
                .await;
            let mut votes = playground
                .wait_for_messages(1, NetworkPlayground::votes_only)
                .await;
            let vote_msg = VoteMsg::from_proto(votes[0].1.take_vote()).unwrap();
            let proposal_id = vote_msg.vote_data().block_id();
            proposals.push(proposal_id);
        }

        let mut node0_commits = vec![];
        for i in 0..7 {
            node0_commits.push(
                nodes[0]
                    .commit_cb_receiver
                    .next()
                    .await
                    .unwrap()
                    .ledger_info()
                    .consensus_block_id(),
            );
            assert_eq!(node0_commits[i], proposals[i]);
        }

        // Next proposal is delivered to all: as a result nodes[2] should be able to retrieve the
        // missing blocks from nodes[0] and commit the first eight proposals as well.
        playground.stop_drop_message_for(&nodes[0].author, &nodes[2].author);
        playground
            .wait_for_messages(2, NetworkPlayground::proposals_only)
            .await;
        let mut node2_commits = vec![];
        // The only notification we will receive is for the last (8th) proposal.
        node2_commits.push(
            nodes[2]
                .commit_cb_receiver
                .next()
                .await
                .unwrap()
                .ledger_info()
                .consensus_block_id(),
        );
        assert_eq!(node2_commits[0], proposals[7]);

        // wait for the vote from node2
        playground
            .wait_for_messages(1, NetworkPlayground::votes_only)
            .await;
        for (_, proposal) in playground
            .wait_for_messages(2, NetworkPlayground::proposals_only)
            .await
        {
            assert_eq!(proposal.has_proposal(), true);
        }
        // Verify that node 2 has notified its mempool about the committed txn of next block.
        nodes[2]
            .mempool_notif_receiver
            .next()
            .await
            .expect("Fail to be notified by a mempool committed txns");
        assert_eq!(nodes[2].mempool.get_committed_txns().len(), 50);
    });
}

#[test]
/// Verify that a node syncs up when receiving a timeout message with a relevant ledger info
fn state_sync_on_timeout() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    // This test depends on the fixed proposer on nodes[0]
    let mut nodes = SMRNode::start_num_nodes(3, 2, &mut playground, FixedProposer);
    block_on(async move {
        let mut proposals = vec![];
        // The first ten proposals are delivered just to nodes[0] and nodes[1], which should commit
        // the first seven blocks.
        playground.drop_message_for(&nodes[0].author, nodes[2].author);
        for _ in 0..10 {
            playground
                .wait_for_messages(1, NetworkPlayground::proposals_only)
                .await;
            let mut votes = playground
                .wait_for_messages(1, NetworkPlayground::votes_only)
                .await;
            let vote_msg = VoteMsg::from_proto(votes[0].1.take_vote()).unwrap();
            let proposal_id = vote_msg.vote_data().block_id();
            proposals.push(proposal_id);
        }

        // Start dropping messages from 0 to 1 as well: node 0 is now disconnected and we can
        // expect timeouts from both 0 and 1.
        playground.drop_message_for(&nodes[0].author, nodes[1].author);

        // Wait for a timeout message from 2 to {0, 1} and from 1 to {0, 2}
        // (node 0 cannot send to anyone).  Note that there are 6 messages waited on
        // since 2 can timeout 2x while waiting for 1 to timeout.
        playground
            .wait_for_messages(6, NetworkPlayground::timeout_msg_only)
            .await;

        let mut node2_commits = vec![];
        // The only notification we will receive is for the last commit known to nodes[1]: 7th
        // proposal.
        node2_commits.push(
            nodes[2]
                .commit_cb_receiver
                .next()
                .await
                .unwrap()
                .ledger_info()
                .consensus_block_id(),
        );
        assert_eq!(node2_commits[0], proposals[6]);
    });
}

#[test]
/// Verify that in case a node receives timeout message from a remote peer that is lagging behind,
/// then this node sends a sync info, which helps the remote to properly catch up.
fn sync_info_sent_if_remote_stale() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());
    // This test depends on the fixed proposer on nodes[0]
    // We're going to drop messages from 0 to 2: as a result we expect node 2 to broadcast timeout
    // messages, for which node 1 should respond with sync_info, which should eventually
    // help node 2 to catch up.
    let mut nodes = SMRNode::start_num_nodes(3, 2, &mut playground, FixedProposer);
    block_on(async move {
        playground.drop_message_for(&nodes[0].author, nodes[2].author);
        // Don't want to receive timeout messages from 2 until 1 has some real stuff to contribute.
        playground.drop_message_for(&nodes[2].author, nodes[1].author);
        for _ in 0..10 {
            playground
                .wait_for_messages(1, NetworkPlayground::proposals_only)
                .await;
            playground
                .wait_for_messages(1, NetworkPlayground::votes_only)
                .await;
        }

        // Wait for some timeout message from 2 to {0, 1}.
        playground.stop_drop_message_for(&nodes[2].author, &nodes[1].author);
        playground
            .wait_for_messages(2, NetworkPlayground::timeout_msg_only)
            .await;
        // Now wait for a sync info message from 1 to 2.
        playground
            .wait_for_messages(1, NetworkPlayground::sync_info_only)
            .await;

        let node2_commit = nodes[2]
            .commit_cb_receiver
            .next()
            .await
            .unwrap()
            .ledger_info()
            .consensus_block_id();

        // Close node 1 channel for new commit callbacks and iterate over all its commits: we should
        // find the node 2 commit there.
        let mut found = false;
        nodes[1].commit_cb_receiver.close();
        while let Ok(Some(node1_commit)) = nodes[1].commit_cb_receiver.try_next() {
            let node1_commit_id = node1_commit.ledger_info().consensus_block_id();
            if node1_commit_id == node2_commit {
                found = true;
                break;
            }
        }

        assert_eq!(found, true);
    });
}

#[test]
/// Verify that a QC can be formed by aggregating the votes piggybacked by TimeoutMsgs
fn aggregate_timeout_votes() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());

    // The proposer node[0] sends its proposal to nodes 1 and 2, which cannot respond back,
    // because their messages are dropped.
    // Upon timeout nodes 1 and 2 are sending timeout messages with attached votes for the original
    // proposal: both can then aggregate the QC for the first proposal.
    let nodes = SMRNode::start_num_nodes(3, 2, &mut playground, FixedProposer);
    block_on(async move {
        playground.drop_message_for(&nodes[1].author, nodes[0].author);
        playground.drop_message_for(&nodes[2].author, nodes[0].author);

        // Node 0 sends proposals to nodes 1 and 2
        let mut msg = playground
            .wait_for_messages(2, NetworkPlayground::proposals_only)
            .await;
        let first_proposal: ProposalMsg<Vec<u64>> =
            ProposalUncheckedSignatures::<Vec<u64>>::from_proto(msg[0].1.take_proposal())
                .unwrap()
                .into();
        let proposal_id = first_proposal.proposal().id();
        playground.drop_message_for(&nodes[0].author, nodes[1].author);
        playground.drop_message_for(&nodes[0].author, nodes[2].author);

        // Wait for the timeout messages sent by 1 and 2 to each other
        playground
            .wait_for_messages(2, NetworkPlayground::timeout_msg_only)
            .await;

        // Node 0 cannot form a QC
        assert_eq!(
            nodes[0]
                .smr
                .block_store()
                .unwrap()
                .highest_quorum_cert()
                .certified_block_round(),
            0
        );
        // Nodes 1 and 2 form a QC and move to the next round.
        // Wait for the timeout messages from 1 and 2
        playground
            .wait_for_messages(2, NetworkPlayground::timeout_msg_only)
            .await;

        assert_eq!(
            nodes[1]
                .smr
                .block_store()
                .unwrap()
                .highest_quorum_cert()
                .certified_block_id(),
            proposal_id
        );
        assert_eq!(
            nodes[2]
                .smr
                .block_store()
                .unwrap()
                .highest_quorum_cert()
                .certified_block_id(),
            proposal_id
        );
    });
}

#[test]
/// Verify that the NIL blocks formed during timeouts can be used to form commit chains.
fn chain_with_nil_blocks() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());

    // The proposer node[0] sends 3 proposals, after that its proposals are dropped and it cannot
    // communicate with nodes 1 and 2. Nodes 1 and 2 should be able to commit the 3 proposal
    // via NIL blocks commit chain.
    let nodes = SMRNode::start_num_nodes(3, 2, &mut playground, FixedProposer);
    block_on(async move {
        // Wait for the first 3 proposals (each one sent to two nodes).
        playground
            .wait_for_messages(2 * 3, NetworkPlayground::proposals_only)
            .await;
        playground.drop_message_for(&nodes[0].author, nodes[1].author);
        playground.drop_message_for(&nodes[0].author, nodes[2].author);

        // After the first timeout nodes 1 and 2 should have last_proposal votes and
        // they can generate its QC independently.
        // Upon the second timeout nodes 1 and 2 send NIL block_1 with a QC to last_proposal.
        // Upon the third timeout nodes 1 and 2 send NIL block_2 with a QC to NIL block_1.
        // G <- p1 <- p2 <- p3 <- NIL1 <- NIL2
        playground
            .wait_for_messages(4 * 3, NetworkPlayground::timeout_msg_only)
            .await;
        // We can't guarantee the timing of the last timeout processing, the only thing we can
        // look at is that HQC round is at least 4.
        assert!(
            nodes[2]
                .smr
                .block_store()
                .unwrap()
                .highest_quorum_cert()
                .certified_block_round()
                >= 4
        );

        assert!(nodes[2].smr.block_store().unwrap().root().round() >= 1)
    });
}

#[test]
/// Test secondary proposal processing
fn secondary_proposers() {
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.executor());

    let mut nodes = SMRNode::start_num_nodes(3, 2, &mut playground, MultipleOrderedProposers);
    block_on(async move {
        // Node 0 is disconnected.
        playground.drop_message_for(&nodes[0].author, nodes[1].author);
        playground.drop_message_for(&nodes[0].author, nodes[2].author);
        // Run a system until node 0 is a designated primary proposer. In this round the
        // secondary proposal should be voted for and attached to the timeout message.
        let timeout_msgs = playground
            .wait_for_messages(2 * 2, NetworkPlayground::timeout_msg_only)
            .await;
        let mut secondary_proposal_ids = vec![];
        for mut msg in timeout_msgs {
            let timeout_msg = TimeoutMsg::from_proto(msg.1.take_timeout_msg()).unwrap();
            assert!(timeout_msg.pacemaker_timeout().vote_msg().is_some());
            secondary_proposal_ids.push(
                timeout_msg
                    .pacemaker_timeout()
                    .vote_msg()
                    .unwrap()
                    .vote_data()
                    .block_id(),
            );
        }
        assert_eq!(secondary_proposal_ids.len(), 4);
        let secondary_proposal_id = secondary_proposal_ids[0];
        for id in secondary_proposal_ids {
            assert_eq!(secondary_proposal_id, id);
        }
        // The secondary proposal id should get committed at some point in the future:
        // 10 rounds should be more than enough. Note that it's hard to say what round is going to
        // have 2 proposals and what round is going to have just one proposal because we don't want
        // to predict the rounds with proposer 0 being a leader.
        let mut secondary_proposal_committed = false;
        for _ in 0..10 {
            playground
                .wait_for_messages(2, NetworkPlayground::votes_only)
                .await;
            // Retrieve all the ids committed by the node to check whether secondary_proposal_id
            // has been committed.
            while let Ok(Some(li)) = nodes[1].commit_cb_receiver.try_next() {
                if li.ledger_info().consensus_block_id() == secondary_proposal_id {
                    secondary_proposal_committed = true;
                    break;
                }
            }
        }
        assert_eq!(secondary_proposal_committed, true);
    });
}

//! Real-validator coverage for RPC forwarding through gossip-based leader discovery.
use {
    solana_core::validator::ValidatorConfig,
    solana_gossip::contact_info::Protocol,
    solana_local_cluster::{
        integration_tests::{DEFAULT_CLUSTER_LAMPORTS, DEFAULT_NODE_STAKE},
        local_cluster::{ClusterConfig, LocalCluster},
        validator_configs::make_identical_validator_configs,
    },
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcSendTransactionConfig,
    solana_sdk::{
        commitment_config::{CommitmentConfig, CommitmentLevel},
        pubkey::Pubkey,
        signature::Signer,
        system_transaction,
    },
    solana_streamer::socket::SocketAddrSpace,
    std::{
        thread::sleep,
        time::{Duration, Instant},
    },
};

fn wait_for(description: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_non_leader_rpc_forwards_to_gossip_discovered_producer() {
    solana_logger::setup_with_default("error");
    let validator_config = ValidatorConfig::default_for_test();
    assert!(validator_config
        .send_transaction_service_config
        .tpu_peers
        .is_none());
    assert!(validator_config.fixed_leader_schedule.is_none());
    let mut config = ClusterConfig {
        validator_configs: make_identical_validator_configs(&validator_config, 1),
        node_stakes: vec![DEFAULT_NODE_STAKE],
        cluster_lamports: DEFAULT_CLUSTER_LAMPORTS,
        num_listeners: 1,
        ..ClusterConfig::default()
    };
    // LocalCluster starts full Validators with their real gossip, PoH, RPC and TPU
    // services. Only the bootstrap node has stake; the listener can never lead.
    let cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    assert_eq!(cluster.validators.len(), 2);
    let producer_id = *cluster.entry_point_info.pubkey();
    let producer = &cluster.validators[&producer_id];
    let ingress = cluster
        .validators
        .values()
        .find(|node| node.info.keypair.pubkey() != producer_id)
        .unwrap();
    let ingress_id = ingress.info.keypair.pubkey();
    assert_ne!(producer_id, ingress_id);
    assert!(ingress.config.voting_disabled);
    for node in cluster.validators.values() {
        assert!(node
            .config
            .send_transaction_service_config
            .tpu_peers
            .is_none());
        assert!(node.config.fixed_leader_schedule.is_none());
    }
    let producer_validator = producer.validator.as_ref().unwrap();
    let ingress_validator = ingress.validator.as_ref().unwrap();
    let create_client = |address| {
        RpcClient::new_with_timeout_and_commitment(
            format!("http://{address}"),
            Duration::from_secs(2),
            CommitmentConfig::processed(),
        )
    };
    let producer_rpc = create_client(producer.info.contact_info.rpc().unwrap());
    let ingress_rpc = create_client(ingress.info.contact_info.rpc().unwrap());

    // Observe contacts learned by the ingress's actual gossip service. Do not
    // insert contacts, set a leader schedule, or supply any TPU destination.
    wait_for(
        "the producer's gossip contact and replay on the RPC node",
        || {
            ingress_validator
                .cluster_info
                .tpu_peers()
                .iter()
                .any(|contact| {
                    contact.pubkey() == &producer_id && contact.tpu(Protocol::QUIC).is_ok()
                })
                && ingress_rpc.get_slot().map(|slot| slot > 0).unwrap_or(false)
        },
    );
    {
        let bank = ingress_validator.bank_forks.read().unwrap().working_bank();
        let stakes = bank.epoch_staked_nodes(bank.epoch()).unwrap();
        assert_eq!(stakes.get(&ingress_id).copied().unwrap_or(0), 0);
        assert!(stakes.get(&producer_id).copied().unwrap_or(0) > 0);
    }
    let slot = ingress_rpc.get_slot().unwrap();
    assert!(ingress_rpc
        .get_slot_leaders(slot, 16)
        .unwrap()
        .iter()
        .all(|leader| leader == &producer_id));
    let recipient = Pubkey::new_unique();
    let amount = 1_000_000;
    assert_eq!(producer_rpc.get_balance(&recipient).unwrap(), 0);
    let blockhash = ingress_rpc.get_latest_blockhash().unwrap();
    let transaction =
        system_transaction::transfer(&cluster.funding_keypair, &recipient, amount, blockhash);

    // Submit exactly once, exclusively through the unstaked non-leader's RPC.
    // RpcClient is an HTTP client here, not a ThinClient/TPU fanout sender.
    let signature = ingress_rpc
        .send_transaction_with_config(
            &transaction,
            RpcSendTransactionConfig {
                skip_preflight: true,
                preflight_commitment: Some(CommitmentLevel::Processed),
                ..RpcSendTransactionConfig::default()
            },
        )
        .unwrap();
    assert_eq!(signature, transaction.signatures[0]);

    let mut executed_slot = None;
    wait_for("successful execution on the other validator", || {
        if let Ok(response) = producer_rpc.get_signature_statuses(&[signature]) {
            if let Some(status) = &response.value[0] {
                assert_eq!(
                    status.err, None,
                    "producer rejected the forwarded transaction"
                );
                executed_slot = Some(status.slot);
                return true;
            }
        }
        false
    });
    let executed_slot = executed_slot.unwrap();
    assert_eq!(
        producer_rpc.get_slot_leaders(executed_slot, 1).unwrap(),
        vec![producer_id]
    );
    assert_eq!(producer_rpc.get_balance(&recipient).unwrap(), amount);
    {
        let bank = producer_validator.bank_forks.read().unwrap().working_bank();
        assert_eq!(bank.get_signature_status(&signature), Some(Ok(())));
        assert_eq!(bank.get_balance(&recipient), amount);
    }
    wait_for("the producer's recorded ledger entry", || {
        producer_validator
            .blockstore
            .get_slot_entries(executed_slot, 0)
            .map(|entries| {
                entries.iter().any(|entry| {
                    entry
                        .transactions
                        .iter()
                        .any(|recorded| recorded.signatures[0] == signature)
                })
            })
            .unwrap_or(false)
    });
}

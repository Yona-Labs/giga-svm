//! Invalid RPC signatures must be rejected before direct block production.
use {
    solana_local_cluster::{
        integration_tests::{DEFAULT_CLUSTER_LAMPORTS, DEFAULT_NODE_STAKE},
        local_cluster::LocalCluster,
    },
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcSendTransactionConfig,
    solana_sdk::{
        commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Signature,
        system_transaction,
    },
    solana_streamer::socket::SocketAddrSpace,
};

#[test]
fn test_leader_rpc_rejects_invalid_signature_without_recording() {
    let cluster = LocalCluster::new_with_equal_stakes(
        2,
        DEFAULT_CLUSTER_LAMPORTS,
        DEFAULT_NODE_STAKE,
        SocketAddrSpace::Unspecified,
    );
    let rpc = RpcClient::new_with_commitment(
        format!("http://{}", cluster.entry_point_info.rpc().unwrap()),
        CommitmentConfig::processed(),
    );
    let recipient = Pubkey::new_unique();
    let mut transaction = system_transaction::transfer(
        &cluster.funding_keypair,
        &recipient,
        1_000_000,
        rpc.get_latest_blockhash().unwrap(),
    );
    let mut signature: [u8; 64] = transaction.signatures[0].into();
    signature[0] ^= 1;
    transaction.signatures[0] = Signature::from(signature);
    assert!(transaction.verify().is_err());

    let error = rpc
        .send_transaction_with_config(
            &transaction,
            RpcSendTransactionConfig {
                skip_preflight: true,
                ..RpcSendTransactionConfig::default()
            },
        )
        .expect_err("invalid signature must be rejected with skipPreflight=true");
    assert!(
        error.to_string().contains("signature verification failure"),
        "{error}"
    );

    assert_eq!(rpc.get_balance(&recipient).unwrap(), 0);
    assert!(rpc
        .get_signature_statuses(&[transaction.signatures[0]])
        .unwrap()
        .value[0]
        .is_none());
    for validator in cluster.validators.values() {
        let bank = validator
            .validator
            .as_ref()
            .unwrap()
            .bank_forks
            .read()
            .unwrap()
            .working_bank();
        assert_eq!(bank.get_balance(&recipient), 0);
    }
}

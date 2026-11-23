//! Exercise the real HTTP RPC and SendTransactionService, without a local leader.
use {
    base64::{engine::general_purpose::STANDARD, Engine},
    crossbeam_channel::{bounded, Receiver},
    serde_json::json,
    solana_client::{connection_cache::ConnectionCache, rpc_client::RpcClient},
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_ledger::{
        blockstore::Blockstore, genesis_utils::create_genesis_config,
        get_tmp_ledger_path_auto_delete, leader_schedule_cache::LeaderScheduleCache,
    },
    solana_rpc::{
        max_slots::MaxSlots,
        optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
        rpc::{create_validator_exit, JsonRpcConfig},
        rpc_service::JsonRpcService,
    },
    solana_rpc_client_api::{
        client_error::ErrorKind,
        request::{RpcError, RpcRequest},
    },
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        commitment::{BlockCommitmentCache, CommitmentSlots},
        prioritization_fee_cache::PrioritizationFeeCache,
    },
    solana_sdk::{
        message::{v0, Message, VersionedMessage},
        pubkey::Pubkey,
        signature::{Keypair, Signature, Signer},
        system_instruction, system_transaction,
        transaction::{SanitizedTransaction, Transaction, TransactionError, VersionedTransaction},
    },
    solana_send_transaction_service::send_transaction_service,
    solana_streamer::socket::SocketAddrSpace,
    std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, RwLock,
        },
        time::{Duration, Instant},
    },
};

struct RpcGuard {
    service: Option<JsonRpcService>,
    exit: Arc<AtomicBool>,
}

impl Drop for RpcGuard {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        self.service.take().unwrap().join().unwrap();
    }
}

fn with_rpc(
    test: impl FnOnce(&RpcClient, &Arc<Bank>, &Keypair, Receiver<SanitizedTransaction>, &UdpSocket),
) {
    let ledger = get_tmp_ledger_path_auto_delete!();
    let genesis = create_genesis_config(1_000_000_000);
    let parent = Arc::new(Bank::new_for_tests(&genesis.genesis_config));
    // V0 address loading needs the slot-hashes sysvar created after genesis.
    let bank = Bank::new_from_parent(parent, &genesis.validator_pubkey, 1);
    bank.freeze();
    let bank_forks = BankForks::new_rw_arc(bank);
    let bank = bank_forks.read().unwrap().working_bank();
    let blockstore = Arc::new(Blockstore::open(ledger.path()).unwrap());
    let (_, leader_tpu) =
        solana_net_utils::bind_in_range(IpAddr::V4(Ipv4Addr::LOCALHOST), (10000, 60000)).unwrap();
    let (_, local_tpu) =
        solana_net_utils::bind_in_range(IpAddr::V4(Ipv4Addr::LOCALHOST), (10000, 60000)).unwrap();
    let keypair = Arc::new(Keypair::new());
    let mut contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
    // Keep all test traffic on sockets owned by this fixture.
    contact_info
        .set_tpu(local_tpu.local_addr().unwrap())
        .unwrap();
    let cluster_info = Arc::new(ClusterInfo::new(
        contact_info,
        keypair,
        SocketAddrSpace::Unspecified,
    ));
    let exit = Arc::new(AtomicBool::new(false));
    let optimistic_bank = OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let rpc_addr = SocketAddr::new(
        ip,
        solana_net_utils::find_available_port_in_range(ip, (10000, 65535)).unwrap(),
    );
    let (fast_sender, fast_receiver) = bounded(1);
    let service = JsonRpcService::new(
        rpc_addr,
        JsonRpcConfig::default_for_test(),
        None,
        bank_forks,
        Arc::new(RwLock::new(BlockCommitmentCache::new(
            HashMap::new(),
            0,
            CommitmentSlots::new_from_slot(bank.slot()),
        ))),
        blockstore,
        cluster_info,
        None, // No local leader/PoH: an RPC-only node must still forward.
        genesis.genesis_config.hash(),
        ledger.path(),
        create_validator_exit(exit.clone()),
        exit.clone(),
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicBool::new(true)),
        optimistic_bank,
        send_transaction_service::Config {
            tpu_peers: Some(vec![leader_tpu.local_addr().unwrap()]),
            batch_send_rate_ms: 1,
            retry_rate_ms: 100,
            ..send_transaction_service::Config::default()
        },
        Arc::new(MaxSlots::default()),
        Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
        Arc::new(ConnectionCache::with_udp("rpc_fast_path_test", 1)),
        Arc::new(AtomicU64::default()),
        Arc::new(AtomicU64::default()),
        Arc::new(PrioritizationFeeCache::default()),
        fast_sender,
    )
    .unwrap();
    let _guard = RpcGuard {
        service: Some(service),
        exit,
    };
    let client = RpcClient::new_with_timeout(format!("http://{rpc_addr}"), Duration::from_secs(5));
    test(
        &client,
        &bank,
        &genesis.mint_keypair,
        fast_receiver,
        &leader_tpu,
    );
}

fn submit(
    client: &RpcClient,
    tx: &VersionedTransaction,
    skip_preflight: bool,
) -> solana_client::client_error::Result<String> {
    client.send(RpcRequest::SendTransaction, json!([
        STANDARD.encode(bincode::serialize(tx).unwrap()),
        {"encoding": "base64", "skipPreflight": skip_preflight, "preflightCommitment": "processed", "maxRetries": 0}
    ]))
}

fn transfer(bank: &Bank, mint: &Keypair) -> VersionedTransaction {
    system_transaction::transfer(
        mint,
        &Pubkey::new_unique(),
        1_000_000,
        bank.last_blockhash(),
    )
    .into()
}

fn assert_forwarded(socket: &UdpSocket, tx: &VersionedTransaction) {
    let expected = bincode::serialize(tx).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut packet = [0; 1232];
    loop {
        socket
            .set_read_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))
            .unwrap();
        let size = socket
            .recv(&mut packet)
            .expect("RPC-only node must forward to the producer's TPU");
        if packet[..size] == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected transaction was not forwarded"
        );
    }
}

#[test]
fn test_rpc_missing_signatures_returns_error_and_keeps_process_alive() {
    const CHILD_ENV: &str = "GIGA_SVM_ZERO_SIGNATURE_CHILD";
    const CHILD_COMPLETED: &str = "GIGA_SVM_ZERO_SIGNATURE_CASES_COMPLETED";
    if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
        // A production panic hook exits the entire process, including when an
        // HTTP worker panics. Isolate it so the parent can report a regression.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "test_rpc_missing_signatures_returns_error_and_keeps_process_alive",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated RPC process did not survive: {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(CHILD_COMPLETED),
            "child did not complete the RPC regression cases"
        );
        return;
    }

    solana_metrics::set_panic_hook("validator", None);
    with_rpc(|client, bank, mint, fast, tpu| {
        let payer = Pubkey::new_unique();
        let instructions = [system_instruction::transfer(
            &payer,
            &Pubkey::new_unique(),
            1,
        )];
        let messages = [
            VersionedMessage::Legacy(Message::new_with_blockhash(
                &instructions,
                Some(&payer),
                &bank.last_blockhash(),
            )),
            VersionedMessage::V0(
                v0::Message::try_compile(&payer, &instructions, &[], bank.last_blockhash())
                    .unwrap(),
            ),
        ];
        for message in messages {
            // No signing key or funded payer is needed to construct this input.
            let transaction = VersionedTransaction {
                signatures: vec![],
                message,
            };
            let wire = bincode::serialize(&transaction).unwrap();
            for skip_preflight in [None, Some(false), Some(true)] {
                let params = match skip_preflight {
                    // No config object: exercise all sendTransaction defaults.
                    None => json!([bs58::encode(&wire).into_string()]),
                    Some(skip) => json!([
                        STANDARD.encode(&wire),
                        {"encoding": "base64", "skipPreflight": skip, "preflightCommitment": "processed"}
                    ]),
                };
                let error = client
                    .send::<String>(RpcRequest::SendTransaction, params)
                    .expect_err("missing signatures must return an RPC error");
                assert!(
                    matches!(
                        error.kind(),
                        ErrorKind::RpcError(RpcError::RpcResponseError { code: -32602, .. })
                    ),
                    "expected invalid params for missing signatures, got: {error}"
                );
                assert!(error.to_string().contains("invalid transaction"), "{error}");
                assert!(fast.try_recv().is_err());
                assert_eq!(
                    client
                        .get_slot_with_commitment(
                            solana_sdk::commitment_config::CommitmentConfig::processed()
                        )
                        .unwrap(),
                    bank.slot()
                );
            }
        }
        tpu.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert!(tpu.recv(&mut [0; 1232]).is_err());

        // The same HTTP service must still accept and deliver valid traffic.
        let valid = transfer(bank, mint);
        assert_eq!(
            submit(client, &valid, true).unwrap(),
            valid.signatures[0].to_string()
        );
        assert_eq!(
            fast.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .to_versioned_transaction(),
            valid
        );
        assert_forwarded(tpu, &valid);
    });
    println!("{CHILD_COMPLETED}");
}

#[test]
fn rpc_fast_path_rejects_invalid_signature_with_skip_preflight() {
    with_rpc(|client, bank, mint, fast, tpu| {
        let mut tx = transfer(bank, mint);
        tx.signatures[0] = Signature::default();
        let error = submit(client, &tx, true)
            .expect_err("invalid signature must be rejected before either queue");
        assert!(
            error.to_string().contains("signature verification failure"),
            "{error}"
        );
        assert!(fast.try_recv().is_err());
        tpu.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert!(tpu.recv(&mut [0; 1232]).is_err());
    });
}

#[test]
fn rpc_fast_path_rejects_invalid_secondary_signature() {
    with_rpc(|client, bank, mint, fast, _| {
        let other = Keypair::new();
        let mut tx: VersionedTransaction = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(
                &other.pubkey(),
                &Pubkey::new_unique(),
                1,
            )],
            Some(&mint.pubkey()),
            &[mint, &other],
            bank.last_blockhash(),
        )
        .into();
        tx.signatures[1] = Signature::default();
        let error =
            submit(client, &tx, true).expect_err("invalid secondary signature must be rejected");
        assert!(
            error.to_string().contains("signature verification failure"),
            "{error}"
        );
        assert!(fast.try_recv().is_err());
    });
}

#[test]
fn rpc_fast_path_rejects_invalid_v0_signature() {
    with_rpc(|client, bank, mint, fast, _| {
        let message = v0::Message::try_compile(
            &mint.pubkey(),
            &[system_instruction::transfer(
                &mint.pubkey(),
                &Pubkey::new_unique(),
                1_000_000,
            )],
            &[],
            bank.last_blockhash(),
        )
        .unwrap();
        let mut tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[mint]).unwrap();
        tx.signatures[0] = Signature::default();
        let error = submit(client, &tx, true).expect_err("invalid v0 signature must be rejected");
        assert!(
            error.to_string().contains("signature verification failure"),
            "{error}"
        );
        assert!(fast.try_recv().is_err());
    });
}

#[test]
fn rpc_fast_path_forwards_without_local_leadership_and_keeps_direct_copy() {
    with_rpc(|client, bank, mint, fast, tpu| {
        let tx = transfer(bank, mint);
        assert_eq!(
            submit(client, &tx, true).unwrap(),
            tx.signatures[0].to_string()
        );
        assert_eq!(
            fast.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .to_versioned_transaction(),
            tx
        );
        assert_forwarded(tpu, &tx);
    });
}

#[test]
fn rpc_fast_path_forwards_when_direct_queue_is_full() {
    with_rpc(|client, bank, mint, fast, tpu| {
        let first = transfer(bank, mint);
        submit(client, &first, true).unwrap();
        assert_eq!(fast.len(), 1);
        let second = transfer(bank, mint);
        submit(client, &second, true).unwrap();
        assert_forwarded(tpu, &second);
    });
}

#[test]
fn rpc_fast_path_forwards_when_direct_worker_is_disconnected() {
    with_rpc(|client, bank, mint, fast, tpu| {
        drop(fast);
        let tx = transfer(bank, mint);
        submit(client, &tx, true).unwrap();
        assert_forwarded(tpu, &tx);
    });
}

#[test]
fn rpc_fast_path_retries_account_contention_across_slots() {
    with_rpc(|client, bank, mint, _fast, tpu| {
        let leader_id = Pubkey::new_unique();
        let leader = Arc::new(Bank::new_from_parent(
            bank.clone(),
            &leader_id,
            bank.slot() + 1,
        ));
        let recipient = Pubkey::new_unique();
        let transfer =
            system_transaction::transfer(mint, &recipient, 1_000_000, leader.last_blockhash());
        let tx = VersionedTransaction::from(transfer.clone());
        let competing_transfer = system_transaction::transfer(
            mint,
            &Pubkey::new_unique(),
            1_000_000,
            leader.last_blockhash(),
        );
        let account_lock = leader
            .prepare_entry_batch(vec![competing_transfer.into()])
            .unwrap();
        assert_eq!(account_lock.lock_results(), &[Ok(())]);

        let signature: String = client.send(RpcRequest::SendTransaction, json!([
            STANDARD.encode(bincode::serialize(&tx).unwrap()),
            {"encoding": "base64", "skipPreflight": true, "preflightCommitment": "processed", "maxRetries": 1}
        ])).unwrap();
        assert_eq!(signature, tx.signatures[0].to_string());
        assert_forwarded(tpu, &tx);
        assert_eq!(
            leader.process_transaction(&transfer),
            Err(TransactionError::AccountInUse)
        );
        assert_eq!(leader.get_signature_status(&tx.signatures[0]), None);
        drop(account_lock);

        let next_slot = leader.slot() + 1;
        let next_leader = Bank::new_from_parent(leader, &leader_id, next_slot);
        // A distinct second datagram must come from the normal retry loop.
        assert_forwarded(tpu, &tx);
        assert_eq!(next_leader.process_transaction(&transfer), Ok(()));
        assert_eq!(next_leader.get_balance(&recipient), 1_000_000);
    });
}

#[test]
fn rpc_fast_path_forwards_v0_transactions() {
    with_rpc(|client, bank, mint, fast, tpu| {
        let message = v0::Message::try_compile(
            &mint.pubkey(),
            &[system_instruction::transfer(
                &mint.pubkey(),
                &Pubkey::new_unique(),
                1_000_000,
            )],
            &[],
            bank.last_blockhash(),
        )
        .unwrap();
        let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[mint]).unwrap();
        submit(client, &tx, true).unwrap();
        assert_eq!(
            fast.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .to_versioned_transaction(),
            tx
        );
        assert_forwarded(tpu, &tx);
    });
}

#[test]
fn rpc_fast_path_preserves_preflight_simulation() {
    with_rpc(|client, bank, mint, fast, tpu| {
        let tx = transfer(bank, mint);
        submit(client, &tx, false).unwrap();
        assert_eq!(
            fast.recv_timeout(Duration::from_secs(1))
                .unwrap()
                .to_versioned_transaction(),
            tx
        );
        assert_forwarded(tpu, &tx);
        let impossible: VersionedTransaction = system_transaction::transfer(
            mint,
            &Pubkey::new_unique(),
            u64::MAX,
            bank.last_blockhash(),
        )
        .into();
        assert!(submit(client, &impossible, false).is_err());
        assert!(fast.try_recv().is_err());
    });
}

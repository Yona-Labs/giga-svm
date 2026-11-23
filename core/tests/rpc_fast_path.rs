//! Verify direct execution and shutdown through the public BankingStage interface.
use {
    crossbeam_channel::{bounded, unbounded},
    solana_client::connection_cache::ConnectionCache,
    solana_core::{
        banking_stage::BankingStage, banking_trace::BankingTracer, validator::BlockProductionMethod,
    },
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_ledger::{
        blockstore::Blockstore, genesis_utils::create_genesis_config,
        get_tmp_ledger_path_auto_delete,
    },
    solana_poh::poh_recorder::create_test_recorder,
    solana_runtime::{
        bank::{Bank, NewBankOptions},
        prioritization_fee_cache::PrioritizationFeeCache,
    },
    solana_sdk::{
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        system_transaction,
        transaction::{SanitizedTransaction, TransactionError},
    },
    solana_streamer::socket::SocketAddrSpace,
    std::{
        sync::{atomic::Ordering, Arc},
        thread,
        time::Duration,
    },
};

fn check_worker(method: BlockProductionMethod, is_leader: bool, vote_only: bool) {
    let mut genesis = create_genesis_config(1_000_000_000);
    // Leave enough real time to observe execution without a slot-boundary race.
    genesis.genesis_config.poh_config.target_tick_duration = Duration::from_millis(100);
    let (parent_bank, bank_forks) =
        Bank::new_no_wallclock_throttle_for_tests(&genesis.genesis_config);
    let bank = if vote_only {
        let bank = Bank::new_from_parent_with_options(
            parent_bank,
            &Pubkey::new_unique(),
            1,
            NewBankOptions {
                vote_only_bank: true,
            },
        );
        bank_forks
            .write()
            .unwrap()
            .insert(bank)
            .clone_without_scheduler()
    } else {
        parent_bank
    };
    let ledger = get_tmp_ledger_path_auto_delete!();
    let blockstore = Arc::new(Blockstore::open(ledger.path()).unwrap());
    let (exit, recorder, poh_service, entries) = create_test_recorder(
        bank.clone(),
        blockstore,
        Some(genesis.genesis_config.poh_config.clone()),
        None,
    );
    if !is_leader {
        recorder.write().unwrap().reset(bank.clone(), None);
    }
    let keypair = Arc::new(Keypair::new());
    let cluster_info = Arc::new(ClusterInfo::new(
        ContactInfo::new_localhost(&keypair.pubkey(), 0),
        keypair,
        SocketAddrSpace::Unspecified,
    ));
    let tracer = BankingTracer::new_disabled();
    let (non_vote_sender, non_vote_receiver) = tracer.create_channel_non_vote();
    let (tpu_vote_sender, tpu_vote_receiver) = tracer.create_channel_tpu_vote();
    let (gossip_vote_sender, gossip_vote_receiver) = tracer.create_channel_gossip_vote();
    let (vote_sender, _vote_receiver) = unbounded();
    let (fast_sender, fast_receiver) = bounded(128);
    let queue_observer = fast_receiver.clone();
    let stage = BankingStage::new(
        method,
        &cluster_info,
        &recorder,
        non_vote_receiver,
        tpu_vote_receiver,
        gossip_vote_receiver,
        None,
        vote_sender,
        None,
        Arc::new(ConnectionCache::with_udp("rpc_worker_test", 1)),
        bank_forks,
        &Arc::new(PrioritizationFeeCache::default()),
        fast_receiver,
    );
    let recipient = Pubkey::new_unique();
    let tx = system_transaction::transfer(
        &genesis.mint_keypair,
        &recipient,
        1_000_000,
        bank.last_blockhash(),
    );
    fast_sender
        .send(SanitizedTransaction::from_transaction_for_tests(tx.clone()))
        .unwrap();
    if is_leader && !vote_only {
        let record_deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = record_deadline.saturating_duration_since(std::time::Instant::now());
            let (_, (entry, _)) = entries
                .recv_timeout(remaining)
                .expect("fast-path transaction must be recorded without TPU input");
            if entry
                .transactions
                .iter()
                .any(|recorded| recorded.signatures[0] == tx.signatures[0])
            {
                break;
            }
        }
        // Recording precedes commit. Wait for the actual public bank state.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while bank.get_signature_status(&tx.signatures[0]) != Some(Ok(())) {
            assert!(
                std::time::Instant::now() < deadline,
                "direct transaction was not committed"
            );
            thread::yield_now();
        }
        assert_eq!(bank.get_balance(&recipient), 1_000_000);
        // The reliable TPU copy must not be able to apply the transfer twice.
        loop {
            match bank.process_transaction(&tx) {
                Err(TransactionError::AccountInUse) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "direct batch did not release account locks"
                    );
                    thread::yield_now();
                }
                result => {
                    assert_eq!(result, Err(TransactionError::AlreadyProcessed));
                    break;
                }
            }
        }
        assert_eq!(bank.get_balance(&recipient), 1_000_000);
    }
    if vote_only {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while queue_observer.len() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "vote-only transaction was not drained"
            );
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(50));
        assert_eq!(bank.get_signature_status(&tx.signatures[0]), None);
        assert_eq!(bank.get_balance(&recipient), 0);
        while let Ok((_, (entry, _))) = entries.try_recv() {
            assert!(!entry
                .transactions
                .iter()
                .any(|recorded| recorded.signatures[0] == tx.signatures[0]));
        }
    }
    drop(fast_sender);
    drop(non_vote_sender);
    drop(tpu_vote_sender);
    drop(gossip_vote_sender);
    exit.store(true, Ordering::Relaxed);
    let (done_sender, done_receiver) = bounded(1);
    let join = thread::spawn(move || {
        stage.join().unwrap();
        done_sender.send(()).unwrap();
    });
    done_receiver
        .recv_timeout(Duration::from_secs(3))
        .expect("RPC worker must stop after disconnect, including with no leader bank");
    join.join().unwrap();
    poh_service.join().unwrap();
    if !is_leader {
        assert_eq!(bank.get_balance(&recipient), 0);
    }
}

#[test]
fn rpc_fast_path_executes_on_leader_without_tpu_round_trip() {
    for method in [
        BlockProductionMethod::ThreadLocalMultiIterator,
        BlockProductionMethod::CentralScheduler,
    ] {
        check_worker(method, true, false);
    }
}

#[test]
fn rpc_fast_path_skips_non_votes_for_vote_only_bank() {
    for method in [
        BlockProductionMethod::ThreadLocalMultiIterator,
        BlockProductionMethod::CentralScheduler,
    ] {
        check_worker(method, true, true);
    }
}

#[test]
fn rpc_fast_path_does_not_wait_for_leadership_or_hang_on_shutdown() {
    for method in [
        BlockProductionMethod::ThreadLocalMultiIterator,
        BlockProductionMethod::CentralScheduler,
    ] {
        check_worker(method, false, false);
    }
}

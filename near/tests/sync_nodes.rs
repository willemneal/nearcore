use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use actix::{Actor, Addr, System};
use futures::future::Future;
use tempdir::TempDir;

use futures::future;
use near::config::TESTING_INIT_STAKE;
use near::{load_test_config, start_with_config, GenesisConfig, NightshadeRuntime};
use near_chain::{Block, BlockHeader, Chain};
use near_client::{ClientActor, GetBlock};
use near_network::test_utils::{convert_boot_nodes, open_port, WaitOrTimeout};
use near_network::{NetworkClientMessages, PeerInfo};
use near_primitives::crypto::signer::InMemorySigner;
use near_primitives::serialize::BaseEncode;
use near_primitives::test_utils::{init_integration_logger, init_test_logger};
use near_primitives::transaction::{StakeTransaction, TransactionBody};
use near_store::test_utils::create_test_store;
use std::time::Duration;

/// Utility to generate genesis header from config for testing purposes.
fn genesis_header(genesis_config: GenesisConfig) -> BlockHeader {
    let dir = TempDir::new("unused").unwrap();
    let store = create_test_store();
    let genesis_time = genesis_config.genesis_time.clone();
    let runtime = Arc::new(NightshadeRuntime::new(dir.path(), store.clone(), genesis_config));
    let chain = Chain::new(store, runtime, genesis_time).unwrap();
    chain.genesis().clone()
}

// This assumes that there is no index skipped. Otherwise epoch hash calculation will be wrong.
fn add_blocks(
    start: &BlockHeader,
    client: Addr<ClientActor>,
    num: usize,
    signer: Arc<InMemorySigner>,
) -> BlockHeader {
    let mut blocks = vec![];
    let mut prev = start;
    for _ in 0..num {
        let block = Block::empty(prev, signer.clone());
        let _ = client.do_send(NetworkClientMessages::Block(
            block.clone(),
            PeerInfo::random().id,
            false,
        ));
        blocks.push(block);
        prev = &blocks[blocks.len() - 1].header;
    }
    blocks[blocks.len() - 1].header.clone()
}

/// One client is in front, another must sync to it before they can produce blocks.
#[test]
fn sync_nodes() {
    init_test_logger();

    let mut genesis_config = GenesisConfig::test(vec!["other"]);
    genesis_config.epoch_length = 5;
    let genesis_header = genesis_header(genesis_config.clone());

    let (port1, port2) = (open_port(), open_port());
    let mut near1 = load_test_config("test1", port1, &genesis_config);
    near1.network_config.boot_nodes = convert_boot_nodes(vec![("test2", port2)]);
    let mut near2 = load_test_config("test2", port2, &genesis_config);
    near2.network_config.boot_nodes = convert_boot_nodes(vec![("test1", port1)]);

    let system = System::new("NEAR");

    let dir1 = TempDir::new("sync_nodes_1").unwrap();
    let (client1, _) = start_with_config(dir1.path(), near1);

    let signer = Arc::new(InMemorySigner::from_seed("other", "other"));
    let _ = add_blocks(&genesis_header, client1, 11, signer);

    let dir2 = TempDir::new("sync_nodes_2").unwrap();
    let (_, view_client2) = start_with_config(dir2.path(), near2);

    WaitOrTimeout::new(
        Box::new(move |_ctx| {
            actix::spawn(view_client2.send(GetBlock::Best).then(|res| {
                match &res {
                    Ok(Ok(b)) if b.header.height == 11 => System::current().stop(),
                    Err(_) => return futures::future::err(()),
                    _ => {}
                };
                futures::future::ok(())
            }));
        }),
        100,
        60000,
    )
    .start();

    system.run().unwrap();
}

/// Clients connect and then one of them becomes in front. The other one must then sync to it.
#[test]
fn sync_after_sync_nodes() {
    init_test_logger();

    let mut genesis_config = GenesisConfig::test(vec!["other"]);
    genesis_config.epoch_length = 5;
    let genesis_header = genesis_header(genesis_config.clone());

    let (port1, port2) = (open_port(), open_port());
    let mut near1 = load_test_config("test1", port1, &genesis_config);
    near1.network_config.boot_nodes = convert_boot_nodes(vec![("test2", port2)]);
    let mut near2 = load_test_config("test2", port2, &genesis_config);
    near2.network_config.boot_nodes = convert_boot_nodes(vec![("test1", port1)]);

    let system = System::new("NEAR");

    let dir1 = TempDir::new("sync_nodes_1").unwrap();
    let (client1, _) = start_with_config(dir1.path(), near1);

    let dir2 = TempDir::new("sync_nodes_2").unwrap();
    let (_, view_client2) = start_with_config(dir2.path(), near2);

    let signer = Arc::new(InMemorySigner::from_seed("other", "other"));
    let last_block = add_blocks(&genesis_header, client1.clone(), 11, signer.clone());

    let next_step = Arc::new(AtomicBool::new(false));
    WaitOrTimeout::new(
        Box::new(move |_ctx| {
            let last_block1 = last_block.clone();
            let client11 = client1.clone();
            let signer1 = signer.clone();
            let next_step1 = next_step.clone();
            actix::spawn(view_client2.send(GetBlock::Best).then(move |res| {
                match &res {
                    Ok(Ok(b)) if b.header.height == 11 => {
                        if !next_step1.load(Ordering::Relaxed) {
                            let _ = add_blocks(&last_block1, client11, 11, signer1);
                            next_step1.store(true, Ordering::Relaxed);
                        }
                    }
                    Ok(Ok(b)) if b.header.height > 20 => System::current().stop(),
                    Err(_) => return futures::future::err(()),
                    _ => {}
                };
                futures::future::ok(())
            }));
        }),
        100,
        60000,
    )
    .start();

    system.run().unwrap();
}

/// Starts one validation node, it reduces it's stake to 1/2 of the stake.
/// Second node starts after 1s, needs to catchup & state sync and then make sure it's
#[test]
fn sync_state_stake_change() {
    // init_test_logger();
    init_integration_logger();

    let mut genesis_config = GenesisConfig::test(vec!["test1"]);
    genesis_config.epoch_length = 5;

    let (port1, port2) = (open_port(), open_port());
    let mut near1 = load_test_config("test1", port1, &genesis_config);
    near1.network_config.boot_nodes = convert_boot_nodes(vec![("test2", port2)]);
    near1.client_config.min_block_production_delay = Duration::from_millis(100);
    let mut near2 = load_test_config("test2", port2, &genesis_config);
    near2.network_config.boot_nodes = convert_boot_nodes(vec![("test1", port1)]);
    near2.client_config.min_block_production_delay = Duration::from_millis(100);
    near2.client_config.min_num_peers = 1;
    near2.client_config.skip_sync_wait = false;

    let system = System::new("NEAR");

    let unstake_transaction = TransactionBody::Stake(StakeTransaction {
        nonce: 1,
        originator: "test1".to_string(),
        amount: TESTING_INIT_STAKE / 2,
        public_key: near1.block_producer.as_ref().unwrap().signer.public_key().to_base(),
    })
    .sign(&*near1.block_producer.as_ref().unwrap().signer);

    let dir1 = TempDir::new("sync_state_stake_change_1").unwrap();
    let dir2 = TempDir::new("sync_state_stake_change_2").unwrap();
    let (client1, view_client1) = start_with_config(dir1.path(), near1);

    actix::spawn(
        client1
            .send(NetworkClientMessages::Transaction(unstake_transaction))
            .map(|_| ())
            .map_err(|_| ()),
    );

    let started = Arc::new(AtomicBool::new(false));
    let dir2_path = dir2.path().to_path_buf();
    WaitOrTimeout::new(
        Box::new(move |_ctx| {
            let started_copy = started.clone();
            let near2_copy = near2.clone();
            let dir2_path_copy = dir2_path.clone();
            actix::spawn(view_client1.send(GetBlock::Best).then(move |res| {
                let latest_block_height = res.unwrap().unwrap().header.height;
                if !started_copy.load(Ordering::SeqCst) && latest_block_height > 10 {
                    started_copy.store(true, Ordering::SeqCst);
                    let (_, view_client2) = start_with_config(&dir2_path_copy, near2_copy);

                    WaitOrTimeout::new(
                        Box::new(move |_ctx| {
                            actix::spawn(view_client2.send(GetBlock::Best).then(move |res| {
                                if res.unwrap().unwrap().header.height > latest_block_height + 1 {
                                    System::current().stop()
                                }
                                future::result::<_, ()>(Ok(()))
                            }));
                        }),
                        100,
                        30000,
                    )
                    .start();
                }
                future::result(Ok(()))
            }));
        }),
        100,
        35000,
    )
    .start();

    system.run().unwrap();
}

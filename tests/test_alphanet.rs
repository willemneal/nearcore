/// This module does end-to-end testing of the testnet by spinning up multiple nodes and
/// exercising them in different scenarios.
/// Note: tests get executed in parallel, so use different ports / names.

use std::process::Command;

use alphanet::testing_utils::{check_result, configure_chain_spec, Node, wait};
use primitives::transaction::TransactionBody;

#[test]
fn test_two_nodes() {
    let chain_spec = configure_chain_spec();
    // Create boot node.
    let alice = Node::new("t1_alice", "alice.near", 1, Some("127.0.0.1:3000"), 3030, vec![], chain_spec.clone());
    // Create secondary node that boots from the alice node.
    let bob = Node::new("t1_bob", "bob.near", 2, Some("127.0.0.1:3001"), 3031, vec![alice.node_info.clone()], chain_spec);

    // Start both nodes.
    alice.start();
    bob.start();

    // Create an account on alice node.
    Command::new("pynear")
        .arg("create_account")
        .arg("jason")
        .arg("1")
        .arg("-u")
        .arg("http://127.0.0.1:3030/")
        .output()
        .expect("create_account command failed to process");

    // Wait until this account is present on the bob.near node.
    let view_account = || -> bool {
        let res = Command::new("pynear")
            .arg("view_account")
            .arg("-a")
            .arg("jason")
            .arg("-u")
            .arg("http://127.0.0.1:3031/")
            .output()
            .expect("view_account command failed to process");
        check_result(res).is_ok()
    };
    wait(view_account, 500, 60000);
}

#[test]
fn test_multiple_nodes() {
    // Modify the following two variables to run more nodes or to exercise them for multiple
    // trials.
    let num_nodes = 2;
    let num_trials = 1;

    let init_balance = 1_000_000_000;
    let mut account_names = vec![];
    let mut node_names = vec![];
    for i in 0..num_nodes {
        account_names.push(format!("near.{}", i));
        node_names.push(format!("node_{}", i));
    }

    let chain_spec = configure_chain_spec();
//    let chain_spec = generate_test_chain_spec(&account_names, init_balance);

    let mut nodes = vec![];
    let mut boot_nodes = vec![];
    // Launch nodes in a chain, such that X+1 node boots from X node.
    for i in 0..num_nodes {
        let node = Node::new(
            node_names[i].as_str(),
            account_names[i].as_str(),
            i as u32 + 1,
            Some(format!("127.0.0.1:{}", 3000 + i).as_str()),
            3030 + i as u16,
            boot_nodes,
            chain_spec.clone(),
        );
        boot_nodes = vec![node.node_info.clone()];
        node.start();
        nodes.push(node);
    }
    //        thread::sleep(Duration::from_secs(10));

    // Execute N trials. In each trial we submit a transaction to a random node i, that sends
    // 1 token to a random node j. Then we wait for the balance change to propagate by checking
    // the balance of j on node k.
    let mut expected_balances = vec![init_balance; num_nodes];
    let mut nonces = vec![1; num_nodes];
    let trial_duration = 10000;
    for trial in 0..num_trials {
        println!("TRIAL #{}", trial);
        let i = rand::random::<usize>() % num_nodes;
        // Should be a different node.
        let mut j = rand::random::<usize>() % (num_nodes - 1);
        if j >= i {
            j += 1;
        }
        for k in 0..num_nodes {
            nodes[k]
                .client
                .shard_client
                .pool
                .add_transaction(
                    TransactionBody::send_money(
                        nonces[i],
                        account_names[i].as_str(),
                        account_names[j].as_str(),
                        1,
                    )
                        .sign(nodes[i].signer()),
                )
                .unwrap();
        }
        nonces[i] += 1;
        expected_balances[i] -= 1;
        expected_balances[j] += 1;

        wait(
            || {
                let mut state_update = nodes[j].client.shard_client.get_state_update();
                let amt = nodes[j]
                    .client
                    .shard_client
                    .trie_viewer
                    .view_account(&mut state_update, &account_names[j])
                    .unwrap()
                    .amount;
                expected_balances[j] == amt
            },
            1000,
            trial_duration,
        );
    }
}

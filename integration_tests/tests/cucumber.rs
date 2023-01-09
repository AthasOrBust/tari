//   Copyright 2022. The Tari Project
//
//   Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//   following conditions are met:
//
//   1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//   disclaimer.
//
//   2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//   following disclaimer in the documentation and/or other materials provided with the distribution.
//
//   3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//   products derived from this software without specific prior written permission.
//
//   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//   INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//   SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//   WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//   USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
#![feature(internal_output_capture)]

mod utils;

use std::{
    path::PathBuf,
    str,
    sync::{Arc, Mutex},
    time::Duration,
};

use cucumber::{event::ScenarioFinished, gherkin::Scenario, given, then, when, World as _};
use futures::StreamExt;
use indexmap::IndexMap;
use log::*;
use tari_app_grpc::tari_rpc as grpc;
use tari_base_node_grpc_client::grpc::{GetBlocksRequest, ListHeadersRequest};
use tari_common::{configuration::Network, initialize_logging};
use tari_core::{
    consensus::ConsensusManager,
    transactions::transaction_components::{Transaction, UnblindedOutput},
};
use tari_integration_tests::error::GrpcBaseNodeError;
use tari_utilities::hex::Hex;
use tari_wallet::transaction_service::config::TransactionRoutingMechanism;
use tari_wallet_grpc_client::grpc::{
    ClaimHtlcRefundRequest,
    ClaimShaAtomicSwapRequest,
    Empty,
    GetBalanceRequest,
    GetCompletedTransactionsRequest,
    GetIdentityRequest,
    GetTransactionInfoRequest,
    PaymentRecipient,
    SendShaAtomicSwapRequest,
    TransferRequest,
};
use thiserror::Error;
use tokio::runtime::Runtime;

use crate::utils::{
    base_node_process::{spawn_base_node, BaseNodeProcess},
    miner::{
        mine_block,
        mine_block_with_coinbase_on_node,
        mine_blocks_without_wallet,
        register_miner_process,
        MinerProcess,
    },
    transaction::build_transaction_with_output,
    wallet_process::{create_wallet_client, spawn_wallet, WalletProcess},
};

pub const LOG_TARGET: &str = "cucumber";
pub const LOG_TARGET_STDOUT: &str = "stdout";
const CONFIRMATION_PERIOD: u64 = 4;
const NUM_RETIRES: u64 = 240;
const RETRY_TIME_IN_MS: u64 = 250;

#[derive(Error, Debug)]
pub enum TariWorldError {
    #[error("Base node process not found: {0}")]
    BaseNodeProcessNotFound(String),
    #[error("Wallet process not found: {0}")]
    WalletProcessNotFound(String),
    #[error("Miner process not found: {0}")]
    MinerProcessNotFound(String),
    #[error("Base node error: {0}")]
    GrpcBaseNodeError(#[from] GrpcBaseNodeError),
    #[error("No base node, or wallet client found: {0}")]
    ClientNotFound(String),
}

#[derive(Debug, Default, cucumber::World)]
pub struct TariWorld {
    seed_nodes: Vec<String>,
    base_nodes: IndexMap<String, BaseNodeProcess>,
    wallets: IndexMap<String, WalletProcess>,
    miners: IndexMap<String, MinerProcess>,
    transactions: IndexMap<String, Transaction>,
    // mapping from hex string of public key of wallet client to tx_id's
    wallet_tx_ids: IndexMap<String, Vec<u64>>,
    utxos: IndexMap<String, UnblindedOutput>,
    output_hash: Option<String>,
    pre_image: Option<String>,
}

enum NodeClient {
    BaseNode(tari_base_node_grpc_client::BaseNodeGrpcClient<tonic::transport::Channel>),
    Wallet(tari_wallet_grpc_client::WalletGrpcClient<tonic::transport::Channel>),
}

impl TariWorld {
    async fn get_node_client<S: AsRef<str>>(
        &self,
        name: &S,
    ) -> anyhow::Result<tari_base_node_grpc_client::BaseNodeGrpcClient<tonic::transport::Channel>> {
        self.get_node(name)?.get_grpc_client().await
    }

    async fn get_base_node_or_wallet_client<S: core::fmt::Debug + AsRef<str>>(
        &self,
        name: S,
    ) -> anyhow::Result<NodeClient> {
        match self.get_node_client(&name).await {
            Ok(client) => Ok(NodeClient::BaseNode(client)),
            Err(_) => match self.get_wallet_client(&name).await {
                Ok(wallet) => Ok(NodeClient::Wallet(wallet)),
                Err(e) => Err(TariWorldError::ClientNotFound(e.to_string()).into()),
            },
        }
    }

    #[allow(dead_code)]
    async fn get_wallet_client<S: AsRef<str>>(
        &self,
        name: &S,
    ) -> anyhow::Result<tari_wallet_grpc_client::WalletGrpcClient<tonic::transport::Channel>> {
        self.get_wallet(name)?.get_grpc_client().await
    }

    fn get_node<S: AsRef<str>>(&self, node_name: &S) -> anyhow::Result<&BaseNodeProcess> {
        Ok(self
            .base_nodes
            .get(node_name.as_ref())
            .ok_or_else(|| TariWorldError::BaseNodeProcessNotFound(node_name.as_ref().to_string()))?)
    }

    fn get_wallet<S: AsRef<str>>(&self, wallet_name: &S) -> anyhow::Result<&WalletProcess> {
        Ok(self
            .wallets
            .get(wallet_name.as_ref())
            .ok_or_else(|| TariWorldError::WalletProcessNotFound(wallet_name.as_ref().to_string()))?)
    }

    fn get_miner<S: AsRef<str>>(&self, miner_name: S) -> anyhow::Result<&MinerProcess> {
        Ok(self
            .miners
            .get(miner_name.as_ref())
            .ok_or_else(|| TariWorldError::MinerProcessNotFound(miner_name.as_ref().to_string()))?)
    }

    pub fn all_seed_nodes(&self) -> &[String] {
        self.seed_nodes.as_slice()
    }

    pub async fn after(&mut self, _scenario: &Scenario) {
        self.base_nodes.clear();
        self.seed_nodes.clear();
        self.wallets.clear();
        self.miners.clear();
    }
}

#[given(expr = "I have a seed node {word}")]
#[when(expr = "I have a seed node {word}")]
async fn start_base_node(world: &mut TariWorld, name: String) {
    spawn_base_node(world, true, name, vec![], None).await;
}

#[given(expr = "a wallet {word} connected to base node {word}")]
async fn start_wallet(world: &mut TariWorld, wallet_name: String, node_name: String) {
    let seeds = world.base_nodes.get(&node_name).unwrap().seed_nodes.clone();
    spawn_wallet(world, wallet_name, Some(node_name), seeds, None).await;
}

#[when(expr = "I have a base node {word} connected to all seed nodes")]
async fn start_base_node_connected_to_all_seed_nodes(world: &mut TariWorld, name: String) {
    spawn_base_node(world, false, name, world.all_seed_nodes().to_vec(), None).await;
}

#[when(expr = "I have {int} base nodes connected to all seed nodes")]
async fn multiple_base_nodes_connected_to_all_seeds(world: &mut TariWorld, nodes: u64) {
    for i in 0..nodes {
        let node = format!("Node_{}", i);
        println!("Initializing node {}", node.clone());
        spawn_base_node(world, false, node, world.all_seed_nodes().to_vec(), None).await;
    }
}

#[when(expr = "I have wallet {word} connected to all seed nodes")]
async fn start_wallet_connected_to_all_seed_nodes(world: &mut TariWorld, name: String) {
    spawn_wallet(world, name, None, world.all_seed_nodes().to_vec(), None).await;
}

#[when(expr = "I have mining node {word} connected to base node {word} and wallet {word}")]
async fn create_miner(world: &mut TariWorld, miner_name: String, bn_name: String, wallet_name: String) {
    register_miner_process(world, miner_name, bn_name, wallet_name);
}

#[when(expr = "I wait {int} seconds")]
async fn wait_seconds(_world: &mut TariWorld, seconds: u64) {
    tokio::time::sleep(Duration::from_secs(seconds)).await;
}

#[when(expr = "I wait for {word} to connect to {word}")]
#[then(expr = "I wait for {word} to connect to {word}")]
#[then(expr = "{word} is connected to {word}")]
async fn node_pending_connection_to(world: &mut TariWorld, first_node: String, second_node: String) {
    let mut node_client = world.get_base_node_or_wallet_client(&first_node).await.unwrap();
    let second_client = world.get_base_node_or_wallet_client(&second_node).await.unwrap();

    let second_client_pubkey = match second_client {
        NodeClient::Wallet(mut client) => {
            client
                .identify(GetIdentityRequest {})
                .await
                .unwrap()
                .into_inner()
                .public_key
        },
        NodeClient::BaseNode(mut client) => client.identify(Empty {}).await.unwrap().into_inner().public_key,
    };

    for _i in 0..100 {
        let res = match node_client {
            NodeClient::Wallet(ref mut client) => client.list_connected_peers(Empty {}).await.unwrap(),
            NodeClient::BaseNode(ref mut client) => client.list_connected_peers(Empty {}).await.unwrap(),
        };
        let res = res.into_inner();

        if res.connected_peers.iter().any(|p| p.public_key == second_client_pubkey) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    panic!("Peer was not connected in time");
}

#[when(expr = "mining node {word} mines {int} blocks")]
#[given(expr = "mining node {word} mines {int} blocks")]
async fn run_miner(world: &mut TariWorld, miner_name: String, num_blocks: u64) {
    world.get_miner(miner_name).unwrap().mine(world, Some(num_blocks)).await;
}

#[then(expr = "all nodes are at height {int}")]
#[when(expr = "all nodes are at height {int}")]
async fn all_nodes_are_at_height(world: &mut TariWorld, height: u64) {
    let num_retries = NUM_RETIRES * height; // About 2 minutes per block
    let mut nodes_at_height: IndexMap<&String, u64> = IndexMap::new();

    for (name, _) in world.base_nodes.iter() {
        nodes_at_height.insert(name, 0);
    }

    for _ in 0..num_retries {
        for (name, _) in nodes_at_height
            .clone()
            .iter()
            .filter(|(_, at_height)| at_height != &&height)
        {
            let mut client = world.get_node_client(name).await.unwrap();

            let chain_tip = client.get_tip_info(Empty {}).await.unwrap().into_inner();
            let chain_hgt = chain_tip.metadata.unwrap().height_of_longest_chain;

            nodes_at_height.insert(name, chain_hgt);
        }

        if nodes_at_height.values().all(|h| h == &height) {
            return;
        }

        tokio::time::sleep(Duration::from_millis(RETRY_TIME_IN_MS)).await;
    }

    panic!(
        "base nodes not successfully synchronized at height {}, {:?}",
        height, nodes_at_height
    );
}

#[when(expr = "node {word} is at height {int}")]
#[then(expr = "node {word} is at height {int}")]
async fn node_is_at_height(world: &mut TariWorld, base_node: String, height: u64) {
    let num_retries = NUM_RETIRES; // About two minutes

    let mut client = world.get_node_client(&base_node).await.unwrap();
    let mut chain_hgt = 0;

    for _ in 0..=num_retries {
        let chain_tip = client.get_tip_info(Empty {}).await.unwrap().into_inner();
        chain_hgt = chain_tip.metadata.unwrap().height_of_longest_chain;

        if chain_hgt >= height {
            return;
        }

        tokio::time::sleep(Duration::from_millis(RETRY_TIME_IN_MS)).await;
    }

    // base node didn't synchronize successfully at height, so we bail out
    panic!(
        "base node didn't synchronize successfully with height {}, current chain height {}",
        height, chain_hgt
    );
}

#[when(expr = "I wait for wallet {word} to have at least {int} uT")]
#[then(expr = "I wait for wallet {word} to have at least {int} uT")]
async fn wait_for_wallet_to_have_micro_tari(world: &mut TariWorld, wallet: String, amount: u64) {
    let wallet = world.wallets.get(&wallet).unwrap();
    let num_retries = 100;

    let mut client = wallet.get_grpc_client().await.unwrap();
    let mut curr_amount = 0;

    for _ in 0..=num_retries {
        curr_amount = client
            .get_balance(GetBalanceRequest {})
            .await
            .unwrap()
            .into_inner()
            .available_balance;

        if curr_amount >= amount {
            return;
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // failed to get wallet right amount, so we panic
    panic!(
        "wallet failed to get right amount {}, current amount is {}",
        amount, curr_amount
    );
}

#[given(expr = "I have a base node {word} connected to seed {word}")]
#[when(expr = "I have a base node {word} connected to seed {word}")]
async fn base_node_connected_to_seed(world: &mut TariWorld, base_node: String, seed: String) {
    spawn_base_node(world, false, base_node, vec![seed], None).await;
}

#[then(expr = "I mine {int} blocks on {word}")]
#[when(expr = "I mine {int} blocks on {word}")]
async fn mine_blocks_on(world: &mut TariWorld, blocks: u64, base_node: String) {
    let mut client = world
        .get_node_client(&base_node)
        .await
        .expect("Couldn't get the node client to mine with");
    mine_blocks_without_wallet(&mut client, blocks).await;
}

#[when(expr = "I have wallet {word} connected to base node {word}")]
async fn wallet_connected_to_base_node(world: &mut TariWorld, wallet: String, base_node: String) {
    let bn = world.base_nodes.get(&base_node).unwrap();
    let peer_seeds = bn.seed_nodes.clone();
    spawn_wallet(world, wallet, Some(base_node), peer_seeds, None).await;
}

#[when(expr = "mining node {word} mines {int} blocks with min difficulty {int} and max difficulty {int}")]
async fn mining_node_mines_blocks_with_difficulty(
    _world: &mut TariWorld,
    _miner: String,
    _blocks: u64,
    _min_difficulty: u64,
    _max_difficulty: u64,
) {
}

#[when(expr = "I have a base node {word}")]
#[given(expr = "I have a base node {word}")]
async fn create_and_add_base_node(world: &mut TariWorld, base_node: String) {
    spawn_base_node(world, false, base_node, vec![], None).await;
}

#[given(expr = "I have {int} seed nodes")]
async fn have_seed_nodes(world: &mut TariWorld, seed_nodes: u64) {
    for node in 0..seed_nodes {
        spawn_base_node(world, true, format!("seed_node_{}", node), vec![], None).await;
    }
}

#[when(expr = "I have wallet {word} connected to seed node {word}")]
async fn have_wallet_connect_to_seed_node(world: &mut TariWorld, wallet: String, seed_node: String) {
    spawn_wallet(world, wallet, None, vec![seed_node], None).await;
}

#[when(expr = "I mine a block on {word} with coinbase {word}")]
async fn mine_block_with_coinbase_on_node_step(world: &mut TariWorld, base_node: String, coinbase_name: String) {
    mine_block_with_coinbase_on_node(world, base_node, coinbase_name).await;
}

#[then(expr = "{word} has {word} in {word} state")]
async fn transaction_in_state(
    world: &mut TariWorld,
    node: String,
    tx_name: String,
    state: String,
) -> anyhow::Result<()> {
    let mut client = world.get_node_client(&node).await?;
    let tx = world
        .transactions
        .get(&tx_name)
        .unwrap_or_else(|| panic!("Couldn't find transaction {}", tx_name));
    let sig = &tx.body.kernels()[0].excess_sig;

    let resp = client
        .transaction_state(grpc::TransactionStateRequest {
            excess_sig: Some(sig.into()),
        })
        .await?;

    let inner = resp.into_inner();

    let res_state = match inner.result {
        0 => "UNKNOWN",
        1 => "MEMPOOL",
        2 => "NOT STORED",
        3 => "MINED",
        _ => panic!("not getting a good result"),
    };

    if res_state == state {
        Ok(())
    } else {
        panic!(
            "The node {} has tx {} in state {} instead of the expected {}",
            node, tx_name, res_state, state
        );
    }
}

#[then(expr = "{word} is in the {word} of all nodes, where {int}% can fail")]
async fn tx_in_state_all_nodes(
    world: &mut TariWorld,
    tx_name: String,
    pool: String,
    can_fail_percent: u64,
) -> anyhow::Result<()> {
    let tx = world
        .transactions
        .get(&tx_name)
        .unwrap_or_else(|| panic!("Couldn't find transaction {}", tx_name));
    let sig = &tx.body.kernels()[0].excess_sig;

    let num_retries = 480; // About 2 minutes
    let mut node_pool_status: IndexMap<&String, bool> = IndexMap::new();

    let nodes = world.base_nodes.iter().clone();
    let nodes_count = world.base_nodes.len();

    for (name, _) in nodes.clone() {
        node_pool_status.insert(name, false);
    }

    let can_fail = ((can_fail_percent as f64 * nodes.len() as f64) / 100.0).ceil() as u64;

    for _ in 0..num_retries {
        for (name, _) in node_pool_status
            .clone()
            .iter()
            .filter(|(_, in_pool)| in_pool == &&false)
        {
            let mut client = world.get_node_client(name).await?;

            let resp = client
                .transaction_state(grpc::TransactionStateRequest {
                    excess_sig: Some(sig.into()),
                })
                .await?;

            let inner = resp.into_inner();

            let res_state = match inner.result {
                0 => "UNKNOWN",
                1 => "MEMPOOL",
                2 => "NOT STORED",
                3 => "MINED",
                _ => panic!("not getting a good result"),
            };

            if pool == res_state {
                node_pool_status.insert(name, true);
            }
        }

        if node_pool_status
            .values()
            .filter(|v| v == &&true)
            .collect::<Vec<_>>()
            .len() >
            (nodes_count - can_fail as usize)
        {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    panic!(
        "Too many nodes failed to get {} in the {}, {:?}, can_fail: {}",
        tx_name, pool, node_pool_status, can_fail
    );
}

#[then(expr = "I submit transaction {word} to {word}")]
#[when(expr = "I submit transaction {word} to {word}")]
async fn submit_transaction_to(world: &mut TariWorld, tx_name: String, node: String) -> anyhow::Result<()> {
    let mut client = world.get_node_client(&node).await?;
    let tx = world
        .transactions
        .get(&tx_name)
        .unwrap_or_else(|| panic!("Couldn't find transaction {}", tx_name));
    let resp = client
        .submit_transaction(grpc::SubmitTransactionRequest {
            transaction: Some(grpc::Transaction::try_from(tx.clone()).unwrap()),
        })
        .await?;

    let result = resp.into_inner();

    if result.result == 1 {
        Ok(())
    } else {
        panic!("Transaction wasn't submit")
    }
}

#[given(expr = "I have a pruned node {word} connected to node {word} with pruning horizon set to {int}")]
async fn prune_node_connected_to_base_node(
    world: &mut TariWorld,
    pruned_node: String,
    base_node: String,
    pruning_horizon: u64,
) {
    spawn_base_node(world, false, pruned_node, vec![base_node], Some(pruning_horizon)).await;
}

#[when(expr = "wallet {word} detects all transactions as Mined_Confirmed")]
async fn wallet_detects_all_txs_as_mined_confirmed(world: &mut TariWorld, wallet_name: String) {
    let mut client = create_wallet_client(world, wallet_name.clone()).await.unwrap();
    let wallet_address = client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();
    let tx_ids = world.wallet_tx_ids.get(&wallet_address).unwrap();

    let num_retries = 100;

    for tx_id in tx_ids {
        println!("waiting for tx with tx_id = {} to be mined_confirmed", tx_id);
        for retry in 0..=num_retries {
            let request = GetTransactionInfoRequest {
                transaction_ids: vec![*tx_id],
            };
            let tx_info = client.get_transaction_info(request).await.unwrap().into_inner();
            let tx_info = tx_info.transactions.first().unwrap();

            if retry == num_retries {
                panic!(
                    "Wallet {} failed to detect tx with tx_id = {} to be mined_confirmed",
                    wallet_name.as_str(),
                    tx_id
                );
            }
            match tx_info.status() {
                grpc::TransactionStatus::MinedConfirmed => {
                    println!(
                        "Transaction with tx_id = {} has been detected as mined_confirmed by wallet {}",
                        tx_id,
                        wallet_name.as_str()
                    );
                    return;
                },
                _ => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                },
            }
        }
    }
}

#[then(expr = "I have a SHA3 miner {word} connected to node {word}")]
#[when(expr = "I have a SHA3 miner {word} connected to node {word}")]
async fn sha3_miner_connected_to_base_node(world: &mut TariWorld, miner: String, base_node: String) {
    spawn_base_node(world, false, miner.clone(), vec![base_node.clone()], None).await;
    let base_node = world.base_nodes.get(&base_node).unwrap();
    let peers = base_node.seed_nodes.clone();
    spawn_wallet(world, miner.clone(), Some(miner.clone()), peers, None).await;
    register_miner_process(world, miner.clone(), miner.clone(), miner);
}

#[when(expr = "I list all {word} transactions for wallet {word}")]
#[then(expr = "I list all {word} transactions for wallet {word}")]
async fn list_all_txs_for_wallet(world: &mut TariWorld, transaction_type: String, wallet: String) {
    if vec!["COINBASE", "NORMAL"].contains(&transaction_type.as_str()) {
        panic!("Invalid transaction type. Values should be COINBASE or NORMAL, for now");
    }

    let mut client = create_wallet_client(world, wallet.clone()).await.unwrap();

    let request = GetCompletedTransactionsRequest {};
    let mut completed_txs = client.get_completed_transactions(request).await.unwrap().into_inner();

    while let Ok(tx) = completed_txs.next().await.unwrap() {
        let tx_info = tx.transaction.unwrap();
        if (tx_info.message.contains("Coinbase Transaction for Block ") && transaction_type == "COINBASE") ||
            (!tx_info.message.contains("Coinbase Transaction for Block ") && transaction_type == "NORMAL")
        {
            println!("Transaction with status COINBASE found for wallet {}: ", wallet);
        } else {
            continue;
        }
        println!("\n");
        println!("TxId: {}", tx_info.tx_id);
        println!("Status: {}", tx_info.status);
        println!("IsCancelled: {}", tx_info.is_cancelled);
    }
}

#[when(expr = "wallet {word} has at least {int} transactions that are all {word} and not cancelled")]
#[then(expr = "wallet {word} has at least {int} transactions that are all {word} and not cancelled")]
async fn wallet_has_at_least_num_txs(world: &mut TariWorld, wallet: String, num_txs: u64, transaction_status: String) {
    let mut client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let wallet_address = client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();
    let tx_ids = world.wallet_tx_ids.get(&wallet_address).unwrap();

    let transaction_status = match transaction_status.as_str() {
        "TRANSACTION_STATUS_COMPLETED" => 0i32,
        "TRANSACTION_STATUS_BROADCAST" => 1i32,
        "TRANSACTION_STATUS_MINED_UNCONFIRMED" => 2i32,
        "TRANSACTION_STATUS_IMPORTED" => 3i32,
        "TRANSACTION_STATUS_PENDING" => 4i32,
        "TRANSACTION_STATUS_COINBASE" => 5i32,
        "TRANSACTION_STATUS_MINED_CONFIRMED" => 6i32,
        "TRANSACTION_STATUS_NOT_FOUND" => 7i32,
        "TRANSACTION_STATUS_REJECTED" => 8i32,
        "TRANSACTION_STATUS_FAUX_UNCONFIRMED" => 9i32,
        "TRANSACTION_STATUS_FAUX_CONFIRMED" => 10i32,
        "TRANSACTION_STATUS_QUEUED" => 11i32,
        _ => panic!("Invalid transaction status {}", transaction_status),
    };

    let request = GetTransactionInfoRequest {
        transaction_ids: tx_ids.clone(),
    };
    let num_retries = 100;

    for _ in 0..num_retries {
        let txs_info = client.get_transaction_info(request.clone()).await.unwrap().into_inner();
        let txs_info = txs_info.transactions;
        if txs_info.iter().filter(|x| x.status == transaction_status).count() as u64 >= num_txs {
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    panic!(
        "Wallet {} failed to have at least num {} txs with status {}",
        wallet, num_txs, transaction_status
    );
}

#[when(expr = "I create a transaction {word} spending {word} to {word}")]
async fn create_tx_spending_coinbase(world: &mut TariWorld, transaction: String, inputs: String, output: String) {
    let inputs = inputs.split(',').collect::<Vec<&str>>();
    let utxos = inputs
        .iter()
        .map(|i| world.utxos.get(&i.to_string()).unwrap().clone())
        .collect::<Vec<_>>();
    // let (amount, utxo, tx) = build_transaction_with_output(utxos.as_slice());
    let (tx, utxo) = build_transaction_with_output(utxos);
    world.utxos.insert(output, utxo);
    world.transactions.insert(transaction, tx);
}

#[when(expr = "I create a custom fee transaction {word} spending {word} to {word} with fee {word}")]
async fn create_tx_custom_fee(world: &mut TariWorld, transaction: String, inputs: String, output: String, fee: u64) {
    let inputs = inputs.split(',').collect::<Vec<&str>>();
    let utxos = inputs
        .iter()
        .map(|i| world.utxos.get(&i.to_string()).unwrap().clone())
        .collect::<Vec<_>>();

    // world.utxos.insert(output, (0, utxo));
    // world.transactions.insert(transaction, tx);
}

#[when(expr = "I wait for wallet {word} to have less than {int} uT")]
async fn wait_for_wallet_to_have_less_than_micro_tari(world: &mut TariWorld, wallet: String, amount: u64) {
    let mut client = create_wallet_client(world, wallet.clone()).await.unwrap();
    println!("Waiting for wallet {} to have less than {} uT", wallet, amount);

    let num_retries = 100;
    let request = GetBalanceRequest {};

    for _ in 0..num_retries {
        let balance_res = client.get_balance(request.clone()).await.unwrap().into_inner();
        let current_balance = balance_res.available_balance;
        if current_balance < amount {
            println!(
                "Wallet {} now has less than {}, with current balance {}",
                wallet, amount, current_balance
            );
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    panic!(
        "Wallet {} didn't get less than {} after num_retries {}",
        wallet, amount, num_retries
    );
}

#[when(expr = "I have non-default wallet {word} connected to all seed nodes using {word}")]
async fn non_default_wallet_connected_to_all_seed_nodes(world: &mut TariWorld, wallet: String, mechanism: String) {
    let routing_mechanism = TransactionRoutingMechanism::from(mechanism);
    spawn_wallet(
        world,
        wallet,
        None,
        world.all_seed_nodes().to_vec(),
        Some(routing_mechanism),
    )
    .await;
}

#[when(expr = "I have {int} non-default wallets connected to all seed nodes using {word}")]
async fn non_default_wallets_connected_to_all_seed_nodes(world: &mut TariWorld, num: u64, mechanism: String) {
    let routing_mechanism = TransactionRoutingMechanism::from(mechanism);
    for ind in 0..num {
        let wallet_name = format!("Wallet_{}", ind);
        spawn_wallet(
            world,
            wallet_name,
            None,
            world.all_seed_nodes().to_vec(),
            Some(routing_mechanism),
        )
        .await;
    }
}

#[when(expr = "I send {int} uT without waiting for broadcast from wallet {word} to wallet {word} at fee {int}")]
#[then(expr = "I send {int} uT without waiting for broadcast from wallet {word} to wallet {word} at fee {int}")]
async fn send_amount_from_source_wallet_to_dest_wallet_without_broadcast(
    world: &mut TariWorld,
    amount: u64,
    source_wallet: String,
    dest_wallet: String,
    fee: u64,
) {
    let mut source_client = create_wallet_client(world, source_wallet.clone()).await.unwrap();
    let source_wallet_address = source_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let mut dest_client = create_wallet_client(world, dest_wallet.clone()).await.unwrap();
    let dest_wallet_address = dest_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let payment_recipient = PaymentRecipient {
        address: dest_wallet_address.clone(),
        amount,
        fee_per_gram: fee,
        message: format!(
            "transfer amount {} from {} to {}",
            amount,
            source_wallet.as_str(),
            dest_wallet.as_str()
        ),
        payment_type: 0, // normal mimblewimble payment type
    };
    let transfer_req = TransferRequest {
        recipients: vec![payment_recipient],
    };
    let tx_res = source_client.transfer(transfer_req).await.unwrap().into_inner();
    let tx_res = tx_res.results;

    assert_eq!(tx_res.len(), 1usize);

    let tx_res = tx_res.first().unwrap();
    assert!(
        tx_res.is_success,
        "Transacting amount {} uT from wallet {} to {} at fee {} failed",
        amount,
        source_wallet.as_str(),
        dest_wallet.as_str(),
        fee
    );

    let tx_id = tx_res.transaction_id;

    // insert tx_id's to the corresponding world mapping
    let source_tx_ids = world.wallet_tx_ids.entry(source_wallet_address.clone()).or_default();

    source_tx_ids.push(tx_id);

    let dest_tx_ids = world.wallet_tx_ids.entry(dest_wallet_address.clone()).or_default();

    dest_tx_ids.push(tx_id);

    println!(
        "Transfer amount {} from {} to {} at fee {} succeeded",
        amount, source_wallet, dest_wallet, fee
    );
}

#[then(expr = "I send a one-sided transaction of {int} uT from {word} to {word} at fee {int}")]
async fn send_one_sided_transaction_from_source_wallet_to_dest_wallt(
    world: &mut TariWorld,
    amount: u64,
    source_wallet: String,
    dest_wallet: String,
    fee: u64,
) {
    let mut source_client = create_wallet_client(world, source_wallet.clone()).await.unwrap();
    let source_wallet_address = source_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let mut dest_client = create_wallet_client(world, dest_wallet.clone()).await.unwrap();
    let dest_wallet_address = dest_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let payment_recipient = PaymentRecipient {
        address: dest_wallet_address.clone(),
        amount,
        fee_per_gram: fee,
        message: format!(
            "One sided transfer amount {} from {} to {}",
            amount,
            source_wallet.as_str(),
            dest_wallet.as_str()
        ),
        payment_type: 1, // one sided transaction
    };
    let transfer_req = TransferRequest {
        recipients: vec![payment_recipient],
    };
    let tx_res = source_client.transfer(transfer_req).await.unwrap().into_inner();
    let tx_res = tx_res.results;

    assert_eq!(tx_res.len(), 1usize);

    let tx_res = tx_res.first().unwrap();
    assert!(
        tx_res.is_success,
        "One sided transaction with amount {} from wallet {} to {} at fee {} failed",
        amount,
        source_wallet.as_str(),
        dest_wallet.as_str(),
        fee
    );

    // we wait for transaction to be broadcasted
    let tx_id = tx_res.transaction_id;
    let num_retries = 100;
    let tx_info_req = GetTransactionInfoRequest {
        transaction_ids: vec![tx_id],
    };

    for i in 0..num_retries {
        let tx_info_res = source_client
            .get_transaction_info(tx_info_req.clone())
            .await
            .unwrap()
            .into_inner();
        let tx_info = tx_info_res.transactions.first().unwrap();

        // TransactionStatus::TRANSACTION_STATUS_BROADCAST == 1_i32
        if tx_info.status == 1_i32 {
            println!(
                "One sided transaction from {} to {} with amount {} at fee {} has been broadcasted",
                source_wallet.clone(),
                dest_wallet.clone(),
                amount,
                fee
            );
            break;
        }

        if i == num_retries - 1 {
            panic!(
                "One sided transaction from {} to {} with amount {} at fee {} failed to be broadcasted",
                source_wallet.clone(),
                dest_wallet.clone(),
                amount,
                fee
            )
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // insert tx_id's to the corresponding world mapping
    let source_tx_ids = world.wallet_tx_ids.entry(source_wallet_address.clone()).or_default();

    source_tx_ids.push(tx_id);

    let dest_tx_ids = world.wallet_tx_ids.entry(dest_wallet_address.clone()).or_default();

    dest_tx_ids.push(tx_id);

    println!(
        "One sided transaction with amount {} from {} to {} at fee {} succeeded",
        amount, source_wallet, dest_wallet, fee
    );
}

#[then(expr = "wallet {word} detects at least {int} coinbase transactions as Mined_Confirmed")]
async fn wallet_detects_at_least_coinbase_transactions(world: &mut TariWorld, wallet_name: String, coinbases: u64) {
    let mut client = create_wallet_client(world, wallet_name.clone()).await.unwrap();
    let wallet_address = client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();
    let tx_ids = world.wallet_tx_ids.get(&wallet_address).unwrap();

    let num_retries = 100;
    let mut total_mined_confirmed_coinbases = 0;

    'outer: for _ in 0..num_retries {
        println!("Detecting mined confirmed coinbase transactions");
        'inner: for tx_id in tx_ids {
            let request = GetTransactionInfoRequest {
                transaction_ids: vec![*tx_id],
            };
            let tx_info = client.get_transaction_info(request).await.unwrap().into_inner();
            let tx_info = tx_info.transactions.first().unwrap();
            match tx_info.status() {
                grpc::TransactionStatus::MinedConfirmed => {
                    total_mined_confirmed_coinbases += 1;
                    if total_mined_confirmed_coinbases >= coinbases {
                        break 'outer;
                    }
                },
                _ => continue 'inner,
            }
        }

        if total_mined_confirmed_coinbases < coinbases {
            total_mined_confirmed_coinbases = 0;
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    if total_mined_confirmed_coinbases >= coinbases {
        println!(
            "Wallet {} detected at least {} coinbase transactions as Mined_Confirmed",
            &wallet_name, coinbases
        );
    } else {
        panic!(
            "Wallet {} failed to detect at least {} coinbase transactions as Mined_Confirmed",
            wallet_name, coinbases
        );
    }
}

#[then(expr = "wallet {word} detects exactly {int} coinbase transactions as Mined_Confirmed")]
async fn wallet_detects_exactly_coinbase_transactions(world: &mut TariWorld, wallet_name: String, coinbases: u64) {
    let mut client = create_wallet_client(world, wallet_name.clone()).await.unwrap();
    let wallet_address = client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();
    let tx_ids = world.wallet_tx_ids.get(&wallet_address).unwrap();

    let num_retries = 100;
    let mut total_mined_confirmed_coinbases = 0;

    'outer: for _ in 0..num_retries {
        println!("Detecting mined confirmed coinbase transactions");
        'inner: for tx_id in tx_ids {
            let request = GetTransactionInfoRequest {
                transaction_ids: vec![*tx_id],
            };
            let tx_info = client.get_transaction_info(request).await.unwrap().into_inner();
            let tx_info = tx_info.transactions.first().unwrap();
            match tx_info.status() {
                grpc::TransactionStatus::MinedConfirmed => total_mined_confirmed_coinbases += 1,
                _ => continue 'inner,
            }
        }

        if total_mined_confirmed_coinbases >= coinbases {
            break 'outer;
        } else {
            total_mined_confirmed_coinbases = 0;
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    if total_mined_confirmed_coinbases == coinbases {
        println!(
            "Wallet {} detected exactly {} coinbase transactions as Mined_Confirmed",
            &wallet_name, coinbases
        );
    } else {
        panic!(
            "Wallet {} failed to detect exactly {} coinbase transactions as Mined_Confirmed",
            wallet_name, coinbases
        );
    }
}

#[when(expr = "I have a base node {word} connected to node {word}")]
async fn base_node_connected_to_node(world: &mut TariWorld, base_node: String, peer_node: String) {
    spawn_base_node(world, false, base_node, vec![peer_node], None).await;
}

#[then(expr = "node {word} is at the same height as node {word}")]
async fn base_node_is_at_same_height_as_node(world: &mut TariWorld, base_node: String, peer_node: String) {
    let mut peer_node_client = world.get_node_client(&peer_node).await.unwrap();
    let req = Empty {};
    let mut expected_height = peer_node_client
        .get_tip_info(req.clone())
        .await
        .unwrap()
        .into_inner()
        .metadata
        .unwrap()
        .height_of_longest_chain;

    let mut base_node_client = world.get_node_client(&base_node).await.unwrap();
    let mut current_height = 0;
    let num_retries = 100;

    'outer: for _ in 0..12 {
        'inner: for _ in 0..num_retries {
            current_height = base_node_client
                .get_tip_info(req.clone())
                .await
                .unwrap()
                .into_inner()
                .metadata
                .unwrap()
                .height_of_longest_chain;
            if current_height >= expected_height {
                break 'inner;
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        expected_height = peer_node_client
            .get_tip_info(req.clone())
            .await
            .unwrap()
            .into_inner()
            .metadata
            .unwrap()
            .height_of_longest_chain;

        current_height = base_node_client
            .get_tip_info(req.clone())
            .await
            .unwrap()
            .into_inner()
            .metadata
            .unwrap()
            .height_of_longest_chain;

        if current_height == expected_height {
            break 'outer;
        }
    }

    if current_height == expected_height {
        println!(
            "Base node {} is at the same height {} as node {}",
            &base_node, current_height, &peer_node
        );
    } else {
        panic!(
            "Base node {} failed to synchronize at the same height as node {}",
            base_node, peer_node
        );
    }
}

#[then(expr = "while mining via SHA3 miner {word} all transactions in wallet {word} are found to be Mined_Confirmed")]
async fn while_mining_all_txs_in_wallet_are_mined_confirmed(world: &mut TariWorld, miner: String, wallet: String) {
    let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let wallet_address = wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();
    let wallet_tx_ids = world.wallet_tx_ids.get(&wallet_address).unwrap();

    if wallet_tx_ids.is_empty() {
        panic!("Wallet {} has no available transactions", wallet);
    }

    let miner_ps = world.miners.get(&miner).unwrap();
    let num_retries = 100;
    println!(
        "Detecting {} Mined_Confirmed transactions for wallet {}",
        wallet_tx_ids.len(),
        wallet
    );

    for tx_id in wallet_tx_ids {
        'inner: for retry in 0..=num_retries {
            let req = GetTransactionInfoRequest {
                transaction_ids: vec![*tx_id],
            };
            let res = wallet_client.get_transaction_info(req).await.unwrap().into_inner();
            let tx_status = res.transactions.first().unwrap().status;
            // TRANSACTION_STATUS_MINED_CONFIRMED code is currently 6
            if tx_status == 6 {
                println!(
                    "Wallet transaction with id {} has been detected with status Mined_Confirmed",
                    tx_id
                );
                break 'inner;
            }

            if retry == num_retries {
                panic!(
                    "Unable to have wallet transaction with tx_id = {} with status Mined_Confirmed",
                    tx_id
                );
            }

            println!("Mine a block for tx_id {} to have status Mined_Confirmed", tx_id);
            miner_ps.mine(world, Some(1)).await;

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

#[then(expr = "I stop all wallets")]
async fn stop_all_wallets(world: &mut TariWorld) {
    for (wallet, wallet_ps) in &mut world.wallets {
        println!("Stopping wallet {}", wallet);
        wallet_ps.kill();
    }
}

#[when(expr = "I start wallet {word}")]
async fn start_wallet_without_node(world: &mut TariWorld, wallet: String) {
    spawn_wallet(world, wallet, None, vec![], None).await;
}

#[then(expr = "while mining via node {word} all transactions in wallet {word} are found to be Mined_Confirmed")]
async fn while_mining_in_node_all_txs_in_wallet_are_mined_confirmed(
    world: &mut TariWorld,
    node: String,
    wallet: String,
) {
    let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let wallet_address = wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();
    let wallet_tx_ids = world.wallet_tx_ids.get(&wallet_address).unwrap();

    if wallet_tx_ids.is_empty() {
        panic!("Wallet {} on node {} has no available transactions", &wallet, &node);
    }

    let mut node_client = world.get_node_client(&node).await.unwrap();
    let num_retries = 100;
    let mut mined_status_flag = false;

    println!(
        "Detecting transactions on wallet {}, while mining on node {}, to be Mined_Confirmed",
        &wallet, &node
    );

    for tx_id in wallet_tx_ids {
        println!(
            "Waiting for transaction with id {} to have status Mined_Confirmed, while mining on node {}",
            tx_id, &node
        );

        'inner: for _ in 0..num_retries {
            let req = GetTransactionInfoRequest {
                transaction_ids: vec![*tx_id],
            };
            let res = wallet_client.get_transaction_info(req).await.unwrap().into_inner();
            let tx_status = res.transactions.first().unwrap().status;
            // TRANSACTION_STATUS_MINED_CONFIRMED code is currently 6
            if tx_status == 6 {
                println!("Transaction with id {} has been Mined_Confirmed", tx_id);
                mined_status_flag = true;
                break 'inner;
            }

            println!("Mine a block for tx_id {} to have status Mined_Confirmed", tx_id);
            mine_block(&mut node_client, &mut wallet_client).await;

            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        if !mined_status_flag {
            panic!(
                "Failed to have transaction with id {} on wallet {}, while mining on node {}, to be Mined_Confirmed",
                tx_id, &wallet, &node
            );
        }
    }

    println!(
        "Wallet {} has all transactions Mined_Confirmed, while mining on node {}",
        &wallet, &node
    );
}

#[then(expr = "all wallets detect all transactions as Mined_Confirmed")]
async fn all_wallets_detect_all_txs_as_mined_confirmed(world: &mut TariWorld) {
    for wallet in world.wallets.keys() {
        let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
        let wallet_address = wallet_client
            .get_address(Empty {})
            .await
            .unwrap()
            .into_inner()
            .address
            .to_hex();
        let wallet_tx_ids = world.wallet_tx_ids.get(&wallet_address);

        let wallet_tx_ids = if wallet_tx_ids.is_none() {
            println!("Wallet {} has no available transactions", &wallet);
            vec![]
        } else {
            let wallet_tx_ids = wallet_tx_ids.unwrap();
            if wallet_tx_ids.is_empty() {
                panic!("Wallet {} should have available transaction ids", wallet.as_str());
            }
            wallet_tx_ids.clone()
        };

        let num_retries = 100;

        for tx_id in wallet_tx_ids {
            'inner: for retry in 0..=num_retries {
                let req = GetTransactionInfoRequest {
                    transaction_ids: vec![tx_id],
                };
                let res = wallet_client.get_transaction_info(req).await.unwrap().into_inner();
                let tx_status = res.transactions.first().unwrap().status;

                // TRANSACTION_STATUS_MINED_CONFIRMED code is currently 6
                if tx_status == 6 {
                    println!(
                        "Wallet {} has detected transaction with id {} as Mined_Confirmed",
                        &wallet, tx_id
                    );
                    break 'inner;
                }

                if retry == num_retries {
                    panic!(
                        "Transaction with id {} does not have status as Mined_Confirmed, on wallet {}",
                        tx_id, &wallet
                    );
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[then(expr = "wallets {word} should have {word} {int} spendable coinbase outputs")]
async fn wallets_should_have_at_least_num_spendable_coinbase_outs(
    world: &mut TariWorld,
    wallets: String,
    comparison: String,
    amount_of_coinbases: u64,
) {
    let at_least = "AT_LEAST";
    let exactly = "EXACTLY";

    if comparison.as_str() != at_least && comparison.as_str() != exactly {
        panic!("Invalid comparison value provided: {}", comparison);
    }

    let wallets = wallets.split(',').collect::<Vec<_>>();
    let mut wallets_clients: Vec<_> = vec![];
    for w in &wallets {
        wallets_clients.push(create_wallet_client(world, w.to_string()).await.unwrap());
    }

    let num_retries = 100;
    let mut coinbase_count = 0;
    let mut spendable_coinbase_count = 0;

    for ind in 0..wallets_clients.len() {
        let wallet = wallets[ind];
        let mut client = wallets_clients[ind].clone();

        'inner: for _ in 0..num_retries {
            let mut stream = client
                .get_completed_transactions(GetCompletedTransactionsRequest {})
                .await
                .unwrap()
                .into_inner();
            while let Some(completed_tx) = stream.next().await {
                let tx_info = completed_tx.unwrap().transaction.unwrap();

                if tx_info.message.contains("Coinbase Transaction for Block ") && tx_info.fee == 0 {
                    let tx_id = tx_info.tx_id;
                    coinbase_count += 1;

                    println!("Found coinbase transaction with id {} for wallet {}", tx_id, &wallet);

                    // MINED_CONFIRMED status = 6
                    if tx_info.status == 6 {
                        println!(
                            "Coinbase transaction with id {} for wallet {} is Mined_Confirmed",
                            tx_id, &wallet
                        );
                        spendable_coinbase_count += 1;
                    }
                }
            }

            if spendable_coinbase_count >= amount_of_coinbases {
                println!(
                    "Wallet {} has found at least {} within total {} coinbase transaction",
                    &wallet, amount_of_coinbases, coinbase_count
                );
                break 'inner;
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        if comparison == at_least && spendable_coinbase_count >= amount_of_coinbases {
            println!("Wallet {} has found at least {}", &wallet, amount_of_coinbases);
        } else if comparison == exactly && spendable_coinbase_count == amount_of_coinbases {
            println!("Wallet {} has found exactly {}", &wallet, amount_of_coinbases);
        } else {
            panic!(
                "Wallet {} hasn't found {} {} spendable outputs",
                wallet, comparison, amount_of_coinbases
            );
        }
    }
}

#[when(expr = "I send {int} transactions of {int} uT each from wallet {word} to wallet {word} at fee_per_gram {int}")]
async fn send_num_transactions_to_wallets_at_fee(
    world: &mut TariWorld,
    num_txs: u64,
    amount: u64,
    sender_wallet: String,
    receiver_wallet: String,
    fee_per_gram: u64,
) {
    let mut sender_wallet_client = create_wallet_client(world, sender_wallet.clone()).await.unwrap();
    let sender_wallet_address = sender_wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let mut receiver_wallet_client = create_wallet_client(world, receiver_wallet.clone()).await.unwrap();
    let receiver_wallet_address = receiver_wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let mut tx_ids = vec![];

    for _ in 0..num_txs {
        let payment_recipient = PaymentRecipient {
            address: receiver_wallet_address.clone(),
            amount,
            fee_per_gram,
            message: format!(
                "transfer amount {} from {} to {}",
                amount,
                sender_wallet.as_str(),
                receiver_wallet.as_str()
            ),
            payment_type: 0, // standard mimblewimble transaction
        };
        let transfer_req = TransferRequest {
            recipients: vec![payment_recipient],
        };
        let transfer_res = sender_wallet_client.transfer(transfer_req).await.unwrap().into_inner();
        let transfer_res = transfer_res.results.first().unwrap();

        if !transfer_res.is_success {
            panic!(
                "Failed to send transaction from wallet {} to wallet {}, with message \n {}",
                &sender_wallet, &receiver_wallet, &transfer_res.failure_message
            );
        }
        tx_ids.push(transfer_res.transaction_id);

        // insert tx_id's to the corresponding world mapping
        let source_tx_ids = world.wallet_tx_ids.entry(sender_wallet_address.clone()).or_default();

        source_tx_ids.append(&mut tx_ids);

        let dest_tx_ids = world.wallet_tx_ids.entry(receiver_wallet_address.clone()).or_default();

        dest_tx_ids.append(&mut tx_ids);

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let num_retries = 100;
    println!(
        "Waiting for transactions from wallet {} to wallet {} to be broadcasted",
        &sender_wallet, &receiver_wallet
    );

    for tx_id in tx_ids {
        println!("Waiting for transaction with id {} to be broadcasted", tx_id);
        let request = GetTransactionInfoRequest {
            transaction_ids: vec![tx_id],
        };

        let mut is_broadcast = false;

        'inner: for _ in 0..num_retries {
            let txs_info = sender_wallet_client
                .get_transaction_info(request.clone())
                .await
                .unwrap()
                .into_inner();
            let txs_info = txs_info.transactions.first().unwrap();

            if txs_info.status == 1 {
                println!(
                    "Transaction from wallet {} to wallet {} with id {} has been broadcasted to the network",
                    &sender_wallet, &receiver_wallet, tx_id
                );
                is_broadcast = true;
                break 'inner;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        if !is_broadcast {
            panic!(
                "Transaction from wallet {} to wallet {} with id {} was not broacasted to the network",
                &sender_wallet, &receiver_wallet, tx_id
            );
        }
    }
}

#[given(expr = "I have a SHA3 miner {word} connected to seed node {word}")]
#[when(expr = "I have a SHA3 miner {word} connected to seed node {word}")]
async fn sha3_miner_connected_to_seed_node(world: &mut TariWorld, sha3_miner: String, seed_node: String) {
    println!("Create base node for SHA3 miner {}", &sha3_miner);
    spawn_base_node(world, false, sha3_miner.clone(), vec![seed_node.clone()], None).await;

    println!("Create wallet for SHA3 miner {}", &sha3_miner);
    spawn_wallet(
        world,
        sha3_miner.clone(),
        Some(sha3_miner.clone()),
        vec![seed_node],
        None,
    )
    .await;

    println!("Register SHA3 miner {}", &sha3_miner);
    register_miner_process(world, sha3_miner.clone(), sha3_miner.clone(), sha3_miner);
}

#[when(expr = "I have individual mining nodes connected to each wallet and base node {word}")]
async fn mining_nodes_connected_to_each_wallet_and_base_node(world: &mut TariWorld, base_node: String) {
    let wallets = world.wallets.clone();

    for (ind, wallet_name) in wallets.keys().enumerate() {
        let miner = format!("Miner_{}", ind);
        register_miner_process(world, miner, base_node.clone(), wallet_name.clone());
    }
}

#[then(expr = "I have each mining node mine {int} blocks")]
async fn mining_node_mine_blocks(world: &mut TariWorld, blocks: u64) {
    let miners = world.miners.clone();
    for (miner, miner_ps) in miners {
        println!("Miner {} is mining {} blocks", miner, blocks);
        miner_ps.mine(world, Some(blocks)).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[then(expr = "I wait for {word} to have {int} node connections")]
async fn wait_for_wallet_to_have_num_connections(world: &mut TariWorld, wallet: String, connections: u64) {
    let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let num_retries = 100;

    println!("Waiting for wallet {} to have {} connections", &wallet, connections);
    let mut actual_connections = 0_u32;

    for _ in 0..num_retries {
        let network_status_res = wallet_client.get_network_status(Empty {}).await.unwrap().into_inner();
        actual_connections = network_status_res.num_node_connections;
        if u64::from(actual_connections) >= connections {
            println!("Wallet {} has at least {} connections", &wallet, connections);
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    if u64::from(actual_connections) != connections {
        panic!("Wallet {} does not have {} connections", &wallet, connections);
    }
}

#[then(expr = "I wait for {word} to have {word} connectivity")]
async fn wait_for_wallet_to_have_specific_connectivity(world: &mut TariWorld, wallet: String, connectivity: String) {
    let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let num_retries = 100;

    println!("Waiting for wallet {} to have connectivity {}", &wallet, &connectivity);
    let connectivity = connectivity.to_uppercase();

    let connectivity_index = match connectivity.as_str() {
        "INITIALIZING" => 0,
        "ONLINE" => 1,
        "DEGRADED" => 2,
        "OFFLINE" => 3,
        _ => panic!("Invalid connectivity value {}", connectivity),
    };

    for _ in 0..=num_retries {
        let network_status_res = wallet_client.get_network_status(Empty {}).await.unwrap().into_inner();
        let connectivity_status = network_status_res.status;
        if connectivity_status == connectivity_index {
            println!("Wallet {} has {} connectivity", &wallet, &connectivity);
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    panic!(
        "Wallet {} did not get correct connectivity status {}",
        &wallet, connectivity
    );
}

#[then(expr = "node {word} lists heights {int} to {int}")]
async fn node_lists_heights(world: &mut TariWorld, node: String, start: u64, end: u64) {
    let mut node_client = world.get_node_client(&node).await.unwrap();
    let heights = (start..=end).collect::<Vec<_>>();
    let blocks_req = GetBlocksRequest { heights };
    let mut blocks_stream = node_client.get_blocks(blocks_req).await.unwrap().into_inner();

    let mut height = start;
    while let Some(block) = blocks_stream.next().await {
        let block = block.unwrap().block.unwrap();
        let block_height = block.header.unwrap().height;
        if height != block_height {
            panic!(
                "Invalid block height for node {}: expected height {} != current height {}",
                &node, block_height, height
            );
        }
        println!("Valid block height {}, listed by node {}", height, &node);
        height += 1;
    }
}

#[then(expr = "node {word} lists headers {int} to {int} with correct heights")]
async fn node_lists_headers_with_correct_heights(world: &mut TariWorld, node: String, start: u64, end: u64) {
    let mut node_client = world.get_node_client(&node).await.unwrap();
    let list_headers_req = ListHeadersRequest {
        from_height: start,
        num_headers: end - start + 1,
        sorting: 1,
    };
    let mut headers_stream = node_client.list_headers(list_headers_req).await.unwrap().into_inner();

    let mut height = start;
    while let Some(header) = headers_stream.next().await {
        let header_res = header.unwrap();
        let header_height = header_res.header.unwrap().height;

        if header_height != height {
            panic!(
                "incorrect listing of height headers by node {}: expected height to be {} but got height {}",
                &node, height, header_height
            );
        }
        println!("correct listing of height header {} by node {}", height, &node);
        height += 1;
    }
}

#[then(expr = "all nodes are at height {int}*{int}")]
#[when(expr = "all nodes are at height {int}*{int}")]
async fn all_nodes_are_at_product_height(world: &mut TariWorld, a: u64, b: u64) {
    all_nodes_are_at_height(world, a * b).await;
}

#[when(expr = "I transfer {int}T from {word} to {word}")]
async fn transfer_tari_from_wallet_to_receiver(world: &mut TariWorld, amount: u64, sender: String, receiver: String) {
    let mut sender_wallet_client = create_wallet_client(world, sender.clone()).await.unwrap();
    let sender_wallet_address = sender_wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let mut receiver_wallet_client = create_wallet_client(world, receiver.clone()).await.unwrap();
    let receiver_wallet_address = receiver_wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let payment_recipient = PaymentRecipient {
        address: receiver_wallet_address.clone(),
        amount: amount * 1_000_000_u64, // 1T = 1_000_000uT
        fee_per_gram: 10,               // as in the js cucumber tests
        message: format!(
            "transfer amount {} from {} to {}",
            amount,
            sender.as_str(),
            receiver.as_str()
        ),
        payment_type: 0, // normal mimblewimble payment type
    };
    let transfer_req = TransferRequest {
        recipients: vec![payment_recipient],
    };
    let tx_res = sender_wallet_client.transfer(transfer_req).await.unwrap().into_inner();
    let tx_res = tx_res.results;

    assert_eq!(tx_res.len(), 1usize);

    let tx_res = tx_res.first().unwrap();
    assert!(
        tx_res.is_success,
        "Transacting amount {}T from wallet {} to {} at fee {} failed",
        amount,
        sender.as_str(),
        receiver.as_str(),
        10
    );

    // we wait for transaction to be broadcasted
    let tx_id = tx_res.transaction_id;
    let num_retries = 100;
    let tx_info_req = GetTransactionInfoRequest {
        transaction_ids: vec![tx_id],
    };

    for i in 0..=num_retries {
        let tx_info_res = sender_wallet_client
            .get_transaction_info(tx_info_req.clone())
            .await
            .unwrap()
            .into_inner();
        let tx_info = tx_info_res.transactions.first().unwrap();

        // TransactionStatus::TRANSACTION_STATUS_BROADCAST == 1_i32
        if tx_info.status == 1_i32 {
            println!(
                "Transaction from {} to {} with amount {} at fee {} has been broadcasted",
                sender.clone(),
                receiver.clone(),
                amount,
                10
            );
            break;
        }

        if i == num_retries {
            panic!(
                "Transaction from {} to {} with amount {} at fee {} failed to be broadcasted",
                sender.clone(),
                receiver.clone(),
                amount,
                10
            )
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // insert tx_id's to the corresponding world mapping
    let source_tx_ids = world.wallet_tx_ids.entry(sender_wallet_address.clone()).or_default();

    source_tx_ids.push(tx_id);

    let dest_tx_ids = world.wallet_tx_ids.entry(receiver_wallet_address.clone()).or_default();

    dest_tx_ids.push(tx_id);

    println!(
        "Transfer amount {} from {} to {} at fee {} succeeded",
        amount, sender, receiver, 10
    );
}

#[when(expr = "wallet {word} has {int}T")]
#[then(expr = "wallet {word} has {int}T")]
async fn wallet_has_tari(world: &mut TariWorld, wallet: String, amount: u64) {
    let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let num_retries = 100;

    let mut available_balance = 0;

    for _ in 0..num_retries {
        let balance_res = wallet_client
            .get_balance(GetBalanceRequest {})
            .await
            .unwrap()
            .into_inner();

        available_balance = balance_res.available_balance;
        if available_balance >= amount * 1_000_000 {
            println!("Wallet {} has at least {}T", wallet.as_str(), amount);
            return;
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    panic!(
        "Wallet {} failed to have at least {}T, it ended with {}T",
        wallet, amount, available_balance
    );
}

#[when(expr = "I have wallet {word} with {int}T connected to base node {word}")]
async fn wallet_with_tari_connected_to_base_node(
    world: &mut TariWorld,
    wallet: String,
    amount: u64,
    base_node: String,
) {
    let peer_seeds = world.base_nodes.get(&base_node).unwrap().seed_nodes.clone();
    println!(
        "Start a new wallet {} connected to base node {}",
        wallet.as_str(),
        base_node.as_str()
    );
    spawn_wallet(world, wallet.clone(), Some(base_node.clone()), peer_seeds, None).await;

    let mut base_node_client = world.get_node_client(&base_node).await.unwrap();
    let tip_info_res = base_node_client.get_tip_info(Empty {}).await.unwrap().into_inner();
    let mut current_height = tip_info_res.metadata.unwrap().height_of_longest_chain;

    let mut num_blocks = 0;
    let mut reward = 0;

    let consensus_manager = ConsensusManager::builder(Network::LocalNet).build();

    while reward < amount {
        current_height += 1;
        num_blocks += 1;
        reward += consensus_manager.get_block_reward_at(current_height).as_u64() / 1_000_000; // 1 T = 1_000_000 uT
    }

    println!("Creating miner...");
    create_miner(world, "temp_miner".to_string(), base_node.clone(), wallet.clone()).await;

    println!("Mining {} blocks", num_blocks + CONFIRMATION_PERIOD);
    let miner = world.miners.get(&"temp_miner".to_string()).unwrap();
    miner.mine(world, Some(num_blocks + CONFIRMATION_PERIOD)).await; // mine some additional blocks to confirm txs

    let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let num_retries = 100;

    for _ in 0..num_retries {
        let balance_res = wallet_client
            .get_balance(GetBalanceRequest {})
            .await
            .unwrap()
            .into_inner();

        if balance_res.available_balance >= amount * 1_000_000 {
            println!("Wallet {} has at least {}T", wallet.as_str(), amount);
            return;
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    panic!("Wallet {} failed to have at least {}T", wallet, amount);
}

#[when(expr = "I transfer {int} uT from {word} to {word} and {word} at fee {int}")]
#[allow(clippy::too_many_lines)]
async fn transfer_from_wallet_to_two_recipients_at_fee(
    world: &mut TariWorld,
    amount: u64,
    sender: String,
    receiver1: String,
    receiver2: String,
    fee_per_gram: u64,
) {
    let mut sender_client = create_wallet_client(world, sender.clone()).await.unwrap();
    let sender_wallet_address = sender_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let mut receiver1_client = create_wallet_client(world, receiver1.clone()).await.unwrap();
    let receiver1_address = receiver1_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let mut receiver2_client = create_wallet_client(world, receiver2.clone()).await.unwrap();
    let receiver2_address = receiver2_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let payment_recipient1 = PaymentRecipient {
        address: receiver1_address.clone(),
        amount,
        fee_per_gram,
        message: format!(
            "transfer amount {} from {} to {}",
            amount,
            sender.as_str(),
            receiver1.as_str()
        ),
        payment_type: 0, // normal mimblewimble payment type
    };

    let payment_recipient2 = PaymentRecipient {
        address: receiver2_address.clone(),
        amount,
        fee_per_gram,
        message: format!(
            "transfer amount {} from {} to {}",
            amount,
            sender.as_str(),
            receiver2.as_str()
        ),
        payment_type: 0, // normal mimblewimble payment type
    };
    let transfer_req = TransferRequest {
        recipients: vec![payment_recipient1, payment_recipient2],
    };
    let tx_res = sender_client.transfer(transfer_req).await.unwrap().into_inner();
    let tx_res = tx_res.results;

    assert_eq!(tx_res.len(), 2_usize);

    let tx_res1 = tx_res.first().unwrap();
    let tx_res2 = tx_res.last().unwrap();

    assert!(
        tx_res1.is_success,
        "Transacting amount {} uT from wallet {} to {} at fee {} failed",
        amount,
        sender.as_str(),
        receiver1.as_str(),
        fee_per_gram
    );
    assert!(
        tx_res2.is_success,
        "Transacting amount {} uT from wallet {} to {} at fee {} failed",
        amount,
        sender.as_str(),
        receiver2.as_str(),
        fee_per_gram
    );

    // we wait for transaction to be broadcasted
    let tx_id1 = tx_res1.transaction_id;
    let tx_id2 = tx_res2.transaction_id;

    let num_retries = 100;
    let tx_info_req = GetTransactionInfoRequest {
        transaction_ids: vec![tx_id1, tx_id2],
    };

    for i in 0..=num_retries {
        let tx_info_res = sender_client
            .get_transaction_info(tx_info_req.clone())
            .await
            .unwrap()
            .into_inner();
        let tx_info1 = tx_info_res.transactions.first().unwrap();
        let tx_info2 = tx_info_res.transactions.last().unwrap();

        println!(
            "Tx_info for first recipient {} is {}, for tx_id = {}",
            receiver1, tx_info1.status, tx_id1
        );
        println!(
            "Tx_info for second recipient {} is {}, for tx_id = {}",
            receiver2, tx_info2.status, tx_id2
        );
        // TransactionStatus::TRANSACTION_STATUS_BROADCAST == 1_i32
        if tx_info1.status == 1_i32 && tx_info2.status == 1_i32 {
            println!(
                "Transaction from {} to {} and {} with amount {} at fee {} has been broadcasted",
                sender.as_str(),
                receiver1.as_str(),
                receiver2.as_str(),
                amount,
                fee_per_gram
            );
            break;
        }

        if i == num_retries {
            panic!(
                "Transaction from {} to {} and {} with amount {} at fee {} failed to be broadcasted",
                sender.as_str(),
                receiver1.as_str(),
                receiver2.as_str(),
                amount,
                10
            )
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // insert tx_id's to the corresponding world mapping
    let sender_tx_ids = world.wallet_tx_ids.entry(sender_wallet_address.clone()).or_default();

    sender_tx_ids.push(tx_id1);
    sender_tx_ids.push(tx_id2);

    let receiver1_tx_ids = world.wallet_tx_ids.entry(receiver1_address.clone()).or_default();
    receiver1_tx_ids.push(tx_id1);

    let receiver2_tx_ids = world.wallet_tx_ids.entry(receiver2_address.clone()).or_default();
    receiver2_tx_ids.push(tx_id2);

    println!(
        "Transfer amount {} from {} to {} and {} at fee {} succeeded",
        amount, sender, receiver1, receiver2, fee_per_gram
    );
}

#[when(expr = "I transfer {int} uT to self from wallet {word} at fee {int}")]
async fn transfer_tari_to_self(world: &mut TariWorld, amount: u64, sender: String, fee_per_gram: u64) {
    let mut sender_wallet_client = create_wallet_client(world, sender.clone()).await.unwrap();
    let sender_wallet_address = sender_wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let payment_recipient = PaymentRecipient {
        address: sender_wallet_address.clone(),
        amount,
        fee_per_gram,
        message: format!("transfer amount {} from {} to self", amount, sender.as_str(),),
        payment_type: 0, // normal mimblewimble payment type
    };
    let transfer_req = TransferRequest {
        recipients: vec![payment_recipient],
    };
    let tx_res = sender_wallet_client.transfer(transfer_req).await.unwrap().into_inner();
    let tx_res = tx_res.results;

    assert_eq!(tx_res.len(), 1usize);

    let tx_res = tx_res.first().unwrap();
    assert!(
        tx_res.is_success,
        "Transacting amount {} to self from wallet {} at fee {} failed",
        amount,
        sender.as_str(),
        fee_per_gram
    );

    // we wait for transaction to be broadcasted
    let tx_id = tx_res.transaction_id;
    let num_retries = 100;
    let tx_info_req = GetTransactionInfoRequest {
        transaction_ids: vec![tx_id],
    };

    for i in 0..=num_retries {
        let tx_info_res = sender_wallet_client
            .get_transaction_info(tx_info_req.clone())
            .await
            .unwrap()
            .into_inner();
        let tx_info = tx_info_res.transactions.first().unwrap();

        // TransactionStatus::TRANSACTION_STATUS_BROADCAST == 1_i32
        if tx_info.status == 1_i32 {
            println!(
                "Transaction to self from {} with amount {} at fee {} has been broadcasted",
                sender.clone(),
                amount,
                fee_per_gram
            );
            break;
        }

        if i == num_retries {
            panic!(
                "Transaction to self from {} with amount {} at fee {} failed to be broadcasted",
                sender.clone(),
                amount,
                fee_per_gram
            )
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // insert tx_id's to the corresponding world mapping
    let sender_tx_ids = world.wallet_tx_ids.entry(sender_wallet_address.clone()).or_default();

    sender_tx_ids.push(tx_id);

    println!(
        "Transfer amount {} to self from {} at fee {} succeeded",
        amount, sender, fee_per_gram
    );
}

#[when(expr = "I broadcast HTLC transaction with {int} uT from wallet {word} to wallet {word} at fee {int}")]
async fn htlc_transaction(world: &mut TariWorld, amount: u64, sender: String, receiver: String, fee_per_gram: u64) {
    let mut sender_wallet_client = create_wallet_client(world, sender.clone()).await.unwrap();
    let sender_wallet_address = sender_wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let mut receiver_wallet_client = create_wallet_client(world, receiver.clone()).await.unwrap();
    let receiver_wallet_address = receiver_wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let payment_recipient = PaymentRecipient {
        address: receiver_wallet_address.clone(),
        amount,
        fee_per_gram,
        message: format!(
            "Atomic Swap from {} to {} with amount {} at fee {}",
            sender.as_str(),
            receiver.as_str(),
            amount,
            fee_per_gram
        ),
        payment_type: 0, // normal mimblewimble transaction
    };

    let atomic_swap_request = SendShaAtomicSwapRequest {
        recipient: Some(payment_recipient),
    };
    let sha_atomic_swap_tx_res = sender_wallet_client
        .send_sha_atomic_swap_transaction(atomic_swap_request)
        .await
        .unwrap()
        .into_inner();

    assert!(
        sha_atomic_swap_tx_res.is_success,
        "Atomic swap transacting amount uT {} from wallet {} to {} at fee {} failed",
        amount,
        sender.as_str(),
        receiver.as_str(),
        fee_per_gram
    );

    // we wait for transaction to be broadcasted
    let tx_id = sha_atomic_swap_tx_res.transaction_id;
    let num_retries = 100;
    let tx_info_req = GetTransactionInfoRequest {
        transaction_ids: vec![tx_id],
    };

    for i in 0..=num_retries {
        let tx_info_res = sender_wallet_client
            .get_transaction_info(tx_info_req.clone())
            .await
            .unwrap()
            .into_inner();
        let tx_info = tx_info_res.transactions.first().unwrap();

        // TransactionStatus::TRANSACTION_STATUS_BROADCAST == 1_i32
        if tx_info.status == 1_i32 {
            println!(
                "Atomic swap transaction from {} to {} with amount {} at fee {} has been broadcasted",
                sender.as_str(),
                receiver.as_str(),
                amount,
                fee_per_gram
            );
            break;
        }

        if i == num_retries {
            panic!(
                "Atomic swap transaction from {} to {} with amount {} at fee {} failed to be broadcasted",
                sender.as_str(),
                receiver.as_str(),
                amount,
                fee_per_gram
            )
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // insert tx_id's to the corresponding world mapping
    let sender_tx_ids = world.wallet_tx_ids.entry(sender_wallet_address.clone()).or_default();

    sender_tx_ids.push(tx_id);

    let receiver_tx_ids = world.wallet_tx_ids.entry(receiver_wallet_address.clone()).or_default();

    receiver_tx_ids.push(tx_id);
    world.output_hash = Some(sha_atomic_swap_tx_res.output_hash);
    world.pre_image = Some(sha_atomic_swap_tx_res.pre_image);

    println!(
        "Atomic swap transfer amount {} from {} to {} at fee {} succeeded",
        amount, sender, receiver, fee_per_gram
    );
}

#[when(expr = "I claim an HTLC refund transaction with wallet {word} at fee {int}")]
async fn claim_htlc_refund_transaction_with_wallet_at_fee(world: &mut TariWorld, wallet: String, fee_per_gram: u64) {
    let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let wallet_address = wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let output_hash = world.output_hash.clone().unwrap();

    let claim_htlc_req = ClaimHtlcRefundRequest {
        output_hash,
        fee_per_gram,
    };

    let claim_htlc_refund_res = wallet_client
        .claim_htlc_refund_transaction(claim_htlc_req)
        .await
        .unwrap()
        .into_inner();

    assert!(
        claim_htlc_refund_res.clone().results.unwrap().is_success,
        "Claim HTLC refund transaction with wallet {} at fee {} failed",
        wallet.as_str(),
        fee_per_gram
    );

    // we wait for transaction to be broadcasted
    let tx_id = claim_htlc_refund_res.results.unwrap().transaction_id;
    let num_retries = 100;
    let tx_info_req = GetTransactionInfoRequest {
        transaction_ids: vec![tx_id],
    };

    for i in 0..=num_retries {
        let tx_info_res = wallet_client
            .get_transaction_info(tx_info_req.clone())
            .await
            .unwrap()
            .into_inner();
        let tx_info = tx_info_res.transactions.first().unwrap();

        // TransactionStatus::TRANSACTION_STATUS_BROADCAST == 1_i32
        if tx_info.status == 1_i32 {
            println!(
                "Claim HTLC refund transaction with wallet {} at fee {} has been broadcasted",
                wallet.as_str(),
                fee_per_gram
            );
            break;
        }

        if i == num_retries {
            panic!(
                "Claim HTLC refund transaction with wallet {} at fee {} failed to be broadcasted",
                wallet.as_str(),
                fee_per_gram
            )
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // insert tx_id's to the corresponding world mapping
    let wallet_tx_ids = world.wallet_tx_ids.entry(wallet_address.clone()).or_default();
    wallet_tx_ids.push(tx_id);

    println!(
        "Claim HTLC refund transaction with wallet {} at fee {} succeeded",
        wallet, fee_per_gram
    );
}

#[when(expr = "I claim an HTLC transaction with wallet {word} at fee {int}")]
async fn wallet_claims_htlc_transaction_at_fee(world: &mut TariWorld, wallet: String, fee_per_gram: u64) {
    let mut wallet_client = create_wallet_client(world, wallet.clone()).await.unwrap();
    let wallet_address = wallet_client
        .get_address(Empty {})
        .await
        .unwrap()
        .into_inner()
        .address
        .to_hex();

    let output_hash = world.output_hash.clone().unwrap();
    let pre_image = world.pre_image.clone().unwrap();

    let claim_htlc_req = ClaimShaAtomicSwapRequest {
        output: output_hash,
        pre_image,
        fee_per_gram,
    };

    let claim_htlc_res = wallet_client
        .claim_sha_atomic_swap_transaction(claim_htlc_req)
        .await
        .unwrap()
        .into_inner();

    assert!(
        claim_htlc_res.clone().results.unwrap().is_success,
        "Claim HTLC transaction with wallet {} at fee {} failed",
        wallet.as_str(),
        fee_per_gram
    );

    // we wait for transaction to be broadcasted
    let tx_id = claim_htlc_res.results.unwrap().transaction_id;
    let num_retries = 100;
    let tx_info_req = GetTransactionInfoRequest {
        transaction_ids: vec![tx_id],
    };

    for i in 0..=num_retries {
        let tx_info_res = wallet_client
            .get_transaction_info(tx_info_req.clone())
            .await
            .unwrap()
            .into_inner();
        let tx_info = tx_info_res.transactions.first().unwrap();

        // TransactionStatus::TRANSACTION_STATUS_BROADCAST == 1_i32
        if tx_info.status == 1_i32 {
            println!(
                "Claim HTLC transaction with wallet {} at fee {} has been broadcasted",
                wallet.as_str(),
                fee_per_gram
            );
            break;
        }

        if i == num_retries {
            panic!(
                "Claim HTLC transaction with wallet {} at fee {} failed to be broadcasted",
                wallet.as_str(),
                fee_per_gram
            )
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // insert tx_id's to the corresponding world mapping
    let wallet_tx_ids = world.wallet_tx_ids.entry(wallet_address.clone()).or_default();
    wallet_tx_ids.push(tx_id);

    println!(
        "Claim HTLC transaction with wallet {} at fee {} succeeded",
        wallet, fee_per_gram
    );
}

#[when(expr = "I print the cucumber world")]
async fn print_world(world: &mut TariWorld) {
    eprintln!();
    eprintln!("======================================");
    eprintln!("============= TEST NODES =============");
    eprintln!("======================================");
    eprintln!();

    // base nodes
    for (name, node) in world.base_nodes.iter() {
        eprintln!(
            "Base node \"{}\": grpc port \"{}\", temp dir path \"{}\"",
            name, node.grpc_port, node.temp_dir_path
        );
    }

    // wallets
    for (name, node) in world.wallets.iter() {
        eprintln!(
            "Wallet \"{}\": grpc port \"{}\", temp dir path \"{}\"",
            name, node.grpc_port, node.temp_dir_path
        );
    }

    eprintln!();
    eprintln!("======================================");
    eprintln!();
}

fn flush_stdout(buffer: &Arc<Mutex<Vec<u8>>>) {
    // After each test we flush the stdout to the logs.
    info!(
        target: LOG_TARGET_STDOUT,
        "{}",
        str::from_utf8(&buffer.lock().unwrap()).unwrap()
    );
    buffer.lock().unwrap().clear();
}

fn main() {
    initialize_logging(
        &PathBuf::from("log4rs/cucumber.yml"),
        include_str!("../log4rs/cucumber.yml"),
    )
    .expect("logging not configured");
    let stdout_buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    #[cfg(test)]
    std::io::set_output_capture(Some(stdout_buffer.clone()));
    // Never move this line below the runtime creation!!! It will cause that any new thread created via task::spawn will
    // not be affected by the output capture.
    let stdout_buffer_clone = stdout_buffer.clone();
    let runtime = Runtime::new().unwrap();
    runtime.block_on(async {
        let world = TariWorld::cucumber()
        .repeat_failed()
        // following config needed to use eprint statements in the tests
        .max_concurrent_scenarios(5)
        //.with_writer(
        //    writer::Basic::raw(io::stdout(), writer::Coloring::Never, 0)
        //        .summarized()
        //        .assert_normalized(),
        //)
        .after(move |_feature, _rule, scenario, ev, maybe_world| {
            let stdout_buffer = stdout_buffer_clone.clone();
            Box::pin(async move {
                flush_stdout(&stdout_buffer);
                match ev {
                    ScenarioFinished::StepFailed(_capture_locations, _location, _error) => {
                        error!(target: LOG_TARGET, "Scenario failed");
                    },
                    ScenarioFinished::StepPassed => {
                        info!(target: LOG_TARGET, "Scenario was successful.");
                    },
                    ScenarioFinished::StepSkipped => {
                        warn!(target: LOG_TARGET, "Some steps were skipped.");
                    },
                    ScenarioFinished::BeforeHookFailed(_info) => {
                        error!(target: LOG_TARGET, "Before hook failed!");
                    },
                }
                if let Some(maybe_world) = maybe_world {
                    maybe_world.after(scenario).await;
                }
            })
        })
        .before(|_feature,_rule,scenario,_world| {
            Box::pin(async move {
                println!("{} : {}", scenario.keyword, scenario.name); // This will be printed into the stdout_buffer
                info!(target: LOG_TARGET, "Starting {} {}", scenario.keyword, scenario.name);
            })
        });
        world.run_and_exit("tests/features/").await;
    });

    // If by any chance we have anything in the stdout buffer just log it.
    flush_stdout(&stdout_buffer);
}

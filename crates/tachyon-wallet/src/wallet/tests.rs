use super::*;
use primitives::transaction::{Authorized, TransactionData};
use protocol::{consensus::BlockHeight, value::Zatoshis};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use transparent::bundle::{Bundle, TxOut};
use zcash_tachyon::TachyonBundle;

struct NodeData {
    blocks: Vec<Block>,
    raw_funding: String,
    mempool: Vec<String>,
    fail_submission: bool,
}
struct Node {
    url: String,
    data: Arc<Mutex<NodeData>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for Node {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}
impl Node {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}", listener.local_addr()?);
        let data = Arc::new(Mutex::new(NodeData {
            blocks: vec![crate::tests::genesis()],
            raw_funding: String::new(),
            mempool: vec![],
            fail_submission: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_data = data.clone();
        let thread_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                let (stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("mock listener: {e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                assert!(length < 16 * 1024 * 1024);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let mut data = thread_data.lock().unwrap();
                let params = &request["params"];
                let mut error = Value::Null;
                let result = match request["method"].as_str().unwrap() {
                    "getblockchaininfo" => json!({"chain":"test"}),
                    "getblockcount" => json!(data.blocks.last().unwrap().height),
                    "getblockhash" => {
                        let h = params[0].as_u64().unwrap();
                        if h == 0 {
                            json!(hex::encode([0; 32]))
                        } else {
                            json!(
                                data.blocks
                                    .iter()
                                    .find(|b| u64::from(b.height) == h)
                                    .unwrap()
                                    .hash
                            )
                        }
                    }
                    "gettachyonblock" => {
                        let h: u32 = params[0].as_str().unwrap().parse().unwrap();
                        serde_json::to_value(data.blocks.iter().find(|b| b.height == h).unwrap())
                            .unwrap()
                    }
                    "getblock" => {
                        let block = data
                            .blocks
                            .iter()
                            .find(|b| b.hash == params[0].as_str().unwrap())
                            .unwrap();
                        json!({"hash":block.hash,"tx":block.transactions})
                    }
                    "gettxout" => json!({"confirmations":100,"coinbase":true}),
                    "getrawtransaction" => json!(data.raw_funding),
                    "sendrawtransaction" => {
                        let raw = params[0].as_str().unwrap().to_owned();
                        let tx =
                            Transaction::read(&hex::decode(&raw).unwrap()[..], BranchId::NuTachyon)
                                .unwrap();
                        if !data.mempool.contains(&raw) {
                            data.mempool.push(raw);
                        }
                        if data.fail_submission {
                            error = json!({"code":-1,"message":"simulated ambiguous broadcast"});
                        }
                        json!(tx.txid().to_string())
                    }
                    other => panic!("unexpected RPC {other}"),
                };
                drop(data);
                let response = serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":1,"result":result,"error":error}),
                )
                .unwrap();
                let mut stream = reader.into_inner();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",response.len()).unwrap();
                stream.write_all(&response).unwrap();
            }
        });
        Ok(Self {
            url,
            data,
            stop,
            worker: Some(worker),
        })
    }
    fn mine(&self) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        let raw = data.mempool.remove(0);
        let tx = Transaction::read(&hex::decode(raw)?[..], BranchId::NuTachyon)?;
        crate::tests::mine(&mut data.blocks, Some(&tx))
    }
}

fn command(dir: &std::path::Path, command: Operation) -> Result<Value> {
    Command {
        wallet_dir: dir.into(),
        command,
    }
    .execute()
}
fn synced_balance(dir: &std::path::Path) -> Result<Value> {
    command(dir, Operation::Sync { max_blocks: 1000 })?;
    command(dir, Operation::Balance)
}

#[test]
fn two_wallet_rpc_flow_recovers_broadcast_failure_and_reorg() -> Result<()> {
    let node = Node::start()?;
    let temp = tempfile::tempdir()?;
    let alice = temp.path().join("alice");
    let bob = temp.path().join("bob");
    for dir in [&alice, &bob] {
        command(
            dir,
            Operation::Init {
                rpc_url: node.url.clone(),
                activation_height: 8,
                cookie_file: None,
                accept_mock_proofs: true,
            },
        )?;
    }
    let alice_address = command(&alice, Operation::Address)?;
    let funding_output = TxOut::new(
        Zatoshis::from_u64(1_050_000)?,
        transaction::transparent_script(alice_address["funding_address"].as_str().unwrap())?,
    );
    let funding = TransactionData::<Authorized>::from_parts_v7(
        BranchId::NuTachyon,
        0,
        BlockHeight::from_u32(0),
        Zatoshis::ZERO,
        Some(Bundle {
            vin: vec![],
            vout: vec![funding_output.clone()],
            authorization: transparent::bundle::Authorized,
        }),
        None,
        None,
        None,
        TachyonBundle::NoBundle,
    )
    .freeze()?;
    let mut raw = Vec::new();
    funding.write(&mut raw)?;
    node.data.lock().unwrap().raw_funding = hex::encode(raw);
    command(
        &alice,
        Operation::Shield {
            txid: funding.txid().to_string(),
            index: 0,
        },
    )?;
    {
        let data = node.data.lock().unwrap();
        let tx = Transaction::read(&hex::decode(&data.mempool[0])?[..], BranchId::NuTachyon)?;
        transaction::verify_for_test(&tx, Some(funding_output))?;
    }
    node.mine()?;
    assert_eq!(synced_balance(&alice)?["available_zat"], 1_000_000);
    let bob_address = command(&bob, Operation::Address)?["tachyon_address"]
        .as_str()
        .unwrap()
        .to_owned();
    node.data.lock().unwrap().fail_submission = true;
    assert!(
        command(
            &alice,
            Operation::Send {
                to: bob_address.clone(),
                amount_zat: 300_000
            }
        )
        .is_err()
    );
    // Every command reopens SQLite: reservation and raw bytes survive process boundaries.
    assert_eq!(command(&alice, Operation::Balance)?["available_zat"], 0);
    assert!(
        command(
            &alice,
            Operation::Send {
                to: bob_address,
                amount_zat: 300_000
            }
        )
        .is_err()
    );
    let transactions = command(&alice, Operation::Transactions)?;
    let txid = transactions["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["status"] == "pending")
        .unwrap()["txid"]
        .as_str()
        .unwrap()
        .to_owned();
    let receipt_file = temp.path().join("payment.json");
    command(
        &alice,
        Operation::ExportReceipt {
            txid: txid.clone(),
            out: receipt_file.clone(),
        },
    )?;
    command(
        &bob,
        Operation::Receive {
            receipt: receipt_file.clone(),
        },
    )?;
    assert_eq!(synced_balance(&bob)?["available_zat"], 0);
    node.data.lock().unwrap().fail_submission = false;
    command(&alice, Operation::Submit { txid: txid.clone() })?;
    assert_eq!(node.data.lock().unwrap().mempool.len(), 1); // Retry did not create a replacement.
    node.mine()?;
    assert_eq!(synced_balance(&bob)?["available_zat"], 300_000);
    assert_eq!(synced_balance(&alice)?["available_zat"], 650_000);
    // Same-height reorg: remove the payment, replace it with an empty block.
    {
        let mut data = node.data.lock().unwrap();
        data.blocks.pop();
        crate::tests::mine(&mut data.blocks, None)?;
        data.blocks.last_mut().unwrap().hash = hex::encode([99; 32]);
    }
    assert_eq!(synced_balance(&bob)?["available_zat"], 0);
    assert_eq!(synced_balance(&alice)?["available_zat"], 0); // Original note is reserved again.
    command(&alice, Operation::Submit { txid })?;
    node.mine()?;
    assert_eq!(synced_balance(&bob)?["available_zat"], 300_000);
    command(
        &bob,
        Operation::Unshield {
            to: alice_address["funding_address"].as_str().unwrap().into(),
            amount_zat: 100_000,
        },
    )?;
    {
        let data = node.data.lock().unwrap();
        let tx = Transaction::read(&hex::decode(&data.mempool[0])?[..], BranchId::NuTachyon)?;
        transaction::verify_for_test(&tx, None)?;
    }
    node.mine()?;
    assert_eq!(synced_balance(&bob)?["available_zat"], 150_000);
    Ok(())
}

#[test]
fn reservations_survive_restarts_and_expire_only_after_expiry_height() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let wallet_dir = dir.path().join("wallet");
    let store = Store::open(&wallet_dir, true)?;
    let receipt = crate::tests::own(1_000_000);
    let mut blocks = vec![crate::tests::genesis()];
    let tx = crate::tests::shield(&blocks, &receipt)?;
    crate::tests::mine(&mut blocks, Some(&tx))?;
    for block in &blocks {
        store.put_block(block)?;
    }
    store.put_note(&receipt)?;
    store.reserve(&Pending {
        txid: hex::encode([3; 32]),
        raw: "00".into(),
        expiry: 10,
        source: format!("note:{}", receipt.id()?),
        receipt: None,
    })?;
    store.commit()?;
    drop(store);
    let store = Store::open(&wallet_dir, false)?;
    let config = Config {
        version: 1,
        rpc_url: String::new(),
        cookie_file: None,
        activation: 8,
        activation_hash: blocks[0].hash.clone(),
        genesis: hex::encode([0; 32]),
        network_id: crate::tests::NETWORK.into(),
        seed: crate::tests::SEED,
    };
    assert_eq!(note_states(&store, &config, &blocks)?[0].1, "reserved");
    crate::tests::mine(&mut blocks, None)?;
    assert_eq!(note_states(&store, &config, &blocks)?[0].1, "reserved");
    crate::tests::mine(&mut blocks, None)?;
    assert_eq!(note_states(&store, &config, &blocks)?[0].1, "available");
    Ok(())
}

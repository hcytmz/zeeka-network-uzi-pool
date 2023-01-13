mod client;

use bazuka::core::{Header, Money, MpnAddress, MpnDeposit};
use bazuka::wallet::{TxBuilder, Wallet};
use bazuka::zk::MpnTransaction;
use chrono::prelude::*;
use client::SyncClient;
use colored::Colorize;
use rust_randomx::{Context, Hasher};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use structopt::StructOpt;
use tiny_http::{Response, Server};

const LISTEN: &'static str = "0.0.0.0:8766";

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Solution {
    pub nonce: String,
}

#[derive(Debug, StructOpt, Clone)]
#[structopt(name = "Uzi Pool", about = "Mine Ziesha with Uzi!")]
struct Opt {
    #[structopt(short = "n", long = "node")]
    node: SocketAddr,

    #[structopt(long, default_value = LISTEN)]
    listen: SocketAddr,

    #[structopt(long, default_value = "mainnet")]
    network: String,

    #[structopt(long, default_value = "")]
    miner_token: String,

    #[structopt(long, default_value = "10")]
    share_easiness: usize,

    #[structopt(long, default_value = "10")]
    share_capacity: usize,

    #[structopt(long, default_value = "5")]
    reward_delay: u64,

    #[structopt(long, default_value = "0.01")]
    owner_reward_ratio: f32,

    #[structopt(long)]
    pool_mpn_address: MpnAddress,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Share {
    miner: Miner,
    nonce: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Job {
    puzzle: Puzzle,
    shares: Vec<Share>,
    nonces: HashSet<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Puzzle {
    key: String,
    blob: String,
    offset: usize,
    size: usize,
    target: u32,
    reward: Money,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct PuzzleWrapper {
    puzzle: Option<Puzzle>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct History {
    solved: HashMap<Header, HashMap<MpnAddress, Money>>,
    sent: HashMap<Header, (MpnDeposit, Vec<MpnTransaction>)>,
}

fn save_history(h: &History) -> Result<(), Box<dyn Error>> {
    let history_path = home::home_dir()
        .unwrap()
        .join(Path::new(".uzi-pool-history"));
    File::create(history_path)?.write_all(&bincode::serialize(h)?)?;
    Ok(())
}

fn get_history() -> Result<History, Box<dyn Error>> {
    let history_path = home::home_dir()
        .unwrap()
        .join(Path::new(".uzi-pool-history"));
    Ok(if let Ok(mut f) = File::open(history_path.clone()) {
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        drop(f);
        bincode::deserialize(&bytes)?
    } else {
        History {
            solved: HashMap::new(),
            sent: HashMap::new(),
        }
    })
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct AddMinerRequest {
    mpn_addr: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct AddMinerResponse {
    miner_token: String,
}

fn job_solved(
    total_reward: Money,
    owner_reward_ratio: f32,
    shares: &[Share],
) -> HashMap<MpnAddress, Money> {
    let total_reward_64 = Into::<u64>::into(total_reward);
    let owner_reward_64 = (total_reward_64 as f64 * owner_reward_ratio as f64) as u64;
    let per_share_reward: Money =
        ((total_reward_64 - owner_reward_64) / (shares.len() as u64)).into();
    let mut rewards: HashMap<MpnAddress, Money> = HashMap::new();
    for share in shares {
        *rewards.entry(share.miner.mpn_addr.clone()).or_default() += per_share_reward;
    }
    rewards
}

fn fetch_miner_token(req: &tiny_http::Request) -> Option<String> {
    for h in req.headers() {
        if h.field.equiv("X-ZIESHA-MINER-TOKEN") {
            return Some(h.value.clone().into());
        }
    }
    None
}

fn generate_miner_token() -> String {
    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(30)
        .map(char::from)
        .collect()
}

fn process_request(
    context: Arc<Mutex<MinerContext>>,
    mut request: tiny_http::Request,
    opt: &Opt,
) -> Result<(), Box<dyn Error>> {
    let mut ctx = context.lock().unwrap();
    let miners = ctx.eligible_miners.clone();
    let miner = if let Some(Some(miner)) = fetch_miner_token(&request).map(|tkn| miners.get(&tkn)) {
        Some(miner.clone())
    } else {
        None
    };

    match request.url() {
        "/get-miners" => {
            if !request.remote_addr().ip().is_loopback() {
                request.respond(Response::from_string("ERR"))?;
            } else {
                let miners: HashMap<String, String> = miners
                    .into_iter()
                    .map(|(k, v)| (k, v.mpn_addr.to_string()))
                    .collect();
                let mut resp = Response::from_string(serde_json::to_string(&miners).unwrap());
                resp.add_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                request.respond(resp)?;
            }
        }
        "/add-miner" => {
            if !request.remote_addr().ip().is_loopback() {
                request.respond(Response::from_string("ERR"))?;
            } else {
                let miners_path = home::home_dir()
                    .unwrap()
                    .join(Path::new(".uzi-pool-miners"));
                let add_miner_req: AddMinerRequest = {
                    let mut content = String::new();
                    request.as_reader().read_to_string(&mut content)?;
                    serde_json::from_str(&content)?
                };
                let mut miners = miners.clone().into_values().collect::<Vec<Miner>>();
                let miner_token = generate_miner_token();
                miners.push(Miner {
                    mpn_addr: add_miner_req.mpn_addr.parse()?,
                    token: miner_token.clone(),
                });
                File::create(miners_path)?.write_all(&serde_json::to_vec(&miners)?)?;
                let mut resp = Response::from_string(
                    serde_json::to_string(&AddMinerResponse { miner_token }).unwrap(),
                );
                resp.add_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                request.respond(resp)?;
            }
        }
        "/miner/puzzle" => {
            if miner.is_none() {
                log::warn!("Miner not authorized!");
                request.respond(Response::empty(401))?;
                return Ok(());
            };

            let easy_puzzle = ctx.current_job.as_ref().map(|j| {
                let mut new_pzl = j.puzzle.clone();
                new_pzl.target = rust_randomx::Difficulty::new(new_pzl.target)
                    .scale(1f32 / (opt.share_easiness as f32))
                    .to_u32();
                new_pzl
            });
            request.respond(Response::from_string(
                serde_json::to_string(&PuzzleWrapper {
                    puzzle: easy_puzzle.clone(),
                })
                .unwrap(),
            ))?;
        }
        "/miner/solution" => {
            let miner = if let Some(miner) = miner {
                miner
            } else {
                log::warn!("Miner not authorized!");
                request.respond(Response::empty(401))?;
                return Ok(());
            };

            let sol: Solution = {
                let mut content = String::new();
                request.as_reader().read_to_string(&mut content)?;
                serde_json::from_str(&content)?
            };

            let mut block_solved: Option<(Header, HashMap<MpnAddress, Money>)> = None;
            let hasher = Hasher::new(ctx.hasher.clone());
            if let Some(current_job) = ctx.current_job.as_mut() {
                let easy_puzzle = {
                    let mut new_pzl = current_job.puzzle.clone();
                    new_pzl.target = rust_randomx::Difficulty::new(new_pzl.target)
                        .scale(1f32 / (opt.share_easiness as f32))
                        .to_u32();
                    new_pzl
                };

                let block_diff = rust_randomx::Difficulty::new(current_job.puzzle.target);
                let share_diff = rust_randomx::Difficulty::new(easy_puzzle.target);
                let mut blob = hex::decode(easy_puzzle.blob.clone())?;
                let header: Header = bincode::deserialize(&blob)?;
                let (b, e) = (easy_puzzle.offset, easy_puzzle.offset + easy_puzzle.size);
                blob[b..e].copy_from_slice(&hex::decode(&sol.nonce)?);
                let out = hasher.hash(&blob);

                if out.meets_difficulty(share_diff) {
                    if !current_job.nonces.contains(&sol.nonce) {
                        current_job.shares.push(Share {
                            miner: miner.clone(),
                            nonce: sol.nonce.clone(),
                        });
                        current_job.nonces.insert(sol.nonce.clone());
                        while current_job.shares.len() > opt.share_capacity {
                            current_job.shares.remove(0);
                        }
                        if out.meets_difficulty(block_diff) {
                            block_solved = Some((
                                header,
                                job_solved(
                                    current_job.puzzle.reward,
                                    opt.owner_reward_ratio,
                                    &current_job.shares,
                                ),
                            ));

                            println!(
                                "{} -> {} {}",
                                Local::now(),
                                "Solution found by:".bright_green(),
                                miner.token
                            );
                            ureq::post(&format!("http://{}/miner/solution", opt.node))
                                .set("X-ZIESHA-MINER-TOKEN", &opt.miner_token)
                                .send_json(json!({ "nonce": sol.nonce }))?;
                        } else {
                            println!(
                                "{} -> {} {}",
                                Local::now(),
                                "Share found by:".bright_green(),
                                miner.token
                            );
                        }
                    } else {
                        log::warn!(
                            "{} {}",
                            "Duplicated share submitted by:".bright_green(),
                            miner.token
                        );
                    }
                }
            }
            if let Some((header, entries)) = block_solved {
                let mut h = get_history()?;
                h.solved.insert(header, entries);
                save_history(&h)?;
                ctx.current_job = None;
            }
            request.respond(Response::empty(200))?;
        }
        _ => {}
    }
    Ok(())
}

fn new_puzzle(context: Arc<Mutex<MinerContext>>, req: PuzzleWrapper) -> Result<(), Box<dyn Error>> {
    let mut ctx = context.lock().unwrap();
    if ctx.current_job.as_ref().map(|j| j.puzzle.clone()) == req.puzzle {
        return Ok(());
    }
    if let Some(req) = req.puzzle.clone() {
        ctx.current_job = Some(Job {
            puzzle: req.clone(),
            shares: vec![],
            nonces: HashSet::new(),
        });
        let req_key = hex::decode(&req.key)?;

        if ctx.hasher.key() != req_key {
            println!("{}", "Initializing hasher...".bright_yellow());
            ctx.hasher = Arc::new(Context::new(&req_key, false));
        }

        let target = rust_randomx::Difficulty::new(req.target);
        println!(
            "{} Approximately {} hashes need to be calculated...",
            "Got new puzzle!".bright_yellow(),
            target.power()
        );
    } else {
        ctx.current_job = None;
        println!("No puzzles to mine...");
    }

    Ok(())
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Miner {
    token: String,
    mpn_addr: MpnAddress,
}

struct MinerContext {
    client: SyncClient,
    hasher: Arc<Context>,
    current_job: Option<Job>,
    eligible_miners: HashMap<String, Miner>,
}

fn create_tx(
    wallet: &mut Wallet,
    entries: HashMap<MpnAddress, Money>,
    remote_nonce: u32,
    remote_mpn_nonce: u64,
    pool_mpn_address: MpnAddress,
) -> Result<(bazuka::core::MpnDeposit, Vec<MpnTransaction>), Box<dyn Error>> {
    let mpn_id = bazuka::config::blockchain::get_blockchain_config().mpn_contract_id;
    let tx_builder = TxBuilder::new(&wallet.seed());
    let new_nonce = wallet.new_r_nonce().unwrap_or(remote_nonce + 1);
    let new_mpn_nonce = wallet
        .new_z_nonce(pool_mpn_address.account_index)
        .unwrap_or(remote_mpn_nonce);
    let sum_all = entries
        .iter()
        .map(|(_, m)| Into::<u64>::into(*m))
        .sum::<u64>();
    let tx = tx_builder.deposit_mpn(
        mpn_id,
        pool_mpn_address.clone(),
        0,
        new_nonce,
        bazuka::core::TokenId::Ziesha,
        sum_all.into(),
        bazuka::core::TokenId::Ziesha,
        0.into(),
    );
    wallet.add_deposit(tx.clone());
    let mut ztxs = Vec::new();
    for (i, (addr, mon)) in entries.iter().enumerate() {
        let tx = tx_builder.create_mpn_transaction(
            pool_mpn_address.account_index,
            0,
            addr.clone(),
            0,
            bazuka::core::TokenId::Ziesha,
            *mon,
            0,
            bazuka::core::TokenId::Ziesha,
            0.into(),
            new_mpn_nonce + i as u64,
        );
        wallet.add_zsend(tx.clone());
        ztxs.push(tx);
    }
    Ok((tx, ztxs))
}

fn get_miners() -> Result<HashMap<String, Miner>, Box<dyn Error>> {
    let miners_path = home::home_dir()
        .unwrap()
        .join(Path::new(".uzi-pool-miners"));
    Ok(if let Ok(mut f) = File::open(miners_path.clone()) {
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        let miners_bincode: Option<Vec<Miner>> = bincode::deserialize(&bytes).ok();
        if let Some(miners) = miners_bincode {
            File::create(miners_path)?.write_all(&serde_json::to_vec(&miners)?)?;
            miners
        } else {
            serde_json::from_slice(&bytes)?
        }
    } else {
        Vec::new()
    }
    .into_iter()
    .map(|m| (m.token.clone(), m))
    .collect())
}

fn main() {
    println!(
        "{} v{} - RandomX Mining Pool for Ziesha Cryptocurrency",
        "Uzi-Pool!".bright_green(),
        env!("CARGO_PKG_VERSION")
    );

    env_logger::init();
    let opt = Opt::from_args();
    println!("{} {}", "Listening to:".bright_yellow(), opt.listen);
    if opt.owner_reward_ratio > 1.0 || opt.owner_reward_ratio < 0.0 {
        println!("Owner reward ratio should be between 0.0 and 1.0!")
    }

    let server = Server::http(opt.listen).unwrap();
    let context = Arc::new(Mutex::new(MinerContext {
        client: SyncClient::new(
            bazuka::client::PeerAddress(opt.node),
            &opt.network,
            opt.miner_token.clone(),
        ),
        current_job: None,
        hasher: Arc::new(Context::new(b"", false)),
        eligible_miners: get_miners().unwrap(),
    }));

    let puzzle_getter = {
        let ctx = Arc::clone(&context);
        let opt = opt.clone();
        thread::spawn(move || loop {
            if let Err(e) = || -> Result<(), Box<dyn Error>> {
                let pzl = ureq::get(&format!("http://{}/miner/puzzle", opt.node))
                    .set("X-ZIESHA-MINER-TOKEN", &opt.miner_token)
                    .call()?
                    .into_string()?;

                let pzl_json: PuzzleWrapper = serde_json::from_str(&pzl)?;
                new_puzzle(ctx.clone(), pzl_json)?;
                Ok(())
            }() {
                log::error!("Error: {}", e);
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        })
    };

    let reward_sender = {
        let ctx = Arc::clone(&context);
        let opt = opt.clone();
        thread::spawn(move || loop {
            if let Err(e) = || -> Result<(), Box<dyn Error>> {
                let mut ctx = ctx.lock()?;
                ctx.eligible_miners = get_miners()?;
                let mut hist = get_history()?;
                let curr_height = ctx.client.get_height()?;
                let wallet_path = home::home_dir().unwrap().join(Path::new(".bazuka-wallet"));
                let mut wallet = Wallet::open(wallet_path.clone()).unwrap().unwrap();
                let curr_nonce = ctx
                    .client
                    .get_account(TxBuilder::new(&wallet.seed()).get_address())?
                    .account
                    .nonce;
                let curr_mpn_nonce = ctx
                    .client
                    .get_mpn_account(opt.pool_mpn_address.account_index)?
                    .account
                    .nonce;
                for (h, entries) in hist.solved.clone().into_iter() {
                    if let Some(mut actual_header) = ctx.client.get_header(h.number)? {
                        actual_header.proof_of_work.nonce = 0;
                        if curr_height - h.number >= opt.reward_delay {
                            hist.solved.remove(&h);
                            if actual_header == h {
                                let (tx, ztxs) = create_tx(
                                    &mut wallet,
                                    entries,
                                    curr_nonce,
                                    curr_mpn_nonce,
                                    opt.pool_mpn_address.clone(),
                                )?;
                                wallet.save(wallet_path.clone()).unwrap();
                                println!("Tx with nonce {} created...", tx.payment.nonce);
                                hist.sent.insert(h, (tx, ztxs));
                            }
                            save_history(&hist)?;
                        }
                    }
                }
                println!("Current nonce: {}", curr_nonce);
                let mut sent_sorted = hist.sent.clone().into_iter().collect::<Vec<_>>();
                sent_sorted.sort_unstable_by_key(|s| s.1 .0.payment.nonce);
                for (h, (tx, ztxs)) in sent_sorted {
                    if tx.payment.nonce > curr_nonce {
                        println!(
                            "Sending rewards for block #{} (Nonce: {})...",
                            h.number, tx.payment.nonce
                        );
                        ctx.client.transact_deposit(tx.clone())?;
                        for ztx in ztxs {
                            ctx.client.transact_zero(ztx.clone())?;
                        }
                    } else {
                        hist.sent.remove(&h);
                        println!("Tx with nonce {} removed...", tx.payment.nonce);
                        save_history(&hist)?;
                    }
                }

                Ok(())
            }() {
                log::error!("Error: {}", e);
            }
            std::thread::sleep(std::time::Duration::from_secs(60));
        })
    };

    for request in server.incoming_requests() {
        if let Err(e) = process_request(context.clone(), request, &opt) {
            log::error!("Error: {}", e);
        }
    }

    puzzle_getter.join().unwrap();
    reward_sender.join().unwrap();
}

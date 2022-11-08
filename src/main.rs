use colored::Colorize;
use rust_randomx::{Context, Hasher};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
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
#[structopt(name = "Uzi Pool", about = "Mine Zeeka with Uzi!")]
struct Opt {
    #[structopt(short = "n", long = "node")]
    node: SocketAddr,

    #[structopt(long, default_value = LISTEN)]
    listen: SocketAddr,

    #[structopt(long, default_value = "")]
    miner_token: String,

    #[structopt(long, default_value = "10")]
    share_easiness: usize,

    #[structopt(long, default_value = "10")]
    share_capacity: usize,
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
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Puzzle {
    key: String,
    blob: String,
    offset: usize,
    size: usize,
    target: u32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct PuzzleWrapper {
    puzzle: Option<Puzzle>,
}

fn job_solved(shares: &[Share]) {
    println!("{:?}", shares);
}

fn fetch_miner_token(req: &tiny_http::Request) -> Option<String> {
    for h in req.headers() {
        if h.field.equiv("X-ZEEKA-MINER-TOKEN") {
            return Some(h.value.clone().into());
        }
    }
    None
}

fn process_request(
    context: Arc<Mutex<MinerContext>>,
    mut request: tiny_http::Request,
    opt: &Opt,
) -> Result<(), Box<dyn Error>> {
    let mut ctx = context.lock().unwrap();

    let miner =
        if let Some(Some(miner)) = fetch_miner_token(&request).map(|tkn| ctx.miners.get(&tkn)) {
            miner.clone()
        } else {
            return Err(Box::<dyn Error>::from("Miner not authorized!".to_string()));
        };

    match request.url() {
        "/miner/puzzle" => {
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
            let sol: Solution = {
                let mut content = String::new();
                request.as_reader().read_to_string(&mut content)?;
                serde_json::from_str(&content)?
            };

            let mut block_solved = false;
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
                let (b, e) = (easy_puzzle.offset, easy_puzzle.offset + easy_puzzle.size);
                blob[b..e].copy_from_slice(&hex::decode(&sol.nonce)?);
                let out = hasher.hash(&blob);

                if out.meets_difficulty(share_diff) {
                    current_job.shares.push(Share {
                        miner: miner.clone(),
                        nonce: sol.nonce.clone(),
                    });
                    while current_job.shares.len() > opt.share_capacity {
                        current_job.shares.remove(0);
                    }
                    if out.meets_difficulty(block_diff) {
                        job_solved(&current_job.shares);
                        block_solved = true;

                        println!("{} {}", "Solution found by:".bright_green(), miner.token);
                        ureq::post(&format!("http://{}/miner/solution", opt.node))
                            .set("X-ZEEKA-MINER-TOKEN", &opt.miner_token)
                            .send_json(json!({ "nonce": sol.nonce }))?;
                    } else {
                        println!("{} {}", "Share found by:".bright_green(), miner.token);
                    }
                    request.respond(Response::from_string("OK"))?;
                }
            }
            if block_solved {
                ctx.current_job = None;
            }
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
    pub_key: String,
}

struct MinerContext {
    miners: HashMap<String, Miner>,
    hasher: Arc<Context>,
    current_job: Option<Job>,
}

fn main() {
    println!(
        "{} v{} - RandomX Mining Pool for Zeeka Cryptocurrency",
        "Uzi-Pool!".bright_green(),
        env!("CARGO_PKG_VERSION")
    );

    env_logger::init();
    let opt = Opt::from_args();
    println!("{} {}", "Listening to:".bright_yellow(), opt.listen);

    let server = Server::http(opt.listen).unwrap();

    let context = Arc::new(Mutex::new(MinerContext {
        miners: [Miner {
            token: "haha".into(),
            pub_key: "hehe".into(),
        }]
        .into_iter()
        .map(|m| (m.token.clone(), m))
        .collect(),
        current_job: None,
        hasher: Arc::new(Context::new(b"", false)),
    }));

    let puzzle_getter = {
        let ctx = Arc::clone(&context);
        let opt = opt.clone();
        thread::spawn(move || loop {
            if let Err(e) = || -> Result<(), Box<dyn Error>> {
                let pzl = ureq::get(&format!("http://{}/miner/puzzle", opt.node))
                    .set("X-ZEEKA-MINER-TOKEN", &opt.miner_token)
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

    for request in server.incoming_requests() {
        if let Err(e) = process_request(context.clone(), request, &opt) {
            log::error!("Error: {}", e);
        }
    }

    puzzle_getter.join().unwrap();
}
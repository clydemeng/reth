use eyre::Result;
use revm::{
    bytecode::Bytecode,
    context::TxEnv,
    database::{CacheDB, EmptyDB},
    primitives::{address, keccak256, Address, Bytes, TxKind, U256},
    state::AccountInfo,
    Context, ExecuteCommitEvm, MainContext, MainBuilder,
};
use std::time::Instant;

fn encode_transfer(to: Address, amount: U256) -> Bytes {
    // function transfer(address,uint256) -> 0xa9059cbb
    let mut data = Vec::with_capacity(4 + 32 + 32);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(to.as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

fn encode_deposit() -> Bytes {
    // deposit() selector for the WBNB example used in prior benches
    Bytes::from(vec![0xd0, 0xe3, 0x0d, 0xb0])
}

fn load_code(path: &str) -> Bytes {
    Bytes::from(std::fs::read(path).expect("read erc20 runtime bin"))
}

fn main() -> Result<()> {
    // args: iters path_to_runtime_bin
    let iters: u64 = std::env::args().nth(1).map(|s| s.parse().unwrap()).unwrap_or(10_000);
    let code_path = std::env::args().nth(2).unwrap_or_else(|| {
        "/Users/ruojunm/workspace_2025/rust/remvc_workspace/block_processing_improve/other_shared_files/smart_contract/wbnb/wbnb.bin".to_string()
    });

    let contract = address!("0000000000000000000000000000000000009999");
    let from = address!("0000000000000000000000000000000000000001");
    let to = address!("000000000000000000000000000000000000BEEF");
    let amount = U256::from(1u64);
    let warmup = 0u64;

    let code_bytes = load_code(&code_path);
    let hash = keccak256(code_bytes.clone());

    let initial = amount * U256::from(iters + warmup);

    // Prepare DB and code/state
    let mut cache_db = CacheDB::new(EmptyDB::default());
    let mut info = AccountInfo::default();
    let b = Bytecode::new_raw(code_bytes.clone());
    info.set_code_and_hash(b, hash);
    cache_db.insert_account_info(contract, info);
    cache_db.insert_account_info(from, AccountInfo { balance: initial, ..Default::default() });

    // Build EVM
    let mut evm = Context::mainnet().with_db(cache_db).build_mainnet();

    // Deposit via msg.value
    let tx = TxEnv::builder()
        .caller(from)
        .kind(TxKind::Call(contract))
        .data(encode_deposit())
        .gas_limit(30_000_000)
        .gas_price(0)
        .value(initial)
        .nonce(0)
        .build()
        .unwrap();
    let _ = evm.transact_commit(tx).unwrap();

    // Warmup transfers
    for i in 0..warmup {
        let tx = TxEnv::builder()
            .caller(from)
            .kind(TxKind::Call(contract))
            .data(encode_transfer(to, amount))
            .gas_limit(30_000_000)
            .gas_price(0)
            .nonce(1 + i)
            .build()
            .unwrap();
        let _ = evm.transact_commit(tx).unwrap();
    }

    // Timed loop
    let t = Instant::now();
    for i in 0..iters {
        let tx = TxEnv::builder()
            .caller(from)
            .kind(TxKind::Call(contract))
            .data(encode_transfer(to, amount))
            .gas_limit(30_000_000)
            .gas_price(0)
            .nonce(1 + warmup + i)
            .build()
            .unwrap();
        let _ = evm.transact_commit(tx).unwrap();
    }
    let d = t.elapsed();

    println!(
        "reth-baseline-erc20 n_iters={} | Interpreter avg={:?} total={:?}",
        iters,
        d / (iters as u32),
        d
    );

    Ok(())
}



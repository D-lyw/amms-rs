use alloy::{primitives::Address, sol};
use eyre::Result;
use std::{
    collections::HashSet,
    env,
    fs::File,
    io::{BufRead, BufReader},
    str::FromStr,
};

// YieldBasis 特殊 TwoCrypto 池（v2.1.0d periphery 路径），详细背景见 twocrypto_v210d 模块文档。
pub const YIELDBASIS_SPECIAL_TWOCRYPTO_POOLS: [&str; 4] = [
    "0xd9ff8396554a0d18b2cfbec53e1979b7ecce8373",
    "0x83f24023d15d835a213df24fd309c47dab5beb32",
    "0x6e5492f8ea2370844ee098a56dd88e1717e4a9c2",
    "0xf1f435b05d255a5dbde37333c0f61da6f69c6127",
];

pub const PRIORITY_STABLE: [&str; 3] = [
    "0xa632d59b9b804a956bfaa9b48af3a1b74808fc1f",
    "0xd001ae433f254283fece51d4acce8c53263aa186",
    "0x5dc1bf6f1e983c0b21efb003c105133736fa0743",
];

pub const PRIORITY_TWO: [&str; 3] = [
    "0xca546ae6c3b2bb9fba2b6e5eeb0881097cece5b0",
    "0x77146b0a1d08b6844376df6d9da99ba7f1b19e71",
    "0x660a554fc97fabecff47d200367ca1a8bf49c82b",
];

pub const PRIORITY_TRI: [&str; 3] = [
    "0xf5f5b97624542d72a9e06f04804bf81baa15e2b4",
    "0x7f86bf177dd4f3494b841a37e810a34dd56c829b",
    "0x4ebdf703948ddcea3b11f675b4d1fba9d2414a14",
];

sol! {
    #[sol(rpc)]
    interface ICurveStablePoolNG {
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoPoolNG {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoPoolMeta {
        function future_A_gamma_time() external view returns (uint256);
    }
}

#[derive(serde::Deserialize)]
struct PoolIndexEntry {
    address: String,
    pool_type: Option<String>,
    curve_pool_type: Option<String>,
}

fn push_unique(addrs: &mut Vec<Address>, seen: &mut HashSet<Address>, addr: Address, limit: usize) {
    if addrs.len() >= limit {
        return;
    }
    if seen.insert(addr) {
        addrs.push(addr);
    }
}

fn seed_priority(priority: &[&str], limit: usize) -> Result<(Vec<Address>, HashSet<Address>)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for p in priority {
        if out.len() >= limit {
            break;
        }
        let addr = Address::from_str(p)?;
        if seen.insert(addr) {
            out.push(addr);
        }
    }

    Ok((out, seen))
}

pub fn load_curve_ng_pools(
    limit_stable: usize,
    limit_two: usize,
    limit_tri: usize,
) -> Result<(Vec<Address>, Vec<Address>, Vec<Address>)> {
    let (mut stable, mut stable_seen) = seed_priority(&PRIORITY_STABLE, limit_stable)?;
    let (mut two, mut two_seen) = seed_priority(&PRIORITY_TWO, limit_two)?;
    let (mut tri, mut tri_seen) = seed_priority(&PRIORITY_TRI, limit_tri)?;

    if stable.len() >= limit_stable && two.len() >= limit_two && tri.len() >= limit_tri {
        return Ok((stable, two, tri));
    }

    // Optional external pool index expansion for wider real-pool coverage.
    let path = env::var("CURVE_NG_POOL_INDEX_PATH").unwrap_or_else(|_| {
        "/Users/d-lyw/D-lyw/aave-liquidation/config/pool_index_1.json".to_string()
    });

    let file = match File::open(&path) {
        Ok(file) => file,
        Err(err) => {
            println!(
                "CurveNG pool index not found at {} ({}), using priority pools only",
                path, err
            );
            return Ok((stable, two, tri));
        }
    };

    for line in BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        if line.trim().is_empty() {
            continue;
        }

        let entry: PoolIndexEntry = match serde_json::from_str(&line) {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        if entry.pool_type.as_deref() != Some("curve") {
            continue;
        }

        let addr = match Address::from_str(&entry.address) {
            Ok(addr) => addr,
            Err(_) => continue,
        };

        match entry.curve_pool_type.as_deref() {
            Some("StableSwapNG") => {
                push_unique(&mut stable, &mut stable_seen, addr, limit_stable);
            }
            Some("TwoCryptoNG") => {
                push_unique(&mut two, &mut two_seen, addr, limit_two);
            }
            Some("TriCryptoNG") => {
                push_unique(&mut tri, &mut tri_seen, addr, limit_tri);
            }
            _ => {}
        }

        if stable.len() >= limit_stable && two.len() >= limit_two && tri.len() >= limit_tri {
            break;
        }
    }

    Ok((stable, two, tri))
}

use alloy::primitives::{address, U256};
use amms::{
    amms::{
        amm::{AutomatedMarketMaker, AMM},
        curve_legacy::{CurveLegacyPool, CurveLegacyPoolType, LegacyStableSwapType},
    },
    state_space::StateSpace,
};

fn wad(n: u64) -> U256 {
    U256::from(n) * U256::from(10).pow(U256::from(18))
}

fn usdc_amount(n: u64) -> U256 {
    U256::from(n) * U256::from(10).pow(U256::from(6))
}

fn make_base_pool() -> CurveLegacyPool {
    let mut pool = CurveLegacyPool::new(
        address!("7f90122BF0700F9E7e1F688fe926940E8839F353"),
        CurveLegacyPoolType::StableSwap,
    );
    pool.stable_type = LegacyStableSwapType::Plain;
    pool.n_coins = 2;
    pool.coins = vec![
        address!("FF970A61A04b1cA14834A43f5dE4533eBDDB5CC8"),
        address!("Fd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"),
    ];
    pool.balances = vec![usdc_amount(10_000_000), usdc_amount(10_000_000)];
    pool.decimals = vec![6, 6];
    pool.amp = Some(U256::from(2_000));
    pool.uses_a_precision = false;
    pool.fee = U256::from(4_000_000u64);
    pool.admin_fee = U256::ZERO;
    pool.total_supply = Some(wad(20_000_000));
    pool
}

fn make_meta_pool(base_pool: &CurveLegacyPool) -> CurveLegacyPool {
    let mut pool = CurveLegacyPool::new(
        address!("a827a652ead76c6b0b3d19dba05452e06e25c27e"),
        CurveLegacyPoolType::StableSwap,
    );
    pool.stable_type = LegacyStableSwapType::Meta;
    pool.n_coins = 2;
    pool.coins = vec![
        address!("D22a58f79e9481D1a88e00c343885A588b34b68B"),
        base_pool.address,
    ];
    pool.underlying_coins = vec![pool.coins[0], base_pool.coins[0], base_pool.coins[1]];
    pool.balances = vec![wad(2_000_000), wad(20_000_000)];
    pool.decimals = vec![18, 18];
    pool.amp = Some(U256::from(500));
    pool.uses_a_precision = false;
    pool.fee = U256::from(4_000_000u64);
    pool.admin_fee = U256::ZERO;
    pool.base_pool_address = Some(base_pool.address);
    pool.base_lp_token = Some(base_pool.address);
    pool.base_token_index = Some(1);
    pool.base_virtual_price = Some(wad(1));
    pool.total_supply = Some(wad(22_000_000));
    pool.base_pool_view = base_pool.build_base_view();
    pool
}

#[test]
fn meta_pool_tokens_and_route_resolution_use_underlying_space() {
    let base_pool = make_base_pool();
    let meta_pool = make_meta_pool(&base_pool);

    assert!(meta_pool.is_meta_pool());
    assert_eq!(meta_pool.tokens(), meta_pool.underlying_coins);
    assert_eq!(
        meta_pool.decimals(base_pool.coins[0]),
        6,
        "base pool coin decimals should be served through base view"
    );

    let route = meta_pool
        .resolve_swap_route(meta_pool.coins[0], base_pool.coins[1])
        .expect("meta -> base route should resolve");
    assert!(matches!(
        route,
        amms::amms::curve_legacy::CurveLegacySwapRoute::MetaToBase { .. }
    ));

    let route = meta_pool
        .resolve_swap_route(base_pool.coins[0], base_pool.coins[1])
        .expect("base -> base route should resolve");
    assert!(matches!(
        route,
        amms::amms::curve_legacy::CurveLegacySwapRoute::BaseToBase { .. }
    ));
}

#[test]
fn state_space_rebuilds_meta_base_views() {
    let base_pool = make_base_pool();
    let mut meta_pool = make_meta_pool(&base_pool);
    meta_pool.base_pool_view = None;

    let mut state = StateSpace::default();
    state.insert_amm(AMM::CurveLegacyPool(base_pool.clone()));
    state.insert_amm(AMM::CurveLegacyPool(meta_pool));

    let AMM::CurveLegacyPool(stored_meta) = state
        .get(&address!("a827a652ead76c6b0b3d19dba05452e06e25c27e"))
        .expect("meta pool inserted")
    else {
        panic!("expected curve legacy pool");
    };

    let view = stored_meta
        .base_pool_view
        .as_ref()
        .expect("base view should be rebuilt");
    assert_eq!(view.address, base_pool.address);
    assert_eq!(view.coins, base_pool.coins);
}

#[test]
fn state_space_preserves_initialized_meta_base_view_without_base_pool_entry() {
    let base_pool = make_base_pool();
    let meta_pool = make_meta_pool(&base_pool);

    let mut state = StateSpace::default();
    state.insert_amm(AMM::CurveLegacyPool(meta_pool));

    let AMM::CurveLegacyPool(stored_meta) = state
        .get(&address!("a827a652ead76c6b0b3d19dba05452e06e25c27e"))
        .expect("meta pool inserted")
    else {
        panic!("expected curve legacy pool");
    };

    let view = stored_meta
        .base_pool_view
        .as_ref()
        .expect("initialized base view should be preserved");
    assert_eq!(view.address, base_pool.address);
    assert_eq!(view.coins, base_pool.coins);
}

#[test]
fn meta_pool_can_simulate_underlying_routes_locally() {
    let base_pool = make_base_pool();
    let meta_pool = make_meta_pool(&base_pool);

    let eurs = meta_pool.coins[0];
    let usdc = base_pool.coins[0];
    let usdt = base_pool.coins[1];

    let out_meta_to_base = meta_pool
        .simulate_swap(eurs, usdt, wad(10))
        .expect("meta -> base underlying quote should work");
    assert!(out_meta_to_base > U256::ZERO);

    let out_base_to_meta = meta_pool
        .simulate_swap(usdc, eurs, usdc_amount(10))
        .expect("base -> meta underlying quote should work");
    assert!(out_base_to_meta > U256::ZERO);

    let out_base_to_base = meta_pool
        .simulate_swap(usdc, usdt, usdc_amount(10))
        .expect("base -> base underlying quote should work");
    assert!(out_base_to_base > U256::ZERO);
}

#[test]
fn meta_pool_simulate_swap_mut_advances_underlying_routes() {
    let base_pool = make_base_pool();
    let meta_pool = make_meta_pool(&base_pool);

    let eurs = meta_pool.coins[0];
    let usdc = base_pool.coins[0];
    let usdt = base_pool.coins[1];
    let lp_idx = meta_pool.base_token_index.unwrap();

    let cases = [
        ("meta_to_base", eurs, usdt, wad(10)),
        ("base_to_meta", usdc, eurs, usdc_amount(10)),
        // 加大金额确保滑点可见（10 USDC 在 10M 储备上会被取整吞掉）
        ("base_to_base", usdc, usdt, usdc_amount(1_000_000)),
    ];

    for (name, token_in, token_out, amount_in) in cases {
        let mut pool = meta_pool.clone();
        let pre_meta_balances = pool.balances.clone();
        let pre_base_balances = pool.base_pool_view.as_ref().unwrap().balances.clone();
        let pre_base_supply = pool.base_pool_view.as_ref().unwrap().total_supply;

        let out_read = meta_pool
            .simulate_swap(token_in, token_out, amount_in)
            .unwrap();
        let out_mut = pool
            .simulate_swap_mut(token_in, token_out, amount_in)
            .unwrap();
        assert_eq!(out_mut, out_read, "{}: mut output must match read", name);

        // 状态推进：同输入再次模拟，输出必须变小
        let out_again = pool.simulate_swap(token_in, token_out, amount_in).unwrap();
        assert!(out_again < out_read, "{}: state should advance", name);

        // 缓存 spot price 已刷新（非零、有限）
        let spot = pool.spot_price(token_in, token_out).unwrap();
        assert!(
            spot > 0.0 && spot.is_finite(),
            "{}: spot price refreshed",
            name
        );

        match name {
            "meta_to_base" => {
                // meta 池：卖出 meta 币增加、LP 持仓减少；base 快照：base_j 减少、LP 总量销毁
                assert_eq!(pool.balances[0], pre_meta_balances[0] + amount_in);
                assert!(pool.balances[lp_idx] < pre_meta_balances[lp_idx]);
                assert_eq!(
                    pool.base_pool_view.as_ref().unwrap().balances[1],
                    pre_base_balances[1] - out_read
                );
                assert!(pool.base_pool_view.as_ref().unwrap().total_supply < pre_base_supply);
            }
            "base_to_meta" => {
                // meta 池：LP 持仓增加、meta_j 减少；base 快照：base_i 增加、LP 总量铸造
                assert!(pool.balances[lp_idx] > pre_meta_balances[lp_idx]);
                assert_eq!(pool.balances[0], pre_meta_balances[0] - out_read);
                assert_eq!(
                    pool.base_pool_view.as_ref().unwrap().balances[0],
                    pre_base_balances[0] + amount_in
                );
                assert!(pool.base_pool_view.as_ref().unwrap().total_supply > pre_base_supply);
            }
            "base_to_base" => {
                // 只动 base 快照：meta 表完全不变，base_i 增 / base_j 减，LP 总量不变
                assert_eq!(pool.balances, pre_meta_balances);
                assert_eq!(
                    pool.base_pool_view.as_ref().unwrap().balances[0],
                    pre_base_balances[0] + amount_in
                );
                assert_eq!(
                    pool.base_pool_view.as_ref().unwrap().balances[1],
                    pre_base_balances[1] - out_read
                );
                assert_eq!(
                    pool.base_pool_view.as_ref().unwrap().total_supply,
                    pre_base_supply
                );
            }
            _ => unreachable!(),
        }
    }
}

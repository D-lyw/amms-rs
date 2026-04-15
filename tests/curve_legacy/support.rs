use alloy::{
    primitives::{address, Address},
    sol,
};
use amms::amms::curve_legacy::types::CurveLegacyPoolType;

pub const LEGACY_BATCH_POOLS: &[(Address, u8)] = &[
    (address!("1005F7406f32a61BD760CfA14aCCd2737913d546"), 0),
    (address!("4e0915C88bC70750D68C481540F081fEFaF22273"), 0),
    (address!("752eBeb79963cf0732E9c0fec72a49FD1DEfAEAC"), 1),
    (address!("80466c64868E1ab14a1Ddf27A676C3fcBE638Fe5"), 0),
    (address!("9838eCcC42659FA8AA7daF2aD134b53984c9427b"), 1),
    (address!("98638FAcf9a3865cd033F36548713183f6996122"), 1),
    (address!("AdCFcf9894335dC340f6Cd182aFA45999F45Fc44"), 1),
    (address!("B576491F1E6e5E62f1d8F26062Ee822B40B0E0d4"), 1),
    (address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"), 1),
    (address!("DcEF968d416a41Cdac0ED8702fAC8128A64241A2"), 0),
    (address!("E84f5b1582BA325fDf9cE6B0c1F087ccfC924e54"), 1),
    (address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"), 0),
];

pub fn legacy_pool_matrix() -> Vec<(&'static str, Address, CurveLegacyPoolType)> {
    vec![
        (
            "rETH-wstETH",
            address!("447Ddd4960d9fdBF6af9a790560d0AF76795CB08"),
            CurveLegacyPoolType::StableSwap,
        ),
        (
            "ETHx-WETH",
            address!("59Ab5a5b5d617E478a2479B0cAD80DA7e2831492"),
            CurveLegacyPoolType::StableSwap,
        ),
        (
            "3pool",
            address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
            CurveLegacyPoolType::StableSwap,
        ),
        (
            "TricryptoUSDT",
            address!("80466c64868E1ab14a1Ddf27A676C3fcBE638Fe5"),
            CurveLegacyPoolType::CryptoSwap,
        ),
        (
            "FRAX-USDC",
            address!("DcEF968d416a41Cdac0ED8702fAC8128A64241A2"),
            CurveLegacyPoolType::StableSwap,
        ),
        (
            "Tricrypto2",
            address!("D51a44d3FaE010294C616388b506AcdA1bfAAE46"),
            CurveLegacyPoolType::CryptoSwap,
        ),
        (
            "LDO-USDC",
            address!("3211C6cBeF1429da3D0d58494938299C92Ad5860"),
            CurveLegacyPoolType::CryptoSwap,
        ),
        (
            "WETH-Betherfi",
            address!("5FAE7E604FC3e24fd43A72867ceBaC94c65b404A"),
            CurveLegacyPoolType::CryptoSwap,
        ),
        (
            "WETH-rETH",
            address!("0f3159811670c117c372428D4E69AC32325e4D0F"),
            CurveLegacyPoolType::CryptoSwap,
        ),
    ]
}

sol! {
    #[sol(rpc)]
    interface ICurveStablePool {
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveCryptoPool {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ICurveLegacyCryptoSwapUpdate {
        function D() external view returns (uint256);
        function balances(uint256 i) external view returns (uint256);
    }
}

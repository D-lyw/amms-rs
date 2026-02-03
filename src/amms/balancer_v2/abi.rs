use alloy::sol;

sol! {
    struct SwapStep {
        bytes32 poolId;
        uint256 assetInIndex;
        uint256 assetOutIndex;
        uint256 amount;
        bytes userData;
    }

    #[sol(rpc)]
    interface IVault {
        function getPoolTokens(bytes32 poolId) external view returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock);
        function queryBatchSwap(uint8 kind, SwapStep[] memory swaps, address[] memory assets, FundManagement memory funds) external returns (int256[] memory assetDeltas);
        event Swap(bytes32 indexed poolId, address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut);
        event PoolBalanceChanged(bytes32 indexed poolId, address indexed liquidityProvider, address[] tokens, int256[] deltas, uint256[] protocolFeeAmounts);
        // Asset Manager moves funds between cash and managed balances
        // cashDelta: change in Vault's cash balance (positive = deposit, negative = withdraw)
        // managedDelta: change in managed balance (opposite of cashDelta for transfers)
        event PoolBalanceManaged(bytes32 indexed poolId, address indexed assetManager, address indexed token, int256 cashDelta, int256 managedDelta);
    }

    interface IWeightedPool {
        function getNormalizedWeights() external view returns (uint256[] memory);
        function getSwapFeePercentage() external view returns (uint256);
    }

    interface IStablePool {
        function getAmplificationParameter() external view returns (uint256 value, bool isUpdating, uint256 precision);
        function getSwapFeePercentage() external view returns (uint256);
    }

    interface IComposableStablePool {
        function getBptIndex() external view returns (uint256);
    }

    interface IGetPoolId {
        function getPoolId() external view returns (bytes32);
    }

    struct SingleSwap {
        bytes32 poolId;
        uint8 kind;
        address assetIn;
        address assetOut;
        uint256 amount;
        bytes userData;
    }

    struct FundManagement {
        address sender;
        bool fromInternalBalance;
        address payable recipient;
        bool toInternalBalance;
    }

    #[sol(rpc)]
    interface IBalancerQueries {
        function querySwap(SingleSwap memory singleSwap, FundManagement memory funds) external returns (uint256);
    }

    #[sol(rpc)]
    interface IRateProvider {
        function getRate() external view returns (uint256);
    }
}

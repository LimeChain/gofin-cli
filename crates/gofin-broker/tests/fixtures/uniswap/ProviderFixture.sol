// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

// Deliberately permissionless, synthetic test token and execution target. This
// contract is installed ONLY in owned Anvil instances; it is not Uniswap code.
// Tests prove actual EVM cash movements and receipt/event reconciliation while
// provider HTTP responses and filler execution remain controlled fixtures.
contract ProviderFixture {
    mapping(address => uint256) public balanceOf;
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Fill(bytes32 indexed orderHash, address indexed filler, address indexed swapper, uint256 nonce);
    function decimals() external pure returns (uint8) { return 6; }
    function symbol() external pure returns (string memory) { return "SYNTH"; }
    function allowance(address, address) external pure returns (uint256) { return 10_000_000; }
    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }
    function move(address from, address to, uint256 amount) external {
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }
    function exchange(address tokenIn, address tokenOut, address swapper, uint256 amountIn, uint256 amountOut, bytes32 orderHash, uint256 nonce) external payable {
        if (tokenIn == address(0)) require(msg.value == amountIn);
        else ProviderFixture(tokenIn).move(swapper, address(this), amountIn);
        if (tokenOut == address(0)) {
            (bool ok,) = payable(swapper).call{value: amountOut}("");
            require(ok);
        } else ProviderFixture(tokenOut).move(address(this), swapper, amountOut);
        if (orderHash != bytes32(0)) emit Fill(orderHash, msg.sender, swapper, nonce);
    }
}

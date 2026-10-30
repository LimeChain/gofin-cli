Canonical Multicall3 runtime bytecode extracted from the signed deployment transaction in
https://github.com/mds1/multicall3/blob/b667d67ecfa5361a81e8f110234ce242613b0012/README.md#new-deployments

The creation prefix copies 0x0ee0 bytes starting at creation offset 0x20. The
checked-in file contains those 3808 bytes, including Solidity metadata. Its
keccak256 hash is `0xd5c15df687b16f2ff992fc8d767b4216323184a2bbc6ee2f9c398c318e770891`. Runtime
reads compare the full keccak256 hash at the requested block before aggregation.
A deployment with different code falls back to direct reads. No financial writes
are sent to Multicall. Upstream license: MIT.

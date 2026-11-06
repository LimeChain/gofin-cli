# Architecture backlog

The architecture is built as an MVP foundation whose fundamental building blocks are intents and the operations that result from them. Items here extend that foundation later, one at a time, each as a proposal against the agreed architecture.

| # | Item | Notes |
| --- | --- | --- |
| B1 | Streaming | Progress streaming for long-running work. |
| B2 | Plans | Ordered lists of intents approved once and executed step by step (see the paused [intent model](../specs/intent-model.md)). |
| B3 | Multi-operation intents (currently called "compound actions"; name to be agreed) | One intent serviced by several operations, e.g. a bridge (allowance, burn, mint) or approve-and-swap. |
| B4 | Other execution authentication providers | Design for providers beyond macOS Touch ID, e.g. remote approval from a phone. |
| B5 | Other protected secrets storage providers | Design for storage beyond macOS Keychain, e.g. other keystores or 1Password. |
| B6 | Venue-agnostic actions | Actions such as `evm.quote` or a generic swap that are not tied to one venue. Bounds add selection criteria (e.g. cheapest first) and an optional allowed-venues bound that defaults to all venues; the outcome reports the chosen venue. Work as, near or with multi-operation intents. |
| B7 | Linux support | Likely changes the execution authentication provider, protected secrets storage and operating system integrations. |
| B8 | Personal limits | Optional personal limits (e.g. USD budgets) designed so they never cause bricking. |

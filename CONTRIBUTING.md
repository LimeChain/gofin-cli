# Contributing to GoFin

Start with a focused improvement you can explain and verify: an unclear message,
a documentation example, a regression for a reproducible bug, or an adapter fix.
GoFin is in beta; see the [project overview](README.md) for its current scope.

## Choose a change and create a branch

Check [open issues](https://github.com/LimeChain/gofin-cli/issues) and
[pull requests](https://github.com/LimeChain/gofin-cli/pulls) for related work.
For a bug, describe the expected result and a small reproduction. For a new
feature, explain the user workflow it enables. Discuss scope with maintainers
before starting a large integration or authorization change.

Fork the repository on GitHub if you do not have write access. Clone your fork,
add the project as `upstream`, and create a branch from its current `main`.
Replace `YOUR-USERNAME` below with your GitHub username:

```sh
git clone https://github.com/YOUR-USERNAME/gofin-cli.git
cd gofin-cli
git remote add upstream https://github.com/LimeChain/gofin-cli.git
git fetch upstream
git switch -c docs/my-first-improvement upstream/main
```

If you have write access, you can clone the project directly and create a branch
from `origin/main`. Choose a short branch name describing the change. Protected
`main` accepts changes through pull requests; do not push directly to it.

## Start without real funds

You need Git, Make, Python 3 and Rust installed through rustup. The repository
pins Rust, rustfmt and Clippy in [rust-toolchain.toml](rust-toolchain.toml).
Apple’s Command Line Tools and Swift are needed for macOS bundle/native tests.
Anvil is only needed for explicit blockchain fork tests.

```sh
make help
make dev-env-start
make dev-env-cli ARGS='capabilities --json'
make dev-env-cli ARGS="read synthetic.echo --input '{\"value\":\"hello\"}' --json"
make dev-env-stop
```

The defaults are `DEV_ENV=local` and `DEV_MODE=synthetic`. This environment uses
synthetic credentials, state and approval; it needs no installation, exchange
account or real funds. `synthetic.echo` returns the supplied value without a
network call. Each worktree has its own environment. For a named environment,
pass the same `DEV_ENV=feature-a` to every lifecycle and CLI command.

After editing Rust code, stop and start the environment to rebuild both binaries
while retaining configuration, journal state and original operation identities:

```sh
make dev-env-stop
make dev-env-start
```

`dev-env-cli` uses the running binary snapshot; it does not rebuild. Stop the
environment when you finish. Use `make dev-env-reset` only to intentionally
remove a stopped synthetic environment’s state.

Use `make dev-env-*` for normal iteration. `make dev`, `make setup` and
`make update` use or modify the installed protected runtime. See the
[development environment guide](docs/testing/development-environments.md).

## Find the right component

| Change | Start here |
| --- | --- |
| Commands, help and output | `gofin-cli`, `gofin-discovery`, `gofin-presentation` |
| Exchange or protocol behavior | The relevant adapter, such as `gofin-binance`, `gofin-kraken`, `gofin-ethereum` or `gofin-jupiter` |
| Financial limits, accounting and recovery | `gofin-engine`, `gofin-domain`, `gofin-store` |
| Protected configuration, credentials and approval | `gofin-broker`, `gofin-credentials`, `gofin-human-terminal`, `gofin-authorization`, `native/` |
| Shared network transport or pricing | `gofin-evm`, `gofin-solana`, `gofin-exchange`, `gofin-pricing` |
| Development environments and delivery | `scripts/`, `Makefile`, `packaging/` |

The [architecture guide](docs/rust-architecture.md) explains the boundaries.
[AGENTS.md](AGENTS.md) records development principles. Read the sections relevant
to your change. Keep parsing and presentation in the CLI, financial authority
in the broker/engine, and venue-specific behavior in its adapter.

## Verify your change

Start with the smallest meaningful regression when changing behavior. Preserve
existing assertions and run the affected suite after the implementation is stable.

| Change | Relevant checks |
| --- | --- |
| Documentation | Check links and commands; `git diff --check` |
| CLI, discovery or presentation | `make test-ui`, plus affected terminal/JSON tests |
| One adapter | `cargo test --locked -p <crate>`, plus affected broker workflows |
| Shared engine, store or authorization | Affected crate tests and broker workflows; `make test-integration` for the combined surface |
| Broad workspace or dependency changes | `make test` and `make check` |
| Development scripts | The affected Python regression; for lifecycle, `python3 scripts/test-gofin-dev.py` |
| Installation or delivery | Relevant synthetic fixtures or `make test-delivery` |

Replace `<crate>` with the affected package, for example
`cargo test --locked -p gofin-kraken`. For Rust changes, check formatting and Clippy
for affected targets; `make check` runs both across the workspace. `make test`
covers Rust tests, not every script, fork or live workflow. Use the
[verification selection table](docs/testing/README.md#verification-selection)
to decide when broader checks are needed.

Default tests use synthetic credentials and isolated fixtures. Fork tests, live
reads, live writes and native Touch ID checks are separate evidence. Report what
you ran, including failures, ignored tests and missing prerequisites. Follow the
[live-test contract](docs/testing/README.md#live-test-contract) when live
verification is needed.

For financial execution changes, verify approval bounds, original operation
identities, reservations and restart recovery. A transaction hash does not
establish settlement. Reconcile uncertain results instead of submitting a new
write identity.

## Send a pull request

Review your diff, commit the focused change, and push your branch to your fork:

```sh
git diff --check
git add CONTRIBUTING.md
git commit -m "docs: clarify the contributor workflow"
git push -u origin docs/my-first-improvement
```

Adapt the file list, commit message and branch name to your change. Open a PR
against `LimeChain/gofin-cli`'s `main`; link a related issue when useful. Explain the
final change for a reviewer who has not seen the discussion:

- What behavior changes and why it matters.
- How you verified it, with commands and outcomes.
- Relevant compatibility, persistence or verification limits.

Keep help, schemas, examples and documentation aligned. Use synthetic or redacted
examples. Never commit credentials, private keys, seed phrases, private account
mappings or raw live journals/logs.

The [regression workflow](.github/workflows/test.yml) runs on macOS for pushes;
it has no pull-request trigger. Opening or updating a fork PR therefore does
not automatically run upstream CI. Include your local check results so
maintainers can arrange repository CI before merge.

Be respectful and specific in reports and review replies. Separate observed
results from assumptions; explain limitations in plain language. A short report
with relevant commands and outcomes is enough for a small change.

Do not post exploit details or sensitive evidence for a suspected vulnerability
in a public issue or PR. A dedicated private reporting channel is not yet
documented. The [security contract](docs/security.md) describes the current
execution boundary and verification limits.

## Licensing contributions

Contributions to GoFin’s original code and documentation use GPL version 3 only,
as set out in [LICENSE](LICENSE). Contribute material you have authority to
license. Keep existing third-party notices and record the source, version and
applicable terms when adding copied or adapted material. Preserve the relevant
[third-party notices](THIRD_PARTY_NOTICES.md); the project GPL grant does not
replace those terms.

## Further reading

- [Documentation index](docs/README.md)
- [Security and protected execution](docs/security.md)
- [Testing and verification](docs/testing/README.md)
- [Development environments](docs/testing/development-environments.md)
- [Release maintenance](docs/releases.md)

# Distribute Fin through ChatGPT/Codex and Claude plugins

Status: Requested on 2026-10-16 as item 44. No release is assigned. Implementation and publication have not been reported.

## Objective

Package and distribute Fin's existing companion skill as installable plugins for ChatGPT/Codex and Claude Code. Preserve `skills/fin/SKILL.md` as the canonical behavioral source and retain `fin skill install|inspect|update|remove --agent codex|claude` as an independent local/offline installation path.

Current distribution formats and review requirements must be revalidated during implementation against the official [OpenAI plugin packaging documentation](https://developers.openai.com/plugins/build/plugins) and [Claude Code plugin documentation](https://code.claude.com/docs/en/plugins). Do not assume the two marketplaces, manifests or supported components are interchangeable.

## Packaging scope

- Build a ChatGPT/Codex plugin package with the supported portable manifest and required OpenAI presentation metadata, assets and local-executor declaration where applicable. Package the Fin skill and a concise onboarding workflow that discovers the locally installed CLI.
- Build a Claude Code plugin package with its supported manifest and marketplace metadata, packaging the same Fin instructions without maintaining an independently edited behavioral copy.
- Prefer skills-only packages initially. Do not add an MCP server, lifecycle hook, remote service or new financial authority merely to qualify as a plugin.
- Make clear that installing the plugin does not install Fin, enroll credentials, grant financial authority or bypass the protected broker. The plugin must detect or explain a missing CLI and direct setup and secret entry to the human's protected terminal.
- Preserve exact Fin discovery behavior: obtain current commands and schemas from `fin capabilities`, `fin explain` and `fin schema` rather than embedding a second provider or operation catalog in marketplace metadata.

## Versioning and release management

Define one release process that keeps the embedded companion-skill version, plugin versions, generated package contents and release notes aligned. Add deterministic packaging and validation commands, checksums and CI tests. A skill change must identify which distribution artifacts need a new release; marketplace-specific metadata may vary without allowing the core instructions to drift.

Document compatibility across ChatGPT/Codex surfaces and Claude Code CLI, desktop and editor surfaces actually verified. Keep unsupported cloud or remote execution modes explicit. Plugin installation, update, disable and removal must not modify Fin's protected runtime, credentials, configuration or journal.

## Distribution and trust

Prepare repository or local marketplace installation for testing, then prepare the required public-directory or marketplace submission artifacts. Include accurate publisher identity, source repository, license, privacy policy, terms, capabilities, icons and screenshots where required. External directory review and publication are separate outcomes and must not be reported complete until observed.

Users must explicitly install the plugin. Do not silently add marketplace sources, enable plugins or replace modified skill/plugin content. Document package provenance, permissions, local command execution and how users can inspect the shipped instructions before installation.

## Verification

- Validate both manifests and packages with their current official tooling or submission checks.
- Test clean installation, discovery, invocation, disable, update and removal on supported ChatGPT/Codex and Claude Code surfaces.
- In fresh conversations, verify that each plugin discovers the installed Fin CLI, keeps secrets out of chat and agent-controlled terminals, previews before writes, respects approvals or Yolo state, and reconciles uncertain outcomes.
- Test missing/outdated Fin, unsupported platform, modified local content, package-version mismatch and offline behavior with actionable guidance.
- Prove that generated packages contain no credentials, private reports, account mappings, signed requests or development secrets.
- Keep marketplace submission/publication evidence separate from local package tests and from Fin financial acceptance evidence.

Finish with reproducible package artifacts, installation instructions for each ecosystem, verified compatibility, release/update procedures and an explicit list of any directory review or publication steps still pending.

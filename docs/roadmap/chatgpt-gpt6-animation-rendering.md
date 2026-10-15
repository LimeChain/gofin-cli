# ChatGPT GPT-6 animation rendering guidance

Status: Requested on 2026-10-15 as item 41. No release is assigned. Implementation and verification have not been reported.

## Objective

Teach Fin's Codex companion skill to use ChatGPT GPT-6 animation rendering when it materially improves a user's understanding of a financial workflow, plan or changing state. This is expected to be primarily an instruction and skill-verification change rather than a new financial runtime capability.

The exact ChatGPT rendering interface and supported animation formats must be verified against the target GPT-6 environment during implementation. Do not invent syntax, claim support from model naming alone or make Fin commands depend on a ChatGPT-only presentation feature.

## Scope

Update `skills/fin/SKILL.md` with concise guidance for when and how an agent should render an animation. Good candidates include a sequential plan progressing through dependent steps, a cross-network transfer moving through source submission and destination settlement, or a position whose composition changes through a reviewed operation. Prefer ordinary prose, tables or static diagrams when motion does not add meaningful information.

Animations must be derived from public Fin previews, schemas and verified status results. Clearly distinguish proposed, pending, uncertain, failed and settled states. Never animate a submission, transaction hash or expected receipt as completed before Fin provides the required settlement evidence.

Do not include credentials, signed payloads, private account mappings, full wallet or account identifiers, raw request JSON, transaction IDs, exact private destinations, free-text errors or other sensitive values. Use concise user-facing labels and only the financial quantities and effects that are already appropriate to disclose in the conversation.

This feature concerns media rendered in the ChatGPT conversation. It must not change Fin CLI terminal spinners or progress rows, and it must not add animation, chatter or media fields to the stable `--json` contract. Financial authority remains entirely in the protected broker; a visualization cannot approve, dispatch, retry or reconcile an operation.

## Delivery and verification

- Verify the current GPT-6 animation-rendering mechanism and document any required capability checks or fallback behavior.
- Add bounded, reusable instructions and examples to the Codex companion skill without duplicating Fin's operation catalog; obtain current commands and semantics from `capabilities`, `explain` and `schema`.
- Fall back cleanly to concise text or a static representation when animation rendering is unavailable, unsupported or unnecessary.
- Update the embedded companion-skill version and relevant maintenance documentation together with the skill content.
- Test the updated skill in a fresh ChatGPT GPT-6 conversation using representative plan, bridge and uncertain-outcome scenarios. Confirm that state transitions and financial effects are accurate, sensitive data is absent, and no visual implies unsupported authority or premature completion.
- Preserve installation, inspection, update, conflict and modified-copy behavior for the managed companion skill.

If implementation reveals that a structured Fin output is required to render accurate state transitions, scope the smallest discoverable, public schema addition separately and keep it free of presentation-specific or ChatGPT-specific authority.

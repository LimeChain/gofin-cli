//! Recent-blockhash non-inclusion evidence. An absent signature index is never
//! sufficient: every produced finalized block in the signed message's entire
//! admissible lifetime must be covered, including failed transactions.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::*;

// Solana's pinned SDK defines MAX_PROCESSING_AGE as 150 (solana-clock 4.0.0).
// A saved provider bound shorter than the observed origin plus that age is
// insufficient evidence. This constant never extends signing authority.
const MAX_PROCESSING_AGE: u64 = 150;
const SEARCH_SLOT_WINDOW: u64 = 65_536;
const LOCATOR_SLOT_WINDOW: u64 = 1024;
const MAX_LOCATOR_PROBES: usize = 16;
const MAX_COVERED_BLOCKS: usize = 512;
const MAX_CACHED_BLOCKS: usize = 4096;
const BLOCK_READ_CONCURRENCY: usize = 4;
const MAX_BLOCK_SIGNATURES: usize = 30_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExpiredWithoutInclusionProof {
    pub evidence_version: u32,
    pub minimum_rpc_origins_per_block: u32,
    pub recent_blockhash: String,
    pub last_valid_block_height: u64,
    pub finalized_context_slot: u64,
    pub finalized_block_height: u64,
    pub first_slot: u64,
    pub last_slot: u64,
    pub first_block_height: u64,
    pub last_block_height: u64,
    pub checked_blocks: u64,
    pub block_coverage_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct BlockSummary {
    slot: u64,
    blockhash: String,
    previous_blockhash: String,
    parent_slot: u64,
    block_height: u64,
    signatures_count: usize,
    signatures_sha256: String,
    target_present: bool,
}

/// Disposable read progress, never authority or settlement. Only complete
/// finalized block responses are cached, scoped to the exact target signature.
/// Restarts lose partial progress and therefore cannot manufacture a proof.
#[derive(Default)]
pub(super) struct ExpiryCache {
    blocks: BTreeMap<(String, u64), BlockSummary>,
    locator_upper: BTreeMap<(String, u64), u64>,
    order: VecDeque<(String, u64)>,
    conflicted_targets: BTreeSet<String>,
    all_conflicted: bool,
    included_targets: BTreeSet<String>,
    all_inclusion_unknown: bool,
}

impl ExpiryCache {
    fn check(&self, signature: &str) -> Result<()> {
        if self.all_conflicted || self.conflicted_targets.contains(signature) {
            Err(coverage_conflict())
        } else {
            Ok(())
        }
    }

    fn poison(&mut self, signature: &str) {
        // Never evict a known conflict and accidentally revive an old cached
        // absence. Exhaustion conservatively disables this optional verifier.
        if self.conflicted_targets.len() >= MAX_CACHED_BLOCKS {
            self.all_conflicted = true;
        } else {
            self.conflicted_targets.insert(signature.to_owned());
        }
    }

    fn inclusion_observed(&self, signature: &str) -> bool {
        self.all_inclusion_unknown || self.included_targets.contains(signature)
    }

    fn remember_inclusion(&mut self, signature: &str) {
        if self.included_targets.len() >= MAX_CACHED_BLOCKS {
            self.all_inclusion_unknown = true;
        } else {
            self.included_targets.insert(signature.to_owned());
        }
    }
}

impl SolanaRuntime {
    pub(super) async fn prove_expired_without_inclusion(
        &self,
        data: &Value,
        signature: &str,
    ) -> Result<Option<ExpiredWithoutInclusionProof>> {
        {
            let cache = self
                .expiry_cache
                .lock()
                .map_err(|_| pending_evidence_error())?;
            cache.check(signature)?;
            if cache.inclusion_observed(signature) {
                return Ok(None);
            }
        }
        // Legacy journals and provider transactions with an unknown first
        // signature lack the immutable envelope needed for an absence proof.
        if data["schema_version"].as_u64() != Some(2) {
            return Ok(None);
        }
        let Some(last_valid_block_height) = data["last_valid_block_height"].as_u64() else {
            return Ok(None);
        };
        let tx = decode_supported_transaction(
            data["signed_transaction_base64"]
                .as_str()
                .ok_or_else(receipt_identity_error)?,
        )?;
        if tx.uses_durable_nonce() || !verify_signatures(&tx) {
            return Ok(None);
        }
        if tx.signatures.first().map(ToString::to_string).as_deref() != Some(signature) {
            return Err(receipt_identity_error());
        }
        let recent_blockhash = tx.message.recent_blockhash().to_string();
        let head_slot = self
            .rpc
            .request("getSlot", json!([{"commitment":"finalized"}]))
            .await?
            .as_u64()
            .ok_or_else(pending_evidence_error)?;
        let head = self
            .rpc
            .request_checked(
                "getBlock",
                block_params(head_slot, "none"),
                RpcReadRequirements {
                    require_non_null: true,
                    ..Default::default()
                },
            )
            .await?;
        let Some(head) = parse_block(head_slot, &head, None) else {
            return Ok(None);
        };
        if head.block_height <= last_valid_block_height {
            return Ok(None);
        }
        let finalized_height = self
            .rpc
            .request(
                "getBlockHeight",
                json!([{"commitment":"finalized","minContextSlot":head_slot}]),
            )
            .await?
            .as_u64()
            .ok_or_else(pending_evidence_error)?;
        if finalized_height < head.block_height {
            return Ok(None);
        }
        let validity = self
            .rpc
            .request_checked(
                "isBlockhashValid",
                json!([recent_blockhash,{"commitment":"finalized","minContextSlot":head_slot}]),
                RpcReadRequirements {
                    minimum_context_slot: Some(head_slot),
                    ..Default::default()
                },
            )
            .await?;
        if validity["value"].as_bool() != Some(false) {
            return Ok(None);
        }

        // Locate the first block beyond expiry by height rather than searching
        // only near today's head. The original submission can be arbitrarily
        // old. Header/slot-list observations are search hints, never evidence:
        // the complete corroborated chain below must independently confirm the
        // exact end height and signed blockhash origin.
        let target_height = last_valid_block_height
            .checked_add(1)
            .ok_or_else(pending_evidence_error)?;
        let end_slot = self
            .locate_expiry_block(signature, &head, target_height)
            .await?;
        let slots = self
            .expiry_block_slots(end_slot.saturating_sub(SEARCH_SLOT_WINDOW), end_slot)
            .await?;
        if slots.last() != Some(&end_slot) {
            return Err(coverage_pending(
                "The historical slot list omitted the located expiry block; complete lifetime coverage is still required.",
            ));
        }
        let selected = slots.iter().rev().take(MAX_COVERED_BLOCKS).copied();
        let mut blocks = stream::iter(selected)
            .map(|slot| self.finalized_absence_block(signature, slot))
            .buffered(BLOCK_READ_CONCURRENCY);
        let mut previous: Option<BlockSummary> = None;
        let mut last: Option<BlockSummary> = None;
        let mut digest = Sha256::new();
        // The digest binds the exact transaction and the ordered complete
        // block summaries; it never contains signed wire or raw signatures.
        digest.update(signature.as_bytes());
        digest.update([0]);
        digest.update(
            data["message_sha256"]
                .as_str()
                .ok_or_else(receipt_identity_error)?
                .as_bytes(),
        );
        let mut checked_blocks = 0;
        while let Some(result) = blocks.next().await {
            let Some(block) = result? else {
                return Err(coverage_pending(
                    "A historical finalized block lacks complete signature evidence; lifetime coverage remains incomplete.",
                ));
            };
            if block.target_present {
                // A block presence observation overrides missing indices. The
                // normal receipt verifier must obtain the exact transaction.
                return Ok(None);
            }
            if let Some(child) = &previous {
                if child.parent_slot != block.slot
                    || child.previous_blockhash != block.blockhash
                    || block.block_height.checked_add(1) != Some(child.block_height)
                {
                    return Err(coverage_pending(
                        "Historical finalized blocks do not form a complete linked lifetime interval; the original transaction remains unresolved.",
                    ));
                }
            } else if block.block_height != target_height {
                return Err(coverage_pending(
                    "Corroborated history did not confirm the located expiry block height; the original transaction remains unresolved.",
                ));
            } else {
                last = Some(block.clone());
            }
            digest.update(serde_json::to_vec(&block).map_err(|_| pending_evidence_error())?);
            digest.update([0]);
            checked_blocks += 1;
            if block.blockhash == recent_blockhash {
                if block
                    .block_height
                    .checked_add(MAX_PROCESSING_AGE)
                    .is_none_or(|height| height > last_valid_block_height)
                {
                    return Ok(None);
                }
                let last = last.ok_or_else(pending_evidence_error)?;
                // Another status request may have observed contradictory
                // finalized coverage while this one consumed cached blocks.
                {
                    let cache = self
                        .expiry_cache
                        .lock()
                        .map_err(|_| pending_evidence_error())?;
                    cache.check(signature)?;
                    if cache.inclusion_observed(signature) {
                        return Ok(None);
                    }
                }
                return Ok(Some(ExpiredWithoutInclusionProof {
                    evidence_version: 1,
                    minimum_rpc_origins_per_block: 2,
                    recent_blockhash,
                    last_valid_block_height,
                    finalized_context_slot: validity["context"]["slot"]
                        .as_u64()
                        .ok_or_else(pending_evidence_error)?,
                    finalized_block_height: finalized_height,
                    first_slot: block.slot,
                    last_slot: last.slot,
                    first_block_height: block.block_height,
                    last_block_height: last.block_height,
                    checked_blocks,
                    block_coverage_sha256: hex::encode(digest.finalize()),
                }));
            }
            previous = Some(block);
        }
        Err(coverage_pending(
            "The bounded historical block scan did not reach the original blockhash; complete lifetime coverage is still required.",
        ))
    }

    /// Only the corroborated block cache may skip locating the target again.
    /// A cached target is still checked against the exact height by the proof
    /// loop, and cannot replace missing signatures or broken parent links.
    async fn locate_expiry_block(
        &self,
        signature: &str,
        head: &BlockSummary,
        target_height: u64,
    ) -> Result<u64> {
        if head.block_height == target_height {
            return Ok(head.slot);
        }
        {
            let cache = self
                .expiry_cache
                .lock()
                .map_err(|_| pending_evidence_error())?;
            cache.check(signature)?;
            if let Some((_, block)) = cache.blocks.iter().find(|((target, _), block)| {
                target == signature && block.block_height == target_height
            }) {
                return Ok(block.slot);
            }
        }
        // There can be at most one produced block per slot, so subtracting
        // the height distance gives an upper bound on the target slot. Skipped
        // slots after the target make this bound loose, never too early.
        let mut upper = head
            .slot
            .checked_sub(head.block_height - target_height)
            .ok_or_else(|| {
                coverage_pending(
                    "Finalized head slot and height cannot bound the historical expiry block.",
                )
            })?;
        let locator_key = (signature.to_owned(), target_height);
        if let Some(saved) = self
            .expiry_cache
            .lock()
            .map_err(|_| pending_evidence_error())?
            .locator_upper
            .get(&locator_key)
        {
            upper = upper.min(*saved);
        }
        for _ in 0..MAX_LOCATOR_PROBES {
            let slots = self
                .expiry_block_slots(upper.saturating_sub(LOCATOR_SLOT_WINDOW), upper)
                .await?;
            let newest_slot = *slots.last().ok_or_else(|| coverage_pending(
                "No produced blocks were returned in the bounded historical locator window; expiry coverage remains unresolved.",
            ))?;
            let newest = self.expiry_block_header(newest_slot).await?;
            if newest.block_height < target_height {
                self.expiry_cache
                    .lock()
                    .map_err(|_| pending_evidence_error())?
                    .locator_upper
                    .remove(&locator_key);
                return Err(coverage_pending(
                    "Historical locator responses do not cover the required expiry height; missing history cannot prove absence.",
                ));
            }
            if newest.block_height == target_height {
                return Ok(newest.slot);
            }
            let distance = newest.block_height - target_height;
            // Counting entries is only a hint. An omitted produced block may
            // shift this candidate, so its height must always be read exactly.
            let candidate = if distance < slots.len() as u64 {
                self.expiry_block_header(slots[slots.len() - 1 - distance as usize])
                    .await?
            } else {
                newest
            };
            if candidate.block_height == target_height {
                return Ok(candidate.slot);
            }
            if candidate.block_height < target_height {
                self.expiry_cache
                    .lock()
                    .map_err(|_| pending_evidence_error())?
                    .locator_upper
                    .remove(&locator_key);
                return Err(coverage_pending(
                    "The historical slot list is incomplete around the required expiry height; full lifetime coverage is still required.",
                ));
            }
            let next = candidate
                .slot
                .checked_sub(candidate.block_height - target_height)
                .filter(|next| *next < upper)
                .ok_or_else(|| {
                    coverage_pending(
                        "Historical locator observations could not narrow the expiry block search.",
                    )
                })?;
            upper = next;
            let mut cache = self
                .expiry_cache
                .lock()
                .map_err(|_| pending_evidence_error())?;
            // Search progress may be discarded freely. Even a false header
            // hint can only leave recovery pending: terminal proof still
            // requires the exact independently corroborated linked interval.
            if cache.locator_upper.len() >= MAX_CACHED_BLOCKS {
                cache.locator_upper.pop_first();
            }
            cache
                .locator_upper
                .entry(locator_key.clone())
                .and_modify(|saved| *saved = (*saved).min(upper))
                .or_insert(upper);
        }
        Err(coverage_pending(
            "The bounded historical locator exhausted its probes before finding the exact expiry block; the original transaction remains unresolved.",
        ))
    }

    async fn expiry_block_header(&self, slot: u64) -> Result<BlockSummary> {
        let value = self
            .rpc
            .request_checked(
                "getBlock",
                block_params(slot, "none"),
                RpcReadRequirements {
                    require_non_null: true,
                    ..Default::default()
                },
            )
            .await?;
        parse_block(slot, &value, None).ok_or_else(|| coverage_pending(
            "A historical block header is missing or malformed; the exact expiry block cannot yet be located.",
        ))
    }

    async fn expiry_block_slots(&self, start_slot: u64, end_slot: u64) -> Result<Vec<u64>> {
        let value = self
            .rpc
            .request(
                "getBlocks",
                json!([start_slot,end_slot,{"commitment":"finalized"}]),
            )
            .await?;
        let slots = value.as_array()
            .filter(|slots| slots.len() as u64 <= end_slot - start_slot + 1)
            .and_then(|slots| slots.iter().map(Value::as_u64).collect::<Option<Vec<_>>>())
            .filter(|slots| slots.first().is_none_or(|slot| *slot >= start_slot)
                && slots.last().is_none_or(|slot| *slot <= end_slot)
                && slots.windows(2).all(|pair| pair[0] < pair[1]))
            .ok_or_else(|| coverage_pending("The historical slot response is malformed or outside the bounded search interval."))?;
        Ok(slots)
    }

    pub(super) fn expiry_progress(
        &self,
        signature: &str,
        last_valid_height: u64,
    ) -> Option<String> {
        let cache = self.expiry_cache.lock().ok()?;
        let checked = cache
            .blocks
            .keys()
            .filter(|(target, _)| target == signature)
            .count();
        if checked > 0 {
            return Some(format!(
                "{checked} finalized blocks checked and cached for this original transaction; complete lifetime coverage is not yet proven."
            ));
        }
        let upper = cache
            .locator_upper
            .get(&(signature.to_owned(), last_valid_height.checked_add(1)?))?;
        Some(format!(
            "The historical expiry locator has narrowed its next search to slot {upper} or earlier; no absence proof is established."
        ))
    }

    async fn finalized_absence_block(
        &self,
        signature: &str,
        slot: u64,
    ) -> Result<Option<BlockSummary>> {
        let key = (signature.to_owned(), slot);
        let cached = {
            let cache = self
                .expiry_cache
                .lock()
                .map_err(|_| pending_evidence_error())?;
            cache.check(signature)?;
            cache.blocks.get(&key).cloned()
        };
        if let Some(block) = cached {
            return Ok(Some(block));
        }
        let value = self
            .rpc
            .request_checked(
                "getBlock",
                block_params(slot, "signatures"),
                RpcReadRequirements {
                    require_non_null: true,
                    corroborate_finalized_block: true,
                    ..Default::default()
                },
            )
            .await;
        let value = match value {
            Err(error) if error.code == "SOLANA_RPC_CONFLICT" => {
                self.expiry_cache
                    .lock()
                    .map_err(|_| pending_evidence_error())?
                    .poison(signature);
                return Err(error);
            }
            result => result?,
        };
        let Some(block) = parse_block(slot, &value, Some(signature)) else {
            return Ok(None);
        };
        let mut cache = self
            .expiry_cache
            .lock()
            .map_err(|_| pending_evidence_error())?;
        if block.target_present {
            // Positive evidence cannot disappear when this bounded block
            // cache evicts entries. Only the ordinary receipt verifier may
            // settle a transaction whose inclusion was observed.
            cache.remember_inclusion(signature);
        }
        cache.check(signature)?;
        if let Some(existing) = cache.blocks.get(&key) {
            if existing != &block {
                cache.poison(signature);
                return Err(coverage_conflict());
            }
        } else {
            if cache.blocks.len() >= MAX_CACHED_BLOCKS
                && let Some(oldest) = cache.order.pop_front()
            {
                cache.blocks.remove(&oldest);
            }
            cache.order.push_back(key.clone());
            cache.blocks.insert(key, block.clone());
        }
        Ok(Some(block))
    }
}

fn coverage_pending(message: &'static str) -> GoFinError {
    failure(
        "SOLANA_EXPIRY_COVERAGE_PENDING",
        "reconciliation",
        message,
        "Keep the original submission unresolved and reconcile its existing identity later.",
    )
}

fn coverage_conflict() -> GoFinError {
    failure(
        "SOLANA_RPC_CONFLICT",
        "reconciliation",
        "Conflicting finalized block coverage was observed for this original transaction.",
        "Keep the original operation unresolved and investigate its finalized evidence.",
    )
}

fn block_params(slot: u64, details: &str) -> Value {
    json!([slot,{"commitment":"finalized","transactionDetails":details,"rewards":false,"maxSupportedTransactionVersion":0}])
}

fn parse_block(slot: u64, value: &Value, signature: Option<&str>) -> Option<BlockSummary> {
    let blockhash = value["blockhash"]
        .as_str()?
        .parse::<Hash>()
        .ok()?
        .to_string();
    let previous_blockhash = value["previousBlockhash"]
        .as_str()?
        .parse::<Hash>()
        .ok()?
        .to_string();
    let parent_slot = value["parentSlot"].as_u64()?;
    if parent_slot >= slot {
        return None;
    }
    let mut block = BlockSummary {
        slot,
        blockhash,
        previous_blockhash,
        parent_slot,
        block_height: value["blockHeight"].as_u64()?,
        signatures_count: 0,
        signatures_sha256: String::new(),
        target_present: false,
    };
    if let Some(target) = signature {
        let signatures = value["signatures"]
            .as_array()
            .filter(|items| items.len() <= MAX_BLOCK_SIGNATURES)?;
        let mut digest = Sha256::new();
        for signature in signatures {
            let encoded = signature.as_str()?;
            let signature = encoded.parse::<Signature>().ok()?;
            if signature == Signature::default() {
                return None;
            }
            block.target_present |= encoded == target;
            digest.update(signature.as_ref());
        }
        block.signatures_count = signatures.len();
        block.signatures_sha256 = hex::encode(digest.finalize());
    }
    Some(block)
}

#[cfg(test)]
#[path = "expiry_tests.rs"]
mod tests;

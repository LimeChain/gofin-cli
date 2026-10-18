//! Finite selected/journaled vault coverage, independently observed on chain.
use crate::*;

impl EthereumAdapter {
    pub(crate) fn morpho_snapshot_vaults(&self) -> Result<BTreeSet<String>> {
        let mut vaults = self
            .morpho_observed_vaults
            .lock()
            .map_err(|_| GoFinError::validation("Morpho observation registry is unavailable."))?
            .clone();
        let journal = self
            .execution_state
            .lock()
            .map_err(|_| GoFinError::validation("Execution state is unavailable."))?
            .clone();
        if let Some(journal) = journal {
            for wallet in &self.wallets {
                let resource = resource(self.config.chain_id, address(&wallet.address)?);
                for operation in ["morpho.deposit", "morpho.withdraw", "morpho.redeem"] {
                    for record in journal.operations_by_prepared_binding(
                        operation,
                        "/resource",
                        &json!(resource),
                    )? {
                        if let Some(prepared) = record.preview {
                            // Journal discovery is scope, never proof of current ownership,
                            // value, deployment compatibility, or transaction settlement.
                            if prepared.operation == operation
                                && prepared.binding["transaction"]["chain_id"]
                                    == self.config.chain_id
                                && prepared.binding["resource"] == json!(resource)
                            {
                                let vault =
                                    address(prepared.binding["vault"].as_str().unwrap_or(""))?;
                                vaults.insert(lower_address(vault));
                            }
                        }
                    }
                }
            }
        }
        Ok(vaults)
    }

    pub(crate) async fn morpho_snapshot_position(
        &self,
        vault: &str,
        owner: Address,
        block: u64,
    ) -> Result<PositionState> {
        let state = self
            .morpho_vault_state(vault, Some(owner), Some(block))
            .await?;
        let anchor = super::vault::investment_anchor(&state, owner);
        Ok(PositionState {
            resource: anchor.resource.clone(),
            position: anchor.position.clone(),
            kind: PositionKind::Investment,
            amount: anchor.principal.clone(),
            scaled_receipt: None,
            anchor: Some(anchor),
        })
    }
}

/// A receipt is represented by one verified investment position, rather than
/// both a fungible share holding and its underlying backing.
pub fn represented_share(position: &PositionState, asset: &AssetId) -> bool {
    position.kind == PositionKind::Investment
        && position.anchor.as_ref().is_some_and(|anchor| {
            anchor.evidence_version == 1
                && anchor.evidence["protocol"] == "morpho-vault-v2"
                && anchor.evidence["share_asset"] == json!(asset)
                && anchor.resource == position.resource
                && anchor.position == position.position
                && anchor.principal == position.amount
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_coverage_requires_exact_versioned_backing_identity() {
        let share = AssetId {
            network: "ethereum:8453".into(),
            reference: "0x1111111111111111111111111111111111111111".into(),
            decimals: 18,
        };
        let underlying = AssetId {
            network: "ethereum:8453".into(),
            reference: "0x2222222222222222222222222222222222222222".into(),
            decimals: 6,
        };
        let anchor = PositionAnchor {
            resource: ResourceId("wallet".into()),
            position: "morpho:v2:vault:investment".into(),
            position_kind: PositionKind::Investment,
            principal: Amount {
                asset: underlying,
                atoms: Quantity::new("123").unwrap(),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"protocol":"morpho-vault-v2", "share_asset":share}),
            observed_at: now(),
        };
        let mut position = PositionState {
            resource: anchor.resource.clone(),
            position: anchor.position.clone(),
            kind: PositionKind::Investment,
            amount: anchor.principal.clone(),
            scaled_receipt: None,
            anchor: Some(anchor),
        };
        assert!(represented_share(&position, &share));
        let mut wrong = share.clone();
        wrong.network = "ethereum:1".into();
        assert!(!represented_share(&position, &wrong));
        wrong = share.clone();
        wrong.decimals = 6;
        assert!(!represented_share(&position, &wrong));
        position.amount.atoms = Quantity::zero();
        assert!(!represented_share(&position, &share));
    }
}

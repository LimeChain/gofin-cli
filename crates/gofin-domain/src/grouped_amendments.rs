//! Disjoint native member revisions retain one original financial owner.
#[path = "grouped_accounting.rs"]
mod accounting;
use crate::{
    GoFinError, Result, Submission, amendments::AmendmentScope,
    compound_mutations::MutationChildProof,
};
pub use accounting::derive_accounting_scope;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindingReplacement {
    pub pointer: String,
    pub before: Value,
    pub after: Value,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroupedChildMutation {
    #[default]
    Amend,
    Cancel,
}
impl GroupedChildMutation {
    pub fn is_amend(&self) -> bool {
        matches!(self, Self::Amend)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GroupedAmendmentChild {
    pub child_tag: String,
    #[serde(default, skip_serializing_if = "GroupedChildMutation::is_amend")]
    pub mutation: GroupedChildMutation,
    pub scope: Box<AmendmentScope>,
    pub binding_replacements: Vec<BindingReplacement>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GroupedAmendmentGroup {
    pub operation_id: String,
    pub submission: Submission,
    pub original_scope: Box<AmendmentScope>,
    pub shared_binding_replacements: Vec<BindingReplacement>,
    pub children: Vec<GroupedAmendmentChild>,
    pub approved_envelope: Box<AmendmentScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incremental_valuation: Option<crate::Valuation>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GroupedAmendmentProof {
    pub operation_id: String,
    pub submission: Submission,
    pub children: Vec<MutationChildProof>,
}
fn invalid() -> GoFinError {
    GoFinError::validation(
        "Grouped revisions require disjoint existing binding replacements with exact protected preimages.",
    )
}
fn pointer_segments(pointer: &str) -> Result<Vec<String>> {
    if !pointer.starts_with('/') || pointer.len() > 2048 {
        return Err(invalid());
    }
    let rows = pointer[1..]
        .split('/')
        .map(|part| {
            let decoded = part.replace("~1", "/").replace("~0", "~");
            if decoded.replace('~', "~0").replace('/', "~1") != part {
                return Err(invalid());
            }
            Ok(decoded)
        })
        .collect::<Result<Vec<_>>>()?;
    if rows.len() > 16 {
        return Err(invalid());
    }
    Ok(rows)
}
/// Validate all paths before applying any member. No arrays or objects are
/// created; a replacement cannot change a sibling's or ancestor's preimage.
pub fn validate_replacements(group: &GroupedAmendmentGroup) -> Result<()> {
    if group.children.len() < 2 || group.children.len() > 50 {
        return Err(invalid());
    }
    let mut tags = BTreeSet::new();
    let mut paths = Vec::<Vec<String>>::new();
    for child in &group.children {
        if child.child_tag.is_empty()
            || child.child_tag.len() > 128
            || !tags.insert(&child.child_tag)
            || (child.mutation.is_amend() && child.binding_replacements.is_empty())
            || (child.mutation == GroupedChildMutation::Cancel
                && !child.binding_replacements.is_empty())
        {
            return Err(invalid());
        }
    }
    for patch in group
        .shared_binding_replacements
        .iter()
        .chain(group.children.iter().flat_map(|c| &c.binding_replacements))
    {
        let path = pointer_segments(&patch.pointer)?;
        let mut cursor = &group.original_scope.binding;
        for segment in &path {
            cursor = match cursor {
                Value::Object(object) => object.get(segment).ok_or_else(invalid)?,
                Value::Array(array) => {
                    if segment.is_empty()
                        || !segment.bytes().all(|c| c.is_ascii_digit())
                        || (segment.len() > 1 && segment.starts_with('0'))
                    {
                        return Err(invalid());
                    }
                    let index = segment.parse::<usize>().map_err(|_| invalid())?;
                    array.get(index).ok_or_else(invalid)?
                }
                _ => return Err(invalid()),
            };
        }
        if paths
            .iter()
            .any(|old| old.starts_with(&path) || path.starts_with(old))
            || group.original_scope.binding.pointer(&patch.pointer) != Some(&patch.before)
            || patch.before == patch.after
        {
            return Err(invalid());
        }
        paths.push(path);
    }
    for child in &group.children {
        let binding = compose_binding(group, [child.child_tag.as_str()])?;
        if binding != child.scope.binding {
            return Err(invalid());
        }
    }
    if compose_binding(group, group.children.iter().map(|c| c.child_tag.as_str()))?
        != group.approved_envelope.binding
    {
        return Err(invalid());
    }
    Ok(())
}
/// Compose only independently accepted immutable tags. Unknown or rejected
/// members never enter the original owner's recovery binding.
pub fn compose_binding<'a>(
    group: &GroupedAmendmentGroup,
    accepted: impl IntoIterator<Item = &'a str>,
) -> Result<Value> {
    let tags = accepted.into_iter().collect::<BTreeSet<_>>();
    if tags
        .iter()
        .any(|tag| !group.children.iter().any(|c| c.child_tag == *tag))
    {
        return Err(invalid());
    }
    let mut binding = group.original_scope.binding.clone();
    if tags.is_empty() {
        return Ok(binding);
    }
    let activate_shared = group
        .children
        .iter()
        .any(|c| tags.contains(c.child_tag.as_str()) && c.mutation.is_amend());
    for patch in group
        .shared_binding_replacements
        .iter()
        .filter(|_| activate_shared)
        .chain(
            group
                .children
                .iter()
                .filter(|c| tags.contains(c.child_tag.as_str()))
                .flat_map(|c| &c.binding_replacements),
        )
    {
        let value = binding.pointer_mut(&patch.pointer).ok_or_else(invalid)?;
        if *value != patch.before {
            return Err(invalid());
        }
        *value = patch.after.clone();
    }
    Ok(binding)
}
/// Bind standing-edit evidence to the exact approved replacement after-images.
/// A transport acknowledgement alone cannot stand in for native audit/readback.
pub fn accepted_readback_matches(child: &GroupedAmendmentChild, evidence: &Value) -> bool {
    let digest = crate::binding_hash(
        &serde_json::to_value(&child.binding_replacements).unwrap_or(Value::Null),
    );
    digest.ok().is_some_and(|digest| {
        evidence["bindingReplacementDigest"].as_str() == Some(digest.as_str())
    }) && evidence["nativeAudit"]
        .as_object()
        .is_some_and(|rows| !rows.is_empty())
        && evidence["nativeReadback"]
            .as_object()
            .is_some_and(|rows| !rows.is_empty())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn group() -> GroupedAmendmentGroup {
        let scope = |binding| {
            Box::new(AmendmentScope {
                intent: Default::default(),
                valuation: None,
                binding,
            })
        };
        GroupedAmendmentGroup {
            operation_id: "synthetic-owner".into(),
            submission: Submission {
                kind: "synthetic".into(),
                reference: "synthetic-owner".into(),
                data: json!({}),
            },
            original_scope: scope(json!({"orders":[{"size":"1"},{"size":"2"}],"fee":"1"})),
            shared_binding_replacements: vec![BindingReplacement {
                pointer: "/fee".into(),
                before: json!("1"),
                after: json!("3"),
            }],
            children: vec![
                GroupedAmendmentChild {
                    child_tag: "a".into(),
                    mutation: GroupedChildMutation::Amend,
                    scope: scope(json!({"orders":[{"size":"4"},{"size":"2"}],"fee":"3"})),
                    binding_replacements: vec![BindingReplacement {
                        pointer: "/orders/0/size".into(),
                        before: json!("1"),
                        after: json!("4"),
                    }],
                },
                GroupedAmendmentChild {
                    child_tag: "b".into(),
                    mutation: GroupedChildMutation::Amend,
                    scope: scope(json!({"orders":[{"size":"1"},{"size":"5"}],"fee":"3"})),
                    binding_replacements: vec![BindingReplacement {
                        pointer: "/orders/1/size".into(),
                        before: json!("2"),
                        after: json!("5"),
                    }],
                },
            ],
            approved_envelope: scope(json!({"orders":[{"size":"4"},{"size":"5"}],"fee":"3"})),
            incremental_valuation: None,
        }
    }
    #[test]
    fn accepted_subset_does_not_adopt_unknown_member() {
        let g = group();
        validate_replacements(&g).unwrap();
        assert_eq!(
            compose_binding(&g, ["a"]).unwrap(),
            g.children[0].scope.binding
        );
        assert_eq!(compose_binding(&g, []).unwrap(), g.original_scope.binding);
    }
    #[test]
    fn overlapping_wrong_preimage_or_unknown_tags_fail() {
        let mut g = group();
        g.children[1].binding_replacements[0].pointer = "/orders/0".into();
        assert!(validate_replacements(&g).is_err());
        let mut g = group();
        g.children[0].binding_replacements[0].before = json!("wrong");
        assert!(validate_replacements(&g).is_err());
        assert!(compose_binding(&group(), ["unknown"]).is_err());
        let mut g = group();
        g.children[1].binding_replacements[0].pointer = "/orders/01/size".into();
        assert!(validate_replacements(&g).is_err());
    }
}

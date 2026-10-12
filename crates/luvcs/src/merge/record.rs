//! `merge.record.v1`: deterministic three-way merge of `record.v1` values
//! under a [`RecordSchema`].
//!
//! The driver walks `(base, left, right)` by field path. At every path the
//! same three checks come first: equal on both sides → keep; one side equal
//! to base → take the other; one side absent while the other edited →
//! [`TermKind::DeletedVsEdited`]. Only when both sides changed differently
//! does the schema policy of the path decide how to go deeper:
//!
//! - `Scalar`: objects recurse per key, anything else is a `Value` term.
//! - `Set` / `List`: elements merge per stable ID (policy looked up with
//!   `*` for the ID segment); `List` additionally merges the ID order and
//!   raises one `Order` term at the collection path when both sides
//!   reordered it differently.
//! - `Text`: bytes go through the text driver; every conflicting region is
//!   a `TextRegion` term at `path/#<index>`.
//! - `Opaque`: whole-value comparison, never recursed.
//!
//! The provisional record of a conflicted merge carries every field that
//! did merge; conflicting paths carry the base value, or are absent when
//! there is no base. The driver never invents element IDs and never
//! reorders a `Set`: output order is base order, then left-only adds, then
//! right-only adds.

use std::collections::{BTreeMap, BTreeSet};

use super::text::{TextMergeOutcome, merge_text};
use crate::model::TermKind;
use crate::record::{Element, FieldPath, FieldPolicy, RecordSchema, RecordValue};

/// One unresolved question of a record merge. `None` on a side means the
/// value is absent there (deleted key or element). For a `TextRegion` the
/// three values are the region bytes as `RecordValue::Text`; for an `Order`
/// term they are the merged collection in each side's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordConflictTerm {
    pub path: FieldPath,
    pub kind: TermKind,
    pub base: Option<RecordValue>,
    pub left: Option<RecordValue>,
    pub right: Option<RecordValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordMergeOutcome {
    Merged(RecordValue),
    /// `merged` is the provisional record: every non-conflicting field
    /// merged, conflicting paths carry the base value (or are absent when
    /// there is no base).
    Conflicted {
        merged: RecordValue,
        terms: Vec<RecordConflictTerm>,
    },
}

impl RecordMergeOutcome {
    pub fn merged(&self) -> &RecordValue {
        match self {
            RecordMergeOutcome::Merged(v) | RecordMergeOutcome::Conflicted { merged: v, .. } => v,
        }
    }

    pub fn terms(&self) -> &[RecordConflictTerm] {
        match self {
            RecordMergeOutcome::Merged(_) => &[],
            RecordMergeOutcome::Conflicted { terms, .. } => terms,
        }
    }
}

/// Which candidate of a term the resolver picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermChoice {
    Left,
    Right,
    Base,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("expected {expected} choices, got {got}")]
    ChoiceCountMismatch { expected: usize, got: usize },
    /// The chosen candidate does not exist for that term (e.g. `Base` on a
    /// term without a base value).
    #[error("choice for term {index} is not available")]
    UnsupportedChoice { index: usize },
}

/// Three-way merge of two records against their common base.
pub fn merge_record(
    schema: &RecordSchema,
    base: Option<&RecordValue>,
    left: &RecordValue,
    right: &RecordValue,
) -> RecordMergeOutcome {
    let mut terms = Vec::new();
    let merged = merge_value(
        schema,
        &FieldPath::root(),
        base,
        Some(left),
        Some(right),
        &mut terms,
    )
    .unwrap_or_else(|| RecordValue::Object(BTreeMap::new()));
    if terms.is_empty() {
        RecordMergeOutcome::Merged(merged)
    } else {
        RecordMergeOutcome::Conflicted { merged, terms }
    }
}

/// Applies one choice per term to the provisional record. `Left` / `Right`
/// on an absent candidate removes the path; `Base` on a term without a
/// base value is [`ResolveError::UnsupportedChoice`].
///
/// `TextRegion` terms carry only the region bytes, and the provisional
/// record carries the field's base text, so a region cannot be placed from
/// here: such a term is [`ResolveError::UnsupportedChoice`]. Text regions
/// are resolved by re-running [`merge_text`] on the full sides and applying
/// [`super::text::resolve`] to the driver's own outcome.
pub fn resolve(
    merged: &RecordValue,
    terms: &[RecordConflictTerm],
    choices: &[TermChoice],
) -> Result<RecordValue, ResolveError> {
    if terms.len() != choices.len() {
        return Err(ResolveError::ChoiceCountMismatch {
            expected: terms.len(),
            got: choices.len(),
        });
    }
    let mut out = merged.clone();
    for (index, (term, choice)) in terms.iter().zip(choices).enumerate() {
        if term.kind == TermKind::TextRegion {
            return Err(ResolveError::UnsupportedChoice { index });
        }
        let chosen = match choice {
            TermChoice::Left => term.left.clone(),
            TermChoice::Right => term.right.clone(),
            TermChoice::Base => match &term.base {
                Some(b) => Some(b.clone()),
                None => return Err(ResolveError::UnsupportedChoice { index }),
            },
        };
        set_at(&mut out, &term.path.0, chosen);
    }
    Ok(out)
}

fn merge_value(
    schema: &RecordSchema,
    path: &FieldPath,
    base: Option<&RecordValue>,
    left: Option<&RecordValue>,
    right: Option<&RecordValue>,
    terms: &mut Vec<RecordConflictTerm>,
) -> Option<RecordValue> {
    if left == right {
        return left.cloned();
    }
    match base {
        None => match (left, right) {
            (Some(_), None) => return left.cloned(),
            (None, Some(_)) => return right.cloned(),
            _ => {}
        },
        Some(b) => {
            if left == Some(b) {
                return right.cloned();
            }
            if right == Some(b) {
                return left.cloned();
            }
            if left.is_none() || right.is_none() {
                terms.push(term(path, TermKind::DeletedVsEdited, base, left, right));
                return base.cloned();
            }
        }
    }
    let (Some(l), Some(r)) = (left, right) else {
        // Only (Some, Some) survives the checks above.
        return base.cloned();
    };
    match (schema.policy_of(path), l, r) {
        (FieldPolicy::Scalar, RecordValue::Object(lm), RecordValue::Object(rm)) => {
            Some(merge_object(
                schema,
                path,
                base.and_then(RecordValue::as_object),
                lm,
                rm,
                terms,
            ))
        }
        (FieldPolicy::Set, RecordValue::Collection(le), RecordValue::Collection(re)) => {
            Some(merge_collection(
                schema,
                path,
                base.and_then(RecordValue::as_collection),
                le,
                re,
                false,
                terms,
            ))
        }
        (FieldPolicy::List, RecordValue::Collection(le), RecordValue::Collection(re)) => {
            Some(merge_collection(
                schema,
                path,
                base.and_then(RecordValue::as_collection),
                le,
                re,
                true,
                terms,
            ))
        }
        (FieldPolicy::Text, RecordValue::Text(lt), RecordValue::Text(rt)) => {
            merge_text_field(path, base.and_then(RecordValue::as_text), lt, rt, terms)
        }
        // Opaque, plain scalars, and a type change on one side against an
        // edit on the other.
        _ => {
            terms.push(term(path, TermKind::Value, base, left, right));
            base.cloned()
        }
    }
}

fn merge_object(
    schema: &RecordSchema,
    path: &FieldPath,
    base: Option<&BTreeMap<String, RecordValue>>,
    left: &BTreeMap<String, RecordValue>,
    right: &BTreeMap<String, RecordValue>,
    terms: &mut Vec<RecordConflictTerm>,
) -> RecordValue {
    let keys: BTreeSet<&String> = base
        .into_iter()
        .flat_map(BTreeMap::keys)
        .chain(left.keys())
        .chain(right.keys())
        .collect();
    let mut out = BTreeMap::new();
    for key in keys {
        let child = path.child(key.as_str());
        if let Some(v) = merge_value(
            schema,
            &child,
            base.and_then(|m| m.get(key)),
            left.get(key),
            right.get(key),
            terms,
        ) {
            out.insert(key.clone(), v);
        }
    }
    RecordValue::Object(out)
}

fn merge_collection(
    schema: &RecordSchema,
    path: &FieldPath,
    base: Option<&[Element]>,
    left: &[Element],
    right: &[Element],
    ordered: bool,
    terms: &mut Vec<RecordConflictTerm>,
) -> RecordValue {
    let base = base.unwrap_or(&[]);
    let (bm, lm, rm) = (by_id(base), by_id(left), by_id(right));
    let (base_ids, left_ids, right_ids) = (ids(base), ids(left), ids(right));

    // Base order first, then left-only adds, then right-only adds.
    let mut union: Vec<&str> = base_ids.clone();
    union.extend(left_ids.iter().filter(|id| !bm.contains_key(*id)));
    union.extend(
        right_ids
            .iter()
            .filter(|id| !bm.contains_key(*id) && !lm.contains_key(*id)),
    );

    let mut element_terms = Vec::new();
    let mut merged: Vec<Element> = Vec::new();
    for id in union {
        if let Some(value) = merge_value(
            schema,
            &path.child(id),
            bm.get(id).copied(),
            lm.get(id).copied(),
            rm.get(id).copied(),
            &mut element_terms,
        ) {
            merged.push(Element {
                id: id.to_owned(),
                value,
            });
        }
    }

    if ordered {
        let left_changed = order_changed(&base_ids, &left_ids);
        let right_changed = order_changed(&base_ids, &right_ids);
        let both_differ = left_changed && right_changed && {
            let in_left: BTreeSet<&str> = left_ids.iter().copied().collect();
            let in_right: BTreeSet<&str> = right_ids.iter().copied().collect();
            let l: Vec<&str> = left_ids
                .iter()
                .copied()
                .filter(|id| in_right.contains(id))
                .collect();
            let r: Vec<&str> = right_ids
                .iter()
                .copied()
                .filter(|id| in_left.contains(id))
                .collect();
            l != r
        };
        if both_differ {
            terms.push(RecordConflictTerm {
                path: path.clone(),
                kind: TermKind::Order,
                base: Some(RecordValue::Collection(reorder(&merged, &base_ids))),
                left: Some(RecordValue::Collection(reorder(&merged, &left_ids))),
                right: Some(RecordValue::Collection(reorder(&merged, &right_ids))),
            });
            // Provisional order: base order with adds appended (already the
            // union order).
        } else if right_changed && !left_changed {
            merged = reorder(&merged, &right_ids);
        } else {
            merged = reorder(&merged, &left_ids);
        }
    }
    terms.extend(element_terms);
    RecordValue::Collection(merged)
}

fn by_id(elements: &[Element]) -> BTreeMap<&str, &RecordValue> {
    elements.iter().map(|e| (e.id.as_str(), &e.value)).collect()
}

fn ids(elements: &[Element]) -> Vec<&str> {
    elements.iter().map(|e| e.id.as_str()).collect()
}

/// Whether `side` orders the IDs it shares with `base` differently from
/// `base` (adds and removes are ignored).
fn order_changed(base: &[&str], side: &[&str]) -> bool {
    let in_base: BTreeSet<&str> = base.iter().copied().collect();
    let in_side: BTreeSet<&str> = side.iter().copied().collect();
    let base_common = base.iter().filter(|id| in_side.contains(*id));
    let side_common = side.iter().filter(|id| in_base.contains(*id));
    !base_common.eq(side_common)
}

/// `elements` in the order of `sequence`; elements not in `sequence` keep
/// their relative order and follow at the end.
fn reorder(elements: &[Element], sequence: &[&str]) -> Vec<Element> {
    let mut out: Vec<Element> = sequence
        .iter()
        .filter_map(|id| elements.iter().find(|e| e.id == *id).cloned())
        .collect();
    let placed: BTreeSet<&str> = sequence.iter().copied().collect();
    out.extend(
        elements
            .iter()
            .filter(|e| !placed.contains(e.id.as_str()))
            .cloned(),
    );
    out
}

fn merge_text_field(
    path: &FieldPath,
    base: Option<&str>,
    left: &str,
    right: &str,
    terms: &mut Vec<RecordConflictTerm>,
) -> Option<RecordValue> {
    match merge_text(base.map(str::as_bytes), left.as_bytes(), right.as_bytes()) {
        TextMergeOutcome::Merged(bytes) => Some(RecordValue::Text(text_of(bytes))),
        TextMergeOutcome::Conflicted { regions, .. } => {
            for region in regions {
                terms.push(RecordConflictTerm {
                    path: path.child(format!("#{}", region.index)),
                    kind: TermKind::TextRegion,
                    base: base.map(|_| RecordValue::Text(text_of(region.base))),
                    left: Some(RecordValue::Text(text_of(region.left))),
                    right: Some(RecordValue::Text(text_of(region.right))),
                });
            }
            // Provisional: the base text (or absent), never half-merged text.
            base.map(|b| RecordValue::Text(b.to_owned()))
        }
    }
}

fn text_of(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn term(
    path: &FieldPath,
    kind: TermKind,
    base: Option<&RecordValue>,
    left: Option<&RecordValue>,
    right: Option<&RecordValue>,
) -> RecordConflictTerm {
    RecordConflictTerm {
        path: path.clone(),
        kind,
        base: base.cloned(),
        left: left.cloned(),
        right: right.cloned(),
    }
}

/// Sets (or removes, with `None`) the value at `path` inside `root`; see
/// [`set_at`]. For resolvers that answer a term with a value of their own.
pub fn set_path(root: &mut RecordValue, path: &FieldPath, value: Option<RecordValue>) {
    set_at(root, &path.0, value);
}

/// Sets (or removes, with `None`) the value at `path` inside `root`,
/// following object keys and element IDs. A path whose parent does not
/// exist is left alone: the terms of `merge_record` always point into the
/// provisional record.
fn set_at(root: &mut RecordValue, path: &[String], value: Option<RecordValue>) {
    let Some((last, parents)) = path.split_last() else {
        if let Some(v) = value {
            *root = v;
        }
        return;
    };
    let mut cur = root;
    for seg in parents {
        cur = match cur {
            RecordValue::Object(m) => match m.get_mut(seg) {
                Some(v) => v,
                None => return,
            },
            RecordValue::Collection(elements) => match elements.iter_mut().find(|e| &e.id == seg) {
                Some(e) => &mut e.value,
                None => return,
            },
            _ => return,
        };
    }
    match cur {
        RecordValue::Object(m) => match value {
            Some(v) => {
                m.insert(last.clone(), v);
            }
            None => {
                m.remove(last);
            }
        },
        RecordValue::Collection(elements) => {
            let pos = elements.iter().position(|e| &e.id == last);
            match (pos, value) {
                (Some(i), Some(v)) => elements[i].value = v,
                (Some(i), None) => {
                    elements.remove(i);
                }
                (None, Some(v)) => elements.push(Element {
                    id: last.clone(),
                    value: v,
                }),
                (None, None) => {}
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj<const N: usize>(entries: [(&str, RecordValue); N]) -> RecordValue {
        RecordValue::object(entries)
    }

    fn coll<const N: usize>(elements: [(&str, RecordValue); N]) -> RecordValue {
        RecordValue::collection(elements)
    }

    fn text(s: &str) -> RecordValue {
        RecordValue::text(s)
    }

    fn int(i: i64) -> RecordValue {
        RecordValue::Int(i)
    }

    fn schema() -> RecordSchema {
        RecordSchema::new("note.v1")
            .field("body", FieldPolicy::Text)
            .field("tags", FieldPolicy::Set)
            .field("links", FieldPolicy::List)
            .field("meta", FieldPolicy::Opaque)
            .field("tags/*/style", FieldPolicy::Opaque)
    }

    fn merged(outcome: RecordMergeOutcome) -> RecordValue {
        match outcome {
            RecordMergeOutcome::Merged(v) => v,
            RecordMergeOutcome::Conflicted { terms, .. } => {
                panic!("unexpected conflict: {terms:?}")
            }
        }
    }

    fn conflicted(outcome: RecordMergeOutcome) -> (RecordValue, Vec<RecordConflictTerm>) {
        match outcome {
            RecordMergeOutcome::Merged(v) => panic!("unexpected clean merge: {v:?}"),
            RecordMergeOutcome::Conflicted { merged, terms } => (merged, terms),
        }
    }

    fn path(s: &str) -> FieldPath {
        FieldPath::parse(s)
    }

    #[test]
    fn one_side_scalar_change_is_taken() {
        let base = obj([("title", text("A")), ("n", int(1))]);
        let left = obj([("title", text("B")), ("n", int(1))]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &base)),
            left
        );
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &base, &left)),
            left
        );
    }

    #[test]
    fn identical_change_on_both_sides_is_kept() {
        let base = obj([("title", text("A"))]);
        let both = obj([("title", text("B"))]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &both, &both)),
            both
        );
    }

    #[test]
    fn disjoint_field_changes_merge() {
        let base = obj([("title", text("A")), ("n", int(1)), ("gone", int(0))]);
        let left = obj([("title", text("B")), ("n", int(1)), ("gone", int(0))]);
        let right = obj([
            ("title", text("A")),
            ("n", int(2)),
            ("added", RecordValue::Bool(true)),
        ]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &right)),
            obj([
                ("title", text("B")),
                ("n", int(2)),
                ("added", RecordValue::Bool(true))
            ])
        );
    }

    #[test]
    fn conflicting_scalar_yields_value_term_with_base_value_kept() {
        let base = obj([("title", text("A")), ("n", int(1))]);
        let left = obj([("title", text("B")), ("n", int(2))]);
        let right = obj([("title", text("C")), ("n", int(2))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(provisional, obj([("title", text("A")), ("n", int(2))]));
        assert_eq!(
            terms,
            vec![RecordConflictTerm {
                path: path("title"),
                kind: TermKind::Value,
                base: Some(text("A")),
                left: Some(text("B")),
                right: Some(text("C")),
            }]
        );
    }

    #[test]
    fn missing_and_null_are_different_values() {
        let base = obj([("x", int(1))]);
        let left = obj([("x", RecordValue::Null)]);
        let right = obj([]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(provisional, base);
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].kind, TermKind::DeletedVsEdited);
        assert_eq!(terms[0].left, Some(RecordValue::Null));
        assert_eq!(terms[0].right, None);

        // Both removing is no conflict; one removing while the other keeps
        // null is a plain one-sided change.
        let null_base = obj([("x", RecordValue::Null)]);
        assert_eq!(
            merged(merge_record(
                &schema(),
                Some(&null_base),
                &right,
                &null_base
            )),
            right
        );
    }

    #[test]
    fn nested_objects_recurse_per_key() {
        let base = obj([("pos", obj([("x", int(0)), ("y", int(0)), ("z", int(0))]))]);
        let left = obj([("pos", obj([("x", int(1)), ("y", int(0)), ("z", int(5))]))]);
        let right = obj([("pos", obj([("x", int(0)), ("y", int(2)), ("z", int(6))]))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(
            provisional,
            obj([("pos", obj([("x", int(1)), ("y", int(2)), ("z", int(0))]))])
        );
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path, path("pos/z"));
        assert_eq!(terms[0].kind, TermKind::Value);

        // Key removed on one side, modified on the other.
        let right = obj([("pos", obj([("y", int(0)), ("z", int(0))]))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(
            provisional,
            obj([("pos", obj([("x", int(0)), ("y", int(0)), ("z", int(5))]))])
        );
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path, path("pos/x"));
        assert_eq!(terms[0].kind, TermKind::DeletedVsEdited);
        assert_eq!(terms[0].right, None);
    }

    #[test]
    fn type_change_against_edit_is_a_value_term() {
        let base = obj([("pos", obj([("x", int(0))]))]);
        let left = obj([("pos", text("gone flat"))]);
        let right = obj([("pos", obj([("x", int(1))]))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(provisional, base);
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path, path("pos"));
        assert_eq!(terms[0].kind, TermKind::Value);
    }

    #[test]
    fn set_add_add_unions_in_base_left_right_order() {
        let base = obj([("tags", coll([("a", int(1)), ("b", int(2))]))]);
        let left = obj([("tags", coll([("l", int(9)), ("a", int(1)), ("b", int(2))]))]);
        let right = obj([("tags", coll([("a", int(1)), ("b", int(2)), ("r", int(8))]))]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &right)),
            obj([(
                "tags",
                coll([("a", int(1)), ("b", int(2)), ("l", int(9)), ("r", int(8))])
            )])
        );
    }

    #[test]
    fn set_remove_vs_untouched_removes_and_remove_vs_modify_conflicts() {
        let base = obj([("tags", coll([("a", int(1)), ("b", int(2))]))]);
        let left = obj([("tags", coll([("b", int(2))]))]);
        let right = obj([("tags", coll([("a", int(1)), ("b", int(3))]))]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &right)),
            obj([("tags", coll([("b", int(3))]))])
        );

        let right = obj([("tags", coll([("a", int(7)), ("b", int(2))]))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(provisional, base);
        assert_eq!(
            terms,
            vec![RecordConflictTerm {
                path: path("tags/a"),
                kind: TermKind::DeletedVsEdited,
                base: Some(int(1)),
                left: None,
                right: Some(int(7)),
            }]
        );
    }

    #[test]
    fn set_modify_both_sides_recurses_with_star_policy() {
        let elem =
            |label: &str, style: RecordValue| obj([("label", text(label)), ("style", style)]);
        let base = obj([("tags", coll([("a", elem("x", obj([("c", int(0))])))]))]);
        // Disjoint keys inside the element merge; `style` is Opaque via
        // `tags/*/style`, so a both-sided change is one Value term with no
        // recursion into it.
        let left = obj([("tags", coll([("a", elem("y", obj([("c", int(1))])))]))]);
        let right = obj([("tags", coll([("a", elem("x", obj([("c", int(2))])))]))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(
            provisional,
            obj([("tags", coll([("a", elem("y", obj([("c", int(0))])))]))])
        );
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path, path("tags/a/style"));
        assert_eq!(terms[0].kind, TermKind::Value);

        let left = obj([("tags", coll([("a", elem("y", obj([("c", int(0))])))]))]);
        let right = obj([("tags", coll([("a", elem("x", obj([("c", int(2))])))]))]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &right)),
            obj([("tags", coll([("a", elem("y", obj([("c", int(2))])))]))])
        );
    }

    #[test]
    fn list_order_changed_on_one_side_is_taken_with_other_sides_elements() {
        let base = obj([("links", coll([("a", int(1)), ("b", int(2)), ("c", int(3))]))]);
        let left = obj([("links", coll([("c", int(3)), ("a", int(1)), ("b", int(2))]))]);
        let right = obj([(
            "links",
            coll([("a", int(1)), ("b", int(20)), ("c", int(3)), ("d", int(4))]),
        )]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &right)),
            obj([(
                "links",
                coll([("c", int(3)), ("a", int(1)), ("b", int(20)), ("d", int(4))])
            )])
        );
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &right, &left)),
            obj([(
                "links",
                coll([("c", int(3)), ("a", int(1)), ("b", int(20)), ("d", int(4))])
            )])
        );
    }

    #[test]
    fn list_insert_in_place_without_reorder_keeps_position() {
        let base = obj([("links", coll([("a", int(1)), ("b", int(2))]))]);
        let left = obj([("links", coll([("a", int(1)), ("x", int(0)), ("b", int(2))]))]);
        let right = obj([("links", coll([("a", int(1)), ("b", int(2)), ("y", int(5))]))]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &right)),
            obj([(
                "links",
                coll([("a", int(1)), ("x", int(0)), ("b", int(2)), ("y", int(5))])
            )])
        );
    }

    #[test]
    fn list_order_changed_differently_on_both_sides_is_one_order_term() {
        let base = obj([("links", coll([("a", int(1)), ("b", int(2)), ("c", int(3))]))]);
        let left = obj([(
            "links",
            coll([("c", int(3)), ("a", int(1)), ("b", int(2)), ("l", int(0))]),
        )]);
        let right = obj([(
            "links",
            coll([("b", int(20)), ("a", int(1)), ("c", int(3))]),
        )]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        // Elements still merged, provisional order = base order + adds.
        let elements = |ids: [&str; 4]| {
            let value = |id: &str| match id {
                "a" => int(1),
                "b" => int(20),
                "c" => int(3),
                _ => int(0),
            };
            coll(ids.map(|id| (id, value(id))))
        };
        assert_eq!(
            provisional,
            obj([("links", elements(["a", "b", "c", "l"]))])
        );
        assert_eq!(
            terms,
            vec![RecordConflictTerm {
                path: path("links"),
                kind: TermKind::Order,
                base: Some(elements(["a", "b", "c", "l"])),
                left: Some(elements(["c", "a", "b", "l"])),
                right: Some(elements(["b", "a", "c", "l"])),
            }]
        );

        // Same reordering on both sides is not a conflict.
        let right = obj([(
            "links",
            coll([("c", int(3)), ("a", int(1)), ("b", int(20))]),
        )]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &right)),
            obj([("links", elements(["c", "a", "b", "l"]))])
        );
    }

    #[test]
    fn list_order_term_precedes_element_terms_so_resolve_applies_both() {
        let base = obj([("links", coll([("a", int(1)), ("b", int(2))]))]);
        let left = obj([("links", coll([("b", int(2)), ("a", int(10))]))]);
        let right = obj([("links", coll([("b", int(2)), ("a", int(11))]))]);
        // Only one order (both the same) but the element conflicts.
        let (_, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path, path("links/a"));

        // Three base elements so both sides can reorder differently
        // (left: b a c, right: c a b) while `a` also conflicts.
        let base = obj([("links", coll([("a", int(1)), ("b", int(2)), ("c", int(3))]))]);
        let left = obj([(
            "links",
            coll([("b", int(2)), ("a", int(10)), ("c", int(3))]),
        )]);
        let right = obj([(
            "links",
            coll([("c", int(3)), ("a", int(11)), ("b", int(2))]),
        )]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[0].kind, TermKind::Order);
        assert_eq!(terms[1].path, path("links/a"));
        let resolved =
            resolve(&provisional, &terms, &[TermChoice::Right, TermChoice::Left]).unwrap();
        assert_eq!(
            resolved,
            obj([(
                "links",
                coll([("c", int(3)), ("a", int(10)), ("b", int(2))])
            )])
        );
    }

    #[test]
    fn text_field_delegates_to_the_text_driver_and_merges() {
        let base = obj([("body", text("a\nb\nc\n")), ("title", text("T"))]);
        let left = obj([("body", text("A\nb\nc\n")), ("title", text("T"))]);
        let right = obj([("body", text("a\nb\nC\n")), ("title", text("U"))]);
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &right)),
            obj([("body", text("A\nb\nC\n")), ("title", text("U"))])
        );
    }

    #[test]
    fn text_field_conflict_is_one_region_term_per_region_with_base_provisional() {
        let base = obj([("body", text("a\nb\nc\n")), ("title", text("T"))]);
        let left = obj([("body", text("a\nB\nc\n")), ("title", text("U"))]);
        let right = obj([("body", text("a\nX\nc\n")), ("title", text("T"))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        // Title merged, body shows the base text rather than half-merged text.
        assert_eq!(
            provisional,
            obj([("body", text("a\nb\nc\n")), ("title", text("U"))])
        );
        assert_eq!(
            terms,
            vec![RecordConflictTerm {
                path: path("body/#0"),
                kind: TermKind::TextRegion,
                base: Some(text("b\n")),
                left: Some(text("B\n")),
                right: Some(text("X\n")),
            }]
        );
        assert_eq!(
            resolve(&provisional, &terms, &[TermChoice::Left]),
            Err(ResolveError::UnsupportedChoice { index: 0 })
        );

        // Without a base the whole text is one region and the term has no
        // base candidate; the provisional record lacks the field (and the
        // differing title is a Value term with no base either).
        let (provisional, terms) = conflicted(merge_record(&schema(), None, &left, &right));
        assert_eq!(provisional, obj([]));
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[0].path, path("body/#0"));
        assert_eq!(terms[0].kind, TermKind::TextRegion);
        assert_eq!(terms[0].base, None);
        assert_eq!(terms[0].left, Some(text("a\nB\nc\n")));
        assert_eq!(terms[1].path, path("title"));
        assert_eq!(terms[1].kind, TermKind::Value);
        assert_eq!(terms[1].base, None);
    }

    #[test]
    fn opaque_object_changed_on_both_sides_is_a_single_value_term() {
        let base = obj([("meta", obj([("a", int(0)), ("b", int(0))]))]);
        let left = obj([("meta", obj([("a", int(1)), ("b", int(0))]))]);
        let right = obj([("meta", obj([("a", int(0)), ("b", int(2))]))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(provisional, base);
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path, path("meta"));
        assert_eq!(terms[0].kind, TermKind::Value);
        assert_eq!(terms[0].left, Some(obj([("a", int(1)), ("b", int(0))])));
        // One-sided change on an opaque field is still taken whole.
        assert_eq!(
            merged(merge_record(&schema(), Some(&base), &left, &base)),
            left
        );
    }

    #[test]
    fn no_base_merges_equal_and_one_sided_keys_and_conflicts_on_the_rest() {
        let same = obj([("title", text("A"))]);
        assert_eq!(merged(merge_record(&schema(), None, &same, &same)), same);

        let left = obj([("title", text("A")), ("l", int(1)), ("n", int(1))]);
        let right = obj([("title", text("A")), ("r", int(2)), ("n", int(2))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), None, &left, &right));
        assert_eq!(
            provisional,
            obj([("title", text("A")), ("l", int(1)), ("r", int(2))])
        );
        assert_eq!(
            terms,
            vec![RecordConflictTerm {
                path: path("n"),
                kind: TermKind::Value,
                base: None,
                left: Some(int(1)),
                right: Some(int(2)),
            }]
        );
        assert_eq!(
            resolve(&provisional, &terms, &[TermChoice::Base]),
            Err(ResolveError::UnsupportedChoice { index: 0 })
        );
    }

    #[test]
    fn resolve_round_trips_left_right_base_and_checks_choice_count() {
        let base = obj([("title", text("A")), ("tags", coll([("a", int(1))]))]);
        let left = obj([("title", text("B")), ("tags", coll([]))]);
        let right = obj([("title", text("C")), ("tags", coll([("a", int(7))]))]);
        let (provisional, terms) = conflicted(merge_record(&schema(), Some(&base), &left, &right));
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[0].path, path("tags/a"));
        assert_eq!(terms[1].path, path("title"));

        assert_eq!(
            resolve(&provisional, &terms, &[TermChoice::Left, TermChoice::Left]).unwrap(),
            left
        );
        assert_eq!(
            resolve(
                &provisional,
                &terms,
                &[TermChoice::Right, TermChoice::Right]
            )
            .unwrap(),
            right
        );
        assert_eq!(
            resolve(&provisional, &terms, &[TermChoice::Base, TermChoice::Base]).unwrap(),
            base
        );
        assert_eq!(
            resolve(&provisional, &terms, &[TermChoice::Left]),
            Err(ResolveError::ChoiceCountMismatch {
                expected: 2,
                got: 1
            })
        );
    }

    #[test]
    fn merge_is_deterministic_and_mirrors_when_sides_swap() {
        let base = obj([
            ("title", text("A")),
            ("tags", coll([("a", int(1)), ("b", int(2))])),
            ("links", coll([("x", int(1)), ("y", int(2))])),
        ]);
        let left = obj([
            ("title", text("B")),
            ("tags", coll([("b", int(2)), ("l", int(0))])),
            ("links", coll([("y", int(2)), ("x", int(1)), ("p", int(0))])),
        ]);
        let right = obj([
            ("title", text("C")),
            ("tags", coll([("a", int(5)), ("b", int(2)), ("r", int(0))])),
            ("links", coll([("y", int(2)), ("q", int(0)), ("x", int(1))])),
        ]);
        let first = merge_record(&schema(), Some(&base), &left, &right);
        let second = merge_record(&schema(), Some(&base), &left, &right);
        assert_eq!(first, second);

        let swapped = merge_record(&schema(), Some(&base), &right, &left);
        let (_, terms) = conflicted(first);
        let (_, swapped_terms) = conflicted(swapped);
        assert_eq!(terms.len(), swapped_terms.len());
        for (t, s) in terms.iter().zip(&swapped_terms) {
            assert_eq!(t.path, s.path);
            assert_eq!(t.kind, s.kind);
            assert_eq!(t.base, s.base);
            assert_eq!(t.left, s.right);
            assert_eq!(t.right, s.left);
        }
    }
}

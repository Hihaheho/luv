//! `merge.record.v2`: deterministic three-way merge of `record.v2` values
//! under a [`RecordSchema`].
//!
//! The driver walks `(base, left, right)` by field path, following the
//! schema alongside. At every path the same three checks come first: equal
//! on both sides → keep; one side equal to base → take the other; one side
//! absent while the other edited → [`TermKind::DeletedVsEdited`]. Only when
//! both sides changed differently does the schema decide how to go deeper
//! ([`MergePolicy`]):
//!
//! - `Structural`: maps merge per key and tuples per element; sets per
//!   element (an element stays unless a side removed it, and every add
//!   lands); extensions per name. A union whose sides name different
//!   `$variant`s is one `Value` term. Anything else is a `Value` term.
//! - `Keyed`: array elements merge per key, and their order merges too: it
//!   is taken from the side that changed it, and both changing it
//!   differently is one `Order` term at the array path.
//! - `Text`: the text driver; every conflicting region is a `TextRegion`
//!   term at `path#<index>`.
//! - `Atomic`: whole-value comparison, never recursed.
//!
//! A merge that is clean but breaks the schema (two sides' adds together
//! exceed a maximum, say) is not clean: every violation is an `Invalid` term
//! at the innermost path a side's value can replace.
//!
//! The provisional record of a conflicted merge carries every field that
//! did merge; conflicting paths carry the base value, or are absent when
//! there is no base. The driver never invents keys; the order of a keyed
//! array's merged elements is base order, then left-only adds, then
//! right-only adds.

use std::collections::{BTreeMap, BTreeSet};

use eure_schema::SchemaNodeId;

use super::text::{TextMergeOutcome, merge_text};
use crate::model::TermKind;
use crate::record::{Data, FieldPath, ObjectKey, RecordValue, Segment, Text, element_key};
use crate::schema::{MergePolicy, RecordSchema};

/// One unresolved question of a record merge. `None` on a side means the
/// value is absent there (deleted key or element). For a `TextRegion` the
/// three values are the region as plain text; for an `Order` term they are
/// the merged array in each side's order.
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
    let mut ctx = Merger {
        schema,
        terms: Vec::new(),
    };
    let mut merged = ctx
        .value(
            Some(schema.root()),
            &FieldPath::root(),
            base,
            Some(left),
            Some(right),
        )
        .unwrap_or_else(RecordValue::null);
    let mut terms = ctx.terms;
    if terms.is_empty() {
        let mut seen = BTreeSet::new();
        for violation in schema.violations(&merged) {
            if !seen.insert(violation.path.clone()) {
                continue;
            }
            let at = |v: Option<&RecordValue>| v.and_then(|v| v.at(&violation.path)).cloned();
            terms.push(RecordConflictTerm {
                path: violation.path.clone(),
                kind: TermKind::Invalid,
                base: at(base),
                left: at(Some(left)),
                right: at(Some(right)),
            });
        }
        for t in &terms {
            set_path(&mut merged, &t.path, t.base.clone());
        }
    }
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
/// `TextRegion` terms carry only the region, and the provisional record
/// carries the field's base text, so a region cannot be placed from here:
/// such a term is [`ResolveError::UnsupportedChoice`]. Text regions are
/// resolved by re-running [`merge_text`] on the full sides and applying
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
        set_path(&mut out, &term.path, chosen);
    }
    Ok(out)
}

struct Merger<'s> {
    schema: &'s RecordSchema,
    terms: Vec<RecordConflictTerm>,
}

impl Merger<'_> {
    fn conflict(
        &mut self,
        path: &FieldPath,
        kind: TermKind,
        base: Option<&RecordValue>,
        left: Option<&RecordValue>,
        right: Option<&RecordValue>,
    ) -> Option<RecordValue> {
        self.terms.push(RecordConflictTerm {
            path: path.clone(),
            kind,
            base: base.cloned(),
            left: left.cloned(),
            right: right.cloned(),
        });
        base.cloned()
    }

    fn value(
        &mut self,
        id: Option<SchemaNodeId>,
        path: &FieldPath,
        base: Option<&RecordValue>,
        left: Option<&RecordValue>,
        right: Option<&RecordValue>,
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
                    return self.conflict(path, TermKind::DeletedVsEdited, base, left, right);
                }
            }
        }
        let (Some(l), Some(r)) = (left, right) else {
            // Only (Some, Some) survives the checks above.
            return base.cloned();
        };
        if let Some(id) = id
            && self.schema.is_union(id)
            && l.variant() != r.variant()
        {
            return self.conflict(path, TermKind::Value, base, left, right);
        }
        let policy = id.map_or(MergePolicy::Structural, |id| self.schema.policy(id));
        match policy {
            MergePolicy::Atomic => self.conflict(path, TermKind::Value, base, left, right),
            MergePolicy::Text => match (&l.data, &r.data) {
                (Data::Text(lt), Data::Text(rt))
                    if lt.language == rt.language
                        && l.ext == r.ext
                        && base.is_none_or(|b| b.ext == l.ext) =>
                {
                    let bt = base.and_then(RecordValue::as_text);
                    self.text(path, bt, lt, rt, l)
                }
                _ => self.conflict(path, TermKind::Value, base, left, right),
            },
            MergePolicy::Keyed(field) => {
                match (
                    keyed(base, &field),
                    keyed(Some(l), &field),
                    keyed(Some(r), &field),
                ) {
                    (Some(bk), Some(lk), Some(rk)) => {
                        let data = self.keyed(id, path, l, &field, &bk, &lk, &rk);
                        Some(self.with_ext(id, path, base, l, r, data))
                    }
                    _ => self.conflict(path, TermKind::Value, base, left, right),
                }
            }
            MergePolicy::Structural => {
                let b = base.map(|b| &b.data);
                let data = if l.data == r.data {
                    l.data.clone()
                } else if b == Some(&l.data) {
                    r.data.clone()
                } else if b == Some(&r.data) {
                    l.data.clone()
                } else {
                    match (&l.data, &r.data) {
                        (Data::Map(lm), Data::Map(rm)) => {
                            let bm = base.and_then(RecordValue::as_map);
                            Data::Map(self.map(id, path, l, bm, lm, rm))
                        }
                        (Data::Tuple(lt), Data::Tuple(rt)) if lt.len() == rt.len() => {
                            let bt = match base.map(|b| &b.data) {
                                Some(Data::Tuple(bt)) if bt.len() == lt.len() => Some(bt),
                                _ => None,
                            };
                            let mut out = Vec::with_capacity(lt.len());
                            for (i, (lv, rv)) in lt.iter().zip(rt).enumerate() {
                                let seg = Segment::Index(i);
                                let child = id.and_then(|id| self.schema.child(id, l, &seg));
                                let bv = bt.map(|bt| &bt[i]);
                                match self.value(child, &path.child(seg), bv, Some(lv), Some(rv)) {
                                    Some(v) => out.push(v),
                                    // Both present: a merge keeps the element.
                                    None => out.push(lv.clone()),
                                }
                            }
                            Data::Tuple(out)
                        }
                        (Data::Set(ls), Data::Set(rs)) => {
                            let bs = match base.map(|b| &b.data) {
                                Some(Data::Set(bs)) => Some(bs),
                                _ => None,
                            };
                            Data::Set(merge_set(bs, ls, rs))
                        }
                        _ => return self.conflict(path, TermKind::Value, base, left, right),
                    }
                };
                Some(self.with_ext(id, path, base, l, r, data))
            }
        }
    }

    /// `data` with the extensions of the sides merged per name.
    fn with_ext(
        &mut self,
        id: Option<SchemaNodeId>,
        path: &FieldPath,
        base: Option<&RecordValue>,
        l: &RecordValue,
        r: &RecordValue,
        data: Data,
    ) -> RecordValue {
        let be = base.map(|b| &b.ext);
        let names: BTreeSet<&String> = be
            .into_iter()
            .flat_map(BTreeMap::keys)
            .chain(l.ext.keys())
            .chain(r.ext.keys())
            .collect();
        let mut ext = BTreeMap::new();
        for name in names {
            let seg = Segment::Ext(name.clone());
            let child = id.and_then(|id| self.schema.child(id, l, &seg));
            if let Some(v) = self.value(
                child,
                &path.child(seg),
                be.and_then(|e| e.get(name)),
                l.ext.get(name),
                r.ext.get(name),
            ) {
                ext.insert(name.clone(), v);
            }
        }
        RecordValue { data, ext }
    }

    fn map(
        &mut self,
        id: Option<SchemaNodeId>,
        path: &FieldPath,
        parent: &RecordValue,
        base: Option<&BTreeMap<ObjectKey, RecordValue>>,
        left: &BTreeMap<ObjectKey, RecordValue>,
        right: &BTreeMap<ObjectKey, RecordValue>,
    ) -> BTreeMap<ObjectKey, RecordValue> {
        let keys: BTreeSet<&ObjectKey> = base
            .into_iter()
            .flat_map(BTreeMap::keys)
            .chain(left.keys())
            .chain(right.keys())
            .collect();
        let mut out = BTreeMap::new();
        for key in keys {
            let seg = Segment::Key(key.clone());
            let child = id.and_then(|id| self.schema.child(id, parent, &seg));
            if let Some(v) = self.value(
                child,
                &path.child(seg),
                base.and_then(|m| m.get(key)),
                left.get(key),
                right.get(key),
            ) {
                out.insert(key.clone(), v);
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn keyed(
        &mut self,
        id: Option<SchemaNodeId>,
        path: &FieldPath,
        parent: &RecordValue,
        field: &str,
        base: &[(ObjectKey, &RecordValue)],
        left: &[(ObjectKey, &RecordValue)],
        right: &[(ObjectKey, &RecordValue)],
    ) -> Data {
        let (bm, lm, rm) = (by_key(base), by_key(left), by_key(right));
        let (base_keys, left_keys, right_keys) = (keys(base), keys(left), keys(right));

        // Base order first, then left-only adds, then right-only adds.
        let mut union: Vec<&ObjectKey> = base_keys.clone();
        union.extend(left_keys.iter().filter(|k| !bm.contains_key(*k)));
        union.extend(
            right_keys
                .iter()
                .filter(|k| !bm.contains_key(*k) && !lm.contains_key(*k)),
        );

        let outer = std::mem::take(&mut self.terms);
        let mut merged: Vec<(ObjectKey, RecordValue)> = Vec::new();
        for key in union {
            let seg = Segment::Item {
                field: field.to_owned(),
                key: key.clone(),
            };
            let child = id.and_then(|id| self.schema.child(id, parent, &seg));
            if let Some(value) = self.value(
                child,
                &path.child(seg),
                bm.get(key).copied(),
                lm.get(key).copied(),
                rm.get(key).copied(),
            ) {
                merged.push((key.clone(), value));
            }
        }
        let element_terms = std::mem::replace(&mut self.terms, outer);

        let left_changed = order_changed(&base_keys, &left_keys);
        let right_changed = order_changed(&base_keys, &right_keys);
        let both_differ = left_changed && right_changed && {
            let in_left: BTreeSet<&ObjectKey> = left_keys.iter().copied().collect();
            let in_right: BTreeSet<&ObjectKey> = right_keys.iter().copied().collect();
            let l: Vec<&ObjectKey> = left_keys
                .iter()
                .copied()
                .filter(|k| in_right.contains(k))
                .collect();
            let r: Vec<&ObjectKey> = right_keys
                .iter()
                .copied()
                .filter(|k| in_left.contains(k))
                .collect();
            l != r
        };
        if both_differ {
            let array = |order: &[&ObjectKey], others: [&[&ObjectKey]; 2]| {
                let ordered = reorder(&merged, order, others);
                Some(RecordValue::array(ordered.into_iter().map(|(_, v)| v)))
            };
            self.terms.push(RecordConflictTerm {
                path: path.clone(),
                kind: TermKind::Order,
                base: array(&base_keys, [&left_keys, &right_keys]),
                left: array(&left_keys, [&right_keys, &base_keys]),
                right: array(&right_keys, [&left_keys, &base_keys]),
            });
            // Provisional order: base order with adds appended (already the
            // union order).
        } else if right_changed && !left_changed {
            merged = reorder(&merged, &right_keys, [&left_keys, &base_keys]);
        } else {
            merged = reorder(&merged, &left_keys, [&right_keys, &base_keys]);
        }
        self.terms.extend(element_terms);
        Data::Array(merged.into_iter().map(|(_, v)| v).collect())
    }

    fn text(
        &mut self,
        path: &FieldPath,
        base: Option<&Text>,
        left: &Text,
        right: &Text,
        shape: &RecordValue,
    ) -> Option<RecordValue> {
        let with = |content: String| RecordValue {
            data: Data::Text(Text {
                content,
                language: left.language.clone(),
            }),
            ext: shape.ext.clone(),
        };
        let base_content = base.map(|b| b.content.as_bytes());
        match merge_text(
            base_content,
            left.content.as_bytes(),
            right.content.as_bytes(),
        ) {
            TextMergeOutcome::Merged(bytes) => Some(with(text_of(bytes))),
            TextMergeOutcome::Conflicted { regions, .. } => {
                for region in regions {
                    self.terms.push(RecordConflictTerm {
                        path: path.child(Segment::Region(region.index as u32)),
                        kind: TermKind::TextRegion,
                        base: base.map(|_| RecordValue::text(text_of(region.base))),
                        left: Some(RecordValue::text(text_of(region.left))),
                        right: Some(RecordValue::text(text_of(region.right))),
                    });
                }
                // Provisional: the base text (or absent), never half-merged text.
                base.map(|b| with(b.content.clone()))
            }
        }
    }
}

/// The elements of a keyed array with their keys, when it is an array whose
/// elements each have a distinct key; an absent base is an empty array.
fn keyed<'v>(
    value: Option<&'v RecordValue>,
    field: &str,
) -> Option<Vec<(ObjectKey, &'v RecordValue)>> {
    let Some(value) = value else {
        return Some(Vec::new());
    };
    let items = value.as_array()?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let key = element_key(item, field)?;
        if !seen.insert(key.clone()) {
            return None;
        }
        out.push((key, item));
    }
    Some(out)
}

fn by_key<'a, 'v>(
    elements: &'a [(ObjectKey, &'v RecordValue)],
) -> BTreeMap<&'a ObjectKey, &'v RecordValue> {
    elements.iter().map(|(k, v)| (k, *v)).collect()
}

fn keys<'a>(elements: &'a [(ObjectKey, &RecordValue)]) -> Vec<&'a ObjectKey> {
    elements.iter().map(|(k, _)| k).collect()
}

/// Whether `side` orders the keys it shares with `base` differently from
/// `base` (adds and removes are ignored).
fn order_changed(base: &[&ObjectKey], side: &[&ObjectKey]) -> bool {
    let in_base: BTreeSet<&ObjectKey> = base.iter().copied().collect();
    let in_side: BTreeSet<&ObjectKey> = side.iter().copied().collect();
    let base_common = base.iter().filter(|k| in_side.contains(*k));
    let side_common = side.iter().filter(|k| in_base.contains(*k));
    !base_common.eq(side_common)
}

/// `elements` in the order of `sequence`. An element `sequence` lacks (the
/// other side's add, or one a conflict keeps) goes right before the next
/// placed element after it in the first of `others` that has it, else last.
fn reorder(
    elements: &[(ObjectKey, RecordValue)],
    sequence: &[&ObjectKey],
    others: [&[&ObjectKey]; 2],
) -> Vec<(ObjectKey, RecordValue)> {
    let present: BTreeSet<&ObjectKey> = elements.iter().map(|(k, _)| k).collect();
    let mut order: Vec<&ObjectKey> = sequence
        .iter()
        .copied()
        .filter(|k| present.contains(k))
        .collect();
    for (key, _) in elements {
        if order.contains(&key) {
            continue;
        }
        let source: &[&ObjectKey] = others
            .iter()
            .find(|o| o.contains(&key))
            .copied()
            .unwrap_or_default();
        let at = source.iter().position(|k| *k == key).unwrap_or(0);
        let place = |k: &&ObjectKey| order.iter().position(|o| o == k);
        let slot = source[at + 1..]
            .iter()
            .find_map(place)
            .unwrap_or(order.len());
        order.insert(slot, key);
    }
    order
        .into_iter()
        .map(|k| {
            let (_, v) = elements.iter().find(|(e, _)| e == k).expect("present");
            (k.clone(), v.clone())
        })
        .collect()
}

/// An element stays when both sides kept it, and every add lands. Without
/// a base, the union.
fn merge_set(
    base: Option<&BTreeSet<ObjectKey>>,
    left: &BTreeSet<ObjectKey>,
    right: &BTreeSet<ObjectKey>,
) -> BTreeSet<ObjectKey> {
    let Some(base) = base else {
        return left.union(right).cloned().collect();
    };
    left.union(right)
        .filter(|e| !base.contains(*e) || (left.contains(*e) && right.contains(*e)))
        .cloned()
        .collect()
}

fn text_of(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Sets (or removes, with `None`) the value at `path` inside `root`,
/// following map keys, extensions, tuple elements and array elements. A path
/// whose parent does not exist is left alone: the terms of `merge_record`
/// always point into the provisional record. A tuple element is never
/// removed, and a region is not a place.
pub fn set_path(root: &mut RecordValue, path: &FieldPath, value: Option<RecordValue>) {
    let Some((last, parents)) = path.0.split_last() else {
        if let Some(v) = value {
            *root = v;
        }
        return;
    };
    let mut cur = root;
    for seg in parents {
        let next = match (seg, &mut cur.data) {
            (Segment::Ext(name), _) => cur.ext.get_mut(name),
            (Segment::Key(k), Data::Map(m)) => m.get_mut(k),
            (Segment::Index(i), Data::Tuple(items))
            | (Segment::Position(i), Data::Array(items)) => items.get_mut(*i),
            (Segment::Item { field, key }, Data::Array(items)) => items
                .iter_mut()
                .find(|e| element_key(e, field).as_ref() == Some(key)),
            _ => None,
        };
        match next {
            Some(n) => cur = n,
            None => return,
        }
    }
    match (last, &mut cur.data, value) {
        (Segment::Ext(name), _, Some(v)) => {
            cur.ext.insert(name.clone(), v);
        }
        (Segment::Ext(name), _, None) => {
            cur.ext.remove(name);
        }
        (Segment::Key(k), Data::Map(m), Some(v)) => {
            m.insert(k.clone(), v);
        }
        (Segment::Key(k), Data::Map(m), None) => {
            m.remove(k);
        }
        (Segment::Index(i), Data::Tuple(items), Some(v))
        | (Segment::Position(i), Data::Array(items), Some(v)) => {
            if let Some(slot) = items.get_mut(*i) {
                *slot = v;
            }
        }
        (Segment::Position(i), Data::Array(items), None) => {
            if *i < items.len() {
                items.remove(*i);
            }
        }
        (Segment::Item { field, key }, Data::Array(items), value) => {
            let pos = items
                .iter()
                .position(|e| element_key(e, field).as_ref() == Some(key));
            match (pos, value) {
                (Some(i), Some(v)) => items[i] = v,
                (Some(i), None) => {
                    items.remove(i);
                }
                (None, Some(v)) => items.push(v),
                (None, None) => {}
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"
title = `text`
title.$optional = true
n = `any`
n.$optional = true
body = `text`
body.$optional = true
body.$merge = "text"
meta = `any`
meta.$optional = true
meta.$merge = "atomic"
pos = `any`
pos.$optional = true
pair = (`integer`, `integer`)
pair.$optional = true
tags {
  $variant: set
  item = `text`
}
tags.$optional = true
links {
  $variant: array
  item {
    id = `text`
    n = `integer`
    n.$optional = true
    style = `any`
    style.$optional = true
    style.$merge = "atomic"
    label = `text`
    label.$optional = true
  }
  $merge-key = "id"
  max-length = 4
}
links.$optional = true
status {
  $variant: union
  variants.open = { since => `integer` }
  variants.closed = { reason => `text` }
}
status.$optional = true
$unknown-fields = "allow"
"#;

    fn schema() -> RecordSchema {
        RecordSchema::from_eure(SCHEMA).unwrap().1
    }

    fn rec(src: &str) -> RecordValue {
        RecordValue::from_eure(src).unwrap()
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

    fn merge(base: Option<&str>, left: &str, right: &str) -> RecordMergeOutcome {
        let base = base.map(rec);
        merge_record(&schema(), base.as_ref(), &rec(left), &rec(right))
    }

    fn link(id: &str, n: i64) -> String {
        format!("links[] {{\n  id = \"{id}\"\n  n = {n}\n}}\n")
    }

    fn links(items: &[(&str, i64)]) -> String {
        items.iter().map(|(id, n)| link(id, *n)).collect()
    }

    fn ids(v: &RecordValue) -> Vec<String> {
        v.get("links")
            .and_then(RecordValue::as_array)
            .unwrap_or_default()
            .iter()
            .map(|e| e.get("id").unwrap().as_text().unwrap().content.clone())
            .collect()
    }

    #[test]
    fn one_side_scalar_change_is_taken() {
        let base = "title = \"A\"\nn = 1\n";
        let left = "title = \"B\"\nn = 1\n";
        assert_eq!(merged(merge(Some(base), left, base)), rec(left));
        assert_eq!(merged(merge(Some(base), base, left)), rec(left));
    }

    #[test]
    fn disjoint_field_changes_merge() {
        let out = merge(
            Some("title = \"A\"\nn = 1\ngone = 0\n"),
            "title = \"B\"\nn = 1\ngone = 0\n",
            "title = \"A\"\nn = 2\nadded = true\n",
        );
        assert_eq!(merged(out), rec("title = \"B\"\nn = 2\nadded = true\n"));
    }

    #[test]
    fn conflicting_scalar_yields_value_term_with_base_value_kept() {
        let (provisional, terms) = conflicted(merge(
            Some("title = \"A\"\nn = 1\n"),
            "title = \"B\"\nn = 2\n",
            "title = \"C\"\nn = 2\n",
        ));
        assert_eq!(provisional, rec("title = \"A\"\nn = 2\n"));
        assert_eq!(
            terms,
            vec![RecordConflictTerm {
                path: FieldPath::root().key("title"),
                kind: TermKind::Value,
                base: Some(RecordValue::text("A")),
                left: Some(RecordValue::text("B")),
                right: Some(RecordValue::text("C")),
            }]
        );
    }

    #[test]
    fn decimals_that_are_equal_do_not_conflict() {
        // `1.10` and `1.1` are the same number: the left side changed nothing.
        let out = merge(Some("n = 1.1\n"), "n = 1.10\n", "n = 2.5\n");
        assert_eq!(merged(out), rec("n = 2.5\n"));
    }

    #[test]
    fn missing_and_null_are_different_values() {
        let (provisional, terms) =
            conflicted(merge(Some("n = 1\n"), "n = null\n", "title = \"x\"\n"));
        // `title` merged in; `n` is in conflict and keeps its base value.
        assert_eq!(provisional, rec("n = 1\ntitle = \"x\"\n"));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].kind, TermKind::DeletedVsEdited);
        assert_eq!(terms[0].left, Some(RecordValue::null()));
        assert_eq!(terms[0].right, None);
    }

    #[test]
    fn nested_maps_recurse_per_key() {
        let (provisional, terms) = conflicted(merge(
            Some("pos { x = 0\ny = 0\nz = 0 }\n"),
            "pos { x = 1\ny = 0\nz = 5 }\n",
            "pos { x = 0\ny = 2\nz = 6 }\n",
        ));
        assert_eq!(provisional, rec("pos { x = 1\ny = 2\nz = 0 }\n"));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path.to_string(), "pos.z");
        assert_eq!(terms[0].kind, TermKind::Value);
    }

    #[test]
    fn type_change_against_edit_is_a_value_term() {
        let base = "pos { x = 0 }\n";
        let (provisional, terms) =
            conflicted(merge(Some(base), "pos = \"flat\"\n", "pos { x = 1 }\n"));
        assert_eq!(provisional, rec(base));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path.to_string(), "pos");
        assert_eq!(terms[0].kind, TermKind::Value);
    }

    #[test]
    fn tuples_merge_per_element() {
        let out = merge(
            Some("pair = (1, 2)\n"),
            "pair = (5, 2)\n",
            "pair = (1, 7)\n",
        );
        assert_eq!(merged(out), rec("pair = (5, 7)\n"));
    }

    #[test]
    fn sets_keep_removes_and_union_adds_without_conflict() {
        let out = merge(
            Some("tags = #{\"a\", \"b\", \"c\"}\n"),
            "tags = #{\"b\", \"c\", \"l\"}\n",
            "tags = #{\"a\", \"b\", \"r\"}\n",
        );
        assert_eq!(merged(out), rec("tags = #{\"b\", \"l\", \"r\"}\n"));
        let out = merge(None, "tags = #{\"a\"}\n", "tags = #{\"b\"}\n");
        assert_eq!(merged(out), rec("tags = #{\"a\", \"b\"}\n"));
    }

    #[test]
    fn keyed_add_add_lands_in_base_left_right_order() {
        let base = links(&[("a", 1), ("b", 2)]);
        let left = links(&[("l", 9), ("a", 1), ("b", 2)]);
        let right = links(&[("a", 1), ("b", 2), ("r", 8)]);
        // Each add keeps its place on its own side.
        let out = merged(merge(Some(&base), &left, &right));
        assert_eq!(ids(&out), ["l", "a", "b", "r"]);
    }

    #[test]
    fn keyed_remove_vs_untouched_removes_and_remove_vs_modify_conflicts() {
        let base = links(&[("a", 1), ("b", 2)]);
        let left = links(&[("b", 2)]);
        let out = merged(merge(Some(&base), &left, &links(&[("a", 1), ("b", 3)])));
        assert_eq!(out, rec(&links(&[("b", 3)])));

        let right = links(&[("a", 7), ("b", 2)]);
        let (provisional, terms) = conflicted(merge(Some(&base), &left, &right));
        assert_eq!(provisional, rec(&base));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path.to_string(), "links[id=\"a\"]");
        assert_eq!(terms[0].kind, TermKind::DeletedVsEdited);
        assert_eq!(terms[0].left, None);
    }

    #[test]
    fn keyed_elements_merge_inside_with_the_item_schema() {
        let base = "links[] {\n  id = \"a\"\n  label = \"x\"\n  style { c = 0 }\n}\n";
        // Disjoint fields inside the element merge; `style` is atomic, so a
        // both-sided change is one Value term with no recursion into it.
        let left = "links[] {\n  id = \"a\"\n  label = \"y\"\n  style { c = 1 }\n}\n";
        let right = "links[] {\n  id = \"a\"\n  label = \"x\"\n  style { c = 2 }\n}\n";
        let (provisional, terms) = conflicted(merge(Some(base), left, right));
        assert_eq!(
            provisional,
            rec("links[] {\n  id = \"a\"\n  label = \"y\"\n  style { c = 0 }\n}\n")
        );
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path.to_string(), "links[id=\"a\"].style");
    }

    #[test]
    fn keyed_order_changed_on_one_side_is_taken_with_other_sides_elements() {
        let base = links(&[("a", 1), ("b", 2), ("c", 3)]);
        let left = links(&[("c", 3), ("a", 1), ("b", 2)]);
        let right = links(&[("a", 1), ("b", 20), ("c", 3), ("d", 4)]);
        let want = rec(&links(&[("c", 3), ("a", 1), ("b", 20), ("d", 4)]));
        assert_eq!(merged(merge(Some(&base), &left, &right)), want);
        assert_eq!(merged(merge(Some(&base), &right, &left)), want);
    }

    #[test]
    fn keyed_order_changed_differently_on_both_sides_is_one_order_term() {
        let base = links(&[("a", 1), ("b", 2), ("c", 3)]);
        let left = links(&[("c", 3), ("a", 1), ("b", 2), ("l", 0)]);
        let right = links(&[("b", 20), ("a", 1), ("c", 3)]);
        let (provisional, terms) = conflicted(merge(Some(&base), &left, &right));
        assert_eq!(ids(&provisional), ["a", "b", "c", "l"]);
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].kind, TermKind::Order);
        assert_eq!(terms[0].path.to_string(), "links");
        let order =
            |v: &Option<RecordValue>| ids(&RecordValue::map([("links", v.clone().unwrap())]));
        assert_eq!(order(&terms[0].left), ["c", "a", "b", "l"]);
        assert_eq!(order(&terms[0].right), ["b", "a", "c", "l"]);

        // Resolving the order and an element both apply.
        let resolved = resolve(&provisional, &terms, &[TermChoice::Right]).unwrap();
        assert_eq!(ids(&resolved), ["b", "a", "c", "l"]);
    }

    #[test]
    fn text_field_delegates_to_the_text_driver() {
        let out = merge(
            Some("body = \"a\\nb\\nc\\n\"\ntitle = \"T\"\n"),
            "body = \"A\\nb\\nc\\n\"\ntitle = \"T\"\n",
            "body = \"a\\nb\\nC\\n\"\ntitle = \"U\"\n",
        );
        assert_eq!(merged(out), rec("body = \"A\\nb\\nC\\n\"\ntitle = \"U\"\n"));

        let (provisional, terms) = conflicted(merge(
            Some("body = \"a\\nb\\nc\\n\"\n"),
            "body = \"a\\nB\\nc\\n\"\n",
            "body = \"a\\nX\\nc\\n\"\n",
        ));
        assert_eq!(provisional, rec("body = \"a\\nb\\nc\\n\"\n"));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path.to_string(), "body#0");
        assert_eq!(terms[0].kind, TermKind::TextRegion);
        assert_eq!(terms[0].left, Some(RecordValue::text("B\n")));
        assert_eq!(
            resolve(&provisional, &terms, &[TermChoice::Left]),
            Err(ResolveError::UnsupportedChoice { index: 0 })
        );
    }

    #[test]
    fn atomic_value_changed_on_both_sides_is_a_single_value_term() {
        let base = "meta { a = 0\nb = 0 }\n";
        let left = "meta { a = 1\nb = 0 }\n";
        let (provisional, terms) = conflicted(merge(Some(base), left, "meta { a = 0\nb = 2 }\n"));
        assert_eq!(provisional, rec(base));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path.to_string(), "meta");
        assert_eq!(merged(merge(Some(base), left, base)), rec(left));
    }

    #[test]
    fn union_sides_on_different_variants_conflict_whole() {
        let base = "status { $variant = \"open\"\nsince = 1 }\n";
        let (_, terms) = conflicted(merge(
            Some(base),
            "status { $variant = \"closed\"\nreason = \"done\" }\n",
            "status { $variant = \"open\"\nsince = 2 }\n",
        ));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path.to_string(), "status");
        assert_eq!(terms[0].kind, TermKind::Value);
    }

    #[test]
    fn a_clean_merge_that_breaks_the_schema_is_an_invalid_term() {
        // Each side adds within max-length 4; together they exceed it.
        let base = links(&[("a", 1), ("b", 2)]);
        let left = links(&[("a", 1), ("b", 2), ("l", 0), ("m", 0)]);
        let right = links(&[("a", 1), ("b", 2), ("r", 0)]);
        let (provisional, terms) = conflicted(merge(Some(&base), &left, &right));
        assert_eq!(terms.len(), 1, "{terms:?}");
        assert_eq!(terms[0].kind, TermKind::Invalid);
        assert_eq!(terms[0].path.to_string(), "links");
        assert_eq!(provisional, rec(&base));
        let resolved = resolve(&provisional, &terms, &[TermChoice::Left]).unwrap();
        assert_eq!(resolved, rec(&left));
    }

    #[test]
    fn no_base_merges_equal_and_one_sided_keys_and_conflicts_on_the_rest() {
        let (provisional, terms) = conflicted(merge(
            None,
            "title = \"A\"\nl = 1\nn = 1\n",
            "title = \"A\"\nr = 2\nn = 2\n",
        ));
        assert_eq!(provisional, rec("title = \"A\"\nl = 1\nr = 2\n"));
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].path.to_string(), "n");
        assert_eq!(terms[0].base, None);
        assert_eq!(
            resolve(&provisional, &terms, &[TermChoice::Base]),
            Err(ResolveError::UnsupportedChoice { index: 0 })
        );
    }

    #[test]
    fn merge_is_deterministic_and_mirrors_when_sides_swap() {
        let base = format!(
            "title = \"A\"\ntags = #{{\"a\", \"b\"}}\n{}",
            links(&[("x", 1), ("y", 2)])
        );
        let left = format!(
            "title = \"B\"\ntags = #{{\"b\", \"l\"}}\n{}",
            links(&[("y", 2), ("x", 1), ("p", 0)])
        );
        let right = format!(
            "title = \"C\"\ntags = #{{\"a\", \"r\"}}\n{}",
            links(&[("y", 2), ("q", 0), ("x", 1)])
        );
        let first = merge(Some(&base), &left, &right);
        assert_eq!(first, merge(Some(&base), &left, &right));
        let swapped = merge(Some(&base), &right, &left);
        let (_, terms) = conflicted(first);
        let (_, swapped_terms) = conflicted(swapped);
        assert_eq!(terms.len(), swapped_terms.len());
        for (t, s) in terms.iter().zip(&swapped_terms) {
            assert_eq!(t.path, s.path);
            assert_eq!(t.kind, s.kind);
            assert_eq!(t.left, s.right);
            assert_eq!(t.right, s.left);
        }
    }
}

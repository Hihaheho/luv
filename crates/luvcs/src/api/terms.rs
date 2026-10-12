//! Resolving a conflict term by term (`VcsResolution::Terms`).
//!
//! A stored conflict keeps its base and sides, and its drivers are pure
//! functions, so the merge is run again here: the text regions and record
//! terms it yields are the conflict's terms, in the same order, and each
//! answer goes where its term is. A merge that does not yield those terms
//! any more (the record schema changed since) is refused rather than
//! guessed at; the whole-body resolutions still work.

use std::collections::BTreeMap;

use super::{ApiResult, VcsError, VcsTermChoice, VcsTermPick, record_from_json_value};
use crate::cbor::{self, Value};
use crate::digest::Digest;
use crate::id::{ContentId, ObjectId, ObjectStateId};
use crate::merge::record::{RecordMergeOutcome, merge_record, set_path};
use crate::merge::text::{ConflictRegion, TextMergeOutcome, assemble, merge_text};
use crate::merge::{RECORD_DRIVER_V1, TEXT_DRIVER_V1};
use crate::model::{Conflict, ConflictTerm, ObjectBody, ObjectState, TermKind};
use crate::record::{FieldPath, RecordValue};
use crate::store::{Repo, SchemaRegistry, StoreError};

fn invalid(message: impl Into<String>) -> VcsError {
    VcsError::invalid(message)
}

fn no_side(index: u32) -> VcsError {
    invalid(format!("no side {index}"))
}

/// The resolved state of `object_id`, whose working state `state` holds
/// `conflict`, from one choice per term.
pub(super) fn resolve<D: Digest>(
    repo: &mut Repo<'_, '_, D>,
    schemas: &SchemaRegistry,
    object_id: ObjectId,
    state: &ObjectState,
    conflict: &Conflict,
    choices: &[VcsTermChoice],
) -> ApiResult<ObjectStateId> {
    let mut picks: Vec<Option<&VcsTermPick>> = vec![None; conflict.terms.len()];
    for c in choices {
        let slot = picks
            .get_mut(c.term as usize)
            .ok_or_else(|| invalid(format!("the conflict has no term {}", c.term)))?;
        if slot.replace(&c.pick).is_some() {
            return Err(invalid(format!("term {} is answered twice", c.term)));
        }
    }
    let missing: Vec<String> = picks
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_none())
        .map(|(i, _)| i.to_string())
        .collect();
    if !missing.is_empty() {
        return Err(invalid(format!(
            "every term needs an answer; missing: {}",
            missing.join(", ")
        )));
    }
    let answers: Vec<(&ConflictTerm, &VcsTermPick)> = conflict
        .terms
        .iter()
        .zip(picks.into_iter().flatten())
        .collect();
    for (term, pick) in &answers {
        if let VcsTermPick::Side { index } = pick
            && *index as usize >= term.candidates.len()
        {
            return Err(no_side(*index));
        }
        if matches!(pick, VcsTermPick::Both) && term.kind != TermKind::TextRegion {
            return Err(invalid(format!(
                "term {:?}: only a text region takes both sides",
                term.path
            )));
        }
    }
    let (body_answers, meta_answers): (Vec<_>, Vec<_>) = answers
        .into_iter()
        .partition(|(t, _)| t.kind != TermKind::Metadata);

    let sides = conflict
        .sides
        .iter()
        .map(|s| repo.get_state(*s))
        .collect::<Result<Vec<_>, _>>()?;
    let base = match conflict.base.as_slice() {
        [b] => Some(repo.get_state(*b)?),
        _ => None,
    };
    let mut ctx = Resolver { repo };
    let body = match conflict.driver.as_str() {
        TEXT_DRIVER_V1 => ctx.text_body(conflict, base.as_ref(), &sides, &body_answers)?,
        RECORD_DRIVER_V1 => {
            ctx.record_body(schemas, conflict, base.as_ref(), &sides, &body_answers)?
        }
        _ => ctx.object_body(base.as_ref(), &sides, &body_answers)?,
    };

    // The conflict state keeps the metadata both sides agree on.
    let mut metadata: BTreeMap<String, Value> = state
        .metadata
        .map_entries()
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.clone()))
        .collect();
    for (term, pick) in meta_answers {
        let value = match pick {
            VcsTermPick::Side { index } => match term.candidates[*index as usize] {
                Some(c) => Some(
                    cbor::decode(&ctx.content(c)?)
                        .map_err(|e| VcsError::new(super::VcsErrorCode::Internal, e.to_string()))?,
                ),
                None => None,
            },
            VcsTermPick::Value { value_json } => {
                match serde_json::from_str::<serde_json::Value>(value_json) {
                    Ok(serde_json::Value::String(s)) => Some(Value::text(s)),
                    _ => {
                        return Err(invalid(format!(
                            "metadata {:?} takes a JSON string",
                            term.path
                        )));
                    }
                }
            }
            VcsTermPick::Absent => None,
            VcsTermPick::Both => unreachable!("checked above"),
        };
        match value {
            Some(v) => metadata.insert(term.path.clone(), v),
            None => metadata.remove(&term.path),
        };
    }
    let resolved = ObjectState::new(object_id, &state.type_id, body).with_metadata(Value::Map(
        metadata
            .into_iter()
            .map(|(k, v)| (Value::text(k), v))
            .collect(),
    ));
    Ok(ctx.repo.put_state(&resolved)?)
}

struct Resolver<'a, 's, 'c, D: Digest> {
    repo: &'a mut Repo<'s, 'c, D>,
}

impl<D: Digest> Resolver<'_, '_, '_, D> {
    fn content(&self, id: ContentId) -> ApiResult<Vec<u8>> {
        match self.repo.get_content(id)? {
            Some(bytes) => Ok(bytes),
            None if self.repo.is_redacted_content(id)? => Err(StoreError::Redacted {
                kind: "content",
                id: id.to_string(),
            }
            .into()),
            None => Err(StoreError::MissingObject {
                kind: "content",
                id: id.to_string(),
            }
            .into()),
        }
    }

    fn text_of(&self, state: &ObjectState) -> ApiResult<Vec<u8>> {
        match state.body {
            ObjectBody::Text { content } => self.content(content),
            _ => Err(invalid("a side of a text conflict is not text")),
        }
    }

    fn record_of(&self, state: &ObjectState) -> ApiResult<RecordValue> {
        match state.body {
            ObjectBody::Record { content, .. } => {
                RecordValue::decode(&self.content(content)?).map_err(|e| invalid(e.to_string()))
            }
            _ => Err(invalid("a side of a record conflict is not a record")),
        }
    }

    fn text_body(
        &mut self,
        conflict: &Conflict,
        base: Option<&ObjectState>,
        sides: &[ObjectState],
        answers: &[(&ConflictTerm, &VcsTermPick)],
    ) -> ApiResult<ObjectBody> {
        if answers.is_empty() {
            let content = conflict
                .merged
                .ok_or_else(|| invalid("text conflict without a merged body"))?;
            return Ok(ObjectBody::Text { content });
        }
        let [left, right] = sides else {
            return Err(invalid("a text conflict has two sides"));
        };
        let base = base.map(|b| self.text_of(b)).transpose()?;
        let (left, right) = (self.text_of(left)?, self.text_of(right)?);
        let outcome = merge_text(base.as_deref(), &left, &right);
        let paths: Vec<String> = answers.iter().map(|(t, _)| t.path.clone()).collect();
        let bytes = fill_regions(&outcome, &paths, |i| answers[i].1)?;
        Ok(ObjectBody::Text {
            content: self.repo.put_content(&bytes)?,
        })
    }

    fn record_body(
        &mut self,
        schemas: &SchemaRegistry,
        conflict: &Conflict,
        base: Option<&ObjectState>,
        sides: &[ObjectState],
        answers: &[(&ConflictTerm, &VcsTermPick)],
    ) -> ApiResult<ObjectBody> {
        let schema_id = sides
            .iter()
            .find_map(|s| match &s.body {
                ObjectBody::Record { schema, .. } => Some(schema.clone()),
                _ => None,
            })
            .ok_or_else(|| invalid("record conflict without a record side"))?;
        if answers.is_empty() {
            let content = conflict
                .merged
                .ok_or_else(|| invalid("record conflict without a merged body"))?;
            return Ok(ObjectBody::Record {
                content,
                schema: schema_id,
            });
        }
        let schema = schemas
            .get(&schema_id)
            .ok_or_else(|| StoreError::UnknownSchema(schema_id.clone()))?;
        let [left, right] = sides else {
            return Err(invalid("a record conflict has two sides"));
        };
        let base_rec = base.map(|b| self.record_of(b)).transpose()?;
        let (lr, rr) = (self.record_of(left)?, self.record_of(right)?);
        let (mut out, rterms) = match merge_record(schema, base_rec.as_ref(), &lr, &rr) {
            RecordMergeOutcome::Conflicted { merged, terms } => (merged, terms),
            RecordMergeOutcome::Merged(_) => (RecordValue::Null, Vec::new()),
        };
        let same = rterms.len() == answers.len()
            && rterms
                .iter()
                .zip(answers)
                .all(|(r, (t, _))| r.path.to_string() == t.path && r.kind == t.kind);
        if !same {
            return Err(invalid(format!(
                "merging the sides again under schema {schema_id} no longer gives this \
                 conflict's terms; resolve it with a whole record"
            )));
        }

        // Text regions are placed per field, after the other answers.
        let mut regions: BTreeMap<FieldPath, Vec<(String, &VcsTermPick)>> = BTreeMap::new();
        for (r, (_, pick)) in rterms.iter().zip(answers) {
            if r.kind == TermKind::TextRegion {
                let field = r.path.parent().expect("a region is under its field");
                let last = r.path.0.last().cloned().unwrap_or_default();
                regions.entry(field).or_default().push((last, pick));
                continue;
            }
            let value = match pick {
                VcsTermPick::Side { index } => [&r.left, &r.right]
                    .get(*index as usize)
                    .map(|v| (*v).clone())
                    .ok_or_else(|| no_side(*index))?,
                VcsTermPick::Value { value_json } => {
                    let json: serde_json::Value = serde_json::from_str(value_json)
                        .map_err(|e| invalid(format!("term {:?}: {e}", r.path.to_string())))?;
                    Some(record_from_json_value(&json)?)
                }
                VcsTermPick::Absent => None,
                VcsTermPick::Both => unreachable!("checked by the caller"),
            };
            set_path(&mut out, &r.path, value);
        }
        for (field, picks) in regions {
            fn text<'v>(r: &'v RecordValue, field: &FieldPath) -> Option<&'v str> {
                r.at(field).and_then(RecordValue::as_text)
            }
            let (Some(l), Some(r)) = (text(&lr, &field), text(&rr, &field)) else {
                return Err(invalid(format!("field {field} is not text on both sides")));
            };
            let b = base_rec.as_ref().and_then(|b| text(b, &field));
            let outcome = merge_text(b.map(str::as_bytes), l.as_bytes(), r.as_bytes());
            let paths: Vec<String> = picks.iter().map(|(p, _)| p.clone()).collect();
            let bytes = fill_regions(&outcome, &paths, |i| picks[i].1)?;
            let merged = String::from_utf8(bytes)
                .map_err(|_| invalid(format!("field {field} is not valid UTF-8 text")))?;
            set_path(&mut out, &field, Some(RecordValue::Text(merged)));
        }
        out.validate().map_err(|e| invalid(e.to_string()))?;
        Ok(ObjectBody::Record {
            content: self
                .repo
                .put_content(&out.encode().map_err(|e| invalid(e.to_string()))?)?,
            schema: schema_id,
        })
    }

    /// A conflict over the whole object (different types, a deletion, a
    /// binary, an ambiguous base), or one whose body changed on one side
    /// only while the metadata conflicts.
    fn object_body(
        &mut self,
        base: Option<&ObjectState>,
        sides: &[ObjectState],
        answers: &[(&ConflictTerm, &VcsTermPick)],
    ) -> ApiResult<ObjectBody> {
        let [left, right] = sides else {
            return Err(invalid("an object conflict has two sides"));
        };
        let pick = match answers {
            [] => {
                // Only the metadata conflicts: the body is the side that
                // changed it.
                let unchanged = base.is_some_and(|b| b.body == left.body);
                return Ok(if unchanged {
                    right.body.clone()
                } else {
                    left.body.clone()
                });
            }
            [(_, pick)] => *pick,
            _ => return Err(invalid("an object conflict has one body term")),
        };
        Ok(match pick {
            VcsTermPick::Side { index } => sides
                .get(*index as usize)
                .ok_or_else(|| no_side(*index))?
                .body
                .clone(),
            VcsTermPick::Absent => ObjectBody::Deleted,
            VcsTermPick::Value { value_json } => {
                let json: serde_json::Value =
                    serde_json::from_str(value_json).map_err(|e| invalid(e.to_string()))?;
                let schema = sides.iter().find_map(|s| match &s.body {
                    ObjectBody::Record { schema, .. } => Some(schema.clone()),
                    _ => None,
                });
                match schema {
                    Some(schema) => {
                        let record = record_from_json_value(&json)?;
                        record.validate().map_err(|e| invalid(e.to_string()))?;
                        ObjectBody::Record {
                            content: self.repo.put_content(
                                &record.encode().map_err(|e| invalid(e.to_string()))?,
                            )?,
                            schema,
                        }
                    }
                    None => {
                        let serde_json::Value::String(text) = json else {
                            return Err(invalid("a text object takes a JSON string"));
                        };
                        ObjectBody::Text {
                            content: self.repo.put_content(text.as_bytes())?,
                        }
                    }
                }
            }
            VcsTermPick::Both => unreachable!("checked by the caller"),
        })
    }
}

/// The text of a merge outcome with each region answered; `paths[i]` is the
/// term (`#<index>`) `pick(i)` answers, in region order.
fn fill_regions<'p>(
    outcome: &TextMergeOutcome,
    paths: &[String],
    pick: impl Fn(usize) -> &'p VcsTermPick,
) -> ApiResult<Vec<u8>> {
    let TextMergeOutcome::Conflicted { merged, regions } = outcome else {
        return Err(invalid(
            "the sides merge cleanly now; this conflict's regions are gone",
        ));
    };
    let same = regions.len() == paths.len()
        && regions
            .iter()
            .zip(paths)
            .all(|(r, p)| *p == format!("#{}", r.index));
    if !same {
        return Err(invalid(
            "merging the sides again no longer gives this conflict's regions",
        ));
    }
    let mut error = None;
    let mut next = 0;
    let bytes = assemble(merged, regions, |region: &ConflictRegion| {
        let answer = pick(next);
        next += 1;
        match answer {
            VcsTermPick::Side { index: 0 } => region.left.clone(),
            VcsTermPick::Side { index: 1 } => region.right.clone(),
            VcsTermPick::Side { index } => {
                error.get_or_insert_with(|| no_side(*index));
                Vec::new()
            }
            VcsTermPick::Both => [region.left.as_slice(), &region.right].concat(),
            VcsTermPick::Absent => Vec::new(),
            VcsTermPick::Value { value_json } => {
                match serde_json::from_str::<serde_json::Value>(value_json) {
                    Ok(serde_json::Value::String(s)) => s.into_bytes(),
                    _ => {
                        error.get_or_insert_with(|| {
                            invalid(format!("region #{} takes a JSON string", region.index))
                        });
                        Vec::new()
                    }
                }
            }
        }
    });
    match error {
        Some(e) => Err(e),
        None => Ok(bytes),
    }
}

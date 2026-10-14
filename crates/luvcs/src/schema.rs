//! Record schemas: an Eure Schema document, kept in the repo as a record so
//! that it travels with the history it governs (its content ID is the schema
//! ID), plus the merge annotations the record driver reads from it.
//!
//! The schema's types decide how a record merges ([`crate::merge::record`]):
//! maps, records and tuples merge per key, sets per element, unions per
//! variant, and everything else as a whole. Two extensions on a schema node
//! refine that:
//!
//! - `$merge = "text"`: text merged by the line driver; `$merge = "atomic"`:
//!   compared as a whole, never merged inside.
//! - `$merge-key = "<field>"` on an array: its elements are maps identified
//!   by that field; they merge per element and their order merges too.
//!   Without it an array is atomic.
//!
//! A schema with `$import` is refused: everything it needs must be in the
//! one document.

use std::collections::{BTreeSet, HashMap};

use eure_document::document::EureDocument;
use eure_schema::{
    SchemaDocument, SchemaNodeContent, SchemaNodeId, TypeReference, UnknownFieldsPolicy,
};

use crate::record::{Data, FieldPath, ObjectKey, RecordError, RecordValue, Segment, element_key};

pub const MERGE_EXT: &str = "merge";
pub const MERGE_KEY_EXT: &str = "merge-key";

/// How the record driver merges the values one schema node describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergePolicy {
    /// By the value's own shape: maps and tuples per key, sets per element,
    /// anything else as a whole.
    Structural,
    /// Text merged by the line driver.
    Text,
    /// Compared as a whole.
    Atomic,
    /// An array of maps identified by this field.
    Keyed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error("{0}")]
    Record(#[from] RecordError),
    #[error("schema: {0}")]
    Invalid(String),
}

/// A compiled record schema.
#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub document: SchemaDocument,
    policies: HashMap<SchemaNodeId, MergePolicy>,
}

/// One way a record breaks its schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The innermost value a merge can take from one side: a map key, an
    /// extension, a tuple element or an array element by its merge key,
    /// never inside an array without one.
    pub path: FieldPath,
    pub message: String,
}

impl RecordSchema {
    /// Compiles a schema kept as a record.
    pub fn from_value(value: &RecordValue) -> Result<Self, SchemaError> {
        let doc = value.to_document();
        Self::from_document(&doc)
    }

    /// Parses Eure Schema source into the record it is kept as, and the
    /// compiled schema.
    pub fn from_eure(source: &str) -> Result<(RecordValue, Self), SchemaError> {
        let value = RecordValue::from_eure(source)?;
        let schema = Self::from_value(&value)?;
        Ok((value, schema))
    }

    fn from_document(doc: &EureDocument) -> Result<Self, SchemaError> {
        let (document, sources) = eure_schema::convert::document_to_schema(doc)
            .map_err(|e| SchemaError::Invalid(e.to_string()))?;
        let mut policies = HashMap::new();
        for (id, source) in &sources {
            let node = doc.node(source.node_id);
            let ext = |name: &str| -> Result<Option<String>, SchemaError> {
                let Some((_, ext_id)) = node.extensions.iter().find(|(k, _)| k.as_ref() == name)
                else {
                    return Ok(None);
                };
                RecordValue::from_document(&doc.node_subtree_to_document(*ext_id))?
                    .as_text()
                    .map(|t| Some(t.content.clone()))
                    .ok_or_else(|| SchemaError::Invalid(format!("${name} takes text")))
            };
            let merge = ext(MERGE_EXT)?;
            let key = ext(MERGE_KEY_EXT)?;
            let policy = match (merge.as_deref(), key) {
                (None, None) => continue,
                (Some("text"), None) => MergePolicy::Text,
                (Some("atomic"), None) => MergePolicy::Atomic,
                (Some(other), None) => {
                    return Err(SchemaError::Invalid(format!(
                        "$merge is \"text\" or \"atomic\", not {other:?}"
                    )));
                }
                (None, Some(field)) => {
                    if !matches!(document.node(*id).content, SchemaNodeContent::Array(_)) {
                        return Err(SchemaError::Invalid(format!(
                            "$merge-key {field:?} is not on an array"
                        )));
                    }
                    MergePolicy::Keyed(field)
                }
                (Some(_), Some(_)) => {
                    return Err(SchemaError::Invalid(
                        "$merge and $merge-key on the same node".into(),
                    ));
                }
            };
            policies.insert(*id, policy);
        }
        Ok(Self { document, policies })
    }

    pub fn root(&self) -> SchemaNodeId {
        self.document.root
    }

    /// The node a reference chain ends at.
    fn deref(&self, mut id: SchemaNodeId) -> Option<SchemaNodeId> {
        for _ in 0..=self.document.nodes.len() {
            match &self.document.node(id).content {
                SchemaNodeContent::Reference(r) => {
                    id = match r {
                        TypeReference::Resolved(to) => *to,
                        named => self.document.resolve_reference(named)?,
                    };
                }
                _ => return Some(id),
            }
        }
        None
    }

    /// The node that describes `value`: references followed, and a union
    /// narrowed to the variant `value`'s `$variant` names. A union value
    /// without `$variant` has no single node.
    pub fn resolve(&self, id: SchemaNodeId, value: &RecordValue) -> Option<SchemaNodeId> {
        let id = self.deref(id)?;
        match &self.document.node(id).content {
            SchemaNodeContent::Union(u) => {
                let variant = u.variants.get(value.variant()?)?;
                self.resolve(*variant, value)
            }
            _ => Some(id),
        }
    }

    /// Whether `id` describes a union.
    pub fn is_union(&self, id: SchemaNodeId) -> bool {
        self.deref(id)
            .is_some_and(|id| matches!(self.document.node(id).content, SchemaNodeContent::Union(_)))
    }

    /// The merge policy of `id`: the first annotation along its reference
    /// chain, else [`MergePolicy::Structural`].
    pub fn policy(&self, mut id: SchemaNodeId) -> MergePolicy {
        for _ in 0..=self.document.nodes.len() {
            if let Some(p) = self.policies.get(&id) {
                return p.clone();
            }
            match &self.document.node(id).content {
                SchemaNodeContent::Reference(TypeReference::Resolved(to)) => id = *to,
                SchemaNodeContent::Reference(named) => match self.document.resolve_reference(named)
                {
                    Some(to) => id = to,
                    None => break,
                },
                _ => break,
            }
        }
        MergePolicy::Structural
    }

    /// The node for `seg` under the node `id` that describes `parent`.
    pub fn child(
        &self,
        id: SchemaNodeId,
        parent: &RecordValue,
        seg: &Segment,
    ) -> Option<SchemaNodeId> {
        if let Segment::Ext(name) = seg {
            let id = self.deref(id)?;
            return self
                .document
                .node(id)
                .ext_types
                .iter()
                .find(|(k, _)| k.as_ref() == name)
                .map(|(_, e)| e.schema);
        }
        let id = self.resolve(id, parent)?;
        match (&self.document.node(id).content, seg) {
            (SchemaNodeContent::Record(r), Segment::Key(ObjectKey::String(name))) => {
                self.record_field(r, name)
            }
            (SchemaNodeContent::Map(m), Segment::Key(_)) => Some(m.value),
            (SchemaNodeContent::Tuple(t), Segment::Index(i)) => t.elements.get(*i).copied(),
            (SchemaNodeContent::Array(a), Segment::Item { .. } | Segment::Position(_)) => {
                Some(a.item)
            }
            _ => None,
        }
    }

    fn record_field(&self, r: &eure_schema::RecordSchema, name: &str) -> Option<SchemaNodeId> {
        if let Some(f) = r.properties.get(name) {
            return Some(f.schema);
        }
        for flat in &r.flatten {
            if let Some(id) = self.deref(*flat)
                && let SchemaNodeContent::Record(inner) = &self.document.node(id).content
                && let Some(found) = self.record_field(inner, name)
            {
                return Some(found);
            }
        }
        match r.unknown_fields {
            UnknownFieldsPolicy::Schema(id) => Some(id),
            _ => None,
        }
    }

    /// Everything that keeps `value` from being saved under this schema: the
    /// schema's own constraints, and every array with a `$merge-key` whose
    /// elements do not each have a distinct key.
    pub fn violations(&self, value: &RecordValue) -> Vec<Violation> {
        let (doc, paths) = value.to_document_traced();
        let paths: HashMap<_, _> = paths.into_iter().collect();
        let output = eure_schema::validate::validate(&doc, &self.document);
        let mut out: Vec<Violation> = Vec::new();
        let mut seen = BTreeSet::new();
        for error in &output.errors {
            let (node, _) = error.node_ids();
            let path = paths
                .get(&node)
                .map(|p| self.merge_path(value, p))
                .unwrap_or_default();
            let message = error.to_string();
            if seen.insert((path.clone(), message.clone())) {
                out.push(Violation { path, message });
            }
        }
        self.keyed_arrays(Some(self.root()), value, &FieldPath::root(), &mut out);
        out
    }

    fn keyed_arrays(
        &self,
        id: Option<SchemaNodeId>,
        value: &RecordValue,
        path: &FieldPath,
        out: &mut Vec<Violation>,
    ) {
        let Some(id) = id else { return };
        for (name, v) in &value.ext {
            let seg = Segment::Ext(name.clone());
            self.keyed_arrays(self.child(id, value, &seg), v, &path.child(seg), out);
        }
        match &value.data {
            Data::Map(m) => {
                for (k, v) in m {
                    let seg = Segment::Key(k.clone());
                    self.keyed_arrays(self.child(id, value, &seg), v, &path.child(seg), out);
                }
            }
            Data::Tuple(items) => {
                for (i, v) in items.iter().enumerate() {
                    let seg = Segment::Index(i);
                    self.keyed_arrays(self.child(id, value, &seg), v, &path.child(seg), out);
                }
            }
            Data::Array(items) => {
                let MergePolicy::Keyed(field) = self.policy(id) else {
                    return;
                };
                let mut keys = BTreeSet::new();
                for (i, item) in items.iter().enumerate() {
                    match element_key(item, &field) {
                        Some(key) if keys.insert(key.clone()) => {
                            let seg = Segment::Item {
                                field: field.clone(),
                                key,
                            };
                            self.keyed_arrays(
                                self.child(id, value, &seg),
                                item,
                                &path.child(seg),
                                out,
                            );
                        }
                        Some(key) => out.push(Violation {
                            path: path.clone(),
                            message: format!("two elements have {field} = {key}"),
                        }),
                        None => out.push(Violation {
                            path: path.child(Segment::Position(i)),
                            message: format!("element {i} has no {field} to identify it"),
                        }),
                    }
                }
            }
            _ => {}
        }
    }

    /// A structural path (array elements by position) as a merge path: an
    /// element of a keyed array by its key, and a path into an array
    /// without one cut at the array.
    fn merge_path(&self, value: &RecordValue, structural: &FieldPath) -> FieldPath {
        let mut out = FieldPath::root();
        let mut cur = value;
        let mut id = Some(self.root());
        for seg in &structural.0 {
            let seg = match seg {
                Segment::Position(i) => {
                    let keyed = id.map(|id| self.policy(id));
                    let item = cur.as_array().and_then(|a| a.get(*i));
                    match (keyed, item) {
                        (Some(MergePolicy::Keyed(field)), Some(item)) => {
                            match element_key(item, &field) {
                                Some(key) => Segment::Item { field, key },
                                None => break,
                            }
                        }
                        _ => break,
                    }
                }
                other => other.clone(),
            };
            let Some(next) = cur.child(&seg) else { break };
            id = id.and_then(|id| self.child(id, cur, &seg));
            out = out.child(seg);
            cur = next;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = r#"
title = `text`
body = `text`
body.$merge = "text"
meta = `any`
meta.$merge = "atomic"
links {
  $variant: array
  item {
    id = `text`
    url = `text`
  }
  $merge-key = "id"
  max-length = 3
}
tags {
  $variant: set
  item = `text`
}
"#;

    #[test]
    fn annotations_become_policies() {
        let (_, schema) = RecordSchema::from_eure(NOTE).unwrap();
        let rec = RecordValue::from_eure("title = \"t\"").unwrap();
        let child = |name: &str| {
            schema
                .child(schema.root(), &rec, &Segment::Key(name.into()))
                .unwrap()
        };
        assert_eq!(schema.policy(child("body")), MergePolicy::Text);
        assert_eq!(schema.policy(child("meta")), MergePolicy::Atomic);
        assert_eq!(
            schema.policy(child("links")),
            MergePolicy::Keyed("id".into())
        );
        assert_eq!(schema.policy(child("title")), MergePolicy::Structural);
        assert_eq!(schema.policy(child("tags")), MergePolicy::Structural);
    }

    #[test]
    fn bad_annotations_are_refused() {
        assert!(RecordSchema::from_eure("a = `text`\na.$merge = \"lines\"").is_err());
        assert!(RecordSchema::from_eure("a = `text`\na.$merge-key = \"id\"").is_err());
    }

    #[test]
    fn violations_point_at_merge_paths() {
        let (_, schema) = RecordSchema::from_eure(NOTE).unwrap();
        let ok = RecordValue::from_eure(
            "title = \"t\"\nbody = \"b\"\nmeta = 1\ntags = #{\"a\"}\nlinks[] {\n  id = \"a\"\n  url = \"u\"\n}\n",
        )
        .unwrap();
        assert_eq!(schema.violations(&ok), vec![]);

        let bad = RecordValue::from_eure(
            "title = \"t\"\nbody = \"b\"\nmeta = 1\ntags = #{\"a\"}\nlinks[] {\n  id = \"a\"\n  url = 1\n}\n",
        )
        .unwrap();
        let v = schema.violations(&bad);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].path.to_string(), "links[id=\"a\"].url");

        let dup = RecordValue::from_eure(
            "title = \"t\"\nbody = \"b\"\nmeta = 1\ntags = #{}\nlinks[] {\n  id = \"a\"\n  url = \"u\"\n}\nlinks[] {\n  id = \"a\"\n  url = \"v\"\n}\n",
        )
        .unwrap();
        let v = schema.violations(&dup);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].path.to_string(), "links");
    }
}

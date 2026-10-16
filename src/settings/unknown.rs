//! Tolerance for settings written by a newer build.
//!
//! A newer build can add an enum variant (a provider driver, a widget kind)
//! that this build cannot decode. Rejecting the whole file would reset every
//! setting, so decoding drops only the values that fail and keeps them here.
//! Saving writes them back, so the newer build still finds its entries after
//! an older build ran in between.

use toml::{Value, map::Map};

use super::Settings;

/// Values that this build could not decode, in the order they were dropped.
///
/// They are not user-editable state, so they never make two snapshots unequal.
#[derive(Clone, Debug, Default)]
pub struct UnknownEntries(Vec<Entry>);

#[derive(Clone, Debug)]
struct Entry {
    /// Keys from the document root to the field, or to the array holding the
    /// dropped element.
    path: Vec<String>,
    kind: EntryKind,
    value: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    /// One element removed from the array at `path`.
    Element,
    /// The field at `path`.
    Field,
}

impl PartialEq for UnknownEntries {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for UnknownEntries {}

impl UnknownEntries {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Human-readable paths of the dropped values, for diagnostics.
    pub fn describe(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|entry| match entry.kind {
                EntryKind::Element => format!("{}[]", entry.path.join(".")),
                EntryKind::Field => entry.path.join("."),
            })
            .collect()
    }

    /// Writes the dropped values back into a serialized `settings` document.
    ///
    /// Array elements are appended. A field is restored only where this build
    /// did not write one: a value it does write is the user's current choice
    /// and must win over an undecodable one.
    pub fn restore_into(&self, document: &mut Value) {
        for entry in &self.0 {
            let Some((last, parents)) = entry.path.split_last() else {
                continue;
            };
            let Some(parent) = table_at_mut(document, parents) else {
                continue;
            };
            match entry.kind {
                EntryKind::Element => {
                    let slot = parent
                        .entry(last.clone())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Value::Array(items) = slot {
                        items.push(entry.value.clone());
                    }
                }
                EntryKind::Field => {
                    parent
                        .entry(last.clone())
                        .or_insert_with(|| entry.value.clone());
                }
            }
        }
    }
}

/// Removes the values `document` cannot decode, keeping everything else.
///
/// Each root key is decoded on its own against defaults. A failing array
/// loses only its failing elements; a failing table recurses into the field
/// responsible; anything else is dropped whole and decodes as its default.
pub(super) fn prune(document: &mut Value) -> UnknownEntries {
    let mut dropped = Vec::new();
    let Some(root) = document.as_table_mut() else {
        return UnknownEntries(dropped);
    };
    let version = root.get("version").cloned();
    let keys = root.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let mut probe = Map::new();
        if let Some(version) = &version {
            probe.insert("version".into(), version.clone());
        }
        probe.insert(key.clone(), root[&key].clone());
        let mut probe = Value::Table(probe);
        if decodes(&probe) {
            continue;
        }
        let path = vec![key.clone()];
        if prune_at(&mut probe, &path, &mut dropped) {
            root.insert(key.clone(), probe[&key].clone());
        } else if let Some(value) = root.remove(&key) {
            dropped.push(Entry {
                path,
                kind: EntryKind::Field,
                value,
            });
        }
    }
    UnknownEntries(dropped)
}

fn decodes(document: &Value) -> bool {
    document.clone().try_into::<Settings>().is_ok()
}

/// Makes `document` decode by changing only the value at `path`. On failure
/// the value and `dropped` are left exactly as they were.
fn prune_at(document: &mut Value, path: &[String], dropped: &mut Vec<Entry>) -> bool {
    let mark = dropped.len();
    let Some(original) = value_at_mut(document, path).cloned() else {
        return false;
    };
    let fixed = match &original {
        // Elements are independent records such as provider instances.
        // Never repair one field by field: a provider instance without its
        // unknown `driver` would decode as a Codex instance.
        Value::Array(items) => {
            set(document, path, Value::Array(Vec::new()));
            for item in items.iter().cloned() {
                if let Some(Value::Array(kept)) = value_at_mut(document, path) {
                    kept.push(item);
                }
                if decodes(document) {
                    continue;
                }
                if let Some(Value::Array(kept)) = value_at_mut(document, path)
                    && let Some(value) = kept.pop()
                {
                    dropped.push(Entry {
                        path: path.to_vec(),
                        kind: EntryKind::Element,
                        value,
                    });
                }
            }
            decodes(document)
        }
        Value::Table(table) => prune_table(document, path, table, dropped),
        _ => false,
    };
    if !fixed {
        dropped.truncate(mark);
        set(document, path, original);
    }
    fixed
}

/// Finds the one field whose removal lets the table decode, then repairs
/// inside it when possible. Several broken fields in one table fail here and
/// the caller drops the table.
fn prune_table(
    document: &mut Value,
    path: &[String],
    table: &Map<String, Value>,
    dropped: &mut Vec<Entry>,
) -> bool {
    for key in table.keys() {
        let mut without = table.clone();
        let value = without.remove(key).expect("key comes from the table");
        set(document, path, Value::Table(without));
        if !decodes(document) {
            set(document, path, Value::Table(table.clone()));
            continue;
        }
        let mut child = path.to_vec();
        child.push(key.clone());
        set(document, path, Value::Table(table.clone()));
        if prune_at(document, &child, dropped) {
            return true;
        }
        if let Some(Value::Table(current)) = value_at_mut(document, path) {
            current.remove(key);
        }
        dropped.push(Entry {
            path: child,
            kind: EntryKind::Field,
            value,
        });
        return true;
    }
    set(document, path, Value::Table(table.clone()));
    false
}

fn value_at_mut<'a>(document: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    path.iter()
        .try_fold(document, |value, key| value.as_table_mut()?.get_mut(key))
}

fn table_at_mut<'a>(
    document: &'a mut Value,
    path: &[String],
) -> Option<&'a mut Map<String, Value>> {
    let mut table = document.as_table_mut()?;
    for key in path {
        table = table
            .entry(key.clone())
            .or_insert_with(|| Value::Table(Map::new()))
            .as_table_mut()?;
    }
    Some(table)
}

fn set(document: &mut Value, path: &[String], value: Value) {
    if let Some(slot) = value_at_mut(document, path) {
        *slot = value;
    }
}

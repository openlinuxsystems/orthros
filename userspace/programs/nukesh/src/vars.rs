// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Variables: strings, indexed arrays and associative arrays, with their
//! attributes, in scopes.
//!
//! Scoping is dynamic, as in bash and ksh's POSIX functions: the global
//! scope is at the bottom; each function call pushes a scope for its
//! `local` variables, and `NAME=value command` pushes a scope for the
//! command's own assignments. A name is looked up from the top down, and
//! assigning to a name nobody declared makes it global.

use std::collections::{BTreeMap, HashMap};

use indexmap::IndexMap;

/// Attributes, as `declare` sets them.
pub mod attr {
    pub const EXPORT: u16 = 1 << 0;
    pub const READONLY: u16 = 1 << 1;
    /// `-i`: assignments are evaluated as arithmetic.
    pub const INTEGER: u16 = 1 << 2;
    /// `-l` and `-u`: assignments are made lower or upper case.
    pub const LOWER: u16 = 1 << 3;
    pub const UPPER: u16 = 1 << 4;
    /// `-n`: the value is the name of another variable, which every use
    /// goes to instead.
    pub const NAMEREF: u16 = 1 << 5;
    /// `-a` and `-A`, for a variable declared without a value yet.
    pub const ARRAY: u16 = 1 << 6;
    pub const ASSOC: u16 = 1 << 7;
    /// `-t`, kept for `declare -p`.
    pub const TRACE: u16 = 1 << 8;
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    /// Indexed arrays are sparse.
    Array(BTreeMap<i64, String>),
    /// Associative arrays list their keys in bash's order.
    Assoc(Assoc),
}

/// An associative array, whose keys come in the order bash's hash table
/// gives them, so that a script lists them as it does in bash: the table
/// starts with 1024 buckets and grows four times over when a new key
/// finds twice as many keys as buckets; a key goes in at the head of its
/// bucket, by the FNV-1 hash of its bytes (as signed chars, as on
/// x86-64), and a growth re-links each bucket's keys from its head, which
/// reverses them. The keys are kept in the order they went in, each with
/// the number of growths before it, and the table is replayed to list
/// them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Assoc {
    entries: IndexMap<String, (String, u32)>,
    growths: u32,
}

/// bash's ASSOC_HASH_BUCKETS.
const ASSOC_BUCKETS: usize = 1024;

impl Assoc {
    pub fn new() -> Assoc {
        Assoc::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.entries.get(key).map(|(value, _)| value)
    }

    /// Sets `key`, which keeps its place if it was there.
    pub fn insert(&mut self, key: String, value: String) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.0 = value;
            return;
        }
        if self.entries.len() >= self.buckets(self.growths) * 2 {
            self.growths += 1;
        }
        self.entries.insert(key, (value, self.growths));
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.shift_remove(key);
    }

    /// Takes every key away; the table keeps its size, as bash's does.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn buckets(&self, growths: u32) -> usize {
        ASSOC_BUCKETS << (2 * growths)
    }

    /// The keys and their values, in bash's order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.order().into_iter().map(|i| {
            let (key, (value, _)) = self.entries.get_index(i).expect("an index from order");
            (key, value)
        })
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.iter().map(|(key, _)| key)
    }

    pub fn values(&self) -> impl Iterator<Item = &String> {
        self.iter().map(|(_, value)| value)
    }

    /// The entries' indexes as bash's table lists them: each bucket's
    /// list is kept tail first, so that going in at the head is a push.
    fn order(&self) -> Vec<usize> {
        let hashes: Vec<u32> = self.entries.keys().map(|key| hash(key)).collect();
        let mut growths = 0;
        let mut table: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for (i, (_, (_, grown))) in self.entries.iter().enumerate() {
            while growths < *grown {
                growths += 1;
                let mask = (self.buckets(growths) - 1) as u32;
                let mut grown_table: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
                for list in table.values() {
                    for &j in list.iter().rev() {
                        grown_table.entry(hashes[j] & mask).or_default().push(j);
                    }
                }
                table = grown_table;
            }
            let mask = (self.buckets(growths) - 1) as u32;
            table.entry(hashes[i] & mask).or_default().push(i);
        }
        table.values().flat_map(|list| list.iter().rev().copied()).collect()
    }
}

/// bash's hash_string: FNV-1, with the bytes taken as signed chars.
fn hash(key: &str) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for byte in crate::text::encode(key) {
        hash = hash.wrapping_mul(16_777_619);
        hash ^= i32::from(byte as i8) as u32;
    }
    hash
}

impl<'a> IntoIterator for &'a Assoc {
    type Item = (&'a String, &'a String);
    type IntoIter = std::vec::IntoIter<(&'a String, &'a String)>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter().collect::<Vec<_>>().into_iter()
    }
}

impl FromIterator<(String, String)> for Assoc {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(items: I) -> Assoc {
        let mut assoc = Assoc::new();
        for (key, value) in items {
            assoc.insert(key, value);
        }
        assoc
    }
}

impl Value {
    /// The value as a string: an array's element 0 (or, for an
    /// associative array, key `0`), as `$name` reads it.
    pub fn scalar(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Array(map) => map.get(&0).map(String::as_str),
            Value::Assoc(map) => map.get("0").map(String::as_str),
        }
    }

    /// The elements in order: one for a string.
    pub fn elements(&self) -> Vec<String> {
        match self {
            Value::Str(s) => vec![s.clone()],
            Value::Array(map) => map.values().cloned().collect(),
            Value::Assoc(map) => map.values().cloned().collect(),
        }
    }

    /// The subscripts in order: `0` for a string.
    pub fn keys(&self) -> Vec<String> {
        match self {
            Value::Str(_) => vec!["0".into()],
            Value::Array(map) => map.keys().map(|k| k.to_string()).collect(),
            Value::Assoc(map) => map.keys().cloned().collect(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Value::Str(_) => 1,
            Value::Array(map) => map.len(),
            Value::Assoc(map) => map.len(),
        }
    }
}

/// The element that reading `name[index]` names in an indexed array, a
/// string (element 0) or an unset variable: a negative index counts back
/// from an array's end. `None` when it points before the start, which
/// bash calls a bad array subscript; only an array has an end to count
/// back from, as in bash, where an assignment makes a string an array
/// first.
pub fn position(value: Option<&Value>, index: i64) -> Option<i64> {
    match value {
        Some(Value::Array(map)) => array_position(map, index),
        _ => (index >= 0).then_some(index),
    }
}

/// [`position`] in an indexed array.
pub fn array_position(map: &BTreeMap<i64, String>, index: i64) -> Option<i64> {
    if index >= 0 {
        return Some(index);
    }
    map.keys().next_back().map(|last| last + (index + 1)).filter(|at| *at >= 0)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Var {
    /// `None` for a variable that is declared (`local x`, `declare -i x`)
    /// but has no value: it is unset, but keeps its attributes and scope.
    pub value: Option<Value>,
    pub attrs: u16,
}

impl Var {
    pub fn has(&self, attr: u16) -> bool {
        self.attrs & attr != 0
    }
}

#[derive(Debug, Default)]
struct Scope {
    vars: HashMap<String, Var>,
    /// A scope of `NAME=value` assignments before a command, rather than
    /// a function's.
    temp: bool,
}

#[derive(Debug)]
pub struct Vars {
    scopes: Vec<Scope>,
}

/// The scopes [`Vars::hide_locals`] took away.
#[must_use]
pub struct Hidden(Vec<Scope>);

impl Default for Vars {
    fn default() -> Vars {
        Vars { scopes: vec![Scope::default()] }
    }
}

impl Vars {
    /// A function's scope, for its `local` variables.
    pub fn push_function(&mut self) {
        self.scopes.push(Scope::default());
    }

    /// The scope of a command's own assignments.
    pub fn push_temp(&mut self) {
        self.scopes.push(Scope { temp: true, ..Scope::default() });
    }

    pub fn pop(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    /// How many function scopes are open: 0 outside functions.
    pub fn function_depth(&self) -> usize {
        self.scopes.iter().filter(|s| !s.temp).count() - 1
    }

    pub fn get(&self, name: &str) -> Option<&Var> {
        self.scopes.iter().rev().find_map(|scope| scope.vars.get(name))
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Var> {
        self.scopes.iter_mut().rev().find_map(|scope| scope.vars.get_mut(name))
    }

    /// The variable to assign `name` to: the nearest one by that name, or
    /// a new global one.
    pub fn entry(&mut self, name: &str) -> &mut Var {
        let at = self.scopes.iter().rposition(|scope| scope.vars.contains_key(name)).unwrap_or(0);
        self.scopes[at].vars.entry(name.to_string()).or_default()
    }

    /// The global variable `name`, made if need be (`declare -g`).
    pub fn global(&mut self, name: &str) -> &mut Var {
        self.scopes[0].vars.entry(name.to_string()).or_default()
    }

    /// Makes `name` local to the innermost function, starting unset (or,
    /// with `inherit`, with the value it has outside), and returns it. It
    /// is exported if the variable it hides is, as in bash.
    pub fn local(&mut self, name: &str, inherit: bool) -> &mut Var {
        let at = self.scopes.iter().rposition(|scope| !scope.temp).unwrap_or(0);
        let outer = self.get(name).cloned();
        let scope = &mut self.scopes[at];
        scope.vars.entry(name.to_string()).or_insert_with(|| match outer {
            Some(var) if inherit => Var { attrs: var.attrs & !attr::NAMEREF, ..var },
            Some(var) => Var { value: None, attrs: var.attrs & attr::EXPORT },
            None => Var::default(),
        })
    }

    /// Takes away every scope but the global one, for `declare -g`, until
    /// [`Vars::reveal`] puts them back.
    pub fn hide_locals(&mut self) -> Hidden {
        Hidden(self.scopes.split_off(1))
    }

    pub fn reveal(&mut self, hidden: Hidden) {
        self.scopes.extend(hidden.0);
    }

    /// Whether `name` is local to the innermost function.
    pub fn is_local(&self, name: &str) -> bool {
        let at = self.scopes.iter().rposition(|scope| !scope.temp).unwrap_or(0);
        at > 0 && self.scopes[at].vars.contains_key(name)
    }

    /// Puts `name` in the innermost scope, for a command's assignments.
    pub fn set_temp(&mut self, name: &str, var: Var) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.vars.insert(name.to_string(), var);
        }
    }

    /// Removes the nearest variable `name`; false if it is read-only.
    pub fn unset(&mut self, name: &str) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(var) = scope.vars.get(name) {
                if var.has(attr::READONLY) {
                    return false;
                }
                scope.vars.remove(name);
                return true;
            }
        }
        true
    }

    /// Every visible variable, by name.
    pub fn all(&self) -> BTreeMap<&str, &Var> {
        let mut all = BTreeMap::new();
        for scope in &self.scopes {
            for (name, var) in &scope.vars {
                all.insert(name.as_str(), var);
            }
        }
        all
    }

    /// The environment for a program: every exported variable with a
    /// value, as `NAME=value`.
    pub fn environment(&self) -> Vec<String> {
        // For each name, the nearest variable that is set and exported, as
        // bash takes it: a local with no value, or one not exported, leaves
        // the variable it hides in the environment.
        let mut exported = BTreeMap::new();
        for scope in &self.scopes {
            for (name, var) in &scope.vars {
                if var.has(attr::EXPORT) && var.value.is_some() {
                    exported.insert(name.as_str(), var);
                }
            }
        }
        exported
            .into_iter()
            .filter_map(|(name, var)| match &var.value {
                Some(Value::Str(s)) => Some(format!("{name}={s}")),
                // Arrays are not exported.
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Option<Value> {
        Some(Value::Str(text.into()))
    }

    #[test]
    fn a_local_hiding_an_exported_variable_is_exported() {
        let mut vars = Vars::default();
        let x = vars.entry("X");
        x.value = s("1");
        x.attrs = attr::EXPORT;
        vars.push_function();
        vars.local("X", false).value = s("2");
        assert!(vars.get("X").unwrap().has(attr::EXPORT));
        assert_eq!(vars.environment(), ["X=2"]);
        // One with no value, or not exported, leaves the one it hides.
        vars.push_function();
        assert!(vars.local("X", false).has(attr::EXPORT));
        assert_eq!(vars.environment(), ["X=2"]);
        *vars.local("X", false) = Var { value: s("3"), attrs: 0 };
        assert_eq!(vars.environment(), ["X=2"]);
    }

    #[test]
    fn scopes_are_dynamic() {
        let mut vars = Vars::default();
        vars.entry("x").value = s("global");
        vars.push_function();
        assert_eq!(vars.get("x").unwrap().value, s("global"));
        vars.local("x", false).value = s("local");
        assert_eq!(vars.get("x").unwrap().value, s("local"));
        // A function called from here sees the caller's local.
        vars.push_function();
        assert_eq!(vars.get("x").unwrap().value, s("local"));
        vars.entry("x").value = s("changed");
        vars.entry("y").value = s("new");
        vars.pop();
        assert_eq!(vars.get("x").unwrap().value, s("changed"));
        vars.pop();
        assert_eq!(vars.get("x").unwrap().value, s("global"));
        assert_eq!(vars.get("y").unwrap().value, s("new"));
    }

    #[test]
    fn temporary_assignments_go_away() {
        let mut vars = Vars::default();
        vars.entry("x").value = s("1");
        vars.push_temp();
        vars.set_temp("x", Var { value: s("2"), attrs: attr::EXPORT });
        assert!(vars.environment().contains(&"x=2".to_string()));
        vars.pop();
        assert_eq!(vars.get("x").unwrap().value, s("1"));
        assert!(vars.environment().is_empty());
    }

    #[test]
    fn read_only_variables_stay() {
        let mut vars = Vars::default();
        vars.entry("x").attrs |= attr::READONLY;
        assert!(!vars.unset("x"));
        assert!(vars.get("x").is_some());
        assert!(vars.unset("never_set"));
    }

    #[test]
    fn arrays_read_as_their_first_element() {
        let mut map = BTreeMap::new();
        map.insert(3, "c".to_string());
        map.insert(0, "a".to_string());
        let value = Value::Array(map);
        assert_eq!(value.scalar(), Some("a"));
        assert_eq!(value.elements(), ["a", "c"]);
        assert_eq!(value.keys(), ["0", "3"]);
    }
}

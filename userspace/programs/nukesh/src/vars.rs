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
    /// Associative arrays keep their keys in the order they were added.
    Assoc(IndexMap<String, String>),
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
    /// with `inherit`, with the value it has outside), and returns it.
    pub fn local(&mut self, name: &str, inherit: bool) -> &mut Var {
        let at = self.scopes.iter().rposition(|scope| !scope.temp).unwrap_or(0);
        let outer = if inherit { self.get(name).cloned() } else { None };
        let scope = &mut self.scopes[at];
        scope.vars.entry(name.to_string()).or_insert_with(|| match outer {
            Some(var) => Var { attrs: var.attrs & !attr::NAMEREF, ..var },
            None => Var::default(),
        })
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
        self.all()
            .into_iter()
            .filter(|(_, var)| var.has(attr::EXPORT))
            .filter_map(|(name, var)| {
                let value = match &var.value {
                    Some(Value::Str(s)) => s.as_str(),
                    // Arrays are not exported.
                    _ => return None,
                };
                Some(format!("{name}={value}"))
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

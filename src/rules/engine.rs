//-
// Copyright (c) 2016, Jason Lingle
//
// Permission to  use, copy,  modify, and/or distribute  this software  for any
// purpose  with or  without fee  is hereby  granted, provided  that the  above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE  IS PROVIDED "AS  IS" AND  THE AUTHOR DISCLAIMS  ALL WARRANTIES
// WITH  REGARD   TO  THIS  SOFTWARE   INCLUDING  ALL  IMPLIED   WARRANTIES  OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT  SHALL THE AUTHOR BE LIABLE FOR ANY
// SPECIAL,  DIRECT,   INDIRECT,  OR  CONSEQUENTIAL  DAMAGES   OR  ANY  DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF  CONTRACT, NEGLIGENCE  OR OTHER  TORTIOUS ACTION,  ARISING OUT  OF OR  IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! Implements the actual rules engine defined in `defs.rs`.
//!
//! Configuration is taken from the `rules` table in the configuration. The
//! semantics of the rules engine is described in the README and not repeated
//! here.

use std::collections::HashMap;
use std::fmt;
use std::iter;
use std::ops::Deref;
use std::result;

use quick_error::ResultExt;
use regex::{self,Regex};
use toml;

use defs::*;
use rules::defs::*;

/// A single rule condition. These correspond directly to the conditions
/// described in the project README.
#[derive(Clone,Debug)]
enum Condition {
    Name(Regex),
    Path(Regex),
    Permissions(Regex),
    Type(Regex),
    Target(Regex),
    Bigger(FileSize),
    Smaller(FileSize),
}

/// A single rule action. These are listed in the order they should be
/// evaluated. They correspond directly to the actions described in the project
/// README.
#[derive(Clone,Debug,PartialEq,Eq)]
enum Action {
    Mode(SyncMode),
    Include(Vec<usize>),
    Switch(usize),
    Stop(StopType),
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum StopType {
    Return,
    All,
}

/// The conditions and actions for a single rule.
#[derive(Clone,Debug,Default)]
struct Rule {
    conditions: Vec<Condition>,
    actions: Vec<Action>,
}

/// A full state in the rules engine.
///
/// Rules are referenced by index into a single `rules` array instead of being
/// owned by the state so that `siblings` handling can easily simply track the
/// rule indices that matched.
#[derive(Clone,Debug,Default)]
struct RuleState {
    files: Vec<usize>,
    siblings: Vec<usize>,
}

/// The full ruleset parsed from the configuration.
#[derive(Clone,Debug,Default)]
pub struct SyncRules {
    /// All states in the ruleset. Certain actions index into this array.
    states: Vec<RuleState>,
    /// All rules in the ruleset, flattened across the states. The order of
    /// this array is not meaningful; one must traverse the arrays in
    /// `RuleState` instead. `RuleState` indexes into this vec.
    rules: Vec<Rule>,
    /// The index of the `root` state.
    root_ix: usize,
}

/// The current persistent state of the rules engine, inherited by files within
/// directories, etc.
#[derive(Clone,Copy,Debug)]
struct EngineState {
    /// The sync mode in effect.
    mode: SyncMode,
    /// The index of the `RuleState` in effect.
    state: usize,
    /// If present, the new value of `state` the next time the engine descends
    /// (into siblings or files).
    switch: Option<usize>,
}

#[derive(Clone,Debug)]
pub struct ErrorLocation {
    section: String,
    ix: usize,
    field: String,
}

impl ErrorLocation {
    fn new(section: String, ix: usize, field: String) -> Self {
        ErrorLocation {
            section: section,
            ix: ix,
            field: field,
        }
    }
}

impl fmt::Display for ErrorLocation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "section {}, rule #{}, field {}",
               self.section, self.ix + 1, self.field)
    }
}

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        BadRegex(loc: ErrorLocation, err: regex::Error) {
            cause(err)
            description("Bad regex")
            display("Bad regex in {}: {}", loc, err)
            context(loc: ErrorLocation, err: regex::Error) ->
                (loc, err)
        }
        BadMode(loc: ErrorLocation, orig: String,
                err: SyncModeParseError) {
            cause(err)
            description("Invalid mode")
            display("Invalid mode '{}' in {}: {}", orig, loc, err)
            context(cxt: (ErrorLocation, &'a str),
                    err: SyncModeParseError) ->
                (cxt.0, cxt.1.to_owned(), err)
        }
        BadStateReference(loc: ErrorLocation, target: String) {
            description("Bad state reference")
            display("Bad reference to non-existent state '{}' in {}",
                    target, loc)
        }
        BadStopType(loc: ErrorLocation, what: String) {
            description("Bad 'stop' type")
            display("Bad 'stop' type '{}' in {}", loc, what)
        }
        InvalidRuleConfig(loc: ErrorLocation, field: String) {
            description("Invalid field in rule")
            display("Invalid field {} in {}", field, loc)
        }
        NoRootState {
            description("No 'root' rules state defined")
        }
        NotATable(key: String) {
            description("Non-table found where table expected")
            display("Config key '{}' should be a table, but isn't", key)
        }
        NotAnArray(key: String) {
            description("Non-array found where array expected")
            display("Config key '{}' should be a array, but isn't", key)
        }
        WrongType(loc: ErrorLocation, expected: &'static str) {
            description("Condition or action config with wrong type")
            display("Non-{0} found where {0} expected in {1}", expected, loc)
        }
        InvalidRulesGroup(state: String, group: String) {
            description("Invalid rules-group name")
            display("Invalid rules-group name in state '{}': '{}'",
                    state, group)
        }
        FileSizeOutOfRange(loc: ErrorLocation, i: i64) {
            description("File size out of range")
            display("Size {} is out of file size range, in {}", i, loc)
        }
        UnreachableState(path: String) {
            description("Unreachable state")
            display("Unreachable state: {} (maybe a typo or you forgot \
                     to create a rule leading to this state?)", path)
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

impl SyncRules {
    pub fn parse(rules: &toml::Table, base_section: &str)
                 -> Result<SyncRules> {
        let mut this: SyncRules = Default::default();
        let mut state_indices: HashMap<&str,usize> = HashMap::new();

        // First, go through and create all the states themselves so we know
        // their indices when we later need to refer to them.
        for state_name in rules.keys() {
            state_indices.insert(state_name, this.states.len());
            this.states.push(Default::default());
        }

        if let Some(root_ix) = state_indices.get("root") {
            this.root_ix = *root_ix;
        } else {
            return Err(Error::NoRootState);
        }

        // Now actually read all the states in
        for (state_name, state_def) in rules {
            let ix = state_indices[state_name.deref()];
            try!(this.parse_state(state_def, ix,
                                  format!("{}.{}", base_section, state_name),
                                  &state_indices));
        }

        // Check that all states are reachable
        let mut keep_going = true;
        let mut reachable: Vec<_> = iter::repeat(false).take(
            this.states.len()).collect();
        reachable[this.root_ix] = true;
        while keep_going {
            keep_going = false;
            for i in 0..this.states.len() {
                if !reachable[i] { continue; }

                for &r_ix in this.states[i].files.iter().chain(
                    this.states[i].siblings.iter())
                {
                    for action in &this.rules[r_ix].actions {
                        match action {
                            &Action::Mode(..) |
                            &Action::Stop(..) => (),
                            &Action::Include(ref reffed) => {
                                for &r in reffed {
                                    keep_going |= !reachable[r];
                                    reachable[r] = true;
                                }
                            },
                            &Action::Switch(reffed) => {
                                keep_going |= !reachable[reffed];
                                reachable[reffed] = true;
                            },
                        }
                    }
                }
            }
        }

        for (name, &ix) in &state_indices {
            if !reachable[ix] {
                return Err(Error::UnreachableState(
                    format!("{}.{}", base_section, name)));
            }
        }

        Ok(this)
    }

    fn parse_state(&mut self, def_raw: &toml::Value, ix: usize, path: String,
                   state_indices: &HashMap<&str,usize>) -> Result<()> {
        if let Some(def) = def_raw.as_table() {
            for (group_name, group_def) in def {
                let group_path = format!("{}.{}", path, group_name);
                if "files" == group_name {
                    self.states[ix].files =
                        try!(self.parse_group(group_def, group_path,
                                              state_indices));
                } else if "siblings" == group_name {
                    self.states[ix].siblings =
                        try!(self.parse_group(group_def, group_path,
                                              state_indices));
                } else {
                    return Err(Error::InvalidRulesGroup(
                        path, group_name.to_owned()));
                }
            }

            Ok(())
        } else {
            Err(Error::NotATable(path))
        }
    }

    fn parse_group(&mut self, def_raw: &toml::Value,
                   path: String, state_indices: &HashMap<&str,usize>)
                   -> Result<Vec<usize>> {
        if let Some(def) = def_raw.as_slice() {
            let mut rules = Vec::new();

            for (r_ix, rule) in def.iter().enumerate() {
                rules.push(try!(self.parse_rule(
                    rule, r_ix, &path, state_indices)));
            }

            Ok(rules)
        } else {
            Err(Error::NotAnArray(path))
        }
    }

    fn parse_rule(&mut self, def_raw: &toml::Value, ref_ix: usize,
                  path: &str, state_indices: &HashMap<&str,usize>)
                  -> Result<usize> {
        if let Some(def) = def_raw.as_table() {
            let ix = self.rules.len();
            let mut rule: Rule = Default::default();

            for (e_name, e_val) in def {
                let loc = ErrorLocation::new(path.to_owned(), ref_ix,
                                             e_name.to_owned());
                if "name" == e_name {
                    rule.conditions.push(Condition::Name(
                        try!(parse_regex(e_val, loc))));
                } else if "path" == e_name {
                    rule.conditions.push(Condition::Path(
                        try!(parse_regex(e_val, loc))));
                } else if "permissions" == e_name {
                    rule.conditions.push(Condition::Permissions(
                        try!(parse_regex(e_val, loc))));
                } else if "type" == e_name {
                    rule.conditions.push(Condition::Type(
                        try!(parse_regex(e_val, loc))));
                } else if "target" == e_name {
                    rule.conditions.push(Condition::Target(
                        try!(parse_regex(e_val, loc))));
                } else if "bigger" == e_name {
                    rule.conditions.push(Condition::Bigger(
                        try!(parse_file_size(e_val, loc))));
                } else if "smaller" == e_name {
                    rule.conditions.push(Condition::Smaller(
                        try!(parse_file_size(e_val, loc))));
                } else if "mode" == e_name {
                    rule.actions.push(Action::Mode(
                        try!(parse_mode(e_val, loc))));
                } else if "include" == e_name {
                    rule.actions.push(Action::Include(
                        try!(parse_state_ref_list(e_val, loc,
                                                  &state_indices))));
                } else if "switch" == e_name {
                    rule.actions.push(Action::Switch(
                        try!(parse_state_ref(e_val, &loc, &state_indices))));
                } else if "stop" == e_name {
                    rule.actions.push(Action::Stop(
                        try!(parse_stop_type(e_val, loc))));
                } else {
                    return Err(Error::InvalidRuleConfig(
                        loc, e_name.to_owned()));
                }
            }

            rule.actions.sort_by_key(|a| match a {
                &Action::Mode(..) => 0,
                &Action::Include(..) => 1,
                &Action::Switch(..) => 2,
                &Action::Stop(..) => 3,
            });

            self.rules.push(rule);
            Ok(ix)
        } else {
            Err(Error::NotATable(format!("{} #{}", path, ref_ix + 1)))
        }
    }
}

fn parse_regex(val: &toml::Value, loc: ErrorLocation)
               -> Result<Regex> {
    if let Some(s) = val.as_str() {
        Ok(try!(Regex::new(s).context(loc)))
    } else {
        Err(Error::WrongType(loc, "string"))
    }
}

fn parse_file_size(val: &toml::Value, loc: ErrorLocation)
                   -> Result<FileSize> {
    if let Some(i) = val.as_integer() {
        if i >= 0 && i == (i as FileSize as i64) {
            Ok(i)
        } else {
            Err(Error::FileSizeOutOfRange(loc, i))
        }
    } else {
        Err(Error::WrongType(loc, "integer"))
    }
}

fn parse_mode(val: &toml::Value, loc: ErrorLocation)
              -> Result<SyncMode> {
    if let Some(s) = val.as_str() {
        Ok(try!(s.parse().context((loc, s))))
    } else {
        Err(Error::WrongType(loc, "string"))
    }
}

fn parse_state_ref_list(val: &toml::Value, loc: ErrorLocation,
                        state_indices: &HashMap<&str,usize>)
                        -> Result<Vec<usize>> {
    match val {
        &toml::Value::String(_) =>
            Ok(vec![try!(parse_state_ref(val, &loc, state_indices))]),
        &toml::Value::Array(ref elts) => {
            let mut accum = Vec::new();
            for elt in elts {
                accum.push(try!(parse_state_ref(elt, &loc, state_indices)));
            }
            Ok(accum)
        },
        _ => Err(Error::WrongType(loc, "string-or-array")),
    }
}

fn parse_state_ref(val: &toml::Value, loc: &ErrorLocation,
                   state_indices: &HashMap<&str,usize>)
                   -> Result<usize> {
    if let Some(s) = val.as_str() {
        if let Some(&ix) = state_indices.get(s) {
            Ok(ix)
        } else {
            Err(Error::BadStateReference(loc.clone(), s.to_owned()))
        }
    } else {
        Err(Error::WrongType(loc.clone(), "string"))
    }
}

fn parse_stop_type(val: &toml::Value, loc: ErrorLocation)
                   -> Result<StopType> {
    if let Some(s) = val.as_str() {
        if "all" == s {
            Ok(StopType::All)
        } else if "return" == s {
            Ok(StopType::Return)
        } else {
            Err(Error::BadStopType(loc, s.to_owned()))
        }
    } else {
        Err(Error::WrongType(loc, "string"))
    }
}

#[cfg(test)]
mod test {
    use toml;

    use defs::*;
    use rules::defs::*;
    use super::*;
    use super::{Condition,Action,StopType,Rule,RuleState};

    fn parse_rules(s: &str) -> Result<SyncRules> {
        let mut parser = toml::Parser::new(s);
        if let Some(table) = parser.parse() {
            SyncRules::parse(table["rules"].as_table().unwrap(), "rules")
        } else {
            panic!("TOML failed to parse.\n{:?}", parser.errors);
        }
    }

    #[test]
    fn parse_minimal() {
        let rules = parse_rules(r#"
[[rules.root.files]]
mode = "---/---"
"#).unwrap();

        assert_eq!(0, rules.root_ix);
        assert_eq!(1, rules.states.len());
        assert_eq!(0, rules.states[0].siblings.len());
        assert_eq!(1, rules.states[0].files.len());
        assert_eq!(0, rules.states[0].files[0]);
        assert_eq!(1, rules.rules.len());
        assert_eq!(0, rules.rules[0].conditions.len());
        assert_eq!(1, rules.rules[0].actions.len());
        assert_eq!(&Action::Mode("---/---".parse().unwrap()),
                   &rules.rules[0].actions[0]);
    }

    #[test]
    fn parse_all_fields() {
        let rules = parse_rules(r#"
[[rules.root.files]]
# These end up in alphabetical order in the current implementation.
bigger = 1024
name = "foo"
path = "/bar"
permissions = "0777"
smaller = 2048
target = "something"
type = "f"

# These are expected to be sorted in exactly this order.
mode = "cud/cud"
include = [ "z1", "z2" ]
switch = "z3"
stop = "all"

[[rules.z1.files]]
include = "z2"
stop = "return"

[[rules.z2.files]]

[[rules.z3.files]]
"#).unwrap();

        assert_eq!(0, rules.root_ix);
        assert_eq!(4, rules.states.len());
        assert_eq!(1, rules.states[0].files.len());

        let rr = &rules.rules[rules.states[0].files[0]];
        assert_eq!(7, rr.conditions.len());
        match (&rr.conditions[0], &rr.conditions[1], &rr.conditions[2],
               &rr.conditions[3], &rr.conditions[4], &rr.conditions[5],
               &rr.conditions[6]) {
            (&Condition::Bigger(1024), &Condition::Name(..),
             &Condition::Path(..), &Condition::Permissions(..),
             &Condition::Smaller(2048), &Condition::Target(..),
             &Condition::Type(..)) => (),
            unexpected => panic!("Conditions unexpected: {:?}", unexpected),
        }
        assert_eq!(4, rr.actions.len());
        match (&rr.actions[0], &rr.actions[1],
               &rr.actions[2], &rr.actions[3]) {
            (&Action::Mode(mode), &Action::Include(ref included),
             &Action::Switch(switched), &Action::Stop(stop)) => {
                assert_eq!("cud/cud".parse::<SyncMode>().unwrap(), mode);
                assert_eq!(2, included.len());
                assert_eq!(1, included[0]);
                assert_eq!(2, included[1]);
                assert_eq!(3, switched);
                assert_eq!(StopType::All, stop);
            },
            unexpected => panic!("Actions unexpected: {:?}", unexpected),
        }

        assert_eq!(1, rules.states[1].files.len());
        let z1r = &rules.rules[rules.states[1].files[0]];
        assert_eq!(0, z1r.conditions.len());
        assert_eq!(2, z1r.actions.len());
        match (&z1r.actions[0], &z1r.actions[1]) {
            (&Action::Include(ref included), &Action::Stop(stop)) => {
                assert_eq!(1, included.len());
                assert_eq!(2, included[0]);
                assert_eq!(StopType::Return, stop);
            },
            unexpected => panic!("Actions unexpected: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_siblings_rules_group() {
        let rules = parse_rules(r#"
[[rules.root.siblings]]
mode = "cud/cud"
"#).unwrap();

        assert_eq!(0, rules.root_ix);
        assert_eq!(1, rules.states.len());
        assert_eq!(1, rules.states[0].siblings.len());
        assert_eq!(0, rules.states[0].files.len());
        assert_eq!(0, rules.states[0].siblings[0]);
        assert_eq!(1, rules.rules.len());
        assert_eq!(0, rules.rules[0].conditions.len());
        assert_eq!(1, rules.rules[0].actions.len());
        assert_eq!(&Action::Mode("cud/cud".parse().unwrap()),
                   &rules.rules[0].actions[0]);
    }

    #[test]
    fn parse_error_bad_regex() {
        let res = parse_rules(r#"
[[rules.root.files]]
name = "*"
"#);
        match res {
            Err(Error::BadRegex(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_bad_mode() {
        let res = parse_rules(r#"
[[rules.root.files]]
mode = "foo"
"#);
        match res {
            Err(Error::BadMode(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_reference_to_nx_state() {
        let res = parse_rules(r#"
[[rules.root.files]]
switch = "foo"
"#);
        match res {
            Err(Error::BadStateReference(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_bad_stop_type() {
        let res = parse_rules(r#"
[[rules.root.files]]
stop = "tomare"
"#);
        match res {
            Err(Error::BadStopType(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_invalid_rule_field() {
        let res = parse_rules(r#"
[[rules.root.files]]
xyzzy = "plugh"
"#);
        match res {
            Err(Error::InvalidRuleConfig(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_no_root_state() {
        let res = parse_rules(r#"
[[rules.foo.files]]
mode = "---/---"
"#);
        match res {
            Err(Error::NoRootState) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_state_not_a_table() {
        let res = parse_rules(r#"
[rules]
root = 42"#);
        match res {
            Err(Error::NotATable(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_rules_group_not_an_array() {
        let res = parse_rules(r#"
[rules.root]
files = 42"#);
        match res {
            Err(Error::NotAnArray(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_rules_group_elt_not_a_table() {
        let res = parse_rules(r#"
[rules.root]
files = [42]"#);
        match res {
            Err(Error::NotATable(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_negative_file_size() {
        let res = parse_rules(r#"
[[rules.root.files]]
bigger = -1"#);
        match res {
            Err(Error::FileSizeOutOfRange(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_bad_rules_group_name() {
        let res = parse_rules(r#"
[[rules.root.stuff]]
"#);
        match res {
            Err(Error::InvalidRulesGroup(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }

    #[test]
    fn parse_error_unreachable_state() {
        let res = parse_rules(r#"
[[rules.root.files]]

# a and b are considered unreachable even though each references the other.
[[rules.a.files]]
switch = "b"

[[rules.b.files]]
switch = "a"
"#);
        match res {
            Err(Error::UnreachableState(..)) => (),
            unexpected => panic!("Unexpected parse result: {:?}", unexpected),
        }
    }
}

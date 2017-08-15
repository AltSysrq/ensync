//-
// Copyright (c) 2016, 2017, Jason Lingle
//
// This file is part of Ensync.
//
// Ensync is free software: you can  redistribute it and/or modify it under the
// terms of  the GNU General Public  License as published by  the Free Software
// Foundation, either version  3 of the License, or (at  your option) any later
// version.
//
// Ensync is distributed  in the hope that  it will be useful,  but WITHOUT ANY
// WARRANTY; without  even the implied  warranty of MERCHANTABILITY  or FITNESS
// FOR  A PARTICULAR  PURPOSE.  See the  GNU General  Public  License for  more
// details.
//
// You should have received a copy of the GNU General Public License along with
// Ensync. If not, see <http://www.gnu.org/licenses/>.

//! Implements the actual rules engine defined in `defs.rs`.
//!
//! Configuration is taken from the `rules` table in the configuration. The
//! semantics of the rules engine is described in the README and not repeated
//! here.

use std::collections::HashMap;
use std::fmt;
use std::mem;
use std::ops::Deref;
use std::result;
use std::sync::Arc;

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

impl Condition {
    fn matches(&self, path: &str, name: &str, data: &FileData)
               -> bool {
        match *self {
            Condition::Name(ref rx) => rx.is_match(name),
            Condition::Path(ref rx) => rx.is_match(path),
            Condition::Permissions(ref rx) => match *data {
                FileData::Regular(mode, _, _, _) | FileData::Directory(mode) =>
                    rx.is_match(&format!("{:04o}", mode)),
                FileData::Symlink(..) | FileData::Special => false,
            },
            Condition::Type(ref rx) => match *data {
                FileData::Regular(..) => Some("f"),
                FileData::Directory(..) => Some("d"),
                FileData::Symlink(..) => Some("s"),
                FileData::Special => None
            }.map(|s| rx.is_match(s)).unwrap_or(false),
            Condition::Target(ref rx) => match *data {
                FileData::Symlink(ref target) =>
                    rx.is_match(&*target.to_string_lossy()),
                _ => false,
            },
            Condition::Bigger(min) => match *data {
                FileData::Regular(_, size, _, _) => size > min,
                _ => false,
            },
            Condition::Smaller(max) => match *data {
                FileData::Regular(_, size, _, _) => size < max,
                _ => false,
            },
        }
    }
}

/// A single rule action. These are listed in the order they should be
/// evaluated. They correspond directly to the actions described in the project
/// README.
#[derive(Clone,Debug,PartialEq,Eq)]
enum Action {
    Mode(SyncMode),
    TrustClientUnixMode(bool),
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

impl Rule {
    fn matches(&self, path: &str, name: &str, data: &FileData) -> bool {
        self.conditions.iter().all(|r| r.matches(path, name, data))
    }
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
#[derive(Clone,Debug)]
struct EngineState {
    /// The sync mode in effect.
    mode: SyncMode,
    /// Whether `trust_client_unix_mode` is on.
    trust_client_unix_mode: bool,
    /// The index of the `RuleState` in effect.
    state: usize,
    /// If present, the new value of `state` the next time the engine descends
    /// (into siblings or files).
    switch: Option<usize>,
    /// The current path, relative to the sync root, with leading slash (and
    /// translated lossily into UTF-8).
    path: String,
}

impl EngineState {
    fn new(init_state: usize) -> Self {
        EngineState {
            mode: SyncMode::default(),
            trust_client_unix_mode: true,
            state: init_state,
            switch: None,
            path: String::default(),
        }
    }

    fn apply_switch(&mut self) {
        if let Some(new) = self.switch.take() {
            self.state = new;
        }
    }

    fn push_dir(&mut self, sub: &str) {
        self.path.push('/');
        self.path.push_str(sub);
    }
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
    pub fn parse(rules: &toml::value::Table, base_section: &str)
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
        let mut reachable = Vec::new();
        reachable.resize(this.states.len(), false);
        reachable[this.root_ix] = true;
        while keep_going {
            keep_going = false;
            for i in 0..this.states.len() {
                if !reachable[i] { continue; }

                for &r_ix in this.states[i].files.iter().chain(
                    this.states[i].siblings.iter())
                {
                    for action in &this.rules[r_ix].actions {
                        match *action {
                            Action::Mode(..) |
                            Action::TrustClientUnixMode(..) |
                            Action::Stop(..) => (),
                            Action::Include(ref reffed) => {
                                for &r in reffed {
                                    keep_going |= !reachable[r];
                                    reachable[r] = true;
                                }
                            },
                            Action::Switch(reffed) => {
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
        if let Some(def) = def_raw.as_array() {
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
                } else if "trust_client_unix_mode" == e_name {
                    rule.actions.push(Action::TrustClientUnixMode(
                        try!(convert_bool(e_val, loc))));
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

            rule.actions.sort_by_key(|a| match *a {
                Action::Mode(..) => 0,
                Action::TrustClientUnixMode(..) => 1,
                Action::Include(..) => 2,
                Action::Switch(..) => 3,
                Action::Stop(..) => 4,
            });

            self.rules.push(rule);
            Ok(ix)
        } else {
            Err(Error::NotATable(format!("{} #{}", path, ref_ix + 1)))
        }
    }
}

fn convert_bool(val: &toml::Value, loc: ErrorLocation)
                -> Result<bool> {
    match *val {
        toml::Value::Boolean(r) => Ok(r),
        _ => Err(Error::WrongType(loc, "boolean")),
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
            Ok(i as FileSize)
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

#[derive(Clone,Debug)]
pub struct DirEngine {
    rules: Arc<SyncRules>,
    state: EngineState,
}

#[derive(Clone,Debug)]
pub struct FileEngine {
    rules: Arc<SyncRules>,
    state: EngineState,
}

#[derive(Clone,Debug)]
pub struct DirEngineBuilder {
    rules: Arc<SyncRules>,
    state: EngineState,
    rules_matched: Vec<bool>,
}


impl SyncRules {
    fn apply_rules<F : Fn (&RuleState) -> &[usize],
                   M : Fn (usize) -> bool>(
        &self, engstate: &mut EngineState, group: F, matches: M)
    {
        let mut occurs = Vec::new();
        occurs.resize(self.states.len(), false);
        self.apply_rules_impl(&mut occurs, engstate.state,
                              engstate, &group, &matches);
    }

    // Applies the matched rules from a single state. If the `occurs` value for
    // `state_ix` is set, this does nothing. Otherwise it applies each action
    // in each matching rule until all rules have been examined or a `stop`
    // action is encountered.
    //
    // Returns whether the caller should return. A `false` value indicates a
    // `stop = "all"` directive, and thus that the caller itself should
    // immediately return false.
    fn apply_rules_impl<F : Fn (&RuleState) -> &[usize],
                        M : Fn (usize) -> bool>(
        &self, occurs: &mut [bool], state_ix: usize,
        engstate: &mut EngineState, group: &F, matches: &M)
        -> bool
    {
        let mut keep_going = true;

        if occurs[state_ix] { return true; }
        occurs[state_ix] = true;

        'rules_loop:
        for &rule in group(&self.states[state_ix]) {
            if !matches(rule) { continue; }

            for action in &self.rules[rule].actions {
                match *action {
                    Action::Mode(mode) => engstate.mode = mode,
                    Action::TrustClientUnixMode(trust) =>
                        engstate.trust_client_unix_mode = trust,
                    Action::Include(ref subs) => for &sub in subs {
                        if !self.apply_rules_impl(occurs, sub, engstate,
                                                  group, matches) {
                            keep_going = false;
                            break 'rules_loop;
                        }
                    },
                    Action::Switch(new) => engstate.switch = Some(new),
                    Action::Stop(StopType::Return) => break 'rules_loop,
                    Action::Stop(StopType::All) => {
                        keep_going = false;
                        break 'rules_loop;
                    },
                }
            }
        }

        occurs[state_ix] = false;
        keep_going
    }
}

impl DirRules for DirEngine {
    type Builder = DirEngineBuilder;
    type FileRules = FileEngine;

    fn file(&self, file: File) -> FileEngine {
        let name = file.0.to_string_lossy();

        let mut new_state = self.state.clone();
        new_state.push_dir(&*name);
        new_state.apply_switch();

        let mut path = String::new();
        mem::swap(&mut path, &mut new_state.path);
        self.rules.apply_rules(&mut new_state, |g| &g.files,
                               |r| self.rules.rules[r].matches(
                                   &path, &*name, file.1));
        mem::swap(&mut new_state.path, &mut path);

        FileEngine {
            rules: self.rules.clone(),
            state: new_state,
        }
    }
}

impl FileEngine {
    pub fn new(rules: Arc<SyncRules>) -> Self {
        let init_state = rules.root_ix;
        FileEngine {
            rules: rules,
            state: EngineState::new(init_state),
        }
    }
}

impl FileRules for FileEngine {
    type DirRules = DirEngine;

    fn sync_mode(&self) -> SyncMode {
        self.state.mode
    }

    fn trust_client_unix_mode(&self) -> bool {
        self.state.trust_client_unix_mode
    }

    fn subdir(self) -> DirEngineBuilder {
        let mut matched = Vec::new();
        matched.resize(self.rules.rules.len(), false);

        DirEngineBuilder {
            rules: self.rules,
            state: self.state,
            rules_matched: matched,
        }
    }
}

impl DirRulesBuilder for DirEngineBuilder {
    type DirRules = DirEngine;

    fn contains(&mut self, file: File) {
        let name = file.0.to_string_lossy();
        let path = format!("{}/{}", self.state.path, name);

        for (ix, rule) in self.rules.rules.iter().enumerate() {
            self.rules_matched[ix] |= rule.matches(&path, &name, file.1);
        }
    }

    fn build(self) -> DirEngine {
        let mut state = self.state;
        let rules = self.rules;
        let rules_matched = self.rules_matched;

        rules.apply_rules(&mut state, |g| &g.siblings,
                          |r| rules_matched[r]);
        state.apply_switch();
        DirEngine {
            rules: rules,
            state: state,
        }
    }
}

#[cfg(test)]
mod test {
    use toml;

    use defs::*;
    use defs::test_helpers::*;
    use rules::defs::*;
    use super::*;
    use super::{Condition,Action,StopType};

    fn parse_rules(s: &str) -> Result<SyncRules> {
        let table: toml::value::Table = toml::from_str(s).unwrap();
        SyncRules::parse(table["rules"].as_table().unwrap(), "rules")
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
trust_client_unix_mode = false
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
        assert_eq!(5, rr.actions.len());
        match (&rr.actions[0], &rr.actions[1],
               &rr.actions[2], &rr.actions[3],
               &rr.actions[4]) {
            (&Action::Mode(mode), &Action::TrustClientUnixMode(false),
             &Action::Include(ref included), &Action::Switch(switched),
             &Action::Stop(stop)) => {
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

    fn engine(text: &str) -> DirEngine {
        use std::sync::Arc;
        FileEngine::new(Arc::new(parse_rules(text).unwrap())).subdir().build()
    }

    fn regular(de: &DirEngine, name: &str, mode: FileMode,
               size: FileSize) -> FileEngine {
        de.file(File(&oss(name), &FileData::Regular(mode, size, 0, [0;32])))
    }

    fn dir(de: &DirEngine, name: &str, mode: FileMode) -> FileEngine {
        de.file(File(&oss(name), &FileData::Directory(mode)))
    }

    fn symlink(de: &DirEngine, name: &str, target: &str) -> FileEngine {
        de.file(File(&oss(name), &FileData::Symlink(oss(target))))
    }

    #[test]
    fn simple_file_matching() {
        let de = engine(r#"
[[rules.root.files]]
name = "^fo*$"
mode = "cud/cud"

[[rules.root.files]]
name = "^ba+r$"
mode = "cud/---"
"#);

        assert_eq!("---/---", regular(&de, "plugh", 0, 0)
                   .sync_mode().to_string());
        assert_eq!("cud/cud", regular(&de, "foo", 0, 0)
                   .sync_mode().to_string());
        assert_eq!("cud/---", regular(&de, "bar", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn all_conditions_must_match_for_rule_to_apply() {
        let de = engine(r#"
[[rules.root.files]]
bigger = 100
smaller = 200
mode = "cud/cud"
"#);

        assert_eq!("---/---", regular(&de, "a", 0, 50)
                   .sync_mode().to_string());
        assert_eq!("---/---", regular(&de, "b", 0, 250)
                   .sync_mode().to_string());
        assert_eq!("cud/cud", regular(&de, "c", 0, 150)
                   .sync_mode().to_string());
    }

    #[test]
    fn rule_with_no_conditions_always_applies() {
        let de = engine(r#"
[[rules.root.files]]
mode = "cud/cud"
trust_client_unix_mode = false
"#);

        assert_eq!("cud/cud", regular(&de, "a", 0, 0)
                   .sync_mode().to_string());
        assert!(!regular(&de, "a", 0, 0).trust_client_unix_mode());
    }

    #[test]
    fn sync_mode_inherited_from_parent_dir() {
        let de = engine(r#"
[[rules.root.files]]
name = "^dir$"
mode = "cud/cud"
"#);

        let sde = dir(&de, "dir", 0).subdir().build();
        assert_eq!("cud/cud", regular(&sde, "foo", 0, 0)
                   .sync_mode().to_string());
        assert!(regular(&sde, "foo", 0, 0).trust_client_unix_mode());
    }

    #[test]
    fn match_condition_path() {
        let de = engine(r#"
[[rules.root.files]]
path = "^/foo/bar$"
mode = "cud/cud"
"#);

        let sde = dir(&de, "foo", 0).subdir().build();
        assert_eq!("cud/cud", regular(&sde, "bar", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn match_condition_permissions() {
        let de = engine(r#"
[[rules.root.files]]
permissions = "^07{3}$"
mode = "cud/cud"
"#);

        assert_eq!("---/---", regular(&de, "foo", 0o666, 0)
                   .sync_mode().to_string());
        assert_eq!("cud/cud", regular(&de, "bar", 0o777, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn match_condition_type() {
        let de = engine(r#"
[[rules.root.files]]
type = "f"
mode = "c--/---"

[[rules.root.files]]
type = "d"
mode = "-u-/---"

[[rules.root.files]]
type = "s"
mode = "--d/---"
"#);
        assert_eq!("c--/---", regular(&de, "foo", 0, 0)
                   .sync_mode().to_string());
        assert_eq!("-u-/---", dir(&de, "bar", 0)
                   .sync_mode().to_string());
        assert_eq!("--d/---", symlink(&de, "baz", "plugh")
                   .sync_mode().to_string());
        assert_eq!("---/---", de.file(File(&oss("dev"), &FileData::Special))
                   .sync_mode().to_string());
    }

    #[test]
    fn match_condition_target() {
        let de = engine(r#"
[[rules.root.files]]
target = 'x$'
mode = "cud/cud"
"#);
        assert_eq!("---/---", regular(&de, "foo", 0, 0)
                   .sync_mode().to_string());
        assert_eq!("---/---", symlink(&de, "quux", "xyzzy")
                   .sync_mode().to_string());
        assert_eq!("cud/cud", symlink(&de, "foo", "quux")
                   .sync_mode().to_string());
    }

    #[test]
    fn files_rules_inclusion() {
        let de = engine(r#"
[[rules.root.files]]
name = '^foo$'
include = "foo"

[[rules.foo.files]]
mode = "cud/cud"
"#);

        assert_eq!("---/---", regular(&de, "bar", 0, 0)
                   .sync_mode().to_string());
        assert_eq!("cud/cud", regular(&de, "foo", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn include_recursion_is_noop() {
        let de = engine(r#"
[[rules.root.files]]
include = "root"

# Use two includes so that a possible bug where the occurs value is cleared
# above would result in infinite recursion.
[[rules.root.files]]
include = "root"
"#);

        assert_eq!("---/---", regular(&de, "foo", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn files_switch_state_on_subdir() {
        let de = engine(r#"
[[rules.root.files]]
name = '^foo$'
switch = "foo"

[[rules.foo.files]]
name = '^bar$'
mode = "cud/cud"
"#);

        assert_eq!("---/---", regular(&de, "bar", 0, 0)
                   .sync_mode().to_string());
        let sdf = dir(&de, "foo", 0);
        assert_eq!("---/---", sdf.sync_mode().to_string());
        let sde = sdf.subdir().build();
        assert_eq!("cud/cud", regular(&sde, "bar", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn stop_all_action() {
        let de = engine(r#"
[[rules.root.files]]
include = "a"

[[rules.root.files]]
mode = "cud/cud"

[[rules.a.files]]
mode = "cud/---"
include = "b"

[[rules.b.files]]
stop = "all"

[[rules.b.files]]
mode = "---/cud"
"#);

        assert_eq!("cud/---", regular(&de, "foo", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn stop_return_action() {
        let de = engine(r#"
[[rules.root.files]]
mode = "cud/cud"

[[rules.root.files]]
include = "a"

[[rules.a.files]]
include = "b"

[[rules.a.files]]
mode = "cud/---"

[[rules.b.files]]
stop = "return"

[[rules.b.files]]
# If `stop = "return"` is a noop, this breaks the test by preventing the mode
# from being set by rules.a.files.
stop = "all"
"#);

        assert_eq!("cud/---", regular(&de, "foo", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn sibling_multi_file_match() {
        let de = engine(r#"
# Initial non-matched rule in case rules are matched spuriously.
[[rules.root.siblings]]
name = "^c$"
stop = "all"

[[rules.root.siblings]]
name = "^a$"
mode = "cud/---"

[[rules.root.siblings]]
name = "^b$"
stop = "all"

[[rules.root.siblings]]
# No single file could match all three rules; yet because there is a file
# matching each rule, all (but the first 'c' rule) will apply.
name = "^a$"
mode = "---/cud"
"#);

        let mut sdb = dir(&de, "foo", 0).subdir();
        sdb.contains(File(&oss("b"), &FileData::Directory(0)));
        sdb.contains(File(&oss("a"), &FileData::Directory(0)));

        let sde = sdb.build();
        assert_eq!("cud/---", regular(&sde, "bar", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn ignore_git_repo_via_siblings() {
        let de = engine(r#"
[[rules.root.files]]
mode = "cud/cud"

[[rules.root.siblings]]
name = '^\.git$'
switch = "git"

[[rules.git.files]]
mode = "---/---"
"#);

        let sd = dir(&de, "ensync", 0);
        assert_eq!("cud/cud", sd.sync_mode().to_string());

        let mut sdb = sd.subdir();
        sdb.contains(File(&oss(".git"), &FileData::Directory(0)));
        sdb.contains(File(&oss("README"), &FileData::Regular(
            0, 0, 0, [0;32])));
        let sde = sdb.build();

        assert_eq!("---/---", regular(&sde, "README", 0, 0)
                   .sync_mode().to_string());
    }

    #[test]
    fn match_path_from_siblings() {
        let de = engine(r#"
[[rules.root.siblings]]
path = "^/foo/bar$"
mode = "cud/cud"
"#);

        let mut sdb = dir(&de, "foo", 0).subdir();
        sdb.contains(File(&oss("bar"), &FileData::Directory(0)));
        let sde = sdb.build();

        assert_eq!("cud/cud", regular(&sde, "quux", 0, 0)
                   .sync_mode().to_string());
    }
}

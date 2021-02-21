//-
// Copyright (c) 2016, 2017, 2021, Jason Lingle
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

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::defs::*;
use crate::log::ReplicaSide;
use crate::rules::*;

/// Generates a new string based on `orig` for which `in_use` returns `false`.
///
/// If the stem of `orig` contains a `~`, all characters after the last `~` in
/// the stem are dropped; otherwise, `~` is appended to the stem. The function
/// then appends successive suffixes until it finds one not in use.
///
/// The extension is always preserved; eg, "foo.txt" may become "foo~1.txt".
///
/// If `orig` is not valid UTF-8, invalid sequences may be clobbered.
pub fn gen_alternate_name<F: Fn(&OsStr) -> bool>(
    orig: &OsStr,
    in_use: F,
) -> OsString {
    let orig_osstr: String = orig.to_string_lossy().into_owned();
    let mut path = PathBuf::new();
    path.set_file_name(orig_osstr);

    let extension = path
        .extension()
        .map_or_else(|| "".to_owned(), |x| format!(".{}", x.to_string_lossy()));

    let mut base = path.file_stem().unwrap().to_string_lossy().into_owned();
    if let Some(tilde) = base.rfind('~') {
        base.truncate(tilde + 1);
    } else {
        base.push('~');
    }

    for n in 1u64.. {
        let new: OsString = format!("{}{}{}", &base, n, &extension).into();
        if !in_use(&new) {
            return new;
        }
    }

    // Not really reachable in practise; no physical system can have 2**64
    // files in a directory. (It would also take quite a while to count up to
    // this point.)
    panic!(
        "Unable to generate a new name for {}, every possible suffix \
            in that directory is already in use",
        orig.to_string_lossy()
    );
}
/// When a reconciliation sources from or affects one side, indicates which
/// replica is to be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReconciliationSide {
    Client,
    Server,
}

impl From<ReconciliationSide> for ReplicaSide {
    fn from(r: ReconciliationSide) -> ReplicaSide {
        match r {
            ReconciliationSide::Client => ReplicaSide::Client,
            ReconciliationSide::Server => ReplicaSide::Server,
        }
    }
}

impl ReconciliationSide {
    pub fn rev(self) -> Self {
        match self {
            ReconciliationSide::Client => ReconciliationSide::Server,
            ReconciliationSide::Server => ReconciliationSide::Client,
        }
    }
}

/// Indicates how the ancestor replica is to be treated for
/// `Reconciliation::Split`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAncestorState {
    /// Rename the ancestor to match the renamed replica (the new file will now
    /// look like a delete).
    Move,
    /// Remove the ancestor entirely (both files will look like creations).
    Delete,
}

/// Describes the end-state of the reconciliation of a single file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconciliation {
    /// The client and server are in-sync. The ancestor should be updated to
    /// match, if needed. If the file is a directory, recurse if it is dirty on
    /// the client or server.
    InSync,
    /// The client and server are not in-sync and are to be left that way. Take
    /// no action and do not recurse.
    Unsync,
    /// The client and server are not in-sync and are to be left that way, but
    /// this is due to a potentially unexpected situation. Warn the user if
    /// appropriate, but otherwise take no action and do not recurse.
    ///
    /// Changes are only considered "irreconcilable" if the sync mode would
    /// imply propagating changes, but does not permit any automatic
    /// resolution. For example, edit/edit conflicts are only irreconcilable if
    /// one of the update directions is enabled.
    Irreconcilable,
    /// Use either the client or the server state, setting the ancestor to
    /// match. If the file is a directory, recurse. If the file is to be
    /// deleted and is a directory, recurse and delete if empty after recursion
    /// completes.
    Use(ReconciliationSide),
    /// Rename the file on the given side to a new, unique name, then retry
    /// reconciliation. The ancestor may be unchanged, renamed, or deleted
    /// based on the second field.
    Split(ReconciliationSide, SplitAncestorState),
}

/// Indicates what kind of conflict, if any, was encountered during
/// reconciliation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Conflict {
    /// No conflict.
    NoConflict,
    /// The file was edited on one side and deleted on the other. The side
    /// which deleted is indicated in the value.
    EditDelete(ReconciliationSide),
    /// The file was edited on both sides. Fields are the edit types (relative
    /// to the ancestor) on client and server, respectively.
    EditEdit(ConflictingEdit, ConflictingEdit),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictingEdit {
    /// Only the mode of the file was changed relative to the ancestor, or the
    /// server and client agree on content but not mode.
    Mode,
    /// The content or type of the file was changed relative to the ancestor
    /// and the server and client do not agree on content.
    Content,
}

/// Determines the abstract reconciliation path for the given file triple and
/// sync mode.
///
/// Whether the caller should recurse is determined by the reconciliation type
/// and whether the file in question is actually a directory.
///
/// This function will never suggest to simply replace a directory with
/// something that is not a directory; it will instead indicate to rename the
/// existing directory first via `Reconciliation::Split`.
pub fn choose_reconciliation(
    cli: Option<&FileData>,
    anc: Option<&FileData>,
    srv: Option<&FileData>,
    mode: SyncMode,
) -> (Reconciliation, Conflict) {
    use self::Conflict::*;
    use self::Reconciliation::*;
    use self::ReconciliationSide::*;

    fn replace(
        dst_file: Option<&FileData>,
        dst_side: ReconciliationSide,
        src_file: Option<&FileData>,
        src_side: ReconciliationSide,
    ) -> Reconciliation {
        match (dst_file, src_file) {
            // If either side is nonexistent, we don't need anything special
            (None, _) | (_, None) |
            // If the src is a directory, it doesn't matter what dst is. If not
            // a directory, it can be simply removed; if a directory, we merge
            // the two.
            (Some(_), Some(&FileData::Directory(..))) => Use(src_side),
            // If the dst is a directory but src is not, we need to move dst
            // out of the way first and treat it like an edit/delete conflict,
            // since we can't (and don't want to) remove the whole thing
            // atomically.
            (Some(&FileData::Directory(..)), Some(_)) =>
                Split(dst_side, SplitAncestorState::Move),
            // Any pair of atomic files needs no special handling.
            (Some(_), Some(_)) => Use(src_side),
        }
    }

    let use_server = replace(cli, Client, srv, Server);
    let use_client = replace(srv, Server, cli, Client);

    fn create(
        away: HalfSyncMode,
        toward: HalfSyncMode,
        propagate: Reconciliation,
        revert: Reconciliation,
    ) -> (Reconciliation, Conflict) {
        // Creation. If create is enabled away from the file, create the file
        // on the other side. Else, if force delete is enabled toward the file,
        // delete it; otherwise, desync.
        if away.create.on() {
            (propagate, NoConflict)
        } else if toward.delete.force() {
            (revert, NoConflict)
        } else {
            (Unsync, NoConflict)
        }
    }

    fn delete(
        anc: &FileData,
        file: &FileData,
        deleted_side: ReconciliationSide,
        away: HalfSyncMode,
        toward: HalfSyncMode,
        propagate: Reconciliation,
        revert: Reconciliation,
    ) -> (Reconciliation, Conflict) {
        // Deletion. If delete is enabled towards the existing file, delete it.
        // Else, if force create is enabled away from it, create it on the
        // other side; otherwise, desync. There are two cases for resurrection
        // so that we give force-create precedence over force-delete.
        //
        // Note that we don't count edit/delete conflicts as such when only the
        // mode has changed.
        if anc.matches_content(file) {
            (
                if away.delete.on() {
                    propagate
                } else if toward.create.force() {
                    revert
                } else {
                    Unsync
                },
                NoConflict,
            )
        } else {
            (
                if toward.create.on() {
                    revert
                } else if away.delete.force() {
                    propagate
                } else if away.delete.on() || toward.update.on() {
                    Irreconcilable
                } else {
                    Unsync
                },
                EditDelete(deleted_side),
            )
        }
    }

    fn update(
        c: &FileData,
        a: Option<&FileData>,
        s: &FileData,
        mode: SyncMode,
        use_client: Reconciliation,
        use_server: Reconciliation,
    ) -> (Reconciliation, Conflict) {
        use crate::rules::SyncModeSetting::*;

        // What to do on an edit/edit conflict that can't be reconciled
        // automatically.
        let need_split =
            if mode.inbound.update.on() || mode.outbound.update.on() {
                if mode.inbound.create.on() && mode.outbound.create.on() {
                    // We're allowed to create files on both sides, so rename
                    // the server file and keep both versions.
                    Split(Server, SplitAncestorState::Delete)
                } else {
                    // Not allowed to create files, so we can't do anything.
                    Irreconcilable
                }
            } else {
                // Update conflicts are of no interest because updates are
                // disabled.
                Unsync
            };

        // If the client and server agree other than metadata, pick the one
        // that disagrees with the ancestor, or the one that is newer if they
        // both disagree.
        if !c.matches(s) && c.matches_data(s) {
            let server_wins = if a.map_or(false, |a| c.matches(a)) {
                true
            } else if a.map_or(false, |a| s.matches(a)) {
                false
            } else {
                s.newer_than(c)
            };

            (
                if server_wins {
                    if mode.inbound.update.on() {
                        use_server
                    } else if mode.outbound.update.force() {
                        use_client
                    } else {
                        // If we can't reconcile, simply leave as-is since it's
                        // just metadata. We also don't report the conflict.
                        InSync
                    }
                } else {
                    if mode.outbound.update.on() {
                        use_client
                    } else if mode.inbound.update.force() {
                        use_server
                    } else {
                        InSync
                    }
                },
                NoConflict,
            )

        // No conflict if one side agrees with the ancestor. As with creates
        // and deletes, force settings revert the updated side instead. We
        // don't care about metadata (modified time) here.
        } else if a.map_or(false, |a| a.matches(c)) {
            (
                if mode.inbound.update.on() {
                    use_server
                } else if mode.outbound.update.force() {
                    use_client
                } else {
                    Unsync
                },
                NoConflict,
            )
        } else if a.map_or(false, |a| a.matches(s)) {
            (
                if mode.outbound.update.on() {
                    use_client
                } else if mode.inbound.update.force() {
                    use_server
                } else {
                    Unsync
                },
                NoConflict,
            )
        } else if a
            .map_or(false, |a| a.matches_content(c) && a.matches_content(s))
        {
            // If both sides disagree about mode but not content, propogate the
            // mode whichever direction we can, prefering the client
            (
                match (mode.inbound.update, mode.outbound.update) {
                    (_, Force) => use_client,
                    (Force, _) => use_server,
                    (_, On) => use_client,
                    (On, _) => use_server,
                    _ => Unsync,
                },
                EditEdit(ConflictingEdit::Mode, ConflictingEdit::Mode),
            )
        } else if a.map_or(false, |a| a.matches_content(c)) {
            // If one side changes the mode and the other side changes the
            // content, prefer the content side unless force overrides.
            (
                match (mode.inbound.update, mode.outbound.update) {
                    (Force, _) | (On, _) => use_server,
                    (_, Force) => use_client,
                    _ => need_split,
                },
                EditEdit(ConflictingEdit::Mode, ConflictingEdit::Content),
            )
        } else if a.map_or(false, |a| a.matches_content(s)) {
            (
                match (mode.inbound.update, mode.outbound.update) {
                    (_, Force) | (_, On) => use_client,
                    (Force, _) => use_server,
                    _ => need_split,
                },
                EditEdit(ConflictingEdit::Content, ConflictingEdit::Mode),
            )
        } else {
            // Both files have changed content.

            (
                match (mode.inbound.update, mode.outbound.update) {
                    (Force, Force) =>
                    // Force+Force = use newer, break ties with client
                    {
                        if s.newer_than(c) {
                            use_server
                        } else {
                            use_client
                        }
                    }
                    (Force, _) => use_server,
                    (_, Force) => use_client,
                    _ => need_split,
                },
                EditEdit(ConflictingEdit::Content, ConflictingEdit::Content),
            )
        }
    }

    match (cli, anc, srv) {
        // Trivial: If the server and client agree already, they're in-sync.
        (None, _, None) => (InSync, NoConflict),
        (Some(c), _, Some(s)) if c.matches(s) => (InSync, NoConflict),

        // We never do anything with special files
        (Some(&FileData::Special), _, _) | (_, _, Some(&FileData::Special)) => {
            (Unsync, NoConflict)
        }

        (Some(_), None, None) => {
            create(mode.outbound, mode.inbound, use_client, use_server)
        }
        (None, None, Some(_)) => {
            create(mode.inbound, mode.outbound, use_server, use_client)
        }

        (Some(c), Some(a), None) => delete(
            a,
            c,
            Server,
            mode.inbound,
            mode.outbound,
            use_server,
            use_client,
        ),
        (None, Some(a), Some(s)) => delete(
            a,
            s,
            Client,
            mode.outbound,
            mode.inbound,
            use_client,
            use_server,
        ),

        (Some(c), a, Some(s)) => update(c, a, s, mode, use_client, use_server),
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use super::*;
    use crate::defs::test_helpers::*;

    #[test]
    fn simple_gen_alternate_name() {
        let mut names = HashSet::new();
        names.insert(oss("foo"));

        let result = gen_alternate_name(&oss("foo"), |n| names.contains(n));
        assert_eq!(oss("foo~1"), result);
    }

    #[test]
    fn gen_alternate_name_with_extension() {
        let mut names = HashSet::new();
        names.insert(oss("foo.txt"));

        let result = gen_alternate_name(&oss("foo.txt"), |n| names.contains(n));
        assert_eq!(oss("foo~1.txt"), result);
    }

    #[test]
    fn gen_alternate_name_with_collision() {
        let mut names = HashSet::new();
        names.insert(oss("foo.txt"));
        names.insert(oss("foo~1.txt"));

        let result = gen_alternate_name(&oss("foo.txt"), |n| names.contains(n));
        assert_eq!(oss("foo~2.txt"), result);
    }

    #[test]
    fn gen_alternate_name_reuses_existing_tilde() {
        let mut names = HashSet::new();

        names.insert(oss("foo.txt"));
        names.insert(oss("foo~1.txt"));

        let result =
            gen_alternate_name(&oss("foo~1.txt"), |n| names.contains(n));
        assert_eq!(oss("foo~2.txt"), result);
    }

    fn for_every_sync_triple<
        F: Fn(Option<&FileData>, Option<&FileData>, Option<&FileData>),
    >(
        f: F,
    ) {
        let files = vec![
            FileData::Directory(0o777),
            FileData::Directory(0o770),
            FileData::Directory(0o700),
            FileData::Regular(0o777, 0, 0, [0; 32]),
            FileData::Regular(0o770, 0, 0, [0; 32]),
            FileData::Regular(0o700, 0, 0, [0; 32]),
            FileData::Regular(0o777, 0, 0, [1; 32]),
            FileData::Regular(0o770, 0, 0, [1; 32]),
            FileData::Regular(0o700, 0, 0, [1; 32]),
            FileData::Regular(0o777, 0, 0, [2; 32]),
            FileData::Regular(0o770, 0, 0, [2; 32]),
            FileData::Regular(0o700, 0, 0, [2; 32]),
            FileData::Symlink(oss("foo")),
            FileData::Symlink(oss("bar")),
            FileData::Symlink(oss("baz")),
            FileData::Special,
        ];
        let mut options = vec![None];
        for f in &files {
            options.push(Some(f))
        }

        for cli in &options {
            for anc in &options {
                for srv in &options {
                    f(*cli, *anc, *srv)
                }
            }
        }
    }

    fn panic_reconciliation(
        result: Reconciliation,
        cli: Option<&FileData>,
        anc: Option<&FileData>,
        srv: Option<&FileData>,
    ) -> ! {
        panic!(
            "Reconciled to {:?}:\n{:?}\n{:?}\n{:?}",
            result, cli, anc, srv
        )
    }

    #[test]
    fn always_insync_or_unsync_for_null_sync_mode() {
        use super::Reconciliation::*;

        let mode: SyncMode = "---/---".parse().unwrap();
        for_every_sync_triple(|c, a, s| {
            match choose_reconciliation(c, a, s, mode).0 {
                InSync | Unsync => (),
                other => panic_reconciliation(other, c, a, s),
            }
        });
    }

    #[test]
    fn never_modifies_client_if_client_sync_off() {
        use super::Reconciliation::*;
        use super::ReconciliationSide::*;

        let mode: SyncMode = "---/cud".parse().unwrap();
        for_every_sync_triple(|c, a, s| {
            match choose_reconciliation(c, a, s, mode).0 {
                InSync | Unsync | Irreconcilable => (),
                Use(side) if Client == side => (),
                Split(side, _) if Server == side => (),

                other => panic_reconciliation(other, c, a, s),
            }
        });
    }

    #[test]
    fn never_modifies_server_if_server_sync_off() {
        use super::Reconciliation::*;
        use super::ReconciliationSide::*;

        let mode: SyncMode = "cud/---".parse().unwrap();
        for_every_sync_triple(|c, a, s| {
            match choose_reconciliation(c, a, s, mode).0 {
                InSync | Unsync | Irreconcilable => (),
                Use(side) if Server == side => (),
                Split(side, _) if Client == side => (),

                other => panic_reconciliation(other, c, a, s),
            }
        });
    }

    #[test]
    fn never_unsync_or_irreconcilable_if_outbound_force_all() {
        use super::Reconciliation::*;
        use super::ReconciliationSide::*;

        let mode: SyncMode = "---/CUD".parse().unwrap();
        for_every_sync_triple(|c, a, s| {
            match choose_reconciliation(c, a, s, mode).0 {
                r @ Unsync | r @ Irreconcilable => match (c, s) {
                    (Some(&FileData::Special), _)
                    | (_, Some(&FileData::Special)) => (),
                    _ => panic_reconciliation(r, c, a, s),
                },

                InSync => (),
                Use(side) if Client == side => (),
                Split(side, _) if Server == side => (),

                other => panic_reconciliation(other, c, a, s),
            }
        });
    }

    struct AssertReconcilliation<'a> {
        cli: Option<&'a FileData>,
        anc: Option<&'a FileData>,
        srv: Option<&'a FileData>,
        expected_conflict: Conflict,
    }

    fn assert_reconciliation<'a>(
        cli: Option<&'a FileData>,
        anc: Option<&'a FileData>,
        srv: Option<&'a FileData>,
        expected_conflict: Conflict,
    ) -> AssertReconcilliation<'a> {
        AssertReconcilliation {
            cli: cli,
            anc: anc,
            srv: srv,
            expected_conflict: expected_conflict,
        }
    }

    impl<'a> AssertReconcilliation<'a> {
        fn f(&self, mode: &str, expected_recon: Reconciliation) -> &Self {
            let actual = choose_reconciliation(
                self.cli,
                self.anc,
                self.srv,
                mode.parse().unwrap(),
            );

            if actual != (expected_recon, self.expected_conflict) {
                panic!(
                    "Unexpected reconciliation result.\n\
                        Client  : {:?}\n\
                        Ancestor: {:?}\n\
                        Server  : {:?}\n\
                        Mode    : {}\n\
                        Expected: {:?}\n\
                        Actual  : {:?}",
                    self.cli,
                    self.anc,
                    self.srv,
                    mode,
                    (expected_recon, self.expected_conflict),
                    actual
                );
            }

            self
        }
    }

    #[test]
    fn individual_reconciliation_cases() {
        use super::Conflict::*;
        use super::Reconciliation::*;
        use super::ReconciliationSide::*;

        let reg777_1_data = FileData::Regular(0o777, 0, 1, [1; 32]);
        let reg770_1_data = FileData::Regular(0o770, 0, 1, [1; 32]);
        let reg700_1_data = FileData::Regular(0o700, 0, 1, [1; 32]);
        let reg777_1b_data = FileData::Regular(0o777, 0, 1, [4; 32]);
        let reg777_2_data = FileData::Regular(0o777, 0, 2, [2; 32]);
        let reg777_3_data = FileData::Regular(0o777, 0, 3, [3; 32]);
        let reg777_4t1_data = FileData::Regular(0o777, 0, 1, [0; 32]);
        let reg777_4t2_data = FileData::Regular(0o777, 0, 2, [0; 32]);
        let reg777_4t3_data = FileData::Regular(0o777, 0, 3, [0; 32]);
        let reg777_1 = Some(&reg777_1_data);
        let reg770_1 = Some(&reg770_1_data);
        let reg700_1 = Some(&reg700_1_data);
        let reg777_2 = Some(&reg777_2_data);
        let reg777_3 = Some(&reg777_3_data);
        let reg777_1b = Some(&reg777_1b_data);
        let reg777_4t1 = Some(&reg777_4t1_data);
        let reg777_4t2 = Some(&reg777_4t2_data);
        let reg777_4t3 = Some(&reg777_4t3_data);

        let dir777_data = FileData::Directory(0o777);
        let dir770_data = FileData::Directory(0o770);
        let dir700_data = FileData::Directory(0o700);
        let dir777 = Some(&dir777_data);
        let dir770 = Some(&dir770_data);
        let dir700 = Some(&dir700_data);

        let special_data = FileData::Special;
        let special = Some(&special_data);

        assert_reconciliation(reg777_1, reg777_1, reg777_1, NoConflict)
            .f("---/---", InSync);
        assert_reconciliation(reg777_1, None, reg777_1, NoConflict)
            .f("---/---", InSync);
        assert_reconciliation(None, reg777_1, None, NoConflict)
            .f("---/---", InSync);
        assert_reconciliation(None, None, None, NoConflict)
            .f("---/---", InSync);

        assert_reconciliation(special, None, None, NoConflict)
            .f("CUD/CUD", Unsync);
        assert_reconciliation(None, None, special, NoConflict)
            .f("CUD/CUD", Unsync);

        assert_reconciliation(reg777_1, None, None, NoConflict)
            .f("---/---", Unsync)
            .f("cud/---", Unsync)
            .f("--d/---", Unsync)
            .f("--D/---", Use(Server))
            .f("---/c--", Use(Client))
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Client))
            .f("CUD/CUD", Use(Client));
        assert_reconciliation(dir777, None, None, NoConflict)
            .f("cud/cud", Use(Client))
            .f("---/CUD", Use(Client))
            .f("CUD/---", Use(Server));

        assert_reconciliation(None, None, reg777_1, NoConflict)
            .f("---/---", Unsync)
            .f("cud/---", Use(Server))
            .f("---/--d", Unsync)
            .f("---/--D", Use(Client))
            .f("---/CUD", Use(Client))
            .f("CUD/---", Use(Server))
            .f("cud/cud", Use(Server))
            .f("CUD/CUD", Use(Server));

        assert_reconciliation(reg777_1, reg777_1, None, NoConflict)
            .f("---/---", Unsync)
            .f("--d/---", Use(Server))
            .f("---/C--", Use(Client))
            .f("---/cud", Unsync)
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Server))
            .f("CUD/CUD", Use(Server));
        assert_reconciliation(reg777_1, reg700_1, None, NoConflict)
            .f("cud/cud", Use(Server))
            .f("cu-/cu-", Unsync);

        assert_reconciliation(None, reg777_1, reg777_1, NoConflict)
            .f("---/---", Unsync)
            .f("---/--d", Use(Client))
            .f("C--/---", Use(Server))
            .f("---/cud", Use(Client))
            .f("---/CUD", Use(Client))
            .f("CUD/---", Use(Server))
            .f("cud/cud", Use(Client))
            .f("CUD/CUD", Use(Client));
        assert_reconciliation(None, reg700_1, reg700_1, NoConflict)
            .f("cud/cud", Use(Client))
            .f("cu-/cu-", Unsync);

        assert_reconciliation(reg777_1, reg777_2, None, EditDelete(Server))
            .f("---/---", Unsync)
            .f("--d/---", Irreconcilable)
            .f("---/-u-", Irreconcilable)
            .f("--D/---", Use(Server))
            .f("---/C--", Use(Client))
            .f("---/c--", Use(Client))
            .f("---/cud", Use(Client))
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Client))
            .f("cud/CUD", Use(Client))
            .f("CUD/cud", Use(Client))
            .f("CUD/CUD", Use(Client));

        assert_reconciliation(None, reg777_1, reg777_2, EditDelete(Client))
            .f("---/---", Unsync)
            .f("---/cud", Irreconcilable)
            .f("-u-/---", Irreconcilable)
            .f("---/--D", Use(Client))
            .f("C--/---", Use(Server))
            .f("cud/---", Use(Server))
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Server))
            .f("CUD/CUD", Use(Server));

        assert_reconciliation(reg700_1, reg777_1, reg777_1, NoConflict)
            .f("---/---", Unsync)
            .f("cud/---", Unsync)
            .f("---/cud", Use(Client))
            .f("CUD/---", Use(Server))
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Client))
            .f("CUD/cud", Use(Client))
            .f("cud/CUD", Use(Client))
            .f("CUD/CUD", Use(Client));

        assert_reconciliation(reg777_1, reg777_1, reg700_1, NoConflict)
            .f("---/---", Unsync)
            .f("cud/---", Use(Server))
            .f("---/cud", Unsync)
            .f("CUD/---", Use(Server))
            .f("---/CUD", Use(Client))
            .f("cud/CUD", Use(Server))
            .f("CUD/cud", Use(Server))
            .f("cud/cud", Use(Server))
            .f("CUD/CUD", Use(Server));

        assert_reconciliation(
            reg700_1,
            reg777_1,
            reg770_1,
            EditEdit(ConflictingEdit::Mode, ConflictingEdit::Mode),
        )
        .f("---/---", Unsync)
        .f("-u-/---", Use(Server))
        .f("---/-u-", Use(Client))
        .f("-u-/-u-", Use(Client))
        .f("-U-/-u-", Use(Server))
        .f("-u-/-U-", Use(Client))
        .f("-U-/-U-", Use(Client));

        assert_reconciliation(
            reg777_2,
            reg777_1,
            reg700_1,
            EditEdit(ConflictingEdit::Content, ConflictingEdit::Mode),
        )
        .f("---/---", Unsync)
        .f("-u-/---", Irreconcilable)
        .f("---/-u-", Use(Client))
        .f("-U-/---", Use(Server))
        .f("---/-U-", Use(Client))
        .f("-u-/-u-", Use(Client))
        .f("-U-/-u-", Use(Client))
        .f("-u-/-U-", Use(Client))
        .f("-U-/-U-", Use(Client))
        .f("cud/cud", Use(Client))
        .f("cud/c--", Split(Server, SplitAncestorState::Delete));

        assert_reconciliation(
            reg700_1,
            reg777_1,
            reg777_2,
            EditEdit(ConflictingEdit::Mode, ConflictingEdit::Content),
        )
        .f("---/---", Unsync)
        .f("-u-/---", Use(Server))
        .f("---/-u-", Irreconcilable)
        .f("-U-/---", Use(Server))
        .f("---/-U-", Use(Client))
        .f("-U-/-u-", Use(Server))
        .f("-u-/-U-", Use(Server))
        .f("-U-/-U-", Use(Server))
        .f("cud/cud", Use(Server))
        .f("c--/cud", Split(Server, SplitAncestorState::Delete));

        assert_reconciliation(
            reg777_3,
            reg777_1,
            reg777_2,
            EditEdit(ConflictingEdit::Content, ConflictingEdit::Content),
        )
        .f("---/---", Unsync)
        .f("cud/---", Irreconcilable)
        .f("---/cud", Irreconcilable)
        .f("cud/cud", Split(Server, SplitAncestorState::Delete))
        .f("CUD/cud", Use(Server))
        .f("cud/CUD", Use(Client))
        .f("CUD/CUD", Use(Client)); // Client is newer
        assert_reconciliation(
            reg777_2,
            reg777_1,
            reg777_3,
            EditEdit(ConflictingEdit::Content, ConflictingEdit::Content),
        )
        .f("CUD/CUD", Use(Server)); // Server is newer
        assert_reconciliation(
            reg777_1b,
            reg777_2,
            reg777_1,
            EditEdit(ConflictingEdit::Content, ConflictingEdit::Content),
        )
        .f("CUD/CUD", Use(Client)); // Client wins ties

        // mtime changed to earlier date
        assert_reconciliation(reg777_4t2, reg777_4t2, reg777_4t1, NoConflict)
            .f("---/---", InSync)
            .f("cud/---", Use(Server))
            .f("---/cud", InSync)
            .f("CUD/---", Use(Server))
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Server))
            .f("CUD/cud", Use(Server))
            .f("cud/CUD", Use(Server))
            .f("CUD/CUD", Use(Server));
        assert_reconciliation(reg777_4t1, reg777_4t2, reg777_4t2, NoConflict)
            .f("---/---", InSync)
            .f("cud/---", InSync)
            .f("---/cud", Use(Client))
            .f("CUD/---", Use(Server))
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Client))
            .f("CUD/cud", Use(Client))
            .f("cud/CUD", Use(Client))
            .f("CUD/CUD", Use(Client));
        // mtime changed on both
        assert_reconciliation(reg777_4t1, reg777_4t2, reg777_4t3, NoConflict)
            .f("---/---", InSync)
            .f("cud/---", Use(Server))
            .f("---/cud", InSync)
            .f("CUD/---", Use(Server))
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Server))
            .f("CUD/cud", Use(Server))
            .f("cud/CUD", Use(Server))
            .f("CUD/CUD", Use(Server));
        assert_reconciliation(reg777_4t3, reg777_4t2, reg777_4t1, NoConflict)
            .f("---/---", InSync)
            .f("cud/---", InSync)
            .f("---/cud", Use(Client))
            .f("CUD/---", Use(Server))
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Client))
            .f("CUD/cud", Use(Client))
            .f("cud/CUD", Use(Client))
            .f("CUD/CUD", Use(Client));

        assert_reconciliation(
            dir777,
            dir770,
            dir700,
            EditEdit(ConflictingEdit::Mode, ConflictingEdit::Mode),
        )
        .f("---/---", Unsync)
        .f("cud/---", Use(Server))
        .f("---/cud", Use(Client))
        .f("cud/cud", Use(Client))
        .f("cud/CUD", Use(Client))
        .f("CUD/cud", Use(Server))
        .f("CUD/CUD", Use(Client));

        assert_reconciliation(
            dir777,
            None,
            reg777_1,
            EditEdit(ConflictingEdit::Content, ConflictingEdit::Content),
        )
        .f("---/---", Unsync)
        .f("cud/---", Irreconcilable)
        .f("---/cud", Irreconcilable)
        .f("cud/cud", Split(Server, SplitAncestorState::Delete))
        .f("CUD/---", Split(Client, SplitAncestorState::Move))
        .f("---/CUD", Use(Client))
        .f("cud/cUd", Use(Client));

        assert_reconciliation(dir777, dir777, reg777_1, NoConflict)
            .f("---/---", Unsync)
            .f("cud/---", Split(Client, SplitAncestorState::Move))
            .f("---/cud", Unsync)
            .f("---/CUD", Use(Client))
            .f("cud/cud", Split(Client, SplitAncestorState::Move));

        assert_reconciliation(reg777_1, dir777, dir777, NoConflict)
            .f("---/---", Unsync)
            .f("cud/---", Unsync)
            .f("---/cud", Split(Server, SplitAncestorState::Move))
            .f("CUD/---", Use(Server))
            .f("cud/cud", Split(Server, SplitAncestorState::Move));

        assert_reconciliation(dir777, dir777, None, NoConflict)
            .f("---/---", Unsync)
            .f("cud/---", Use(Server))
            .f("---/cud", Unsync)
            .f("---/CUD", Use(Client))
            .f("cud/cud", Use(Server))
            .f("CUD/CUD", Use(Server));
    }
}

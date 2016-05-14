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

use std::collections::{BTreeMap,HashSet};

use defs::*;
use mutator::*;

enum Reconciliation {
    Nop,
    Download,
    Upload,
    DeleteClient,
    DeleteServer,
    Recurse,
    Conflict,
}

pub fn reconcile<M: Mutator>(mutator: &M) -> Result<()> {
    loop {
        let context = try!(mutator.root());
        let root_hash = try!(reconcile_directory(mutator, &context));
        match mutator.finish(&root_hash) {
            Ok(_) => return Ok(()),
            Err(MutationError::Modified) => {
                mutator.cancel_ancestors();
                continue;
            },
            Err(v) => return Err(v),
        }
    }
}

fn reconcile_directory<M: Mutator>(mutator: &M, context: &M::Context)
                                   -> Result<HashId> {
    let mut client = to_map(try!(mutator.ls_client(context)));
    let mut ancestor = to_map(try!(mutator.ls_ancestor(context)));
    let mut server = to_map(try!(mutator.ls_server(context)));
    let mut names: HashSet<String> = HashSet::new();
    for name in client.keys() { names.insert(name.clone()); }
    for name in server.keys() { names.insert(name.clone()); }

    for name in names.clone() {
        let in_client = client.remove(&name);
        let in_ancestor = ancestor.remove(&name);
        let in_server = server.remove(&name);

        for (out_client, out_ancestor, out_server) in
            try!(reconcile_one(mutator, context,
                               in_client, in_ancestor, in_server,
                               &mut names))
        {
            out_client.map(|v| client.insert(v.name.clone(), v));
            out_ancestor.map(|v| ancestor.insert(v.name.clone(), v));
            out_server.map(|v| server.insert(v.name.clone(), v));
        }
    }

    try!(mutator.put_dir_ancestor(context, ancestor.values()));
    Ok(try!(mutator.put_dir_server(context, server.values())))
}

fn to_map(dir: Vec<DirPtr>) -> BTreeMap<String,DirPtr> {
    dir.into_iter().map(|d| (d.name.clone(), d)).collect()
}

fn reconcile_one<M: Mutator>(
    mutator: &M, context: &M::Context,
    client: Option<DirPtr>, ancestor: Option<DirPtr>, server: Option<DirPtr>,
    names: &mut HashSet<String>)
    -> Result<Vec<(Option<DirPtr>,Option<DirPtr>,Option<DirPtr>)>>
{
    use self::Reconciliation as R;

    fn single(client: Option<DirPtr>, server: Option<DirPtr>)
              -> Vec<(Option<DirPtr>,Option<DirPtr>,Option<DirPtr>)> {
        let ancestor = if DirPtr::matches_opt(&client, &server) {
            server.clone()
        } else {
            None
        };

        vec![(client, ancestor, server)]
    }

    match calc_reconciliation(&SyncTriple { client: client.clone(),
                                            ancestor: ancestor.clone(),
                                            server: server.clone() },
                              context.sync_mode()) {
        R::Nop => Ok(single(client, server)),
        R::Download => {
            try!(mutator.download(context, server.as_ref().unwrap(),
                                  client.as_ref().map(|cli| cli.value)));
            Ok(single(server.clone(), server))
        },
        R::Upload => {
            let value = try!(mutator.upload(context, client.as_ref().unwrap()));
            let res = DirPtr { value: value, .. client.unwrap() };
            Ok(single(Some(res.clone()), Some(res)))
        },
        R::DeleteClient => {
            try!(mutator.rm_client(context, &client.unwrap()));
            assert!(server.is_none());
            Ok(single(None, None))
        },
        R::DeleteServer => {
            assert!(client.is_none());
            Ok(single(None, None))
        },
        R::Recurse => {
            assert!(DirPtrType::Directory == client.as_ref().map_or(
                DirPtrType::Directory, |v| v.typ));
            assert!(DirPtrType::Directory == server.as_ref().map_or(
                DirPtrType::Directory, |v| v.typ));
            loop {
                match reconcile_directory(
                    mutator, &try!(mutator.chdir(
                        context, client.as_ref().or(server.as_ref()).unwrap())))
                {
                    Ok(subhash) => {
                        let res = DirPtr {
                            value: subhash,
                            .. client.as_ref().or(server.as_ref())
                                .unwrap().clone()
                        };
                        return Ok(single(Some(res.clone()), Some(res)));
                    },
                    Err(MutationError::Modified) => continue,
                    Err(e) => return Err(e),
                }
            }
        },
        R::Conflict => {
            let cli = client.unwrap();
            let mut srv = server.unwrap();

            for i in 1.. {
                let new_name = format!("{}~{}", srv.name, i);
                if !names.contains(&new_name) {
                    srv.name = new_name;
                    break;
                }
            }

            // TODO: Should log this resolution
            names.insert(srv.name.clone());

            let mut ret = try!(
                reconcile_one(mutator, context, Some(cli), None, None, names));
            ret.append(&mut try!(
                reconcile_one(mutator, context, None, None, Some(srv), names)));
            Ok(ret)
        },
    }
}

fn calc_reconciliation(inn: &SyncTriple, mode: &SyncMode) -> Reconciliation {
    use self::Reconciliation as Rec;
    enum R {
        Nop, Conflict,
        CreateOut, CreateIn,
        UpdateOut, UpdateIn,
        DeleteOut, DeleteIn,
    }

    let r = match (inn.client.as_ref(), inn.ancestor.as_ref(),
                   inn.server.as_ref()) {
        (None     , _        , None     ) => R::Nop,
        (Some( _ ), None     , None     ) => R::CreateOut,
        (None     , None     , Some( _ )) => R::CreateIn,
        (Some(cli), Some(anc), None     ) if cli.matches(anc) => R::DeleteIn,
        (None     , Some(anc), Some(srv)) if srv.matches(anc) => R::DeleteOut,
        // Resolve edit/delete conflicts via resurrection.
        (Some( _ ), Some( _ ), None     ) => R::CreateOut,
        (None     , Some( _ ), Some( _ )) => R::CreateIn,
        (Some(cli), _        , Some(srv)) if cli.matches(srv) => R::Nop,
        (Some(cli), Some(anc), Some( _ )) if cli.matches(anc) => R::UpdateIn,
        (Some( _ ), Some(anc), Some(srv)) if srv.matches(anc) => R::UpdateOut,
        (Some( _ ), _        , Some( _ )) => R::Conflict,
    };

    fn ip(cond: bool, ifp: Reconciliation) -> Reconciliation {
        if cond { ifp } else { Rec::Nop }
    }

    fn is_dir_or_none(ptr: &Option<DirPtr>) -> bool {
        ptr.as_ref().map_or(true, |p| DirPtrType::Directory == p.typ)
    }

    let is_dir = is_dir_or_none(&inn.client) && is_dir_or_none(&inn.server);

    match r {
        R::Nop => Rec::Nop,
        // edit/edit conflicts and updates to directories are always resolved
        // by recursion.
        R::UpdateIn | R::UpdateOut | R::Conflict if is_dir => Rec::Recurse,
        // For bidirectional updates, edit/edit conflicts are not resolvable at
        // this level.
        R::Conflict if mode.inbound.update && mode.outbound.update =>
            Rec::Conflict,
        // For unidirectional update, resolve the conflict by updating in the
        // one permitted direction
        R::Conflict if mode.inbound.update => Rec::Download,
        R::Conflict if mode.outbound.update => Rec::Upload,
        // If updates are disabled entirely, leave the conflict alone
        R::Conflict => Rec::Nop,
        R::CreateOut => ip(mode.outbound.create, Rec::Upload),
        R::CreateIn => ip(mode.inbound.create, Rec::Download),
        R::UpdateOut => ip(mode.outbound.update, Rec::Upload),
        R::UpdateIn => ip(mode.inbound.update, Rec::Download),
        R::DeleteOut => ip(mode.outbound.delete, Rec::DeleteServer),
        R::DeleteIn => ip(mode.inbound.delete, Rec::DeleteClient),
    }
}

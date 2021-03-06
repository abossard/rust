/* Code to handle method lookups (which can be quite complex) */

import syntax::ast::def_id;
import syntax::ast_map;
import syntax::ast_util::new_def_hash;
import middle::typeck::infer::methods; // next_ty_vars
import dvec::{dvec, extensions};

type candidate = {
    self_ty: ty::t,          // type of a in a.b()
    self_substs: ty::substs, // values for any tvars def'd on the class
    rcvr_ty: ty::t,          // type of receiver in the method def
    n_tps_m: uint,           // number of tvars defined on the method
    fty: ty::t,              // type of the method
    entry: method_map_entry
};

class lookup {
    let fcx: @fn_ctxt;
    let expr: @ast::expr;
    let self_expr: @ast::expr;
    let borrow_scope: ast::node_id;
    let node_id: ast::node_id;
    let m_name: ast::ident;
    let mut self_ty: ty::t;
    let mut derefs: uint;
    let candidates: dvec<candidate>;
    let candidate_impls: hashmap<def_id, ()>;
    let supplied_tps: ~[ty::t];
    let include_private: bool;

    new(fcx: @fn_ctxt,
        expr: @ast::expr,           //expr for a.b in a.b()
        self_expr: @ast::expr,      //a in a.b(...)
        borrow_scope: ast::node_id, //scope to borrow the expr for
        node_id: ast::node_id,      //node id where to store type of fn
        m_name: ast::ident,         //b in a.b(...)
        self_ty: ty::t,             //type of a in a.b(...)
        supplied_tps: ~[ty::t],      //Xs in a.b::<Xs>(...)
        include_private: bool) {

        self.fcx = fcx;
        self.expr = expr;
        self.self_expr = self_expr;
        self.borrow_scope = borrow_scope;
        self.node_id = node_id;
        self.m_name = m_name;
        self.self_ty = self_ty;
        self.derefs = 0u;
        self.candidates = dvec();
        self.candidate_impls = new_def_hash();
        self.supplied_tps = supplied_tps;
        self.include_private = include_private;
    }

    // Entrypoint:
    fn method() -> option<method_map_entry> {
        #debug["method lookup(m_name=%s, self_ty=%s)",
               *self.m_name, self.fcx.infcx.ty_to_str(self.self_ty)];

        loop {
            // First, see whether this is an interface-bounded parameter
            alt ty::get(self.self_ty).struct {
              ty::ty_param(n, did) {
                self.add_candidates_from_param(n, did);
              }
              ty::ty_trait(did, substs) {
                self.add_candidates_from_trait(did, substs);
              }
              ty::ty_class(did, substs) {
                self.add_candidates_from_class(did, substs);
              }
              _ { }
            }

            // if we found anything, stop now.  otherwise continue to
            // loop for impls in scope.  Note: I don't love these
            // semantics, but that's what we had so I am preserving
            // it.
            if self.candidates.len() > 0u { break; }

            // now look for impls in scope, but don't look for impls that
            // would require doing an implicit borrow of the lhs.
            self.add_candidates_from_scope(false);

            // if we found anything, stop before trying borrows
            if self.candidates.len() > 0u { break; }

            // now look for impls in scope that might require a borrow
            self.add_candidates_from_scope(true);

            // if we found anything, stop before attempting auto-deref.
            if self.candidates.len() > 0u { break; }

            // check whether we can autoderef and if so loop around again.
            alt ty::deref(self.tcx(), self.self_ty, false) {
              none { break; }
              some(mt) {
                self.self_ty = mt.ty;
                self.derefs += 1u;
              }
            }
        }

        if self.candidates.len() == 0u { ret none; }

        if self.candidates.len() > 1u {
            self.tcx().sess.span_err(
                self.expr.span,
                "multiple applicable methods in scope");

            for self.candidates.eachi |i, candidate| {
                alt candidate.entry.origin {
                  method_static(did) {
                    self.report_static_candidate(i, did);
                  }
                  method_param(p) {
                    self.report_param_candidate(i, p.trait_id);
                  }
                  method_trait(did, _) {
                    self.report_trait_candidate(i, did);
                  }
                }
            }
        }

        some(self.write_mty_from_candidate(self.candidates[0u]))
    }

    fn tcx() -> ty::ctxt { self.fcx.ccx.tcx }

    fn report_static_candidate(idx: uint, did: ast::def_id) {
        let span = if did.crate == ast::local_crate {
            alt check self.tcx().items.get(did.node) {
              ast_map::node_method(m, _, _) { m.span }
            }
        } else {
            self.expr.span
        };
        self.tcx().sess.span_note(
            span,
            #fmt["candidate #%u is `%s`",
                 (idx+1u),
                 ty::item_path_str(self.tcx(), did)]);
    }

    fn report_param_candidate(idx: uint, did: ast::def_id) {
        self.tcx().sess.span_note(
            self.expr.span,
            #fmt["candidate #%u derives from the bound `%s`",
                 (idx+1u),
                 ty::item_path_str(self.tcx(), did)]);
    }

    fn report_trait_candidate(idx: uint, did: ast::def_id) {
        self.tcx().sess.span_note(
            self.expr.span,
            #fmt["candidate #%u derives from the type of the receiver, \
                  which is the trait `%s`",
                 (idx+1u),
                 ty::item_path_str(self.tcx(), did)]);
    }

    fn add_candidates_from_param(n: uint, did: ast::def_id) {
        #debug["candidates_from_param"];

        let tcx = self.tcx();
        let mut trait_bnd_idx = 0u; // count only trait bounds
        let bounds = tcx.ty_param_bounds.get(did.node);
        for vec::each(*bounds) |bound| {
            let (iid, bound_substs) = alt bound {
              ty::bound_copy | ty::bound_send | ty::bound_const {
                cont; /* ok */
              }
              ty::bound_trait(bound_t) {
                alt check ty::get(bound_t).struct {
                  ty::ty_trait(i, substs) { (i, substs) }
                }
              }
            };

            let ifce_methods = ty::trait_methods(tcx, iid);
            alt vec::position(*ifce_methods, |m| m.ident == self.m_name) {
              none {
                /* check next bound */
                trait_bnd_idx += 1u;
              }

              some(pos) {
                // Replace any appearance of `self` with the type of the
                // generic parameter itself.  Note that this is the only case
                // where this replacement is necessary: in all other cases, we
                // are either invoking a method directly from an impl or class
                // (where the self type is not permitted), or from a trait
                // type (in which case methods that refer to self are not
                // permitted).
                let substs = {self_ty: some(self.self_ty)
                              with bound_substs};

                self.add_candidates_from_m(
                    substs, ifce_methods[pos],
                    method_param({trait_id:iid,
                                  method_num:pos,
                                  param_num:n,
                                  bound_num:trait_bnd_idx}));
              }
            }
        }

    }

    fn add_candidates_from_trait(did: ast::def_id, trait_substs: ty::substs) {

        #debug["method_from_trait"];

        let ms = *ty::trait_methods(self.tcx(), did);
        for ms.eachi |i, m| {
            if m.ident != self.m_name { cont; }

            let m_fty = ty::mk_fn(self.tcx(), m.fty);

            if ty::type_has_self(m_fty) {
                self.tcx().sess.span_err(
                    self.expr.span,
                    "can not call a method that contains a \
                     self type through a boxed iface");
            }

            if (*m.tps).len() > 0u {
                self.tcx().sess.span_err(
                    self.expr.span,
                    "can not call a generic method through a \
                     boxed trait");
            }

            // Note: although it is illegal to invoke a method that uses self
            // through a trait instance, we use a dummy subst here so that we
            // can soldier on with the compilation.
            let substs = {self_ty: some(self.self_ty)
                          with trait_substs};

            self.add_candidates_from_m(
                substs, m, method_trait(did, i));
        }
    }

    fn add_candidates_from_class(did: ast::def_id, class_substs: ty::substs) {

        #debug["method_from_class"];

        let ms = *ty::trait_methods(self.tcx(), did);

        for ms.each |m| {
            if m.ident != self.m_name { cont; }

            if m.vis == ast::private && !self.include_private {
                self.tcx().sess.span_fatal(
                    self.expr.span,
                    "Call to private method not allowed outside \
                     its defining class");
            }

            // look up method named <name>.
            let m_declared = ty::lookup_class_method_by_name(
                self.tcx(), did, self.m_name, self.expr.span);

            self.add_candidates_from_m(
                class_substs, m, method_static(m_declared));
        }
    }

    fn ty_from_did(did: ast::def_id) -> ty::t {
        alt check ty::get(ty::lookup_item_type(self.tcx(), did).ty).struct {
          ty::ty_fn(fty) {
            ty::mk_fn(self.tcx(), {proto: ast::proto_box with fty})
          }
        }
        /*
        if did.crate == ast::local_crate {
            alt check self.tcx().items.get(did.node) {
              ast_map::node_method(m, _, _) {
                // NDM trait/impl regions
                let mt = ty_of_method(self.fcx.ccx, m, ast::rp_none);
                ty::mk_fn(self.tcx(), {proto: ast::proto_box with mt.fty})
              }
            }
        } else {
            alt check ty::get(csearch::get_type(self.tcx(), did).ty).struct {
              ty::ty_fn(fty) {
                ty::mk_fn(self.tcx(), {proto: ast::proto_box with fty})
              }
            }
        }
        */
    }

    fn add_candidates_from_scope(use_assignability: bool) {
        let impls_vecs = self.fcx.ccx.impl_map.get(self.expr.id);
        let mut added_any = false;

        #debug["method_from_scope"];

        for list::each(impls_vecs) |impls| {
            for vec::each(*impls) |im| {
                // Check whether this impl has a method with the right name.
                for im.methods.find(|m| m.ident == self.m_name).each |m| {

                    // determine the `self` of the impl with fresh
                    // variables for each parameter:
                    let {substs: impl_substs, ty: impl_ty} =
                        impl_self_ty(self.fcx, im.did);

                    // Depending on our argument, we find potential
                    // matches either by checking subtypability or
                    // type assignability. Collect the matches.
                    let matches = if use_assignability {
                        self.fcx.can_mk_assignty(
                            self.self_expr, self.borrow_scope,
                            self.self_ty, impl_ty)
                    } else {
                        self.fcx.can_mk_subty(self.self_ty, impl_ty)
                    };
                    #debug["matches = %?", matches];
                    alt matches {
                      result::err(_) { /* keep looking */ }
                      result::ok(_) {
                        if !self.candidate_impls.contains_key(im.did) {
                            let fty = self.ty_from_did(m.did);
                            self.candidates.push(
                                {self_ty: self.self_ty,
                                 self_substs: impl_substs,
                                 rcvr_ty: impl_ty,
                                 n_tps_m: m.n_tps,
                                 fty: fty,
                                 entry: {derefs: self.derefs,
                                         origin: method_static(m.did)}});
                            self.candidate_impls.insert(im.did, ());
                            added_any = true;
                        }
                      }
                    }
                }
            }

            // we want to find the innermost scope that has any
            // matches and then ignore outer scopes
            if added_any {ret;}
        }
    }

    fn add_candidates_from_m(self_substs: ty::substs,
                             m: ty::method,
                             origin: method_origin) {
        let tcx = self.fcx.ccx.tcx;

        // a bit hokey, but the method unbound has a bare protocol, whereas
        // a.b has a protocol like fn@() (perhaps eventually fn&()):
        let fty = ty::mk_fn(tcx, {proto: ast::proto_box with m.fty});

        self.candidates.push(
            {self_ty: self.self_ty,
             self_substs: self_substs,
             rcvr_ty: self.self_ty,
             n_tps_m: (*m.tps).len(),
             fty: fty,
             entry: {derefs: self.derefs, origin: origin}});
    }

    fn write_mty_from_candidate(cand: candidate) -> method_map_entry {
        let tcx = self.fcx.ccx.tcx;

        #debug["write_mty_from_candidate(n_tps_m=%u, fty=%s, entry=%?)",
               cand.n_tps_m,
               self.fcx.infcx.ty_to_str(cand.fty),
               cand.entry];

        // Make the actual receiver type (cand.self_ty) assignable to the
        // required receiver type (cand.rcvr_ty).  If this method is not
        // from an impl, this'll basically be a no-nop.
        alt self.fcx.mk_assignty(self.self_expr, self.borrow_scope,
                                 cand.self_ty, cand.rcvr_ty) {
          result::ok(_) {}
          result::err(_) {
            self.tcx().sess.span_bug(
                self.expr.span,
                #fmt["%s was assignable to %s but now is not?",
                     self.fcx.infcx.ty_to_str(cand.self_ty),
                     self.fcx.infcx.ty_to_str(cand.rcvr_ty)]);
          }
        }

        // Construct the full set of type parameters for the method,
        // which is equal to the class tps + the method tps.
        let n_tps_supplied = self.supplied_tps.len();
        let n_tps_m = cand.n_tps_m;
        let m_substs = {
            if n_tps_supplied == 0u {
                self.fcx.infcx.next_ty_vars(n_tps_m)
            } else if n_tps_m == 0u {
                tcx.sess.span_err(
                    self.expr.span,
                    "this method does not take type parameters");
                self.fcx.infcx.next_ty_vars(n_tps_m)
            } else if n_tps_supplied != n_tps_m {
                tcx.sess.span_err(
                    self.expr.span,
                    "incorrect number of type \
                     parameters given for this method");
                self.fcx.infcx.next_ty_vars(n_tps_m)
            } else {
                self.supplied_tps
            }
        };

        let all_substs = {tps: vec::append(cand.self_substs.tps, m_substs)
                          with cand.self_substs};

        self.fcx.write_ty_substs(self.node_id, cand.fty, all_substs);

        ret cand.entry;
    }
}


//! For each definition, we track the following data. A definition
//! here is defined somewhat circularly as "something with a `DefId`",
//! but it generally corresponds to things like structs, enums, etc.
//! There are also some rather random cases (like const initializer
//! expressions) that are mostly just leftovers.

pub use crate::def_id::DefPathHash;
use crate::def_id::{CrateNum, DefId, DefIndex, LocalDefId, CRATE_DEF_INDEX, LOCAL_CRATE};
use crate::hir;

use rustc_ast::crate_disambiguator::CrateDisambiguator;
use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::stable_hasher::StableHasher;
use rustc_index::vec::IndexVec;
use rustc_span::hygiene::ExpnId;
use rustc_span::symbol::{sym, Symbol};

use log::debug;
use std::fmt::Write;
use std::hash::Hash;

/// The `DefPathTable` maps `DefIndex`es to `DefKey`s and vice versa.
/// Internally the `DefPathTable` holds a tree of `DefKey`s, where each `DefKey`
/// stores the `DefIndex` of its parent.
/// There is one `DefPathTable` for each crate.
#[derive(Clone, Default, RustcDecodable, RustcEncodable)]
pub struct DefPathTable {
    index_to_key: IndexVec<DefIndex, DefKey>,
    def_path_hashes: IndexVec<DefIndex, DefPathHash>,
}

impl DefPathTable {
    fn allocate(&mut self, key: DefKey, def_path_hash: DefPathHash) -> DefIndex {
        let index = {
            let index = DefIndex::from(self.index_to_key.len());
            debug!("DefPathTable::insert() - {:?} <-> {:?}", key, index);
            self.index_to_key.push(key);
            index
        };
        self.def_path_hashes.push(def_path_hash);
        debug_assert!(self.def_path_hashes.len() == self.index_to_key.len());
        index
    }

    pub fn next_id(&self) -> DefIndex {
        DefIndex::from(self.index_to_key.len())
    }

    #[inline(always)]
    pub fn def_key(&self, index: DefIndex) -> DefKey {
        self.index_to_key[index]
    }

    #[inline(always)]
    pub fn def_path_hash(&self, index: DefIndex) -> DefPathHash {
        let hash = self.def_path_hashes[index];
        debug!("def_path_hash({:?}) = {:?}", index, hash);
        hash
    }

    pub fn add_def_path_hashes_to(&self, cnum: CrateNum, out: &mut FxHashMap<DefPathHash, DefId>) {
        out.extend(self.def_path_hashes.iter().enumerate().map(|(index, &hash)| {
            let def_id = DefId { krate: cnum, index: DefIndex::from(index) };
            (hash, def_id)
        }));
    }

    pub fn size(&self) -> usize {
        self.index_to_key.len()
    }
}

/// The definition table containing node definitions.
/// It holds the `DefPathTable` for local `DefId`s/`DefPath`s and it also stores a
/// mapping from `NodeId`s to local `DefId`s.
#[derive(Clone, Default)]
pub struct Definitions {
    table: DefPathTable,

    // FIXME(eddyb) ideally all `LocalDefId`s would be HIR owners.
    pub(super) def_id_to_hir_id: IndexVec<LocalDefId, Option<hir::HirId>>,
    /// The reverse mapping of `def_id_to_hir_id`.
    pub(super) hir_id_to_def_id: FxHashMap<hir::HirId, LocalDefId>,

    /// If `ExpnId` is an ID of some macro expansion,
    /// then `DefId` is the normal module (`mod`) in which the expanded macro was defined.
    parent_modules_of_macro_defs: FxHashMap<ExpnId, DefId>,
    /// Item with a given `LocalDefId` was defined during macro expansion with ID `ExpnId`.
    expansions_that_defined: FxHashMap<LocalDefId, ExpnId>,
    next_disambiguator: FxHashMap<(LocalDefId, DefPathData), u32>,
}

/// A unique identifier that we can use to lookup a definition
/// precisely. It combines the index of the definition's parent (if
/// any) with a `DisambiguatedDefPathData`.
#[derive(Copy, Clone, PartialEq, Debug, RustcEncodable, RustcDecodable)]
pub struct DefKey {
    /// The parent path.
    pub parent: Option<DefIndex>,

    /// The identifier of this node.
    pub disambiguated_data: DisambiguatedDefPathData,
}

impl DefKey {
    fn compute_stable_hash(&self, parent_hash: DefPathHash) -> DefPathHash {
        let mut hasher = StableHasher::new();

        // We hash a `0u8` here to disambiguate between regular `DefPath` hashes,
        // and the special "root_parent" below.
        0u8.hash(&mut hasher);
        parent_hash.hash(&mut hasher);

        let DisambiguatedDefPathData { ref data, disambiguator } = self.disambiguated_data;

        ::std::mem::discriminant(data).hash(&mut hasher);
        if let Some(name) = data.get_opt_name() {
            // Get a stable hash by considering the symbol chars rather than
            // the symbol index.
            name.as_str().hash(&mut hasher);
        }

        disambiguator.hash(&mut hasher);

        DefPathHash(hasher.finish())
    }

    fn root_parent_stable_hash(
        crate_name: &str,
        crate_disambiguator: CrateDisambiguator,
    ) -> DefPathHash {
        let mut hasher = StableHasher::new();
        // Disambiguate this from a regular `DefPath` hash; see `compute_stable_hash()` above.
        1u8.hash(&mut hasher);
        crate_name.hash(&mut hasher);
        crate_disambiguator.hash(&mut hasher);
        DefPathHash(hasher.finish())
    }
}

/// A pair of `DefPathData` and an integer disambiguator. The integer is
/// normally `0`, but in the event that there are multiple defs with the
/// same `parent` and `data`, we use this field to disambiguate
/// between them. This introduces some artificial ordering dependency
/// but means that if you have, e.g., two impls for the same type in
/// the same module, they do get distinct `DefId`s.
#[derive(Copy, Clone, PartialEq, Debug, RustcEncodable, RustcDecodable)]
pub struct DisambiguatedDefPathData {
    pub data: DefPathData,
    pub disambiguator: u32,
}

#[derive(Clone, Debug, RustcEncodable, RustcDecodable)]
pub struct DefPath {
    /// The path leading from the crate root to the item.
    pub data: Vec<DisambiguatedDefPathData>,

    /// The crate root this path is relative to.
    pub krate: CrateNum,
}

impl DefPath {
    pub fn is_local(&self) -> bool {
        self.krate == LOCAL_CRATE
    }

    pub fn make<FN>(krate: CrateNum, start_index: DefIndex, mut get_key: FN) -> DefPath
    where
        FN: FnMut(DefIndex) -> DefKey,
    {
        let mut data = vec![];
        let mut index = Some(start_index);
        loop {
            debug!("DefPath::make: krate={:?} index={:?}", krate, index);
            let p = index.unwrap();
            let key = get_key(p);
            debug!("DefPath::make: key={:?}", key);
            match key.disambiguated_data.data {
                DefPathData::CrateRoot => {
                    assert!(key.parent.is_none());
                    break;
                }
                _ => {
                    data.push(key.disambiguated_data);
                    index = key.parent;
                }
            }
        }
        data.reverse();
        DefPath { data, krate }
    }

    /// Returns a string representation of the `DefPath` without
    /// the crate-prefix. This method is useful if you don't have
    /// a `TyCtxt` available.
    pub fn to_string_no_crate(&self) -> String {
        let mut s = String::with_capacity(self.data.len() * 16);

        for component in &self.data {
            write!(s, "::{}[{}]", component.data.as_symbol(), component.disambiguator).unwrap();
        }

        s
    }

    /// Returns a filename-friendly string for the `DefPath`, with the
    /// crate-prefix.
    pub fn to_string_friendly<F>(&self, crate_imported_name: F) -> String
    where
        F: FnOnce(CrateNum) -> Symbol,
    {
        let crate_name_str = crate_imported_name(self.krate).as_str();
        let mut s = String::with_capacity(crate_name_str.len() + self.data.len() * 16);

        write!(s, "::{}", crate_name_str).unwrap();

        for component in &self.data {
            if component.disambiguator == 0 {
                write!(s, "::{}", component.data.as_symbol()).unwrap();
            } else {
                write!(s, "{}[{}]", component.data.as_symbol(), component.disambiguator).unwrap();
            }
        }

        s
    }

    /// Returns a filename-friendly string of the `DefPath`, without
    /// the crate-prefix. This method is useful if you don't have
    /// a `TyCtxt` available.
    pub fn to_filename_friendly_no_crate(&self) -> String {
        let mut s = String::with_capacity(self.data.len() * 16);

        let mut opt_delimiter = None;
        for component in &self.data {
            s.extend(opt_delimiter);
            opt_delimiter = Some('-');
            if component.disambiguator == 0 {
                write!(s, "{}", component.data.as_symbol()).unwrap();
            } else {
                write!(s, "{}[{}]", component.data.as_symbol(), component.disambiguator).unwrap();
            }
        }
        s
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, RustcEncodable, RustcDecodable)]
pub enum DefPathData {
    // Root: these should only be used for the root nodes, because
    // they are treated specially by the `def_path` function.
    /// The crate root (marker).
    CrateRoot,
    // Catch-all for random `DefId` things like `DUMMY_NODE_ID`.
    Misc,

    // Different kinds of items and item-like things:
    /// An impl.
    Impl,
    /// Something in the type namespace.
    TypeNs(Symbol),
    /// Something in the value namespace.
    ValueNs(Symbol),
    /// Something in the macro namespace.
    MacroNs(Symbol),
    /// Something in the lifetime namespace.
    LifetimeNs(Symbol),
    /// A closure expression.
    ClosureExpr,

    // Subportions of items:
    /// Implicit constructor for a unit or tuple-like struct or enum variant.
    Ctor,
    /// A constant expression (see `{ast,hir}::AnonConst`).
    AnonConst,
    /// An `impl Trait` type node.
    ImplTrait,
}

impl Definitions {
    pub fn def_path_table(&self) -> &DefPathTable {
        &self.table
    }

    /// Gets the number of definitions.
    pub fn def_index_count(&self) -> usize {
        self.table.index_to_key.len()
    }

    pub fn def_key(&self, id: LocalDefId) -> DefKey {
        self.table.def_key(id.local_def_index)
    }

    #[inline(always)]
    pub fn def_path_hash(&self, id: LocalDefId) -> DefPathHash {
        self.table.def_path_hash(id.local_def_index)
    }

    /// Returns the path from the crate root to `index`. The root
    /// nodes are not included in the path (i.e., this will be an
    /// empty vector for the crate root). For an inlined item, this
    /// will be the path of the item in the external crate (but the
    /// path will begin with the path to the external crate).
    pub fn def_path(&self, id: LocalDefId) -> DefPath {
        DefPath::make(LOCAL_CRATE, id.local_def_index, |index| {
            self.def_key(LocalDefId { local_def_index: index })
        })
    }

    #[inline]
    pub fn as_local_hir_id(&self, def_id: LocalDefId) -> hir::HirId {
        self.local_def_id_to_hir_id(def_id)
    }

    #[inline]
    pub fn local_def_id_to_hir_id(&self, id: LocalDefId) -> hir::HirId {
        self.def_id_to_hir_id[id].unwrap()
    }

    #[inline]
    pub fn opt_local_def_id_to_hir_id(&self, id: LocalDefId) -> Option<hir::HirId> {
        self.def_id_to_hir_id[id]
    }

    #[inline]
    pub fn opt_hir_id_to_local_def_id(&self, hir_id: hir::HirId) -> Option<LocalDefId> {
        self.hir_id_to_def_id.get(&hir_id).copied()
    }

    /// Adds a root definition (no parent) and a few other reserved definitions.
    pub fn create_root_def(
        &mut self,
        crate_name: &str,
        crate_disambiguator: CrateDisambiguator,
    ) -> LocalDefId {
        let key = DefKey {
            parent: None,
            disambiguated_data: DisambiguatedDefPathData {
                data: DefPathData::CrateRoot,
                disambiguator: 0,
            },
        };

        let parent_hash = DefKey::root_parent_stable_hash(crate_name, crate_disambiguator);
        let def_path_hash = key.compute_stable_hash(parent_hash);

        // Create the definition.
        let root = LocalDefId { local_def_index: self.table.allocate(key, def_path_hash) };
        assert_eq!(root.local_def_index, CRATE_DEF_INDEX);

        root
    }

    /// Adds a definition with a parent definition.
    pub fn create_def_with_parent(
        &mut self,
        parent: LocalDefId,
        data: DefPathData,
        expn_id: ExpnId,
    ) -> LocalDefId {
        debug!(
            "create_def_with_parent(parent={:?}, data={:?}, expn_id={:?})",
            parent, data, expn_id
        );

        // The root node must be created with `create_root_def()`.
        assert!(data != DefPathData::CrateRoot);

        // Find the next free disambiguator for this key.
        let disambiguator = {
            let next_disamb = self.next_disambiguator.entry((parent, data)).or_insert(0);
            let disambiguator = *next_disamb;
            *next_disamb = next_disamb.checked_add(1).expect("disambiguator overflow");
            disambiguator
        };

        let key = DefKey {
            parent: Some(parent.local_def_index),
            disambiguated_data: DisambiguatedDefPathData { data, disambiguator },
        };

        let parent_hash = self.table.def_path_hash(parent.local_def_index);
        let def_path_hash = key.compute_stable_hash(parent_hash);

        debug!("create_def_with_parent: after disambiguation, key = {:?}", key);

        // Create the definition.
        let def_id = LocalDefId { local_def_index: self.table.allocate(key, def_path_hash) };

        if expn_id != ExpnId::root() {
            self.expansions_that_defined.insert(def_id, expn_id);
        }

        def_id
    }

    /// Initializes the `LocalDefId` to `HirId` mapping once it has been generated during
    /// AST to HIR lowering.
    pub fn init_def_id_to_hir_id_mapping(
        &mut self,
        mapping: IndexVec<LocalDefId, Option<hir::HirId>>,
    ) {
        assert!(
            self.def_id_to_hir_id.is_empty(),
            "trying to initialize `LocalDefId` <-> `HirId` mappings twice"
        );

        // Build the reverse mapping of `def_id_to_hir_id`.
        self.hir_id_to_def_id = mapping
            .iter_enumerated()
            .filter_map(|(def_id, hir_id)| hir_id.map(|hir_id| (hir_id, def_id)))
            .collect();

        self.def_id_to_hir_id = mapping;
    }

    pub fn expansion_that_defined(&self, id: LocalDefId) -> ExpnId {
        self.expansions_that_defined.get(&id).copied().unwrap_or(ExpnId::root())
    }

    pub fn parent_module_of_macro_def(&self, expn_id: ExpnId) -> DefId {
        self.parent_modules_of_macro_defs[&expn_id]
    }

    pub fn add_parent_module_of_macro_def(&mut self, expn_id: ExpnId, module: DefId) {
        self.parent_modules_of_macro_defs.insert(expn_id, module);
    }
}

impl DefPathData {
    pub fn get_opt_name(&self) -> Option<Symbol> {
        use self::DefPathData::*;
        match *self {
            TypeNs(name) | ValueNs(name) | MacroNs(name) | LifetimeNs(name) => Some(name),

            Impl | CrateRoot | Misc | ClosureExpr | Ctor | AnonConst | ImplTrait => None,
        }
    }

    pub fn as_symbol(&self) -> Symbol {
        use self::DefPathData::*;
        match *self {
            TypeNs(name) | ValueNs(name) | MacroNs(name) | LifetimeNs(name) => name,
            // Note that this does not show up in user print-outs.
            CrateRoot => sym::double_braced_crate,
            Impl => sym::double_braced_impl,
            Misc => sym::double_braced_misc,
            ClosureExpr => sym::double_braced_closure,
            Ctor => sym::double_braced_constructor,
            AnonConst => sym::double_braced_constant,
            ImplTrait => sym::double_braced_opaque,
        }
    }

    pub fn to_string(&self) -> String {
        self.as_symbol().to_string()
    }
}

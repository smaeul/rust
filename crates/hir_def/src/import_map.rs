//! A map of all publicly exported items in a crate.

use std::{cmp::Ordering, fmt, hash::BuildHasherDefault, sync::Arc};

use base_db::CrateId;
use fst::{self, Streamer};
use hir_expand::name::Name;
use indexmap::{map::Entry, IndexMap};
use itertools::Itertools;
use rustc_hash::{FxHashSet, FxHasher};

use crate::{
    db::DefDatabase, item_scope::ItemInNs, visibility::Visibility, AssocItemId, ModuleDefId,
    ModuleId, TraitId,
};

type FxIndexMap<K, V> = IndexMap<K, V, BuildHasherDefault<FxHasher>>;

/// Item import details stored in the `ImportMap`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ImportInfo {
    /// A path that can be used to import the item, relative to the crate's root.
    pub path: ImportPath,
    /// The module containing this item.
    pub container: ModuleId,
    /// Whether the import is a trait associated item or not.
    pub is_assoc_item: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ImportPath {
    pub segments: Vec<Name>,
}

impl fmt::Display for ImportPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.segments.iter().format("::"), f)
    }
}

impl ImportPath {
    fn len(&self) -> usize {
        self.segments.len()
    }
}

/// A map from publicly exported items to the path needed to import/name them from a downstream
/// crate.
///
/// Reexports of items are taken into account, ie. if something is exported under multiple
/// names, the one with the shortest import path will be used.
///
/// Note that all paths are relative to the containing crate's root, so the crate name still needs
/// to be prepended to the `ModPath` before the path is valid.
#[derive(Default)]
pub struct ImportMap {
    map: FxIndexMap<ItemInNs, ImportInfo>,

    /// List of keys stored in `map`, sorted lexicographically by their `ModPath`. Indexed by the
    /// values returned by running `fst`.
    ///
    /// Since a path can refer to multiple items due to namespacing, we store all items with the
    /// same path right after each other. This allows us to find all items after the FST gives us
    /// the index of the first one.
    importables: Vec<ItemInNs>,
    fst: fst::Map<Vec<u8>>,
}

impl ImportMap {
    pub fn import_map_query(db: &dyn DefDatabase, krate: CrateId) -> Arc<Self> {
        let _p = profile::span("import_map_query");
        let def_map = db.crate_def_map(krate);
        let mut import_map = Self::default();

        // We look only into modules that are public(ly reexported), starting with the crate root.
        let empty = ImportPath { segments: vec![] };
        let root = ModuleId { krate, local_id: def_map.root };
        let mut worklist = vec![(root, empty)];
        while let Some((module, mod_path)) = worklist.pop() {
            let ext_def_map;
            let mod_data = if module.krate == krate {
                &def_map[module.local_id]
            } else {
                // The crate might reexport a module defined in another crate.
                ext_def_map = db.crate_def_map(module.krate);
                &ext_def_map[module.local_id]
            };

            let visible_items = mod_data.scope.entries().filter_map(|(name, per_ns)| {
                let per_ns = per_ns.filter_visibility(|vis| vis == Visibility::Public);
                if per_ns.is_none() {
                    None
                } else {
                    Some((name, per_ns))
                }
            });

            for (name, per_ns) in visible_items {
                let mk_path = || {
                    let mut path = mod_path.clone();
                    path.segments.push(name.clone());
                    path
                };

                for item in per_ns.iter_items() {
                    let path = mk_path();
                    let path_len = path.len();
                    let import_info = ImportInfo { path, container: module, is_assoc_item: false };

                    // If we've added a path to a trait, add the trait's associated items to the assoc map.
                    if let Some(ModuleDefId::TraitId(tr)) = item.as_module_def_id() {
                        import_map.collect_trait_assoc_items(db, tr, &import_info);
                    }

                    match import_map.map.entry(item) {
                        Entry::Vacant(entry) => {
                            entry.insert(import_info);
                        }
                        Entry::Occupied(mut entry) => {
                            // If the new path is shorter, prefer that one.
                            if path_len < entry.get().path.len() {
                                *entry.get_mut() = import_info;
                            } else {
                                continue;
                            }
                        }
                    }

                    // If we've just added a path to a module, descend into it. We might traverse
                    // modules multiple times, but only if the new path to it is shorter than the
                    // first (else we `continue` above).
                    if let Some(ModuleDefId::ModuleId(mod_id)) = item.as_module_def_id() {
                        worklist.push((mod_id, mk_path()));
                    }
                }
            }
        }

        let mut importables = import_map.map.iter().collect::<Vec<_>>();

        importables.sort_by(cmp);

        // Build the FST, taking care not to insert duplicate values.

        let mut builder = fst::MapBuilder::memory();
        let mut last_batch_start = 0;

        for idx in 0..importables.len() {
            if let Some(next_item) = importables.get(idx + 1) {
                if cmp(&importables[last_batch_start], next_item) == Ordering::Equal {
                    continue;
                }
            }

            let key = fst_path(&importables[last_batch_start].1.path);
            builder.insert(key, last_batch_start as u64).unwrap();

            last_batch_start = idx + 1;
        }

        import_map.fst = fst::Map::new(builder.into_inner().unwrap()).unwrap();
        import_map.importables = importables.iter().map(|(item, _)| **item).collect();

        Arc::new(import_map)
    }

    /// Returns the `ModPath` needed to import/mention `item`, relative to this crate's root.
    pub fn path_of(&self, item: ItemInNs) -> Option<&ImportPath> {
        self.import_info_for(item).map(|it| &it.path)
    }

    pub fn import_info_for(&self, item: ItemInNs) -> Option<&ImportInfo> {
        self.map.get(&item)
    }

    fn collect_trait_assoc_items(
        &mut self,
        db: &dyn DefDatabase,
        tr: TraitId,
        import_info: &ImportInfo,
    ) {
        for (assoc_item_name, item) in db.trait_data(tr).items.iter() {
            let assoc_item = ItemInNs::Types(match item.clone() {
                AssocItemId::FunctionId(f) => f.into(),
                AssocItemId::ConstId(c) => c.into(),
                AssocItemId::TypeAliasId(t) => t.into(),
            });
            let mut assoc_item_info = import_info.to_owned();
            assoc_item_info.path.segments.push(assoc_item_name.to_owned());
            assoc_item_info.is_assoc_item = true;
            self.map.insert(assoc_item, assoc_item_info);
        }
    }
}

impl PartialEq for ImportMap {
    fn eq(&self, other: &Self) -> bool {
        // `fst` and `importables` are built from `map`, so we don't need to compare them.
        self.map == other.map
    }
}

impl Eq for ImportMap {}

impl fmt::Debug for ImportMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut importable_paths: Vec<_> = self
            .map
            .iter()
            .map(|(item, info)| {
                let ns = match item {
                    ItemInNs::Types(_) => "t",
                    ItemInNs::Values(_) => "v",
                    ItemInNs::Macros(_) => "m",
                };
                format!("- {} ({})", info.path, ns)
            })
            .collect();

        importable_paths.sort();
        f.write_str(&importable_paths.join("\n"))
    }
}

fn fst_path(path: &ImportPath) -> String {
    let mut s = path.to_string();
    s.make_ascii_lowercase();
    s
}

fn cmp((_, lhs): &(&ItemInNs, &ImportInfo), (_, rhs): &(&ItemInNs, &ImportInfo)) -> Ordering {
    let lhs_str = fst_path(&lhs.path);
    let rhs_str = fst_path(&rhs.path);
    lhs_str.cmp(&rhs_str)
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub enum ImportKind {
    Module,
    Function,
    Adt,
    EnumVariant,
    Const,
    Static,
    Trait,
    TypeAlias,
    BuiltinType,
}

/// A way to match import map contents against the search query.
#[derive(Debug)]
pub enum SearchMode {
    /// Import map entry should strictly match the query string.
    Equals,
    /// Import map entry should contain the query string.
    Contains,
    /// Import map entry should contain all letters from the query string,
    /// in the same order, but not necessary adjacent.
    Fuzzy,
}

#[derive(Debug)]
pub struct Query {
    query: String,
    lowercased: String,
    name_only: bool,
    search_mode: SearchMode,
    case_sensitive: bool,
    limit: usize,
    exclude_import_kinds: FxHashSet<ImportKind>,
}

impl Query {
    pub fn new(query: String) -> Self {
        let lowercased = query.to_lowercase();
        Self {
            query,
            lowercased,
            name_only: false,
            search_mode: SearchMode::Contains,
            case_sensitive: false,
            limit: usize::max_value(),
            exclude_import_kinds: FxHashSet::default(),
        }
    }

    /// Matches entries' names only, ignoring the rest of
    /// the qualifier.
    /// Example: for `std::marker::PhantomData`, the name is `PhantomData`.
    pub fn name_only(self) -> Self {
        Self { name_only: true, ..self }
    }

    /// Specifies the way to search for the entries using the query.
    pub fn search_mode(self, search_mode: SearchMode) -> Self {
        Self { search_mode, ..self }
    }

    /// Limits the returned number of items to `limit`.
    pub fn limit(self, limit: usize) -> Self {
        Self { limit, ..self }
    }

    /// Respect casing of the query string when matching.
    pub fn case_sensitive(self) -> Self {
        Self { case_sensitive: true, ..self }
    }

    /// Do not include imports of the specified kind in the search results.
    pub fn exclude_import_kind(mut self, import_kind: ImportKind) -> Self {
        self.exclude_import_kinds.insert(import_kind);
        self
    }
}

fn import_matches_query(import: &ImportInfo, query: &Query, enforce_lowercase: bool) -> bool {
    let mut input = if import.is_assoc_item || query.name_only {
        import.path.segments.last().unwrap().to_string()
    } else {
        import.path.to_string()
    };
    if enforce_lowercase || !query.case_sensitive {
        input.make_ascii_lowercase();
    }

    let query_string =
        if !enforce_lowercase && query.case_sensitive { &query.query } else { &query.lowercased };

    match query.search_mode {
        SearchMode::Equals => &input == query_string,
        SearchMode::Contains => input.contains(query_string),
        SearchMode::Fuzzy => {
            let mut unchecked_query_chars = query_string.chars();
            let mut mismatching_query_char = unchecked_query_chars.next();

            for input_char in input.chars() {
                match mismatching_query_char {
                    None => return true,
                    Some(matching_query_char) if matching_query_char == input_char => {
                        mismatching_query_char = unchecked_query_chars.next();
                    }
                    _ => (),
                }
            }
            mismatching_query_char.is_none()
        }
    }
}

/// Searches dependencies of `krate` for an importable path matching `query`.
///
/// This returns a list of items that could be imported from dependencies of `krate`.
pub fn search_dependencies<'a>(
    db: &'a dyn DefDatabase,
    krate: CrateId,
    query: Query,
) -> Vec<ItemInNs> {
    let _p = profile::span("search_dependencies").detail(|| format!("{:?}", query));

    let graph = db.crate_graph();
    let import_maps: Vec<_> =
        graph[krate].dependencies.iter().map(|dep| db.import_map(dep.crate_id)).collect();

    let automaton = fst::automaton::Subsequence::new(&query.lowercased);

    let mut op = fst::map::OpBuilder::new();
    for map in &import_maps {
        op = op.add(map.fst.search(&automaton));
    }

    let mut stream = op.union();
    let mut res = Vec::new();
    while let Some((_, indexed_values)) = stream.next() {
        for indexed_value in indexed_values {
            let import_map = &import_maps[indexed_value.index];
            let importables = &import_map.importables[indexed_value.value as usize..];

            let common_importable_data = &import_map.map[&importables[0]];
            if !import_matches_query(common_importable_data, &query, true) {
                continue;
            }

            // Path shared by the importable items in this group.
            let common_importables_path_fst = fst_path(&common_importable_data.path);
            // Add the items from this `ModPath` group. Those are all subsequent items in
            // `importables` whose paths match `path`.
            let iter = importables
                .iter()
                .copied()
                .take_while(|item| {
                    common_importables_path_fst == fst_path(&import_map.map[item].path)
                })
                .filter(|&item| match item_import_kind(item) {
                    Some(import_kind) => !query.exclude_import_kinds.contains(&import_kind),
                    None => true,
                })
                .filter(|item| {
                    !query.case_sensitive // we've already checked the common importables path case-insensitively
                        || import_matches_query(&import_map.map[item], &query, false)
                });
            res.extend(iter);

            if res.len() >= query.limit {
                res.truncate(query.limit);
                return res;
            }
        }
    }

    res
}

fn item_import_kind(item: ItemInNs) -> Option<ImportKind> {
    Some(match item.as_module_def_id()? {
        ModuleDefId::ModuleId(_) => ImportKind::Module,
        ModuleDefId::FunctionId(_) => ImportKind::Function,
        ModuleDefId::AdtId(_) => ImportKind::Adt,
        ModuleDefId::EnumVariantId(_) => ImportKind::EnumVariant,
        ModuleDefId::ConstId(_) => ImportKind::Const,
        ModuleDefId::StaticId(_) => ImportKind::Static,
        ModuleDefId::TraitId(_) => ImportKind::Trait,
        ModuleDefId::TypeAliasId(_) => ImportKind::TypeAlias,
        ModuleDefId::BuiltinType(_) => ImportKind::BuiltinType,
    })
}

#[cfg(test)]
mod tests {
    use base_db::{fixture::WithFixture, SourceDatabase, Upcast};
    use expect_test::{expect, Expect};
    use stdx::format_to;

    use crate::{data::FunctionData, test_db::TestDB, AssocContainerId, Lookup};

    use super::*;

    fn check_search(ra_fixture: &str, crate_name: &str, query: Query, expect: Expect) {
        let db = TestDB::with_files(ra_fixture);
        let crate_graph = db.crate_graph();
        let krate = crate_graph
            .iter()
            .find(|krate| {
                crate_graph[*krate].display_name.as_ref().map(|n| n.to_string())
                    == Some(crate_name.to_string())
            })
            .unwrap();

        let actual = search_dependencies(db.upcast(), krate, query)
            .into_iter()
            .filter_map(|item| {
                let mark = match item {
                    ItemInNs::Types(ModuleDefId::FunctionId(_))
                    | ItemInNs::Values(ModuleDefId::FunctionId(_)) => "f",
                    ItemInNs::Types(_) => "t",
                    ItemInNs::Values(_) => "v",
                    ItemInNs::Macros(_) => "m",
                };
                item.krate(db.upcast()).map(|krate| {
                    let map = db.import_map(krate);

                    let path = match assoc_to_trait(&db, item) {
                        Some(trait_) => {
                            let mut full_path = map.path_of(trait_).unwrap().to_string();
                            if let ItemInNs::Types(ModuleDefId::FunctionId(function_id))
                            | ItemInNs::Values(ModuleDefId::FunctionId(function_id)) = item
                            {
                                format_to!(
                                    full_path,
                                    "::{}",
                                    FunctionData::fn_data_query(&db, function_id).name,
                                );
                            }
                            full_path
                        }
                        None => map.path_of(item).unwrap().to_string(),
                    };

                    format!(
                        "{}::{} ({})\n",
                        crate_graph[krate].display_name.as_ref().unwrap(),
                        path,
                        mark
                    )
                })
            })
            .collect::<String>();
        expect.assert_eq(&actual)
    }

    fn assoc_to_trait(db: &dyn DefDatabase, item: ItemInNs) -> Option<ItemInNs> {
        let assoc: AssocItemId = match item {
            ItemInNs::Types(it) | ItemInNs::Values(it) => match it {
                ModuleDefId::TypeAliasId(it) => it.into(),
                ModuleDefId::FunctionId(it) => it.into(),
                ModuleDefId::ConstId(it) => it.into(),
                _ => return None,
            },
            _ => return None,
        };

        let container = match assoc {
            AssocItemId::FunctionId(it) => it.lookup(db).container,
            AssocItemId::ConstId(it) => it.lookup(db).container,
            AssocItemId::TypeAliasId(it) => it.lookup(db).container,
        };

        match container {
            AssocContainerId::TraitId(it) => Some(ItemInNs::Types(it.into())),
            _ => None,
        }
    }

    fn check(ra_fixture: &str, expect: Expect) {
        let db = TestDB::with_files(ra_fixture);
        let crate_graph = db.crate_graph();

        let actual = crate_graph
            .iter()
            .filter_map(|krate| {
                let cdata = &crate_graph[krate];
                let name = cdata.display_name.as_ref()?;

                let map = db.import_map(krate);

                Some(format!("{}:\n{:?}\n", name, map))
            })
            .collect::<String>();

        expect.assert_eq(&actual)
    }

    #[test]
    fn smoke() {
        check(
            r"
            //- /main.rs crate:main deps:lib

            mod private {
                pub use lib::Pub;
                pub struct InPrivateModule;
            }

            pub mod publ1 {
                use lib::Pub;
            }

            pub mod real_pub {
                pub use lib::Pub;
            }
            pub mod real_pu2 { // same path length as above
                pub use lib::Pub;
            }

            //- /lib.rs crate:lib
            pub struct Pub {}
            pub struct Pub2; // t + v
            struct Priv;
        ",
            expect![[r#"
                main:
                - publ1 (t)
                - real_pu2 (t)
                - real_pub (t)
                - real_pub::Pub (t)
                lib:
                - Pub (t)
                - Pub2 (t)
                - Pub2 (v)
            "#]],
        );
    }

    #[test]
    fn prefers_shortest_path() {
        check(
            r"
            //- /main.rs crate:main

            pub mod sub {
                pub mod subsub {
                    pub struct Def {}
                }

                pub use super::sub::subsub::Def;
            }
        ",
            expect![[r#"
                main:
                - sub (t)
                - sub::Def (t)
                - sub::subsub (t)
            "#]],
        );
    }

    #[test]
    fn type_reexport_cross_crate() {
        // Reexports need to be visible from a crate, even if the original crate exports the item
        // at a shorter path.
        check(
            r"
            //- /main.rs crate:main deps:lib
            pub mod m {
                pub use lib::S;
            }
            //- /lib.rs crate:lib
            pub struct S;
        ",
            expect![[r#"
                main:
                - m (t)
                - m::S (t)
                - m::S (v)
                lib:
                - S (t)
                - S (v)
            "#]],
        );
    }

    #[test]
    fn macro_reexport() {
        check(
            r"
            //- /main.rs crate:main deps:lib
            pub mod m {
                pub use lib::pub_macro;
            }
            //- /lib.rs crate:lib
            #[macro_export]
            macro_rules! pub_macro {
                () => {};
            }
        ",
            expect![[r#"
                main:
                - m (t)
                - m::pub_macro (m)
                lib:
                - pub_macro (m)
            "#]],
        );
    }

    #[test]
    fn module_reexport() {
        // Reexporting modules from a dependency adds all contents to the import map.
        check(
            r"
            //- /main.rs crate:main deps:lib
            pub use lib::module as reexported_module;
            //- /lib.rs crate:lib
            pub mod module {
                pub struct S;
            }
        ",
            expect![[r#"
                main:
                - reexported_module (t)
                - reexported_module::S (t)
                - reexported_module::S (v)
                lib:
                - module (t)
                - module::S (t)
                - module::S (v)
            "#]],
        );
    }

    #[test]
    fn cyclic_module_reexport() {
        // A cyclic reexport does not hang.
        check(
            r"
            //- /lib.rs crate:lib
            pub mod module {
                pub struct S;
                pub use super::sub::*;
            }

            pub mod sub {
                pub use super::module;
            }
        ",
            expect![[r#"
                lib:
                - module (t)
                - module::S (t)
                - module::S (v)
                - sub (t)
            "#]],
        );
    }

    #[test]
    fn private_macro() {
        check(
            r"
            //- /lib.rs crate:lib
            macro_rules! private_macro {
                () => {};
            }
        ",
            expect![[r#"
                lib:

            "#]],
        );
    }

    #[test]
    fn namespacing() {
        check(
            r"
            //- /lib.rs crate:lib
            pub struct Thing;     // t + v
            #[macro_export]
            macro_rules! Thing {  // m
                () => {};
            }
        ",
            expect![[r#"
                lib:
                - Thing (m)
                - Thing (t)
                - Thing (v)
            "#]],
        );

        check(
            r"
            //- /lib.rs crate:lib
            pub mod Thing {}      // t
            #[macro_export]
            macro_rules! Thing {  // m
                () => {};
            }
        ",
            expect![[r#"
                lib:
                - Thing (m)
                - Thing (t)
            "#]],
        );
    }

    #[test]
    fn fuzzy_import_trait() {
        let ra_fixture = r#"
        //- /main.rs crate:main deps:dep
        //- /dep.rs crate:dep
        pub mod fmt {
            pub trait Display {
                fn format();
            }
        }
    "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_string()).search_mode(SearchMode::Fuzzy),
            expect![[r#"
                dep::fmt (t)
                dep::fmt::Display (t)
                dep::fmt::Display::format (f)
            "#]],
        );
    }

    #[test]
    fn search_mode() {
        let ra_fixture = r#"
            //- /main.rs crate:main deps:dep
            //- /dep.rs crate:dep deps:tdep
            use tdep::fmt as fmt_dep;
            pub mod fmt {
                pub trait Display {
                    fn fmt();
                }
            }
            #[macro_export]
            macro_rules! Fmt {
                () => {};
            }
            pub struct Fmt;

            pub fn format() {}
            pub fn no() {}

            //- /tdep.rs crate:tdep
            pub mod fmt {
                pub struct NotImportableFromMain;
            }
        "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_string()).search_mode(SearchMode::Fuzzy),
            expect![[r#"
                dep::fmt (t)
                dep::Fmt (t)
                dep::Fmt (v)
                dep::Fmt (m)
                dep::fmt::Display (t)
                dep::fmt::Display::fmt (f)
                dep::format (f)
            "#]],
        );

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_string()).search_mode(SearchMode::Equals),
            expect![[r#"
                dep::fmt (t)
                dep::Fmt (t)
                dep::Fmt (v)
                dep::Fmt (m)
                dep::fmt::Display::fmt (f)
            "#]],
        );

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_string()).search_mode(SearchMode::Contains),
            expect![[r#"
                dep::fmt (t)
                dep::Fmt (t)
                dep::Fmt (v)
                dep::Fmt (m)
                dep::fmt::Display (t)
                dep::fmt::Display::fmt (f)
            "#]],
        );
    }

    #[test]
    fn name_only() {
        let ra_fixture = r#"
            //- /main.rs crate:main deps:dep
            //- /dep.rs crate:dep deps:tdep
            use tdep::fmt as fmt_dep;
            pub mod fmt {
                pub trait Display {
                    fn fmt();
                }
            }
            #[macro_export]
            macro_rules! Fmt {
                () => {};
            }
            pub struct Fmt;

            pub fn format() {}
            pub fn no() {}

            //- /tdep.rs crate:tdep
            pub mod fmt {
                pub struct NotImportableFromMain;
            }
        "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_string()),
            expect![[r#"
                dep::fmt (t)
                dep::Fmt (t)
                dep::Fmt (v)
                dep::Fmt (m)
                dep::fmt::Display (t)
                dep::fmt::Display::fmt (f)
            "#]],
        );

        check_search(
            ra_fixture,
            "main",
            Query::new("fmt".to_string()).name_only(),
            expect![[r#"
                dep::fmt (t)
                dep::Fmt (t)
                dep::Fmt (v)
                dep::Fmt (m)
                dep::fmt::Display::fmt (f)
            "#]],
        );
    }

    #[test]
    fn search_casing() {
        let ra_fixture = r#"
            //- /main.rs crate:main deps:dep
            //- /dep.rs crate:dep

            pub struct fmt;
            pub struct FMT;
        "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("FMT".to_string()),
            expect![[r#"
                dep::fmt (t)
                dep::fmt (v)
                dep::FMT (t)
                dep::FMT (v)
            "#]],
        );

        check_search(
            ra_fixture,
            "main",
            Query::new("FMT".to_string()).case_sensitive(),
            expect![[r#"
                dep::FMT (t)
                dep::FMT (v)
            "#]],
        );
    }

    #[test]
    fn search_limit() {
        check_search(
            r#"
        //- /main.rs crate:main deps:dep
        //- /dep.rs crate:dep
        pub mod fmt {
            pub trait Display {
                fn fmt();
            }
        }
        #[macro_export]
        macro_rules! Fmt {
            () => {};
        }
        pub struct Fmt;

        pub fn format() {}
        pub fn no() {}
    "#,
            "main",
            Query::new("".to_string()).limit(2),
            expect![[r#"
                dep::fmt (t)
                dep::Fmt (t)
            "#]],
        );
    }

    #[test]
    fn search_exclusions() {
        let ra_fixture = r#"
            //- /main.rs crate:main deps:dep
            //- /dep.rs crate:dep

            pub struct fmt;
            pub struct FMT;
        "#;

        check_search(
            ra_fixture,
            "main",
            Query::new("FMT".to_string()),
            expect![[r#"
                dep::fmt (t)
                dep::fmt (v)
                dep::FMT (t)
                dep::FMT (v)
            "#]],
        );

        check_search(
            ra_fixture,
            "main",
            Query::new("FMT".to_string()).exclude_import_kind(ImportKind::Adt),
            expect![[r#""#]],
        );
    }
}

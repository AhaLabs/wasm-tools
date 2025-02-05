use crate::ast::lex::Span;
use crate::{
    Document, DocumentId, Error, Function, Interface, InterfaceId, Results, Type, TypeDef,
    TypeDefKind, TypeId, TypeOwner, UnresolvedPackage, World, WorldId, WorldItem,
};
use anyhow::{anyhow, bail, Context, Result};
use id_arena::{Arena, Id};
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::mem;
use std::path::{Path, PathBuf};
use url::Url;

/// Representation of a fully resolved set of WIT packages.
///
/// This structure contains a graph of WIT packages and all of their contents
/// merged together into the contained arenas. All items are sorted
/// topologically and everything here is fully resolved, so with a `Resolve` no
/// name lookups are necessary and instead everything is index-based.
///
/// Working with a WIT package requires inserting it into a `Resolve` to ensure
/// that all of its dependencies are satisfied. This will give the full picture
/// of that package's types and such.
///
/// Each item in a `Resolve` has a parent link to trace it back to the original
/// package as necessary.
#[derive(Default, Clone)]
pub struct Resolve {
    pub worlds: Arena<World>,
    pub interfaces: Arena<Interface>,
    pub types: Arena<TypeDef>,
    pub documents: Arena<Document>,
    pub packages: Arena<Package>,
}

#[derive(Clone)]
pub struct Package {
    /// Locally-known name of this package.
    pub name: String,

    /// Optionally-specified URL of this package, must be specified for remote
    /// dependencies.
    pub url: Option<String>,

    /// Documents contained within this package, organized by name.
    pub documents: IndexMap<String, DocumentId>,
}

pub type PackageId = Id<Package>;

impl Resolve {
    /// Creates a new [`Resolve`] with no packages/items inside of it.
    pub fn new() -> Resolve {
        Resolve::default()
    }

    /// Parses the filesystem directory at `path` as a WIT package and returns
    /// the fully resolved [`PackageId`] as a result.
    ///
    /// This method is intended for testing and convenience for now and isn't
    /// the only way to push packages into this [`Resolve`]. This will
    /// interpret `path` as a directory where all `*.wit` files in that
    /// directory are members of the package.
    ///
    /// Dependencies referenced by the WIT package at `path` will be loaded from
    /// a `deps/$name` directory under `path` where `$name` is the name of the
    /// dependency loaded. If `deps/$name` does not exist then an error will be
    /// returned indicating that the dependency is not defined. All dependencies
    /// are listed in a flat namespace under `$path/deps` so they can refer to
    /// each other.
    ///
    /// This function returns the [`PackageId`] of the root parsed package at
    /// `path`, along with a list of all paths that were consumed during parsing
    /// for the root package and all dependency packages.
    pub fn push_dir(&mut self, path: &Path) -> Result<(PackageId, Vec<PathBuf>)> {
        // Maintain a `to_parse` stack of packages that have yet to be parsed
        // along with an `enqueued` set of all the prior parsed packages and
        // packages enqueued to be parsed. These are then used to fill the
        // `packages` map with parsed, but unresolved, packages. The `pkg_deps`
        // map then tracks dependencies between packages.
        let mut to_parse = Vec::new();
        let mut enqueued = HashSet::new();
        let mut packages = IndexMap::new();
        let mut pkg_deps = IndexMap::new();
        to_parse.push((path.to_path_buf(), None));
        enqueued.insert(path.to_path_buf());
        while let Some((pkg_root, url)) = to_parse.pop() {
            let mut pkg = UnresolvedPackage::parse_dir(&pkg_root)
                .with_context(|| format!("failed to parse package: {}", path.display()))?;
            pkg.url = url;

            let mut deps = Vec::new();
            pkg.source_map.rewrite_error(|| {
                for (i, (dep, _)) in pkg.foreign_deps.iter().enumerate() {
                    let path = path.join("deps").join(dep);
                    let span = pkg.foreign_dep_spans[i];
                    // If this is the first request to parse `path` then push it
                    // onto our stack, otherwise it's already there so skip it.
                    if enqueued.insert(path.clone()) {
                        if !path.is_dir() {
                            bail!(Error {
                                span,
                                msg: format!(
                                    "dependency on `{dep}` doesn't exist at: {}",
                                    path.display()
                                ),
                            })
                        }
                        let url = Some(format!("path:/{dep}"));
                        to_parse.push((path.clone(), url));
                    }
                    deps.push((path, span));
                }
                Ok(())
            })?;

            let prev = packages.insert(pkg_root.clone(), pkg);
            assert!(prev.is_none());
            pkg_deps.insert(pkg_root, deps);
        }

        // Perform a simple topological sort which will bail out on cycles
        // and otherwise determine the order that packages must be added to
        // this `Resolve`.
        let mut order = IndexSet::new();
        let mut visiting = HashSet::new();
        for (dep, _) in pkg_deps.iter() {
            visit(dep, &pkg_deps, &packages, &mut order, &mut visiting)?;
        }

        // Using the topological ordering insert each package incrementally.
        // Additionally note that the last item visited here is the root
        // package, which is the one returned here.
        let mut package_ids = IndexMap::new();
        let mut last = None;
        let mut files = Vec::new();
        for path in order {
            let pkg = packages.remove(path).unwrap();
            let mut deps = HashMap::new();
            for ((dep, _), (path, _span)) in pkg.foreign_deps.iter().zip(&pkg_deps[path]) {
                deps.insert(dep.clone(), package_ids[&**path]);
            }
            files.extend(pkg.source_files().map(|p| p.to_path_buf()));
            let pkgid = self.push(pkg, &deps)?;
            package_ids.insert(path, pkgid);
            last = Some(pkgid);
        }

        return Ok((last.unwrap(), files));

        fn visit<'a>(
            path: &'a Path,
            deps: &'a IndexMap<PathBuf, Vec<(PathBuf, Span)>>,
            pkgs: &IndexMap<PathBuf, UnresolvedPackage>,
            order: &mut IndexSet<&'a Path>,
            visiting: &mut HashSet<&'a Path>,
        ) -> Result<()> {
            if order.contains(path) {
                return Ok(());
            }
            pkgs[path].source_map.rewrite_error(|| {
                for (dep, span) in deps[path].iter() {
                    if !visiting.insert(dep) {
                        bail!(Error {
                            span: *span,
                            msg: format!("package depends on itself"),
                        });
                    }
                    visit(dep, deps, pkgs, order, visiting)?;
                    assert!(visiting.remove(&**dep));
                }
                assert!(order.insert(path));
                Ok(())
            })
        }
    }

    /// Appends a new [`UnresolvedPackage`] to this [`Resolve`], creating a
    /// fully resolved package with no dangling references.
    ///
    /// The `deps` argument indicates that the named dependencies in
    /// `unresolved` to packages are resolved by the mapping specified.
    ///
    /// Any dependency resolution error or otherwise world-elaboration error
    /// will be returned here. If successful a package identifier is returned.
    pub fn push(
        &mut self,
        mut unresolved: UnresolvedPackage,
        deps: &HashMap<String, PackageId>,
    ) -> Result<PackageId> {
        let source_map = mem::take(&mut unresolved.source_map);
        source_map.rewrite_error(|| Remap::default().append(self, unresolved, deps))
    }

    pub fn all_bits_valid(&self, ty: &Type) -> bool {
        match ty {
            Type::U8
            | Type::S8
            | Type::U16
            | Type::S16
            | Type::U32
            | Type::S32
            | Type::U64
            | Type::S64
            | Type::Float32
            | Type::Float64 => true,

            Type::Bool | Type::Char | Type::String => false,

            Type::Id(id) => match &self.types[*id].kind {
                TypeDefKind::List(_)
                | TypeDefKind::Variant(_)
                | TypeDefKind::Enum(_)
                | TypeDefKind::Option(_)
                | TypeDefKind::Result(_)
                | TypeDefKind::Future(_)
                | TypeDefKind::Stream(_)
                | TypeDefKind::Union(_) => false,
                TypeDefKind::Type(t) => self.all_bits_valid(t),
                TypeDefKind::Record(r) => r.fields.iter().all(|f| self.all_bits_valid(&f.ty)),
                TypeDefKind::Tuple(t) => t.types.iter().all(|t| self.all_bits_valid(t)),

                // FIXME: this could perhaps be `true` for multiples-of-32 but
                // seems better to probably leave this as unconditionally
                // `false` for now, may want to reconsider later?
                TypeDefKind::Flags(_) => false,

                TypeDefKind::Unknown => unreachable!(),
            },
        }
    }

    /// Merges all the contents of a different `Resolve` into this one. The
    /// `Remap` structure returned provides a mapping from all old indices to
    /// new indices
    ///
    /// This operation can fail if `resolve` disagrees with `self` about the
    /// packages being inserted. Otherwise though this will additionally attempt
    /// to "union" packages found in `resolve` with those found in `self`.
    /// Unioning packages is keyed on the name/url of packages for those with
    /// URLs present. If found then it's assumed that both `Resolve` instances
    /// were originally created from the same contents and are two views
    /// of the same package.
    pub fn merge(&mut self, resolve: Resolve) -> Result<Remap> {
        log::trace!(
            "merging {} packages into {} packages",
            resolve.packages.len(),
            self.packages.len()
        );

        let mut map = MergeMap::new(&resolve, &self)?;
        map.build()?;
        let MergeMap {
            package_map,
            interface_map,
            type_map,
            doc_map,
            world_map,
            documents_to_add,
            interfaces_to_add,
            worlds_to_add,
            ..
        } = map;

        // With a set of maps from ids in `resolve` to ids in `self` the next
        // operation is to start moving over items and building a `Remap` to
        // update ids.
        //
        // Each component field of `resolve` is moved into `self` so long as
        // its ID is not within one of the maps above. If it's present in a map
        // above then that means the item is already present in `self` so a new
        // one need not be added. If it's not present in a map that means it's
        // not present in `self` so it must be added to an arena.
        //
        // When adding an item to an arena one of the `remap.update_*` methods
        // is additionally called to update all identifiers from pointers within
        // `resolve` to becoming pointers within `self`.
        //
        // Altogether this should weave all the missing items in `self` from
        // `resolve` into one structure while updating all identifiers to
        // be local within `self`.

        let mut remap = Remap::default();
        let Resolve {
            types,
            worlds,
            interfaces,
            documents,
            packages,
        } = resolve;

        let mut moved_types = Vec::new();
        for (id, mut ty) in types {
            let new_id = type_map.get(&id).copied().unwrap_or_else(|| {
                moved_types.push(id);
                remap.update_typedef(&mut ty);
                self.types.alloc(ty)
            });
            assert_eq!(remap.types.len(), id.index());
            remap.types.push(new_id);
        }

        let mut moved_interfaces = Vec::new();
        for (id, mut iface) in interfaces {
            let new_id = interface_map.get(&id).copied().unwrap_or_else(|| {
                moved_interfaces.push(id);
                remap.update_interface(&mut iface);
                self.interfaces.alloc(iface)
            });
            assert_eq!(remap.interfaces.len(), id.index());
            remap.interfaces.push(new_id);
        }

        let mut moved_worlds = Vec::new();
        for (id, mut world) in worlds {
            let new_id = world_map.get(&id).copied().unwrap_or_else(|| {
                moved_worlds.push(id);
                for (_, item) in world.imports.iter_mut().chain(&mut world.exports) {
                    match item {
                        WorldItem::Function(f) => remap.update_function(f),
                        WorldItem::Interface(i) => *i = remap.interfaces[i.index()],
                        WorldItem::Type(i) => *i = remap.types[i.index()],
                    }
                }
                self.worlds.alloc(world)
            });
            assert_eq!(remap.worlds.len(), id.index());
            remap.worlds.push(new_id);
        }

        let mut moved_documents = Vec::new();
        for (id, mut doc) in documents {
            let new_id = doc_map.get(&id).copied().unwrap_or_else(|| {
                moved_documents.push(id);
                remap.update_document(&mut doc);
                self.documents.alloc(doc)
            });
            assert_eq!(remap.documents.len(), id.index());
            remap.documents.push(new_id);
        }

        for (id, mut pkg) in packages {
            for (_, doc) in pkg.documents.iter_mut() {
                *doc = remap.documents[doc.index()];
            }
            let new_id = package_map
                .get(&id)
                .copied()
                .unwrap_or_else(|| self.packages.alloc(pkg));
            assert_eq!(remap.packages.len(), id.index());
            remap.packages.push(new_id);
        }

        // Fixup all "parent" links now.
        //
        // Note that this is only done for items that are actually moved from
        // `resolve` into `self`, which is tracked by the various `moved_*`
        // lists built incrementally above. The ids in the `moved_*` lists
        // are ids within `resolve`, so they're translated through `remap` to
        // ids within `self`.
        for id in moved_documents {
            let id = remap.documents[id.index()];
            if let Some(pkg) = &mut self.documents[id].package {
                *pkg = remap.packages[pkg.index()];
            }
        }
        for id in moved_worlds {
            let id = remap.worlds[id.index()];
            let doc = &mut self.worlds[id].document;
            *doc = remap.documents[doc.index()];
        }
        for id in moved_interfaces {
            let id = remap.interfaces[id.index()];
            let doc = &mut self.interfaces[id].document;
            *doc = remap.documents[doc.index()];
        }
        for id in moved_types {
            let id = remap.types[id.index()];
            match &mut self.types[id].owner {
                TypeOwner::Interface(id) => *id = remap.interfaces[id.index()],
                TypeOwner::World(id) => *id = remap.worlds[id.index()],
                TypeOwner::None => {}
            }
        }

        // And finally process documents that were present in `resolve` but were
        // not present in `self`. This is only done for merged packages as
        // documents may be added to `self.documents` but wouldn't otherwise be
        // present in the `documents` field of the corresponding package.
        for (name, pkg, doc) in documents_to_add {
            let prev = self.packages[pkg]
                .documents
                .insert(name, remap.documents[doc.index()]);
            assert!(prev.is_none());
        }
        for (name, doc, iface) in interfaces_to_add {
            let prev = self.documents[doc]
                .interfaces
                .insert(name, remap.interfaces[iface.index()]);
            assert!(prev.is_none());
        }
        for (name, doc, world) in worlds_to_add {
            let prev = self.documents[doc]
                .worlds
                .insert(name, remap.worlds[world.index()]);
            assert!(prev.is_none());
        }

        log::trace!("now have {} packages", self.packages.len());
        Ok(remap)
    }

    /// Merges the world `from` into the world `into`.
    ///
    /// This will attempt to merge one world into another, unioning all of its
    /// imports and exports together. This is an operation performed by
    /// `wit-component`, for example where two different worlds from two
    /// different libraries were linked into the same core wasm file and are
    /// producing a singular world that will be the final component's
    /// interface.
    ///
    /// This operation can fail if the imports/exports overlap.
    pub fn merge_worlds(&mut self, from: WorldId, into: WorldId) -> Result<()> {
        let mut new_imports = Vec::new();
        let mut new_exports = Vec::new();

        let from_world = &self.worlds[from];
        let into_world = &self.worlds[into];

        // Build a map of the imports/exports in `into` going the reverse
        // direction from what's listed. This is then consulted below to ensure
        // that the same item isn't exported or imported under two different
        // names which isn't allowed in the component model.
        let mut into_imports_by_id = HashMap::new();
        let mut into_exports_by_id = HashMap::new();
        for (name, import) in into_world.imports.iter() {
            if let WorldItem::Interface(id) = *import {
                let prev = into_imports_by_id.insert(id, name);
                assert!(prev.is_none());
            }
        }
        for (name, export) in into_world.exports.iter() {
            if let WorldItem::Interface(id) = *export {
                let prev = into_exports_by_id.insert(id, name);
                assert!(prev.is_none());
            }
        }
        for (name, import) in from_world.imports.iter() {
            // If the "from" world imports an interface which is already
            // imported by the "into" world then this is allowed if the names
            // are the same. Importing the same interface under different names
            // isn't allowed, but otherwise merging imports of
            // same-named-interfaces is allowed to merge them together.
            if let WorldItem::Interface(id) = import {
                if let Some(prev) = into_imports_by_id.get(id) {
                    if *prev != name {
                        bail!("import `{name}` conflicts with previous name of `{prev}`");
                    }
                }
            }
        }
        for (name, export) in from_world.exports.iter() {
            // Note that unlike imports same-named exports are not handled here
            // since if something is exported twice there's no way to "unify" it
            // so it's left as an error.
            if let WorldItem::Interface(id) = export {
                if let Some(prev) = into_exports_by_id.get(id) {
                    bail!("export `{name}` conflicts with previous name of `{prev}`");
                }
            }
        }

        // Next walk over the interfaces imported into `from_world` and queue up
        // imports to get inserted into `into_world`.
        for (name, from_import) in from_world.imports.iter() {
            match into_world.imports.get(name) {
                Some(into_import) => match (from_import, into_import) {
                    // If these imports, which have the same name, are of the
                    // same interface then union them together at this point.
                    (WorldItem::Interface(from), WorldItem::Interface(into)) if from == into => {
                        continue
                    }
                    _ => bail!("duplicate import found for interface `{name}`"),
                },
                None => new_imports.push((name.clone(), from_import.clone())),
            }
        }

        // All exports at this time must be unique. For example the same
        // interface exported from two locations can't really be resolved to one
        // canonical definition, so make sure that merging worlds only succeeds
        // if the worlds have disjoint sets of exports.
        for (name, export) in from_world.exports.iter() {
            match into_world.exports.get(name) {
                Some(_) => bail!("duplicate export found for interface `{name}`"),
                None => new_exports.push((name.clone(), export.clone())),
            }
        }

        // Insert any new imports and new exports found first.
        let into = &mut self.worlds[into];
        for (name, import) in new_imports {
            let prev = into.imports.insert(name, import);
            assert!(prev.is_none());
        }
        for (name, export) in new_exports {
            let prev = into.exports.insert(name, export);
            assert!(prev.is_none());
        }

        Ok(())
    }

    /// Returns the URL of the specified `interface`, if available.
    ///
    /// This currently creates a URL based on the URL of the package that
    /// `interface` resides in. If the package owner of `interface` does not
    /// specify a URL then `None` will be returned.
    ///
    /// If the `interface` specified does not have a name then `None` will be
    /// returned as well.
    pub fn url_of(&self, interface: InterfaceId) -> Option<String> {
        let interface = &self.interfaces[interface];
        let doc = &self.documents[interface.document];
        let package = &self.packages[doc.package.unwrap()];
        let mut base = Url::parse(package.url.as_ref()?).unwrap();
        base.path_segments_mut()
            .unwrap()
            .push(&doc.name)
            .push(interface.name.as_ref()?);
        Some(base.to_string())
    }

    /// Attempts to locate a default world for the `pkg` specified within this
    /// [`Resolve`]. Optionally takes a string-based `world` "specifier" to
    /// resolve the world.
    ///
    /// This is intended for use by bindings generators and such as the default
    /// logic for locating a world within a package used for binding. The
    /// `world` argument is typically a user-specified argument (which again is
    /// optional and not required) where the `pkg` is determined ambiently by
    /// the integration.
    ///
    /// If `world` is `None` (e.g. not specified by a user) then the package
    /// must have exactly one `default world` within its documents, otherwise an
    /// error will be returned. If `world` is `Some` then it's a `.`-separated
    /// name where the first element is the name of the document and the second,
    /// optional, element is the name of the `world`. For example the name `foo`
    /// would mean the `default world` of the `foo` document. The name `foo.bar`
    /// would mean the world named `bar` in the `foo` document.
    pub fn select_world(&self, pkg: PackageId, world: Option<&str>) -> Result<WorldId> {
        match world {
            Some(world) => {
                let mut parts = world.splitn(2, '.');
                let doc = parts.next().unwrap();
                let world = parts.next();
                let doc = *self.packages[pkg]
                    .documents
                    .get(doc)
                    .ok_or_else(|| anyhow!("no document named `{doc}` in package"))?;
                match world {
                    Some(name) => self.documents[doc]
                        .worlds
                        .get(name)
                        .copied()
                        .ok_or_else(|| anyhow!("no world named `{name}` in document")),
                    None => self.documents[doc]
                        .default_world
                        .ok_or_else(|| anyhow!("no default world in document")),
                }
            }
            None => {
                if self.packages[pkg].documents.is_empty() {
                    bail!("no documents found in package")
                }

                let mut unique_default_world = None;
                for (_name, doc) in &self.documents {
                    if let Some(default_world) = doc.default_world {
                        if unique_default_world.is_some() {
                            bail!("multiple default worlds found in package, one must be specified")
                        } else {
                            unique_default_world = Some(default_world);
                        }
                    }
                }

                unique_default_world.ok_or_else(|| anyhow!("no default world in package"))
            }
        }
    }
}

/// Structure returned by [`Resolve::merge`] which contains mappings from
/// old-ids to new-ids after the merge.
#[derive(Default)]
pub struct Remap {
    pub types: Vec<TypeId>,
    pub interfaces: Vec<InterfaceId>,
    pub worlds: Vec<WorldId>,
    pub documents: Vec<DocumentId>,
    pub packages: Vec<PackageId>,
}

impl Remap {
    fn append(
        &mut self,
        resolve: &mut Resolve,
        unresolved: UnresolvedPackage,
        deps: &HashMap<String, PackageId>,
    ) -> Result<PackageId> {
        self.process_foreign_deps(resolve, &unresolved, deps)?;

        let foreign_types = self.types.len();
        let foreign_interfaces = self.interfaces.len();
        let foreign_documents = self.documents.len();
        let foreign_worlds = self.worlds.len();

        // Copy over all types first, updating any intra-type references. Note
        // that types are sorted topologically which means this iteration
        // order should be sufficient. Also note though that the interface
        // owner of a type isn't updated here due to interfaces not being known
        // yet.
        for (id, mut ty) in unresolved.types.into_iter().skip(foreign_types) {
            self.update_typedef(&mut ty);
            let new_id = resolve.types.alloc(ty);
            assert_eq!(self.types.len(), id.index());
            self.types.push(new_id);
        }

        // Next transfer all interfaces into `Resolve`, updating type ids
        // referenced along the way.
        for (id, mut iface) in unresolved.interfaces.into_iter().skip(foreign_interfaces) {
            self.update_interface(&mut iface);
            let new_id = resolve.interfaces.alloc(iface);
            assert_eq!(self.interfaces.len(), id.index());
            self.interfaces.push(new_id);
        }

        // Now that interfaces are identified go back through the types and
        // update their interface owners.
        for id in self.types.iter().skip(foreign_types) {
            match &mut resolve.types[*id].owner {
                TypeOwner::Interface(id) => *id = self.interfaces[id.index()],
                TypeOwner::World(_) | TypeOwner::None => {}
            }
        }

        // Perform a weighty step of full resolution of worlds. This will fully
        // expand imports/exports for a world and create the topological
        // ordering necessary for this.
        //
        // This is done after types/interfaces are fully settled so the
        // transitive relation between interfaces, through types, is understood
        // here.
        assert_eq!(unresolved.worlds.len(), unresolved.world_spans.len());
        for ((id, mut world), (import_spans, export_spans)) in unresolved
            .worlds
            .into_iter()
            .skip(foreign_worlds)
            .zip(unresolved.world_spans)
        {
            self.update_world(&mut world, resolve, &import_spans, &export_spans)?;
            let new_id = resolve.worlds.alloc(world);
            assert_eq!(self.worlds.len(), id.index());
            self.worlds.push(new_id);
        }

        // As with interfaces, now update the ids of world-owned types.
        for id in self.types.iter().skip(foreign_types) {
            match &mut resolve.types[*id].owner {
                TypeOwner::World(id) => *id = self.worlds[id.index()],
                TypeOwner::Interface(_) | TypeOwner::None => {}
            }
        }

        // And the final major step is transferring documents to `Resolve`
        // which is just updating a few identifiers here and there.
        for (id, mut doc) in unresolved.documents.into_iter().skip(foreign_documents) {
            self.update_document(&mut doc);
            let new_id = resolve.documents.alloc(doc);
            assert_eq!(self.documents.len(), id.index());
            self.documents.push(new_id);
        }

        // Fixup "parent" ids now that everything has been identifier
        for id in self.interfaces.iter().skip(foreign_interfaces) {
            let doc = &mut resolve.interfaces[*id].document;
            *doc = self.documents[doc.index()];
        }
        for id in self.worlds.iter().skip(foreign_worlds) {
            let doc = &mut resolve.worlds[*id].document;
            *doc = self.documents[doc.index()];
        }
        let mut documents = IndexMap::new();
        for id in self.documents.iter().skip(foreign_documents) {
            let prev = documents.insert(resolve.documents[*id].name.clone(), *id);
            assert!(prev.is_none());
        }
        let pkgid = resolve.packages.alloc(Package {
            name: unresolved.name,
            url: unresolved.url,
            documents,
        });
        for (_, id) in resolve.packages[pkgid].documents.iter() {
            resolve.documents[*id].package = Some(pkgid);
        }
        Ok(pkgid)
    }

    fn process_foreign_deps(
        &mut self,
        resolve: &mut Resolve,
        unresolved: &UnresolvedPackage,
        deps: &HashMap<String, PackageId>,
    ) -> Result<()> {
        // First, connect all references to foreign documents to actual
        // documents within `resolve`, building up the initial entries of
        // the `self.documents` mapping.
        let mut document_to_package = HashMap::new();
        for (i, (pkg, docs)) in unresolved.foreign_deps.iter().enumerate() {
            for (doc, unresolved_doc_id) in docs {
                let prev = document_to_package.insert(
                    *unresolved_doc_id,
                    (pkg, doc, unresolved.foreign_dep_spans[i]),
                );
                assert!(prev.is_none());
            }
        }
        for (unresolved_doc_id, _doc) in unresolved.documents.iter() {
            let (pkg, doc, span) = match document_to_package.get(&unresolved_doc_id) {
                Some(items) => *items,
                None => break,
            };
            let pkgid = *deps.get(pkg).ok_or_else(|| Error {
                span,
                msg: format!("no package dependency specified for `{pkg}`"),
            })?;
            let package = &resolve.packages[pkgid];

            let docid = *package.documents.get(doc).ok_or_else(|| Error {
                span: unresolved.document_spans[unresolved_doc_id.index()],
                msg: format!("package `{pkg}` does not define document `{doc}`"),
            })?;

            assert_eq!(self.documents.len(), unresolved_doc_id.index());
            self.documents.push(docid);
        }
        for (id, _) in unresolved.documents.iter().skip(self.documents.len()) {
            assert!(
                document_to_package.get(&id).is_none(),
                "found foreign document after local documents"
            );
        }

        // Next, for all documents that are referenced in this `Resolve`
        // determine the mapping of all interfaces that they refer to.
        for (unresolved_iface_id, unresolved_iface) in unresolved.interfaces.iter() {
            let doc_id = match self.documents.get(unresolved_iface.document.index()) {
                Some(i) => *i,
                // All foreign interfaces are defined first, so the first one
                // which is defined in a non-foreign document means that all
                // further interfaces will be non-foreign as well.
                None => break,
            };

            // Functions can't be imported so this should be empty.
            assert!(unresolved_iface.functions.is_empty());

            let document = &resolve.documents[doc_id];
            let span = unresolved.interface_spans[unresolved_iface_id.index()];
            let iface_id = match &unresolved_iface.name {
                Some(name) => *document.interfaces.get(name).ok_or_else(|| Error {
                    span,
                    msg: format!("interface not defined in document"),
                })?,
                None => document.default_interface.ok_or_else(|| Error {
                    span,
                    msg: format!("default interface not specified in document"),
                })?,
            };
            assert_eq!(self.interfaces.len(), unresolved_iface_id.index());
            self.interfaces.push(iface_id);
        }

        for (_, iface) in unresolved.interfaces.iter().skip(self.interfaces.len()) {
            if self.documents.get(iface.document.index()).is_some() {
                panic!("found foreign interface after local interfaces");
            }
        }

        // And finally iterate over all foreign-defined types and determine
        // what they map to.
        for (unresolved_type_id, unresolved_ty) in unresolved.types.iter() {
            // All "Unknown" types should appear first so once we're no longer
            // in unknown territory it's package-defined types so break out of
            // this loop.
            match unresolved_ty.kind {
                TypeDefKind::Unknown => {}
                _ => break,
            }
            let unresolved_iface_id = match unresolved_ty.owner {
                TypeOwner::Interface(id) => id,
                _ => unreachable!(),
            };
            let iface_id = self.interfaces[unresolved_iface_id.index()];
            let name = unresolved_ty.name.as_ref().unwrap();
            let span = unresolved.unknown_type_spans[unresolved_type_id.index()];
            let type_id = *resolve.interfaces[iface_id]
                .types
                .get(name)
                .ok_or_else(|| Error {
                    span,
                    msg: format!("type not defined in interface"),
                })?;
            assert_eq!(self.types.len(), unresolved_type_id.index());
            self.types.push(type_id);
        }

        for (_, ty) in unresolved.types.iter().skip(self.types.len()) {
            if let TypeDefKind::Unknown = ty.kind {
                panic!("unknown type after defined type");
            }
        }

        Ok(())
    }

    fn update_typedef(&self, ty: &mut TypeDef) {
        // NB: note that `ty.owner` is not updated here since interfaces
        // haven't been mapped yet and that's done in a separate step.
        use crate::TypeDefKind::*;
        match &mut ty.kind {
            Record(r) => {
                for field in r.fields.iter_mut() {
                    self.update_ty(&mut field.ty);
                }
            }
            Tuple(t) => {
                for ty in t.types.iter_mut() {
                    self.update_ty(ty);
                }
            }
            Variant(v) => {
                for case in v.cases.iter_mut() {
                    if let Some(t) = &mut case.ty {
                        self.update_ty(t);
                    }
                }
            }
            Option(t) => self.update_ty(t),
            Result(r) => {
                if let Some(ty) = &mut r.ok {
                    self.update_ty(ty);
                }
                if let Some(ty) = &mut r.err {
                    self.update_ty(ty);
                }
            }
            Union(u) => {
                for case in u.cases.iter_mut() {
                    self.update_ty(&mut case.ty);
                }
            }
            List(t) => self.update_ty(t),
            Future(Some(t)) => self.update_ty(t),
            Stream(t) => {
                if let Some(ty) = &mut t.element {
                    self.update_ty(ty);
                }
                if let Some(ty) = &mut t.end {
                    self.update_ty(ty);
                }
            }
            Type(t) => self.update_ty(t),

            // nothing to do for these as they're just names or empty
            Flags(_) | Enum(_) | Future(None) => {}

            Unknown => unreachable!(),
        }
    }

    fn update_ty(&self, ty: &mut Type) {
        if let Type::Id(id) = ty {
            *id = self.types[id.index()];
        }
    }

    fn update_interface(&self, iface: &mut Interface) {
        // NB: note that `iface.doc` is not updated here since interfaces
        // haven't been mapped yet and that's done in a separate step.
        for (_name, ty) in iface.types.iter_mut() {
            *ty = self.types[ty.index()];
        }
        for (_, func) in iface.functions.iter_mut() {
            self.update_function(func);
        }
    }

    fn update_function(&self, func: &mut Function) {
        for (_, ty) in func.params.iter_mut() {
            self.update_ty(ty);
        }
        match &mut func.results {
            Results::Named(named) => {
                for (_, ty) in named.iter_mut() {
                    self.update_ty(ty);
                }
            }
            Results::Anon(ty) => self.update_ty(ty),
        }
    }

    fn update_world(
        &self,
        world: &mut World,
        resolve: &Resolve,
        import_spans: &[Span],
        export_spans: &[Span],
    ) -> Result<()> {
        // NB: this function is more more complicated than the prior versions
        // of merging an item because this is the location that elaboration of
        // imports/exports of a world are fully resolved. With full transitive
        // knowledge of all interfaces a worlds imports, for example, are
        // expanded fully to ensure that all transitive items are necessarily
        // imported.
        assert_eq!(world.imports.len(), import_spans.len());
        assert_eq!(world.exports.len(), export_spans.len());

        // First up, process all the `imports` of the world. Note that this
        // starts by gutting the list of imports stored in `world` to get
        // rebuilt iteratively below.
        //
        // Here each import of an interface is recorded and then additionally
        // explicitly named imports of interfaces are recorded as well for
        // determining names later on.
        let mut explicit_import_names = HashMap::new();
        let mut explicit_export_names = HashMap::new();
        let mut imports = Vec::new();
        let mut exports = Vec::new();
        let mut import_funcs = Vec::new();
        let mut export_funcs = Vec::new();
        let mut import_types = Vec::new();
        for ((name, item), span) in mem::take(&mut world.imports).into_iter().zip(import_spans) {
            match item {
                WorldItem::Interface(id) => {
                    let id = self.interfaces[id.index()];
                    imports.push((id, *span));
                    let prev = explicit_import_names.insert(id, name);
                    assert!(prev.is_none());
                }
                WorldItem::Function(mut f) => {
                    self.update_function(&mut f);
                    import_funcs.push((name, f, *span));
                }
                WorldItem::Type(id) => {
                    let id = self.types[id.index()];
                    import_types.push((name, id, *span));
                }
            }
        }
        for ((name, item), span) in mem::take(&mut world.exports).into_iter().zip(export_spans) {
            match item {
                WorldItem::Interface(id) => {
                    let id = self.interfaces[id.index()];
                    exports.push((id, *span));
                    let prev = explicit_export_names.insert(id, name);
                    assert!(prev.is_none());
                }
                WorldItem::Function(mut f) => {
                    self.update_function(&mut f);
                    export_funcs.push((name, f, *span));
                }
                WorldItem::Type(_) => unreachable!(),
            }
        }

        // Next all imports and their transitive imports are processed. This
        // is done through a `stack` of `Action` items which is processed in
        // LIFO order, meaning that an action of processing the dependencies
        // is pushed after processing the node itself. The dependency processing
        // will push more items onto the stack as necessary.
        let mut elaborate = WorldElaborator {
            resolve,
            world,
            imports_processed: Default::default(),
            exports_processed: Default::default(),
            resolving_stack: Default::default(),
            explicit_import_names: &explicit_import_names,
            explicit_export_names: &explicit_export_names,
            names: Default::default(),
        };
        for (id, span) in imports {
            elaborate.import(id, span)?;
        }
        for (_name, id, span) in import_types.iter() {
            if let TypeDefKind::Type(Type::Id(other)) = resolve.types[*id].kind {
                if let TypeOwner::Interface(owner) = resolve.types[other].owner {
                    elaborate.import(owner, *span)?;
                }
            }
        }
        for (id, span) in exports {
            elaborate.export(id, span)?;
        }

        for (name, id, span) in import_types {
            let prev = elaborate
                .world
                .imports
                .insert(name.clone(), WorldItem::Type(id));
            if prev.is_some() {
                bail!(Error {
                    msg: format!("export of type `{name}` shadows previously imported interface"),
                    span,
                })
            }
        }

        for (name, func, span) in import_funcs {
            let prev = world
                .imports
                .insert(name.clone(), WorldItem::Function(func));
            if prev.is_some() {
                bail!(Error {
                    msg: format!(
                        "import of function `{name}` shadows previously imported interface"
                    ),
                    span,
                })
            }
        }
        for (name, func, span) in export_funcs {
            let prev = world
                .exports
                .insert(name.clone(), WorldItem::Function(func));
            if prev.is_some() {
                bail!(Error {
                    msg: format!(
                        "export of function `{name}` shadows previously exported interface"
                    ),
                    span,
                })
            }
        }

        log::trace!("imports = {:?}", world.imports);
        log::trace!("exports = {:?}", world.exports);

        Ok(())
    }

    fn update_document(&self, doc: &mut Document) {
        for (_name, iface) in doc.interfaces.iter_mut() {
            *iface = self.interfaces[iface.index()];
        }
        for (_name, world) in doc.worlds.iter_mut() {
            *world = self.worlds[world.index()];
        }
        if let Some(default) = &mut doc.default_interface {
            *default = self.interfaces[default.index()];
        }
        if let Some(default) = &mut doc.default_world {
            *default = self.worlds[default.index()];
        }
    }
}

struct WorldElaborator<'a, 'b> {
    resolve: &'a Resolve,
    world: &'b mut World,
    explicit_import_names: &'a HashMap<InterfaceId, String>,
    explicit_export_names: &'a HashMap<InterfaceId, String>,
    names: HashMap<String, bool>,

    /// Set of imports which are either imported into the world already or in
    /// the `stack` to get processed, used to ensure the same dependency isn't
    /// pushed multiple times into the stack.
    imports_processed: HashSet<InterfaceId>,
    exports_processed: HashSet<InterfaceId>,

    /// Dependency chain of why we're importing the top of `stack`, used to
    /// print an error message.
    resolving_stack: Vec<(InterfaceId, bool)>,
}

impl<'a> WorldElaborator<'a, '_> {
    fn import(&mut self, id: InterfaceId, span: Span) -> Result<()> {
        self.recurse(id, span, true)
    }

    fn export(&mut self, id: InterfaceId, span: Span) -> Result<()> {
        self.recurse(id, span, false)
    }

    fn recurse(&mut self, id: InterfaceId, span: Span, import: bool) -> Result<()> {
        let processed = if import {
            &mut self.imports_processed
        } else {
            &mut self.exports_processed
        };
        if !processed.insert(id) {
            return Ok(());
        }

        self.resolving_stack.push((id, import));
        for (_, ty) in self.resolve.interfaces[id].types.iter() {
            let ty = match self.resolve.types[*ty].kind {
                TypeDefKind::Type(Type::Id(id)) => id,
                _ => continue,
            };
            let dep = match self.resolve.types[ty].owner {
                TypeOwner::None => continue,
                TypeOwner::Interface(other) => other,
                TypeOwner::World(_) => unreachable!(),
            };
            let import = import || !self.explicit_export_names.contains_key(&dep);

            self.recurse(dep, span, import)?;
        }
        assert_eq!(self.resolving_stack.pop(), Some((id, import)));

        let name = self.name_of(id, import);
        let prev = self.names.insert(name.clone(), import);

        if prev.is_none() {
            let set = if import {
                &mut self.world.imports
            } else {
                &mut self.world.exports
            };
            let prev = set.insert(name.clone(), WorldItem::Interface(id));
            assert!(prev.is_none());
            return Ok(());
        }

        let desc = |import: bool| {
            if import {
                "import"
            } else {
                "export"
            }
        };

        let mut msg = format!("{} of `{}`", desc(import), self.name_of(id, import));
        if self.resolving_stack.is_empty() {
            msg.push_str(" ");
        } else {
            msg.push_str("\n");
        }
        for (i, import) in self.resolving_stack.iter().rev() {
            writeln!(
                msg,
                "  .. which is depended on by {} `{}`",
                desc(*import),
                self.name_of(*i, *import)
            )
            .unwrap();
        }
        writeln!(
            msg,
            "conflicts with a previous interface using the name `{name}`",
        )
        .unwrap();
        bail!(Error { span, msg })
    }

    fn name_of(&self, id: InterfaceId, import: bool) -> &'a String {
        let set = if import {
            &self.explicit_import_names
        } else {
            &self.explicit_export_names
        };
        set.get(&id)
            .unwrap_or_else(|| self.resolve.interfaces[id].name.as_ref().unwrap())
    }
}

struct MergeMap<'a> {
    /// A map of package ids in `from` to those in `into` for those that are
    /// found to be equivalent.
    package_map: HashMap<PackageId, PackageId>,

    /// A map of interface ids in `from` to those in `into` for those that are
    /// found to be equivalent.
    interface_map: HashMap<InterfaceId, InterfaceId>,

    /// A map of type ids in `from` to those in `into` for those that are
    /// found to be equivalent.
    type_map: HashMap<TypeId, TypeId>,

    /// A map of document ids in `from` to those in `into` for those that are
    /// found to be equivalent.
    doc_map: HashMap<DocumentId, DocumentId>,

    /// A map of world ids in `from` to those in `into` for those that are
    /// found to be equivalent.
    world_map: HashMap<WorldId, WorldId>,

    /// A list of documents that need to be added to packages in `into`.
    ///
    /// The elements here are:
    ///
    /// * The name of the document
    /// * The ID within `into` of the package being added to
    /// * The ID within `from` of the document being added.
    documents_to_add: Vec<(String, PackageId, DocumentId)>,
    interfaces_to_add: Vec<(String, DocumentId, InterfaceId)>,
    worlds_to_add: Vec<(String, DocumentId, WorldId)>,

    /// Which `Resolve` is being merged from.
    from: &'a Resolve,

    /// Which `Resolve` is being merged into.
    into: &'a Resolve,

    /// A cache of packages, keyed by name/url, within `into`.
    packages_in_into: HashMap<(&'a String, &'a Option<String>), PackageId>,
}

impl<'a> MergeMap<'a> {
    fn new(from: &'a Resolve, into: &'a Resolve) -> Result<MergeMap<'a>> {
        let mut packages_in_into = HashMap::new();
        for (id, package) in into.packages.iter() {
            log::trace!("previous package {}/{:?}", package.name, package.url);
            if package.url.is_none() {
                continue;
            }
            let prev = packages_in_into.insert((&package.name, &package.url), id);
            if prev.is_some() {
                bail!(
                    "found duplicate name/url combination in current resolve: {}/{:?}",
                    package.name,
                    package.url
                );
            }
        }
        Ok(MergeMap {
            package_map: Default::default(),
            interface_map: Default::default(),
            type_map: Default::default(),
            doc_map: Default::default(),
            world_map: Default::default(),
            documents_to_add: Default::default(),
            interfaces_to_add: Default::default(),
            worlds_to_add: Default::default(),
            from,
            into,
            packages_in_into,
        })
    }

    fn build(&mut self) -> Result<()> {
        for (from_id, from) in self.from.packages.iter() {
            let into_id = match self.packages_in_into.get(&(&from.name, &from.url)) {
                Some(id) => *id,

                // This package, according to its name and url, is not present
                // in `self` so it needs to get added below.
                None => {
                    log::trace!("adding unique package {} / {:?}", from.name, from.url);
                    continue;
                }
            };
            log::trace!("merging duplicate package {} / {:?}", from.name, from.url);

            self.build_package(from_id, into_id).with_context(|| {
                format!("failed to merge package `{}` into existing copy", from.name)
            })?;
        }

        Ok(())
    }

    fn build_package(&mut self, from_id: PackageId, into_id: PackageId) -> Result<()> {
        let prev = self.package_map.insert(from_id, into_id);
        assert!(prev.is_none());

        let from = &self.from.packages[from_id];
        let into = &self.into.packages[into_id];

        // All documents in `from` should already be present in `into` to get
        // merged, or it's assumed `self.from` contains a view of the package
        // which happens to contain more files. In this situation the job of
        // merging will be to add a new document to the package within
        // `self.into` which is queued up with `self.documents_to_add`.
        for (name, from_id) in from.documents.iter() {
            let into_id = match into.documents.get(name) {
                Some(id) => *id,
                None => {
                    self.documents_to_add
                        .push((name.clone(), into_id, *from_id));
                    continue;
                }
            };

            self.build_document(*from_id, into_id)
                .with_context(|| format!("failed to merge document `{name}` into existing copy"))?;
        }

        Ok(())
    }

    fn build_document(&mut self, from_id: DocumentId, into_id: DocumentId) -> Result<()> {
        let prev = self.doc_map.insert(from_id, into_id);
        assert!(prev.is_none());

        let from_doc = &self.from.documents[from_id];
        let into_doc = &self.into.documents[into_id];

        // Like documents above if an interface is present in `from_id` but not
        // present in `into_id` then it can be copied over wholesale. That
        // copy is scheduled to happen within the `self.interfaces_to_add` list.
        for (name, from_interface_id) in from_doc.interfaces.iter() {
            let into_interface_id = match into_doc.interfaces.get(name) {
                Some(id) => *id,
                None => {
                    self.interfaces_to_add
                        .push((name.clone(), into_id, *from_interface_id));
                    continue;
                }
            };

            self.build_interface(*from_interface_id, into_interface_id)
                .with_context(|| format!("failed to merge interface `{name}`"))?;
        }

        for (name, from_world_id) in from_doc.worlds.iter() {
            let into_world_id = match into_doc.worlds.get(name) {
                Some(id) => *id,
                None => {
                    self.worlds_to_add
                        .push((name.clone(), into_id, *from_world_id));
                    continue;
                }
            };

            self.build_world(*from_world_id, into_world_id)
                .with_context(|| format!("failed to merge world `{name}`"))?;
        }
        Ok(())
    }

    fn build_interface(&mut self, from_id: InterfaceId, into_id: InterfaceId) -> Result<()> {
        let prev = self.interface_map.insert(from_id, into_id);
        assert!(prev.is_none());

        let from_interface = &self.from.interfaces[from_id];
        let into_interface = &self.into.interfaces[into_id];

        // Unlike documents/interfaces above if an interface in `from`
        // differs from the interface in `into` then that's considered an
        // error. Changing interfaces can reflect changes in imports/exports
        // which may not be expected so it's currently required that all
        // interfaces, when merged, exactly match.
        //
        // One case to consider here, for example, is that if a world in
        // `into` exports the interface `into_id` then if `from_id` were to
        // add more items into `into` then it would unexpectedly require more
        // items to be exported which may not work. In an import context this
        // might work since it's "just more items available for import", but
        // for now a conservative route of "interfaces must match" is taken.

        for (name, from_type_id) in from_interface.types.iter() {
            let into_type_id = *into_interface
                .types
                .get(name)
                .ok_or_else(|| anyhow!("expected type `{name}` to be present"))?;
            let prev = self.type_map.insert(*from_type_id, into_type_id);
            assert!(prev.is_none());

            // FIXME: ideally the types should be "structurally
            // equal" but that's not trivial to do in the face of
            // resources.
        }

        for (name, _) in from_interface.functions.iter() {
            if !into_interface.functions.contains_key(name) {
                bail!("expected function `{name}` to be present");
            }

            // FIXME: ideally the functions should be "structurally
            // equal" but that's not trivial to do in the face of
            // resources.
        }

        Ok(())
    }

    fn build_world(&mut self, from_id: WorldId, into_id: WorldId) -> Result<()> {
        let prev = self.world_map.insert(from_id, into_id);
        assert!(prev.is_none());

        let from_world = &self.from.worlds[from_id];
        let into_world = &self.into.worlds[into_id];

        // Same as interfaces worlds are expected to exactly match to avoid
        // unexpectedly changing a particular component's view of imports and
        // exports.
        //
        // FIXME: this should probably share functionality with
        // `Resolve::merge_worlds` to support adding imports but not changing
        // exports.

        if from_world.imports.len() != into_world.imports.len() {
            bail!("world contains different number of imports than expected");
        }
        if from_world.exports.len() != into_world.exports.len() {
            bail!("world contains different number of exports than expected");
        }

        for (name, from) in from_world.imports.iter() {
            let into = into_world
                .imports
                .get(name)
                .ok_or_else(|| anyhow!("import `{name}` not found in target world"))?;
            self.match_world_item(from, into)
                .with_context(|| format!("import `{name}` didn't match target world"))?;
        }

        for (name, from) in from_world.exports.iter() {
            let into = into_world
                .exports
                .get(name)
                .ok_or_else(|| anyhow!("export `{name}` not found in target world"))?;
            self.match_world_item(from, into)
                .with_context(|| format!("export `{name}` didn't match target world"))?;
        }

        Ok(())
    }

    fn match_world_item(&mut self, from: &WorldItem, into: &WorldItem) -> Result<()> {
        match (from, into) {
            (WorldItem::Interface(from), WorldItem::Interface(into)) => {
                match (
                    &self.from.interfaces[*from].name,
                    &self.into.interfaces[*into].name,
                ) {
                    // If one interface is unnamed then they must both be
                    // unnamed and they must both have the same structure for
                    // now.
                    (None, None) => self.build_interface(*from, *into)?,

                    // Otherwise both interfaces must be named and they must
                    // have been previously found to be equivalent. Note that
                    // if either is unnamed it won't be present in
                    // `interface_map` so this'll return an error.
                    _ => {
                        if self.interface_map.get(&from) != Some(&into) {
                            bail!("interfaces are not the same");
                        }
                    }
                }
            }
            (WorldItem::Function(from), WorldItem::Function(into)) => {
                drop((from, into));
                // FIXME: should assert an check that `from` structurally
                // matches `into`
            }
            (WorldItem::Type(from), WorldItem::Type(into)) => {
                // FIXME: should assert an check that `from` structurally
                // matches `into`
                let prev = self.type_map.insert(*from, *into);
                assert!(prev.is_none());
            }

            (WorldItem::Interface(_), _)
            | (WorldItem::Function(_), _)
            | (WorldItem::Type(_), _) => {
                bail!("world items do not have the same type")
            }
        }
        Ok(())
    }
}

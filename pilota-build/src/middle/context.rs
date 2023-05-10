use std::{ops::Deref, path::PathBuf, sync::Arc};

use anyhow::Context as _;
use dashmap::DashMap;
use faststr::FastStr;
use fxhash::{FxHashMap, FxHashSet};
use heck::ToShoutySnakeCase;
use itertools::Itertools;
use normpath::PathExt;
use quote::format_ident;
use salsa::ParallelDatabase;

use self::tls::{with_cur_item, CUR_ITEM};
use super::{
    adjust::Adjust,
    resolver::{DefaultPathResolver, PathResolver, WorkspacePathResolver},
    rir::NodeKind,
};
use crate::{
    db::{RirDatabase, RootDatabase},
    rir::{self, Field, Item, ItemPath, Literal},
    symbol::{DefId, IdentName, Symbol},
    tags::{EnumMode, TagId, Tags},
    ty::{AdtDef, AdtKind, CodegenTy, Visitor},
    Plugin,
};

#[derive(Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Clone)]
pub(crate) enum DefLocation {
    Fixed(ItemPath),
    Dynamic,
}

pub enum CollectMode {
    All,
    OnlyUsed {
        touches: Vec<(std::path::PathBuf, Vec<String>)>,
    },
}

#[derive(Debug)]
pub struct WorkspaceInfo {
    pub(crate) dir: PathBuf,
    pub(crate) location_map: FxHashMap<DefId, DefLocation>,
}

#[derive(Debug)]
pub enum Mode {
    Workspace(WorkspaceInfo),
    SingleFile { file_path: std::path::PathBuf },
}

pub struct Context {
    pub source_type: SourceType,
    pub db: salsa::Snapshot<RootDatabase>,
    pub adjusts: Arc<DashMap<DefId, Adjust>>,
    pub services: Arc<[crate::IdlService]>,
    pub(crate) change_case: bool,
    pub(crate) codegen_items: Arc<Vec<DefId>>,
    pub(crate) path_resolver: Arc<dyn PathResolver>,
    pub(crate) mode: Arc<Mode>,
}

impl Clone for Context {
    fn clone(&self) -> Self {
        Self {
            source_type: self.source_type,
            db: self.db.snapshot(),
            adjusts: self.adjusts.clone(),
            change_case: self.change_case,
            codegen_items: self.codegen_items.clone(),
            path_resolver: self.path_resolver.clone(),
            mode: self.mode.clone(),
            services: self.services.clone(),
        }
    }
}

pub(crate) struct ContextBuilder {
    db: RootDatabase,
    pub(crate) codegen_items: Vec<DefId>,
    input_items: Vec<DefId>,
    mode: Mode,
}

impl ContextBuilder {
    pub fn new(db: RootDatabase, mode: Mode, input_items: Vec<DefId>) -> Self {
        ContextBuilder {
            db,
            mode,
            input_items,
            codegen_items: Default::default(),
        }
    }
    pub(crate) fn collect(&mut self, mode: CollectMode) {
        match mode {
            CollectMode::All => {
                let nodes = self.db.nodes();
                self.codegen_items.extend(nodes.iter().filter_map(|(k, v)| {
                    if let NodeKind::Item(i) = &v.kind {
                        if !matches!(&**i, Item::Mod(_)) {
                            Some(k)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }));
            }
            CollectMode::OnlyUsed { touches } => {
                let extra_def_ids = touches
                    .into_iter()
                    .flat_map(|s| {
                        let path = s.0.normalize().unwrap().into_path_buf();
                        let file_id = *self.db.file_ids_map().get(&path).unwrap();
                        s.1.into_iter()
                            .filter_map(|item_name| {
                                let def_id = self
                                    .db
                                    .files()
                                    .get(&file_id)
                                    .unwrap()
                                    .items
                                    .iter()
                                    .find(|def_id| {
                                        *self.db.item(**def_id).unwrap().symbol_name() == item_name
                                    })
                                    .cloned();
                                if let Some(def_id) = def_id {
                                    Some(def_id)
                                } else {
                                    println!(
                                        "cargo:warning=item `{}` of `{}` not exists",
                                        item_name,
                                        path.display(),
                                    );
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();

                self.input_items.extend(extra_def_ids);

                let def_ids = self.collect_items(&self.input_items);
                self.codegen_items.extend(def_ids.iter());
            }
        }
        if matches!(self.mode, Mode::Workspace(_)) {
            let location_map = self.workspace_collect_def_ids(&self.codegen_items);

            if let Mode::Workspace(info) = &mut self.mode {
                info.location_map = location_map
            }
        }
    }

    pub(crate) fn collect_items(&self, input: &[DefId]) -> FxHashSet<DefId> {
        struct PathCollector<'a> {
            set: &'a mut FxHashSet<DefId>,
            cx: &'a ContextBuilder,
        }

        impl super::ty::Visitor for PathCollector<'_> {
            fn visit_path(&mut self, path: &crate::rir::Path) {
                collect(self.cx, path.did, self.set)
            }
        }

        fn collect(cx: &ContextBuilder, def_id: DefId, set: &mut FxHashSet<DefId>) {
            if set.contains(&def_id) {
                return;
            }
            if !matches!(&*cx.db.item(def_id).unwrap(), rir::Item::Mod(_)) {
                set.insert(def_id);
            }

            let node = cx.db.node(def_id).unwrap();
            tracing::trace!("collecting {:?}", node.expect_item().symbol_name());

            node.related_nodes
                .iter()
                .for_each(|def_id| collect(cx, *def_id, set));

            let item = node.expect_item();

            match item {
                rir::Item::Message(m) => m
                    .fields
                    .iter()
                    .for_each(|f| PathCollector { cx, set }.visit(&f.ty)),
                rir::Item::Enum(e) => e
                    .variants
                    .iter()
                    .flat_map(|v| &v.fields)
                    .for_each(|ty| PathCollector { cx, set }.visit(ty)),
                rir::Item::Service(s) => {
                    s.extend.iter().for_each(|p| collect(cx, p.did, set));
                    s.methods
                        .iter()
                        .flat_map(|m| m.args.iter().map(|f| &f.ty).chain(std::iter::once(&m.ret)))
                        .for_each(|ty| PathCollector { cx, set }.visit(ty));
                }
                rir::Item::NewType(n) => PathCollector { cx, set }.visit(&n.ty),
                rir::Item::Const(c) => {
                    PathCollector { cx, set }.visit(&c.ty);
                }
                rir::Item::Mod(m) => {
                    m.items.iter().for_each(|i| collect(cx, *i, set));
                }
            }
        }
        let mut set = FxHashSet::default();

        input.iter().for_each(|def_id| {
            collect(self, *def_id, &mut set);
        });

        self.db.nodes().iter().for_each(|(def_id, node)| {
            if let NodeKind::Item(item) = &node.kind {
                if let rir::Item::Const(_) = &**item {
                    collect(self, *def_id, &mut set);
                }
            }
        });

        set
    }

    pub(crate) fn workspace_collect_def_ids(
        &self,
        input: &[DefId],
    ) -> FxHashMap<DefId, DefLocation> {
        struct PathCollector<'a> {
            map: &'a mut FxHashMap<DefId, DefLocation>,
            cx: &'a ContextBuilder,
        }

        impl crate::ty::Visitor for PathCollector<'_> {
            fn visit_path(&mut self, path: &crate::rir::Path) {
                collect(self.cx, path.did, self.map)
            }
        }

        fn collect(cx: &ContextBuilder, def_id: DefId, map: &mut FxHashMap<DefId, DefLocation>) {
            if let Some(_location) = map.get_mut(&def_id) {
                return;
            }
            if !matches!(&*cx.db.item(def_id).unwrap(), rir::Item::Mod(_)) {
                let file_id = cx.db.node(def_id).unwrap().file_id;
                if cx.db.input_files().contains(&file_id) {
                    let file = cx.db.file(file_id).unwrap();
                    map.insert(def_id, DefLocation::Fixed(file.package.clone()));
                } else {
                    map.insert(def_id, DefLocation::Dynamic);
                }
            }

            let node = cx.db.node(def_id).unwrap();
            tracing::trace!("collecting {:?}", node.expect_item().symbol_name());

            node.related_nodes
                .iter()
                .for_each(|def_id| collect(cx, *def_id, map));

            let item = node.expect_item();

            match item {
                rir::Item::Message(m) => m
                    .fields
                    .iter()
                    .for_each(|f| PathCollector { cx, map }.visit(&f.ty)),
                rir::Item::Enum(e) => e
                    .variants
                    .iter()
                    .flat_map(|v| &v.fields)
                    .for_each(|ty| PathCollector { cx, map }.visit(ty)),
                rir::Item::Service(s) => {
                    s.extend.iter().for_each(|p| collect(cx, p.did, map));
                    s.methods
                        .iter()
                        .flat_map(|m| m.args.iter().map(|f| &f.ty).chain(std::iter::once(&m.ret)))
                        .for_each(|ty| PathCollector { cx, map }.visit(ty));
                }
                rir::Item::NewType(n) => PathCollector { cx, map }.visit(&n.ty),
                rir::Item::Const(c) => {
                    PathCollector { cx, map }.visit(&c.ty);
                }
                rir::Item::Mod(m) => {
                    m.items.iter().for_each(|i| collect(cx, *i, map));
                }
            }
        }
        let mut map = FxHashMap::default();

        input.iter().for_each(|def_id| {
            collect(self, *def_id, &mut map);
        });

        map
    }

    pub(crate) fn build(
        self,
        services: Arc<[crate::IdlService]>,
        source_type: SourceType,
        change_case: bool,
    ) -> Context {
        Context {
            adjusts: Default::default(),
            source_type,
            db: self.db.snapshot(),
            change_case,
            services,
            codegen_items: Arc::new(self.codegen_items),
            path_resolver: match &self.mode {
                Mode::Workspace(_) => Arc::new(WorkspacePathResolver),
                Mode::SingleFile { .. } => Arc::new(DefaultPathResolver),
            },
            mode: Arc::new(self.mode),
        }
    }
}

impl Deref for Context {
    type Target = salsa::Snapshot<RootDatabase>;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

#[derive(Clone, Copy)]
pub enum SourceType {
    Thrift,
    Protobuf,
}

impl Context {
    pub fn with_adjust<T, F>(&self, def_id: DefId, f: F) -> T
    where
        F: FnOnce(Option<&Adjust>) -> T,
    {
        match self.adjusts.get(&def_id) {
            Some(adj) => f(Some(&*adj)),
            None => f(None),
        }
    }

    pub fn with_adjust_mut<T, F>(&self, def_id: DefId, f: F) -> T
    where
        F: FnOnce(&mut Adjust) -> T,
    {
        let adjust = &mut *self.adjusts.entry(def_id).or_insert_with(Default::default);
        f(adjust)
    }

    pub fn tags(&self, tags_id: TagId) -> Option<Arc<Tags>> {
        self.db.tags_map().get(&tags_id).cloned()
    }

    pub fn node_tags(&self, def_id: DefId) -> Option<Arc<Tags>> {
        let tags_id = self.node(def_id).unwrap().tags;
        self.tags(tags_id)
    }

    pub fn contains_tag<T: 'static>(&self, tags_id: TagId) -> bool {
        self.tags(tags_id)
            .and_then(|tags| tags.contains::<T>().then_some(true))
            .is_some()
    }

    pub fn node_contains_tag<T: 'static>(&self, def_id: DefId) -> bool {
        self.node_tags(def_id)
            .and_then(|tags| tags.contains::<T>().then_some(true))
            .is_some()
    }

    pub fn symbol_name(&self, def_id: DefId) -> Symbol {
        let item = self.item(def_id).unwrap();
        item.symbol_name()
    }

    pub fn default_val(&self, f: &Field) -> Option<(FastStr, bool /* const? */)> {
        f.default.as_ref().map(|d| {
            let ty = self.codegen_item_ty(f.ty.kind.clone());
            match self
                .lit_as_rvalue(d, &ty)
                .with_context(|| format!("calc the default value for field {}", f.name))
            {
                Ok(v) => v,
                Err(err) => {
                    panic!("{:?}", err)
                }
            }
        })
    }

    fn lit_as_rvalue(
        &self,
        lit: &Literal,
        ty: &CodegenTy,
    ) -> anyhow::Result<(FastStr, bool /* const? */)> {
        let mk_map = |m: &Vec<(Literal, Literal)>, k_ty: &Arc<CodegenTy>, v_ty: &Arc<CodegenTy>| {
            let k_ty = &**k_ty;
            let v_ty = &**v_ty;
            let len = m.len();
            let kvs = m
                .iter()
                .map(|(k, v)| {
                    let k = self.lit_into_ty(k, k_ty)?.0;
                    let v = self.lit_into_ty(v, v_ty)?.0;
                    anyhow::Ok(format!("map.insert({k}, {v});"))
                })
                .try_collect::<_, Vec<_>, _>()?
                .join("");
            anyhow::Ok(
                format! {r#"{{
                    let mut map = ::std::collections::HashMap::with_capacity({len});
                    {kvs}
                    map
                }}"#}
                .into(),
            )
        };

        anyhow::Ok(match (lit, ty) {
            (Literal::Map(m), CodegenTy::LazyStaticRef(map)) => match &**map {
                CodegenTy::Map(k_ty, v_ty) => (mk_map(m, k_ty, v_ty)?, false),
                _ => panic!("invalid map type {:?}", map),
            },
            (Literal::Map(m), CodegenTy::Map(k_ty, v_ty)) => (mk_map(m, k_ty, v_ty)?, false),
            _ => self.lit_into_ty(lit, ty)?,
        })
    }

    fn ident_into_ty(
        &self,
        did: DefId,
        ident_ty: &CodegenTy,
        target: &CodegenTy,
    ) -> (FastStr, bool /* const? */) {
        if ident_ty == target {
            let stream = self.cur_related_item_path(did);
            return (stream, true);
        }
        match (ident_ty, target) {
            (CodegenTy::Str, CodegenTy::FastStr) => {
                let stream = self.cur_related_item_path(did);
                (
                    format!("::pilota::FastStr::from_static_str({stream})").into(),
                    true,
                )
            }
            _ => panic!("invalid convert {:?} to {:?}", ident_ty, target),
        }
    }

    fn lit_into_ty(
        &self,
        lit: &Literal,
        ty: &CodegenTy,
    ) -> anyhow::Result<(FastStr, bool /* const? */)> {
        Ok(match (lit, ty) {
            (Literal::Path(p), ty) => {
                let ident_ty = self.codegen_ty(p.did);

                self.ident_into_ty(p.did, &ident_ty, ty)
            }
            (Literal::String(s), CodegenTy::Str) => (format!("\"{s}\"").into(), true),
            (Literal::String(s), CodegenTy::String) => {
                (format! {"\"{s}\".to_string()"}.into(), false)
            }
            (Literal::String(s), CodegenTy::FastStr) => (
                format! { "::pilota::FastStr::from_static_str(\"{s}\")" }.into(),
                true,
            ),
            (Literal::Int(i), CodegenTy::I16) => (format! { "{i}i16" }.into(), true),
            (Literal::Int(i), CodegenTy::I32) => (format! { "{i}i32" }.into(), true),
            (Literal::Int(i), CodegenTy::I64) => (format! { "{i}i64" }.into(), true),
            (Literal::Int(i), CodegenTy::F32) => {
                let f = (*i) as f32;
                (format!("{f}f32").into(), true)
            }
            (Literal::Int(i), CodegenTy::F64) => {
                let f = (*i) as f64;
                (format!("{f}f64").into(), true)
            }
            (
                Literal::Int(i),
                CodegenTy::Adt(AdtDef {
                    did,
                    kind: AdtKind::Enum,
                }),
            ) => {
                let item = self.item(*did).unwrap();
                let e = match &*item {
                    Item::Enum(e) => e,
                    _ => panic!("invalid enum"),
                };

                (
                    e.variants.iter().find(|v| v.discr == Some(*i)).map_or_else(
                        || panic!("invalid enum value"),
                        |v| self.cur_related_item_path(v.did),
                    ),
                    true,
                )
            }
            (Literal::Float(f), CodegenTy::F64) => {
                let f = f.parse::<f64>().unwrap();
                (format! { "{f}f64" }.into(), true)
            }
            (
                l,
                CodegenTy::Adt(AdtDef {
                    kind: AdtKind::NewType(inner_ty),
                    did,
                }),
            ) => {
                let ident = self.cur_related_item_path(*did);
                let (stream, is_const) = self.lit_into_ty(l, inner_ty)?;
                (format! { "{ident}({stream})" }.into(), is_const)
            }
            (Literal::Map(_), CodegenTy::StaticRef(map)) => match &**map {
                CodegenTy::Map(_, _) => {
                    let lazy_map =
                        self.def_lit("INNER_MAP", lit, &mut CodegenTy::LazyStaticRef(map.clone()))?;
                    let stream = format! {
                        r#"
                        {{
                            {lazy_map}
                            &*INNER_MAP
                        }}
                        "#
                    }
                    .into();
                    (stream, false)
                }
                _ => panic!("invalid map type {:?}", map),
            },
            (Literal::List(els), CodegenTy::Array(inner, _)) => {
                let stream = els
                    .iter()
                    .map(|el| self.lit_into_ty(el, inner))
                    .try_collect::<_, Vec<_>, _>()?;
                let is_const = stream.iter().all(|(_, is_const)| *is_const);
                let stream = stream.into_iter().map(|(s, _)| s).join(",");

                (format! {"[{stream}]" }.into(), is_const)
            }
            (Literal::List(els), CodegenTy::Vec(inner)) => {
                let stream = els
                    .iter()
                    .map(|el| self.lit_into_ty(el, inner))
                    .try_collect::<_, Vec<_>, _>()?
                    .into_iter()
                    .map(|(s, _)| s)
                    .join(",");

                (format! { "::std::vec![{stream}]" }.into(), false)
            }
            (Literal::Bool(b), CodegenTy::Bool) => (format! { "{b}" }.into(), true),
            (Literal::String(s), CodegenTy::Bytes) => {
                let s = &**s;
                (
                    format! { "::bytes::Bytes::from_static({s}.as_bytes())" }.into(),
                    true,
                )
            }
            (
                Literal::Map(m),
                CodegenTy::Adt(AdtDef {
                    did,
                    kind: AdtKind::Struct,
                }),
            ) => {
                let def = self.item(*did).unwrap();
                let def = match &*def {
                    Item::Message(m) => m,
                    _ => panic!(),
                };

                let fields: Vec<_> = def
                    .fields
                    .iter()
                    .map(|f| {
                        let v = m.iter().find_map(|(k, v)| {
                            let k = match k {
                                Literal::String(s) => s,
                                _ => panic!(),
                            };
                            if **k == **f.name {
                                Some(v)
                            } else {
                                None
                            }
                        });

                        let name = self.rust_name(f.did);

                        if let Some(v) = v {
                            let (mut v, is_const) =
                                self.lit_into_ty(v, &self.codegen_item_ty(f.ty.kind.clone()))?;

                            if f.is_optional() {
                                v = format!("Some({v})").into()
                            }
                            anyhow::Ok((format!("{name}: {v}"), is_const))
                        } else {
                            anyhow::Ok((format!("{name}: Default::default()"), false))
                        }
                    })
                    .try_collect()?;
                let is_const = fields.iter().all(|(_, is_const)| *is_const);
                let fields = fields.into_iter().map(|f| f.0).join(",");

                let name = self.rust_name(*did);

                (
                    format! {
                        r#"{name} {{
                            {fields}
                        }}"#
                    }
                    .into(),
                    is_const,
                )
            }
            _ => panic!("unexpected literal {:?} with ty {:?}", lit, ty),
        })
    }

    pub(crate) fn def_lit(
        &self,
        name: &str,
        lit: &Literal,
        ty: &mut CodegenTy,
    ) -> anyhow::Result<String> {
        let should_lazy_static = ty.should_lazy_static();
        let name = format_ident!("{}", name.to_shouty_snake_case());
        if let (Literal::List(lit), CodegenTy::Array(_, size)) = (lit, &mut *ty) {
            *size = lit.len()
        }
        Ok(if should_lazy_static {
            let lit = self.lit_as_rvalue(lit, ty)?.0;
            format! {r#"
                ::pilota::lazy_static::lazy_static! {{
                    pub static ref {name}: {ty} = {lit};
                }}
            "#}
        } else {
            let lit = self.lit_into_ty(lit, ty)?.0;
            format!(r#"pub const {name}: {ty} = {lit};"#)
        })
    }

    pub fn rust_name(&self, def_id: DefId) -> Symbol {
        let node = self.node(def_id).unwrap();

        if let Some(name) = self
            .tags(node.tags)
            .and_then(|tags| tags.get::<crate::tags::PilotaName>().cloned())
        {
            return name.0.into();
        }

        if !self.change_case {
            return self.node(def_id).unwrap().name().0.into();
        }

        match self.node(def_id).unwrap().kind {
            NodeKind::Item(item) => match &*item {
                crate::rir::Item::Message(m) => (&**m.name).struct_ident(),
                crate::rir::Item::Enum(e) => (&**e.name).enum_ident(),
                crate::rir::Item::Service(s) => (&**s.name).trait_ident(),
                crate::rir::Item::NewType(t) => (&**t.name).newtype_ident(),
                crate::rir::Item::Const(c) => (&**c.name).const_ident(),
                crate::rir::Item::Mod(m) => (&**m.name).mod_ident(),
            },
            NodeKind::Variant(v) => {
                let parent = self.node(def_id).unwrap().parent.unwrap();

                if self
                    .node_tags(parent)
                    .unwrap()
                    .get::<EnumMode>()
                    .copied()
                    .unwrap_or(EnumMode::Enum)
                    == EnumMode::NewType
                {
                    (&**v.name).shouty_snake_case()
                } else {
                    (&**v.name).variant_ident()
                }
            }
            NodeKind::Field(f) => (&**f.name).field_ident(),
            NodeKind::Method(m) => (&**m.name).fn_ident(),
            NodeKind::Arg(a) => (&**a.name).field_ident(),
        }
        .into()
    }

    pub fn mod_path(&self, def_id: DefId) -> Arc<[Symbol]> {
        self.path_resolver.mod_prefix(self, def_id)
    }

    pub fn item_path(&self, def_id: DefId) -> Arc<[Symbol]> {
        self.path_resolver.path_for_def_id(self, def_id)
    }

    fn related_path(&self, p1: &[Symbol], p2: &[Symbol]) -> FastStr {
        self.path_resolver.related_path(p1, p2)
    }

    pub fn cur_related_item_path(&self, did: DefId) -> FastStr {
        let a = with_cur_item(|def_id| def_id);
        self.related_item_path(a, did)
    }

    pub fn related_item_path(&self, a: DefId, b: DefId) -> FastStr {
        let cur_item_path = self.item_path(a);
        let mut mod_segs = vec![];

        cur_item_path[..cur_item_path.len() - 1]
            .iter()
            .for_each(|p| {
                mod_segs.push(p.clone());
            });

        let other_item_path = self.item_path(b);
        self.related_path(&mod_segs, &other_item_path)
    }

    #[allow(clippy::single_match)]
    pub fn exec_plugin<P: Plugin>(&self, mut p: P) {
        for def_id in self.codegen_items.clone().iter() {
            let node = self.node(*def_id).unwrap();
            CUR_ITEM.set(def_id, || match &node.kind {
                NodeKind::Item(item) => p.on_item(self, *def_id, item.clone()),
                _ => {}
            })
        }

        p.on_emit(self)
    }

    pub(crate) fn workspace_info(&self) -> &WorkspaceInfo {
        let Mode::Workspace(info) = &*self.mode else {
            panic!("can not access workspace info in mode `{:?}`", self.mode)
        };
        info
    }

    pub fn def_id_info(&self, def_id: DefId) -> FastStr {
        let file_path = self
            .file(self.node(def_id).unwrap().file_id)
            .unwrap()
            .package
            .clone();
        file_path
            .iter()
            .chain(&[self.node(def_id).unwrap().name()])
            .join("::")
            .into()
    }

    pub(crate) fn crate_name(&self, location: &DefLocation) -> FastStr {
        match location {
            DefLocation::Fixed(path) => path.iter().join("_").into(),
            DefLocation::Dynamic => "common".into(),
        }
    }
}

pub mod tls {

    use scoped_tls::scoped_thread_local;

    use super::Context;
    use crate::DefId;

    scoped_thread_local!(pub static CONTEXT: Context);
    scoped_thread_local!(pub static CUR_ITEM: DefId);

    pub fn with_cx<T, F>(f: F) -> T
    where
        F: FnOnce(&Context) -> T,
    {
        CONTEXT.with(|cx| f(cx))
    }

    pub fn with_cur_item<T, F>(f: F) -> T
    where
        F: FnOnce(DefId) -> T,
    {
        CUR_ITEM.with(|def_id| f(*def_id))
    }
}

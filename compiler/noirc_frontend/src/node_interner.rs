use std::collections::HashMap;

use arena::{Arena, Index};
use fm::FileId;
use iter_extended::vecmap;
use noirc_errors::{Location, Span, Spanned};

use crate::ast::Ident;
use crate::graph::CrateId;
use crate::hir::def_collector::dc_crate::{UnresolvedStruct, UnresolvedTrait, UnresolvedTypeAlias};
use crate::hir::def_map::{LocalModuleId, ModuleId};

use crate::hir_def::stmt::HirLetStatement;
use crate::hir_def::traits::TraitImpl;
use crate::hir_def::traits::{Trait, TraitConstraint};
use crate::hir_def::types::{StructType, Type};
use crate::hir_def::{
    expr::HirExpression,
    function::{FuncMeta, HirFunction},
    stmt::HirStatement,
};
use crate::token::{Attributes, SecondaryAttribute};
use crate::{
    BinaryOpKind, ContractFunctionType, FunctionDefinition, FunctionVisibility, Generics, Shared,
    TypeAliasType, TypeBindings, TypeVariable, TypeVariableId, TypeVariableKind,
};

/// An arbitrary number to limit the recursion depth when searching for trait impls.
/// This is needed to stop recursing for cases such as `impl<T> Foo for T where T: Eq`
const IMPL_SEARCH_RECURSION_LIMIT: u32 = 10;

type StructAttributes = Vec<SecondaryAttribute>;

/// The node interner is the central storage location of all nodes in Noir's Hir (the
/// various node types can be found in hir_def). The interner is also used to collect
/// extra information about the Hir, such as the type of each node, information about
/// each definition or struct, etc. Because it is used on the Hir, the NodeInterner is
/// useful in passes where the Hir is used - name resolution, type checking, and
/// monomorphization - and it is not useful afterward.
#[derive(Debug)]
pub struct NodeInterner {
    nodes: Arena<Node>,
    func_meta: HashMap<FuncId, FuncMeta>,
    function_definition_ids: HashMap<FuncId, DefinitionId>,

    // For a given function ID, this gives the function's modifiers which includes
    // its visibility and whether it is unconstrained, among other information.
    // Unlike func_meta, this map is filled out during definition collection rather than name resolution.
    function_modifiers: HashMap<FuncId, FunctionModifiers>,

    // Contains the source module each function was defined in
    function_modules: HashMap<FuncId, ModuleId>,

    // Map each `Index` to it's own location
    id_to_location: HashMap<Index, Location>,

    // Maps each DefinitionId to a DefinitionInfo.
    definitions: Vec<DefinitionInfo>,

    // Type checking map
    //
    // Notice that we use `Index` as the Key and not an ExprId or IdentId
    // Therefore, If a raw index is passed in, then it is not safe to assume that it will have
    // a Type, as not all Ids have types associated to them.
    // Further note, that an ExprId and an IdentId will never have the same underlying Index
    // Because we use one Arena to store all Definitions/Nodes
    id_to_type: HashMap<Index, Type>,

    // Struct map.
    //
    // Each struct definition is possibly shared across multiple type nodes.
    // It is also mutated through the RefCell during name resolution to append
    // methods from impls to the type.
    structs: HashMap<StructId, Shared<StructType>>,

    struct_attributes: HashMap<StructId, StructAttributes>,
    // Type Aliases map.
    //
    // Map type aliases to the actual type.
    // When resolving types, check against this map to see if a type alias is defined.
    type_aliases: Vec<TypeAliasType>,

    // Trait map.
    //
    // Each trait definition is possibly shared across multiple type nodes.
    // It is also mutated through the RefCell during name resolution to append
    // methods from impls to the type.
    traits: HashMap<TraitId, Trait>,

    // Trait implementation map
    // For each type that implements a given Trait ( corresponding TraitId), there should be an entry here
    // The purpose for this hashmap is to detect duplication of trait implementations ( if any )
    //
    // Indexed by TraitImplIds
    trait_implementations: Vec<Shared<TraitImpl>>,

    /// Trait implementations on each type. This is expected to always have the same length as
    /// `self.trait_implementations`.
    ///
    /// For lack of a better name, this maps a trait id and type combination
    /// to a corresponding impl if one is available for the type. Due to generics,
    /// we cannot map from Type directly to impl, we need to iterate a Vec of all impls
    /// of that trait to see if any type may match. This can be further optimized later
    /// by splitting it up by type.
    trait_implementation_map: HashMap<TraitId, Vec<(Type, TraitImplKind)>>,

    /// When impls are found during type checking, we tag the function call's Ident
    /// with the impl that was selected. For cases with where clauses, this may be
    /// an Assumed (but verified) impl. In this case the monomorphizer should have
    /// the context to get the concrete type of the object and select the correct impl itself.
    selected_trait_implementations: HashMap<ExprId, TraitImplKind>,

    /// Holds the trait ids of the traits used for operator overloading
    operator_traits: HashMap<BinaryOpKind, TraitId>,

    /// The `Ordering` type is a semi-builtin type that is the result of the comparison traits.
    ordering_type: Option<Type>,

    /// Map from ExprId (referring to a Function/Method call) to its corresponding TypeBindings,
    /// filled out during type checking from instantiated variables. Used during monomorphization
    /// to map call site types back onto function parameter types, and undo this binding as needed.
    instantiation_bindings: HashMap<ExprId, TypeBindings>,

    /// Remembers the field index a given HirMemberAccess expression was resolved to during type
    /// checking.
    field_indices: HashMap<ExprId, usize>,

    globals: HashMap<StmtId, GlobalInfo>, // NOTE: currently only used for checking repeat globals and restricting their scope to a module

    next_type_variable_id: std::cell::Cell<usize>,

    /// A map from a struct type and method name to a function id for the method.
    /// This can resolve to potentially multiple methods if the same method name is
    /// specialized for different generics on the same type. E.g. for `Struct<T>`, we
    /// may have both `impl Struct<u32> { fn foo(){} }` and `impl Struct<u8> { fn foo(){} }`.
    /// If this happens, the returned Vec will have 2 entries and we'll need to further
    /// disambiguate them by checking the type of each function.
    struct_methods: HashMap<(StructId, String), Methods>,

    /// Methods on primitive types defined in the stdlib.
    primitive_methods: HashMap<(TypeMethodKey, String), Methods>,

    // For trait implementation functions, this is their self type and trait they belong to
    func_id_to_trait: HashMap<FuncId, (Type, TraitId)>,
}

/// A trait implementation is either a normal implementation that is present in the source
/// program via an `impl` block, or it is assumed to exist from a `where` clause or similar.
#[derive(Debug, Clone)]
pub enum TraitImplKind {
    Normal(TraitImplId),

    /// Assumed impls don't have an impl id since they don't link back to any concrete part of the source code.
    Assumed {
        object_type: Type,
    },
}

/// Represents the methods on a given type that each share the same name.
///
/// Methods are split into inherent methods and trait methods. If there is
/// ever a name that is defined on both a type directly, and defined indirectly
/// via a trait impl, the direct (inherent) name will always take precedence.
///
/// Additionally, types can define specialized impls with methods of the same name
/// as long as these specialized impls do not overlap. E.g. `impl Struct<u32>` and `impl Struct<u64>`
#[derive(Default, Debug)]
pub struct Methods {
    direct: Vec<FuncId>,
    trait_impl_methods: Vec<FuncId>,
}

/// All the information from a function that is filled out during definition collection rather than
/// name resolution. As a result, if information about a function is needed during name resolution,
/// this is the only place where it is safe to retrieve it (where all fields are guaranteed to be initialized).
#[derive(Debug, Clone)]
pub struct FunctionModifiers {
    pub name: String,

    /// Whether the function is `pub` or not.
    pub visibility: FunctionVisibility,

    pub attributes: Attributes,

    pub is_unconstrained: bool,

    /// This function's type in its contract.
    /// If this function is not in a contract, this is always 'Secret'.
    pub contract_function_type: Option<ContractFunctionType>,

    /// This function's contract visibility.
    /// If this function is internal can only be called by itself.
    /// Will be None if not in contract.
    pub is_internal: Option<bool>,
}

impl FunctionModifiers {
    /// A semi-reasonable set of default FunctionModifiers used for testing.
    #[cfg(test)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            name: String::new(),
            visibility: FunctionVisibility::Public,
            attributes: Attributes::empty(),
            is_unconstrained: false,
            is_internal: None,
            contract_function_type: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct DefinitionId(usize);

impl DefinitionId {
    //dummy id for error reporting
    pub fn dummy_id() -> DefinitionId {
        DefinitionId(std::usize::MAX)
    }
}

impl From<DefinitionId> for Index {
    fn from(id: DefinitionId) -> Self {
        Index::from_raw_parts(id.0, u64::MAX)
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub struct StmtId(Index);

impl StmtId {
    //dummy id for error reporting
    // This can be anything, as the program will ultimately fail
    // after resolution
    pub fn dummy_id() -> StmtId {
        StmtId(Index::from_raw_parts(std::usize::MAX, 0))
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone)]
pub struct ExprId(Index);

impl ExprId {
    pub fn empty_block_id() -> ExprId {
        ExprId(Index::from_raw_parts(0, 0))
    }
}
#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone)]
pub struct FuncId(Index);

impl FuncId {
    //dummy id for error reporting
    // This can be anything, as the program will ultimately fail
    // after resolution
    pub fn dummy_id() -> FuncId {
        FuncId(Index::from_raw_parts(std::usize::MAX, 0))
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone, PartialOrd, Ord)]
pub struct StructId(ModuleId);

impl StructId {
    //dummy id for error reporting
    // This can be anything, as the program will ultimately fail
    // after resolution
    pub fn dummy_id() -> StructId {
        StructId(ModuleId { krate: CrateId::dummy_id(), local_id: LocalModuleId::dummy_id() })
    }

    pub fn module_id(self) -> ModuleId {
        self.0
    }

    pub fn krate(self) -> CrateId {
        self.0.krate
    }

    pub fn local_module_id(self) -> LocalModuleId {
        self.0.local_id
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone, PartialOrd, Ord)]
pub struct TypeAliasId(pub usize);

impl TypeAliasId {
    pub fn dummy_id() -> TypeAliasId {
        TypeAliasId(std::usize::MAX)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraitId(pub ModuleId);

impl TraitId {
    // dummy id for error reporting
    // This can be anything, as the program will ultimately fail
    // after resolution
    pub fn dummy_id() -> TraitId {
        TraitId(ModuleId { krate: CrateId::dummy_id(), local_id: LocalModuleId::dummy_id() })
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub struct TraitImplId(usize);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraitMethodId {
    pub trait_id: TraitId,
    pub method_index: usize, // index in Trait::methods
}

macro_rules! into_index {
    ($id_type:ty) => {
        impl From<$id_type> for Index {
            fn from(t: $id_type) -> Self {
                t.0
            }
        }

        impl From<&$id_type> for Index {
            fn from(t: &$id_type) -> Self {
                t.0
            }
        }
    };
}

macro_rules! partialeq {
    ($id_type:ty) => {
        impl PartialEq<usize> for &$id_type {
            fn eq(&self, other: &usize) -> bool {
                let (index, _) = self.0.into_raw_parts();
                index == *other
            }
        }
    };
}

into_index!(ExprId);
into_index!(StmtId);

partialeq!(ExprId);
partialeq!(StmtId);

/// A Definition enum specifies anything that we can intern in the NodeInterner
/// We use one Arena for all types that can be interned as that has better cache locality
/// This data structure is never accessed directly, so API wise there is no difference between using
/// Multiple arenas and a single Arena
#[derive(Debug, Clone)]
enum Node {
    Function(HirFunction),
    Statement(HirStatement),
    Expression(HirExpression),
}

#[derive(Debug, Clone)]
pub struct DefinitionInfo {
    pub name: String,
    pub mutable: bool,
    pub kind: DefinitionKind,
    pub location: Location,
}

impl DefinitionInfo {
    /// True if this definition is for a global variable.
    /// Note that this returns false for top-level functions.
    pub fn is_global(&self) -> bool {
        self.kind.is_global()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DefinitionKind {
    Function(FuncId),

    Global(ExprId),

    /// Locals may be defined in let statements or parameters,
    /// in which case they will not have an associated ExprId
    Local(Option<ExprId>),

    /// Generic types in functions (T, U in `fn foo<T, U>(...)` are declared as variables
    /// in scope in case they resolve to numeric generics later.
    GenericType(TypeVariable),
}

impl DefinitionKind {
    /// True if this definition is for a global variable.
    /// Note that this returns false for top-level functions.
    pub fn is_global(&self) -> bool {
        matches!(self, DefinitionKind::Global(..))
    }

    pub fn get_rhs(&self) -> Option<ExprId> {
        match self {
            DefinitionKind::Function(_) => None,
            DefinitionKind::Global(id) => Some(*id),
            DefinitionKind::Local(id) => *id,
            DefinitionKind::GenericType(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GlobalInfo {
    pub ident: Ident,
    pub local_id: LocalModuleId,
}

impl Default for NodeInterner {
    fn default() -> Self {
        let mut interner = NodeInterner {
            nodes: Arena::default(),
            func_meta: HashMap::new(),
            function_definition_ids: HashMap::new(),
            function_modifiers: HashMap::new(),
            function_modules: HashMap::new(),
            func_id_to_trait: HashMap::new(),
            id_to_location: HashMap::new(),
            definitions: vec![],
            id_to_type: HashMap::new(),
            structs: HashMap::new(),
            struct_attributes: HashMap::new(),
            type_aliases: Vec::new(),
            traits: HashMap::new(),
            trait_implementations: Vec::new(),
            trait_implementation_map: HashMap::new(),
            selected_trait_implementations: HashMap::new(),
            operator_traits: HashMap::new(),
            ordering_type: None,
            instantiation_bindings: HashMap::new(),
            field_indices: HashMap::new(),
            next_type_variable_id: std::cell::Cell::new(0),
            globals: HashMap::new(),
            struct_methods: HashMap::new(),
            primitive_methods: HashMap::new(),
        };

        // An empty block expression is used often, we add this into the `node` on startup
        let expr_id = interner.push_expr(HirExpression::empty_block());
        assert_eq!(expr_id, ExprId::empty_block_id());
        interner
    }
}

// XXX: Add check that insertions are not overwrites for maps
// XXX: Maybe change push to intern, and remove comments
impl NodeInterner {
    /// Interns a HIR statement.
    pub fn push_stmt(&mut self, stmt: HirStatement) -> StmtId {
        StmtId(self.nodes.insert(Node::Statement(stmt)))
    }
    /// Interns a HIR expression.
    pub fn push_expr(&mut self, expr: HirExpression) -> ExprId {
        ExprId(self.nodes.insert(Node::Expression(expr)))
    }

    /// Stores the span for an interned expression.
    pub fn push_expr_location(&mut self, expr_id: ExprId, span: Span, file: FileId) {
        self.id_to_location.insert(expr_id.into(), Location::new(span, file));
    }

    /// Scans the interner for the item which is located at that [Location]
    ///
    /// The [Location] may not necessarily point to the beginning of the item
    /// so we check if the location's span is contained within the start or end
    /// of each items [Span]
    #[tracing::instrument(skip(self))]
    pub fn find_location_index(&self, location: Location) -> Option<impl Into<Index>> {
        let mut location_candidate: Option<(&Index, &Location)> = None;

        // Note: we can modify this in the future to not do a linear
        // scan by storing a separate map of the spans or by sorting the locations.
        for (index, interned_location) in self.id_to_location.iter() {
            if interned_location.contains(&location) {
                if let Some(current_location) = location_candidate {
                    if interned_location.span.is_smaller(&current_location.1.span) {
                        location_candidate = Some((index, interned_location));
                    }
                } else {
                    location_candidate = Some((index, interned_location));
                }
            }
        }
        location_candidate.map(|(index, _location)| *index)
    }

    /// Interns a HIR Function.
    pub fn push_fn(&mut self, func: HirFunction) -> FuncId {
        FuncId(self.nodes.insert(Node::Function(func)))
    }

    /// Store the type for an interned expression
    pub fn push_expr_type(&mut self, expr_id: &ExprId, typ: Type) {
        self.id_to_type.insert(expr_id.into(), typ);
    }

    pub fn push_empty_trait(&mut self, type_id: TraitId, unresolved_trait: &UnresolvedTrait) {
        let self_type_typevar_id = self.next_type_variable_id();

        let new_trait = Trait {
            id: type_id,
            name: unresolved_trait.trait_def.name.clone(),
            crate_id: unresolved_trait.crate_id,
            location: Location::new(unresolved_trait.trait_def.span, unresolved_trait.file_id),
            generics: vecmap(&unresolved_trait.trait_def.generics, |_| {
                // Temporary type variable ids before the trait is resolved to its actual ids.
                // This lets us record how many arguments the type expects so that other types
                // can refer to it with generic arguments before the generic parameters themselves
                // are resolved.
                let id = TypeVariableId(0);
                (id, TypeVariable::unbound(id))
            }),
            self_type_typevar_id,
            self_type_typevar: TypeVariable::unbound(self_type_typevar_id),
            methods: Vec::new(),
            method_ids: unresolved_trait.method_ids.clone(),
            constants: Vec::new(),
            types: Vec::new(),
        };

        self.traits.insert(type_id, new_trait);
    }

    pub fn new_struct(
        &mut self,
        typ: &UnresolvedStruct,
        krate: CrateId,
        local_id: LocalModuleId,
        file_id: FileId,
    ) -> StructId {
        let struct_id = StructId(ModuleId { krate, local_id });
        let name = typ.struct_def.name.clone();

        // Fields will be filled in later
        let no_fields = Vec::new();
        let generics = vecmap(&typ.struct_def.generics, |_| {
            // Temporary type variable ids before the struct is resolved to its actual ids.
            // This lets us record how many arguments the type expects so that other types
            // can refer to it with generic arguments before the generic parameters themselves
            // are resolved.
            let id = TypeVariableId(0);
            (id, TypeVariable::unbound(id))
        });

        let location = Location::new(typ.struct_def.span, file_id);
        let new_struct = StructType::new(struct_id, name, location, no_fields, generics);
        self.structs.insert(struct_id, Shared::new(new_struct));
        self.struct_attributes.insert(struct_id, typ.struct_def.attributes.clone());
        struct_id
    }

    pub fn push_type_alias(&mut self, typ: &UnresolvedTypeAlias) -> TypeAliasId {
        let type_id = TypeAliasId(self.type_aliases.len());

        self.type_aliases.push(TypeAliasType::new(
            type_id,
            typ.type_alias_def.name.clone(),
            typ.type_alias_def.span,
            Type::Error,
            vecmap(&typ.type_alias_def.generics, |_| {
                let id = TypeVariableId(0);
                (id, TypeVariable::unbound(id))
            }),
        ));

        type_id
    }

    pub fn update_struct(&mut self, type_id: StructId, f: impl FnOnce(&mut StructType)) {
        let mut value = self.structs.get_mut(&type_id).unwrap().borrow_mut();
        f(&mut value);
    }

    pub fn update_trait(&mut self, trait_id: TraitId, f: impl FnOnce(&mut Trait)) {
        let value = self.traits.get_mut(&trait_id).unwrap();
        f(value);
    }

    pub fn set_type_alias(&mut self, type_id: TypeAliasId, typ: Type, generics: Generics) {
        let type_alias_type = &mut self.type_aliases[type_id.0];
        type_alias_type.set_type_and_generics(typ, generics);
    }

    /// Returns the interned statement corresponding to `stmt_id`
    pub fn update_statement(&mut self, stmt_id: &StmtId, f: impl FnOnce(&mut HirStatement)) {
        let def =
            self.nodes.get_mut(stmt_id.0).expect("ice: all statement ids should have definitions");

        match def {
            Node::Statement(stmt) => f(stmt),
            _ => panic!("ice: all statement ids should correspond to a statement in the interner"),
        }
    }

    /// Updates the interned expression corresponding to `expr_id`
    pub fn update_expression(&mut self, expr_id: ExprId, f: impl FnOnce(&mut HirExpression)) {
        let def =
            self.nodes.get_mut(expr_id.0).expect("ice: all expression ids should have definitions");

        match def {
            Node::Expression(expr) => f(expr),
            _ => {
                panic!("ice: all expression ids should correspond to a expression in the interner")
            }
        }
    }

    /// Store the type for an interned Identifier
    pub fn push_definition_type(&mut self, definition_id: DefinitionId, typ: Type) {
        self.id_to_type.insert(definition_id.into(), typ);
    }

    pub fn push_global(&mut self, stmt_id: StmtId, ident: Ident, local_id: LocalModuleId) {
        self.globals.insert(stmt_id, GlobalInfo { ident, local_id });
    }

    /// Intern an empty global stmt. Used for collecting globals
    pub fn push_empty_global(&mut self) -> StmtId {
        self.push_stmt(HirStatement::Error)
    }

    pub fn update_global(&mut self, stmt_id: StmtId, hir_stmt: HirStatement) {
        let def =
            self.nodes.get_mut(stmt_id.0).expect("ice: all function ids should have definitions");

        let stmt = match def {
            Node::Statement(stmt) => stmt,
            _ => {
                panic!("ice: all global ids should correspond to a statement in the interner")
            }
        };
        *stmt = hir_stmt;
    }

    /// Intern an empty function.
    pub fn push_empty_fn(&mut self) -> FuncId {
        self.push_fn(HirFunction::empty())
    }
    /// Updates the underlying interned Function.
    ///
    /// This method is used as we eagerly intern empty functions to
    /// generate function identifiers and then we update at a later point in
    /// time.
    pub fn update_fn(&mut self, func_id: FuncId, hir_func: HirFunction) {
        let def =
            self.nodes.get_mut(func_id.0).expect("ice: all function ids should have definitions");

        let func = match def {
            Node::Function(func) => func,
            _ => panic!("ice: all function ids should correspond to a function in the interner"),
        };
        *func = hir_func;
    }

    pub fn find_function(&self, function_name: &str) -> Option<FuncId> {
        self.func_meta
            .iter()
            .find(|(func_id, _func_meta)| self.function_name(func_id) == function_name)
            .map(|(func_id, _meta)| *func_id)
    }

    ///Interns a function's metadata.
    ///
    /// Note that the FuncId has been created already.
    /// See ModCollector for it's usage.
    pub fn push_fn_meta(&mut self, func_data: FuncMeta, func_id: FuncId) {
        self.func_meta.insert(func_id, func_data);
    }

    pub fn push_definition(
        &mut self,
        name: String,
        mutable: bool,
        definition: DefinitionKind,
        location: Location,
    ) -> DefinitionId {
        let id = DefinitionId(self.definitions.len());
        if let DefinitionKind::Function(func_id) = definition {
            self.function_definition_ids.insert(func_id, id);
        }

        self.definitions.push(DefinitionInfo { name, mutable, kind: definition, location });
        id
    }

    /// Push a function with the default modifiers and [`ModuleId`] for testing
    #[cfg(test)]
    pub fn push_test_function_definition(&mut self, name: String) -> FuncId {
        let id = self.push_fn(HirFunction::empty());
        let mut modifiers = FunctionModifiers::new();
        modifiers.name = name;
        let module = ModuleId::dummy_id();
        let location = Location::dummy();
        self.push_function_definition(id, modifiers, module, location);
        id
    }

    pub fn push_function(
        &mut self,
        id: FuncId,
        function: &FunctionDefinition,
        module: ModuleId,
        location: Location,
    ) -> DefinitionId {
        use ContractFunctionType::*;

        // We're filling in contract_function_type and is_internal now, but these will be verified
        // later during name resolution.
        let modifiers = FunctionModifiers {
            name: function.name.0.contents.clone(),
            visibility: function.visibility,
            attributes: function.attributes.clone(),
            is_unconstrained: function.is_unconstrained,
            contract_function_type: Some(if function.is_open { Open } else { Secret }),
            is_internal: Some(function.is_internal),
        };
        self.push_function_definition(id, modifiers, module, location)
    }

    pub fn push_function_definition(
        &mut self,
        func: FuncId,
        modifiers: FunctionModifiers,
        module: ModuleId,
        location: Location,
    ) -> DefinitionId {
        let name = modifiers.name.clone();
        self.function_modifiers.insert(func, modifiers);
        self.function_modules.insert(func, module);
        self.push_definition(name, false, DefinitionKind::Function(func), location)
    }

    pub fn set_function_trait(&mut self, func: FuncId, self_type: Type, trait_id: TraitId) {
        self.func_id_to_trait.insert(func, (self_type, trait_id));
    }

    pub fn get_function_trait(&self, func: &FuncId) -> Option<(Type, TraitId)> {
        self.func_id_to_trait.get(func).cloned()
    }

    /// Returns the visibility of the given function.
    ///
    /// The underlying function_visibilities map is populated during def collection,
    /// so this function can be called anytime afterward.
    pub fn function_visibility(&self, func: FuncId) -> FunctionVisibility {
        self.function_modifiers[&func].visibility
    }

    /// Returns the module this function was defined within
    pub fn function_module(&self, func: FuncId) -> ModuleId {
        self.function_modules[&func]
    }

    /// Returns the interned HIR function corresponding to `func_id`
    //
    // Cloning HIR structures is cheap, so we return owned structures
    pub fn function(&self, func_id: &FuncId) -> HirFunction {
        let def = self.nodes.get(func_id.0).expect("ice: all function ids should have definitions");

        match def {
            Node::Function(func) => func.clone(),
            _ => panic!("ice: all function ids should correspond to a function in the interner"),
        }
    }

    /// Returns the interned meta data corresponding to `func_id`
    pub fn function_meta(&self, func_id: &FuncId) -> &FuncMeta {
        self.func_meta.get(func_id).expect("ice: all function ids should have metadata")
    }

    pub fn try_function_meta(&self, func_id: &FuncId) -> Option<&FuncMeta> {
        self.func_meta.get(func_id)
    }

    pub fn function_ident(&self, func_id: &FuncId) -> crate::Ident {
        let name = self.function_name(func_id).to_owned();
        let span = self.function_meta(func_id).name.location.span;
        crate::Ident(Spanned::from(span, name))
    }

    pub fn function_name(&self, func_id: &FuncId) -> &str {
        &self.function_modifiers[func_id].name
    }

    pub fn function_modifiers(&self, func_id: &FuncId) -> &FunctionModifiers {
        &self.function_modifiers[func_id]
    }

    pub fn function_modifiers_mut(&mut self, func_id: &FuncId) -> &mut FunctionModifiers {
        self.function_modifiers.get_mut(func_id).expect("func_id should always have modifiers")
    }

    pub fn function_attributes(&self, func_id: &FuncId) -> &Attributes {
        &self.function_modifiers[func_id].attributes
    }

    pub fn struct_attributes(&self, struct_id: &StructId) -> &StructAttributes {
        &self.struct_attributes[struct_id]
    }

    /// Returns the interned statement corresponding to `stmt_id`
    pub fn statement(&self, stmt_id: &StmtId) -> HirStatement {
        let def =
            self.nodes.get(stmt_id.0).expect("ice: all statement ids should have definitions");

        match def {
            Node::Statement(stmt) => stmt.clone(),
            _ => panic!("ice: all statement ids should correspond to a statement in the interner"),
        }
    }

    /// Returns the interned let statement corresponding to `stmt_id`
    pub fn let_statement(&self, stmt_id: &StmtId) -> HirLetStatement {
        let def =
            self.nodes.get(stmt_id.0).expect("ice: all statement ids should have definitions");

        match def {
            Node::Statement(hir_stmt) => {
                match hir_stmt {
                    HirStatement::Let(let_stmt) => let_stmt.clone(),
                    _ => panic!("ice: all let statement ids should correspond to a let statement in the interner"),
                }
            },
            _ => panic!("ice: all statement ids should correspond to a statement in the interner"),
        }
    }

    /// Returns the interned expression corresponding to `expr_id`
    pub fn expression(&self, expr_id: &ExprId) -> HirExpression {
        let def =
            self.nodes.get(expr_id.0).expect("ice: all expression ids should have definitions");

        match def {
            Node::Expression(expr) => expr.clone(),
            _ => {
                panic!("ice: all expression ids should correspond to a expression in the interner")
            }
        }
    }

    /// Retrieves the definition where the given id was defined.
    /// This will panic if given DefinitionId::dummy_id. Use try_definition for
    /// any call with a possibly undefined variable.
    pub fn definition(&self, id: DefinitionId) -> &DefinitionInfo {
        &self.definitions[id.0]
    }

    /// Tries to retrieve the given id's definition.
    /// This function should be used during name resolution or type checking when we cannot be sure
    /// all variables have corresponding definitions (in case of an error in the user's code).
    pub fn try_definition(&self, id: DefinitionId) -> Option<&DefinitionInfo> {
        self.definitions.get(id.0)
    }

    /// Returns the name of the definition
    ///
    /// This is needed as the Environment needs to map variable names to witness indices
    pub fn definition_name(&self, id: DefinitionId) -> &str {
        &self.definition(id).name
    }

    pub fn expr_span(&self, expr_id: &ExprId) -> Span {
        self.id_location(expr_id).span
    }

    pub fn expr_location(&self, expr_id: &ExprId) -> Location {
        self.id_location(expr_id)
    }

    pub fn get_struct(&self, id: StructId) -> Shared<StructType> {
        self.structs[&id].clone()
    }

    pub fn get_trait(&self, id: TraitId) -> &Trait {
        &self.traits[&id]
    }

    pub fn get_trait_mut(&mut self, id: TraitId) -> &mut Trait {
        self.traits.get_mut(&id).expect("get_trait_mut given invalid TraitId")
    }

    pub fn try_get_trait(&self, id: TraitId) -> Option<&Trait> {
        self.traits.get(&id)
    }

    pub fn get_type_alias(&self, id: TypeAliasId) -> &TypeAliasType {
        &self.type_aliases[id.0]
    }

    pub fn get_global(&self, stmt_id: &StmtId) -> Option<GlobalInfo> {
        self.globals.get(stmt_id).cloned()
    }

    pub fn get_all_globals(&self) -> HashMap<StmtId, GlobalInfo> {
        self.globals.clone()
    }

    /// Returns the type of an item stored in the Interner or Error if it was not found.
    pub fn id_type(&self, index: impl Into<Index>) -> Type {
        self.id_to_type.get(&index.into()).cloned().unwrap_or(Type::Error)
    }

    pub fn id_type_substitute_trait_as_type(&self, def_id: DefinitionId) -> Type {
        let typ = self.id_type(def_id);
        if let Type::Function(args, ret, env) = &typ {
            let def = self.definition(def_id);
            if let Type::TraitAsType(..) = ret.as_ref() {
                if let DefinitionKind::Function(func_id) = def.kind {
                    let f = self.function(&func_id);
                    let func_body = f.as_expr();
                    let ret_type = self.id_type(func_body);
                    let new_type = Type::Function(args.clone(), Box::new(ret_type), env.clone());
                    return new_type;
                }
            }
        }
        typ
    }

    /// Returns the span of an item stored in the Interner
    pub fn id_location(&self, index: impl Into<Index>) -> Location {
        self.id_to_location.get(&index.into()).copied().unwrap()
    }

    /// Replaces the HirExpression at the given ExprId with a new HirExpression
    pub fn replace_expr(&mut self, id: &ExprId, new: HirExpression) {
        let old = self.nodes.get_mut(id.into()).unwrap();
        *old = Node::Expression(new);
    }

    pub fn next_type_variable_id(&self) -> TypeVariableId {
        let id = self.next_type_variable_id.get();
        self.next_type_variable_id.set(id + 1);
        TypeVariableId(id)
    }

    pub fn next_type_variable(&self) -> Type {
        Type::type_variable(self.next_type_variable_id())
    }

    pub fn store_instantiation_bindings(
        &mut self,
        expr_id: ExprId,
        instantiation_bindings: TypeBindings,
    ) {
        self.instantiation_bindings.insert(expr_id, instantiation_bindings);
    }

    pub fn get_instantiation_bindings(&self, expr_id: ExprId) -> &TypeBindings {
        &self.instantiation_bindings[&expr_id]
    }

    pub fn get_field_index(&self, expr_id: ExprId) -> usize {
        self.field_indices[&expr_id]
    }

    pub fn set_field_index(&mut self, expr_id: ExprId, index: usize) {
        self.field_indices.insert(expr_id, index);
    }

    pub fn function_definition_id(&self, function: FuncId) -> DefinitionId {
        self.function_definition_ids[&function]
    }

    /// Adds a non-trait method to a type.
    ///
    /// Returns `Some(duplicate)` if a matching method was already defined.
    /// Returns `None` otherwise.
    pub fn add_method(
        &mut self,
        self_type: &Type,
        method_name: String,
        method_id: FuncId,
        is_trait_method: bool,
    ) -> Option<FuncId> {
        match self_type {
            Type::Struct(struct_type, _generics) => {
                let id = struct_type.borrow().id;

                if let Some(existing) = self.lookup_method(self_type, id, &method_name, true) {
                    return Some(existing);
                }

                let key = (id, method_name);
                self.struct_methods.entry(key).or_default().add_method(method_id, is_trait_method);
                None
            }
            Type::Error => None,
            Type::MutableReference(element) => {
                self.add_method(element, method_name, method_id, is_trait_method)
            }

            other => {
                let key = get_type_method_key(self_type).unwrap_or_else(|| {
                    unreachable!("Cannot add a method to the unsupported type '{}'", other)
                });
                self.primitive_methods
                    .entry((key, method_name))
                    .or_default()
                    .add_method(method_id, is_trait_method);
                None
            }
        }
    }

    pub fn get_trait_implementation(&self, id: TraitImplId) -> Shared<TraitImpl> {
        self.trait_implementations[id.0].clone()
    }

    /// Given a `ObjectType: TraitId` pair, try to find an existing impl that satisfies the
    /// constraint. If an impl cannot be found, this will return a vector of each constraint
    /// in the path to get to the failing constraint. Usually this is just the single failing
    /// constraint, but when where clauses are involved, the failing constraint may be several
    /// constraints deep. In this case, all of the constraints are returned, starting with the
    /// failing one.
    pub fn lookup_trait_implementation(
        &self,
        object_type: &Type,
        trait_id: TraitId,
    ) -> Result<TraitImplKind, Vec<TraitConstraint>> {
        let (impl_kind, bindings) = self.try_lookup_trait_implementation(object_type, trait_id)?;
        Type::apply_type_bindings(bindings);
        Ok(impl_kind)
    }

    /// Similar to `lookup_trait_implementation` but does not apply any type bindings on success.
    pub fn try_lookup_trait_implementation(
        &self,
        object_type: &Type,
        trait_id: TraitId,
    ) -> Result<(TraitImplKind, TypeBindings), Vec<TraitConstraint>> {
        let mut bindings = TypeBindings::new();
        let impl_kind = self.lookup_trait_implementation_helper(
            object_type,
            trait_id,
            &mut bindings,
            IMPL_SEARCH_RECURSION_LIMIT,
        )?;
        Ok((impl_kind, bindings))
    }

    fn lookup_trait_implementation_helper(
        &self,
        object_type: &Type,
        trait_id: TraitId,
        type_bindings: &mut TypeBindings,
        recursion_limit: u32,
    ) -> Result<TraitImplKind, Vec<TraitConstraint>> {
        let make_constraint = || TraitConstraint::new(object_type.clone(), trait_id);

        // Prevent infinite recursion when looking for impls
        if recursion_limit == 0 {
            return Err(vec![make_constraint()]);
        }

        let object_type = object_type.substitute(type_bindings);

        let impls =
            self.trait_implementation_map.get(&trait_id).ok_or_else(|| vec![make_constraint()])?;

        for (existing_object_type, impl_kind) in impls {
            let (existing_object_type, instantiation_bindings) =
                existing_object_type.instantiate(self);

            let mut fresh_bindings = TypeBindings::new();

            if object_type.try_unify(&existing_object_type, &mut fresh_bindings).is_ok() {
                // The unification was successful so we can append fresh_bindings to our bindings list
                type_bindings.extend(fresh_bindings);

                if let TraitImplKind::Normal(impl_id) = impl_kind {
                    let trait_impl = self.get_trait_implementation(*impl_id);
                    let trait_impl = trait_impl.borrow();

                    if let Err(mut errors) = self.validate_where_clause(
                        &trait_impl.where_clause,
                        type_bindings,
                        &instantiation_bindings,
                        recursion_limit,
                    ) {
                        errors.push(make_constraint());
                        return Err(errors);
                    }
                }

                return Ok(impl_kind.clone());
            }
        }

        Err(vec![make_constraint()])
    }

    /// Verifies that each constraint in the given where clause is valid.
    /// If an impl cannot be found for any constraint, the erroring constraint is returned.
    fn validate_where_clause(
        &self,
        where_clause: &[TraitConstraint],
        type_bindings: &mut TypeBindings,
        instantiation_bindings: &TypeBindings,
        recursion_limit: u32,
    ) -> Result<(), Vec<TraitConstraint>> {
        for constraint in where_clause {
            // Instantiation bindings are generally safe to force substitute into the same type.
            // This is needed here to undo any bindings done to trait methods by monomorphization.
            // Otherwise, an impl for (A, B) could get narrowed to only an impl for e.g. (u8, u16).
            let constraint_type = constraint.typ.force_substitute(instantiation_bindings);
            let constraint_type = constraint_type.substitute(type_bindings);

            self.lookup_trait_implementation_helper(
                &constraint_type,
                constraint.trait_id,
                // Use a fresh set of type bindings here since the constraint_type originates from
                // our impl list, which we don't want to bind to.
                &mut TypeBindings::new(),
                recursion_limit - 1,
            )?;
        }
        Ok(())
    }

    /// Adds an "assumed" trait implementation to the currently known trait implementations.
    /// Unlike normal trait implementations, these are only assumed to exist. They often correspond
    /// to `where` clauses in functions where we assume there is some `T: Eq` even though we do
    /// not yet know T. For these cases, we store an impl here so that we assume they exist and
    /// can resolve them. They are then later verified when the function is called, and linked
    /// properly after being monomorphized to the correct variant.
    ///
    /// Returns true on success, or false if there is already an overlapping impl in scope.
    pub fn add_assumed_trait_implementation(
        &mut self,
        object_type: Type,
        trait_id: TraitId,
    ) -> bool {
        // Make sure there are no overlapping impls
        if self.try_lookup_trait_implementation(&object_type, trait_id).is_ok() {
            return false;
        }

        let entries = self.trait_implementation_map.entry(trait_id).or_default();
        entries.push((object_type.clone(), TraitImplKind::Assumed { object_type }));
        true
    }

    /// Adds a trait implementation to the list of known implementations.
    #[tracing::instrument(skip(self))]
    pub fn add_trait_implementation(
        &mut self,
        object_type: Type,
        trait_id: TraitId,
        impl_id: TraitImplId,
        trait_impl: Shared<TraitImpl>,
    ) -> Result<(), (Span, FileId)> {
        assert_eq!(impl_id.0, self.trait_implementations.len(), "trait impl defined out of order");

        self.trait_implementations.push(trait_impl.clone());

        // Ignoring overlapping TraitImplKind::Assumed impls here is perfectly fine.
        // It should never happen since impls are defined at global scope, but even
        // if they were, we should never prevent defining a new impl because a where
        // clause already assumes it exists.
        let (instantiated_object_type, substitutions) =
            object_type.instantiate_type_variables(self);

        if let Ok((TraitImplKind::Normal(existing), _)) =
            self.try_lookup_trait_implementation(&instantiated_object_type, trait_id)
        {
            let existing_impl = self.get_trait_implementation(existing);
            let existing_impl = existing_impl.borrow();
            return Err((existing_impl.ident.span(), existing_impl.file));
        }

        for method in &trait_impl.borrow().methods {
            let method_name = self.function_name(method).to_owned();
            self.add_method(&object_type, method_name, *method, true);
        }

        // The object type is generalized so that a generic impl will apply
        // to any type T, rather than just the generic type named T.
        let generalized_object_type = object_type.generalize_from_substitutions(substitutions);
        let entries = self.trait_implementation_map.entry(trait_id).or_default();
        entries.push((generalized_object_type, TraitImplKind::Normal(impl_id)));
        Ok(())
    }

    /// Search by name for a method on the given struct.
    ///
    /// If `check_type` is true, this will force `lookup_method` to check the type
    /// of each candidate instead of returning only the first candidate if there is exactly one.
    /// This is generally only desired when declaring new methods to check if they overlap any
    /// existing methods.
    ///
    /// Another detail is that this method does not handle auto-dereferencing through `&mut T`.
    /// So if an object is of type `self : &mut T` but a method only accepts `self: T` (or
    /// vice-versa), the call will not be selected. If this is ever implemented into this method,
    /// we can remove the `methods.len() == 1` check and the `check_type` early return.
    pub fn lookup_method(
        &self,
        typ: &Type,
        id: StructId,
        method_name: &str,
        force_type_check: bool,
    ) -> Option<FuncId> {
        let methods = self.struct_methods.get(&(id, method_name.to_owned()))?;

        // If there is only one method, just return it immediately.
        // It will still be typechecked later.
        if !force_type_check {
            if let Some(method) = methods.get_unambiguous() {
                return Some(method);
            }
        }

        self.find_matching_method(typ, methods, method_name)
    }

    /// Select the 1 matching method with an object type matching `typ`
    fn find_matching_method(
        &self,
        typ: &Type,
        methods: &Methods,
        method_name: &str,
    ) -> Option<FuncId> {
        if let Some(method) = methods.find_matching_method(typ, self) {
            Some(method)
        } else {
            // Failed to find a match for the type in question, switch to looking at impls
            // for all types `T`, e.g. `impl<T> Foo for T`
            let key = &(TypeMethodKey::Generic, method_name.to_owned());
            let global_methods = self.primitive_methods.get(key)?;
            global_methods.find_matching_method(typ, self)
        }
    }

    /// Looks up a given method name on the given primitive type.
    pub fn lookup_primitive_method(&self, typ: &Type, method_name: &str) -> Option<FuncId> {
        let key = get_type_method_key(typ)?;
        let methods = self.primitive_methods.get(&(key, method_name.to_owned()))?;
        self.find_matching_method(typ, methods, method_name)
    }

    pub fn lookup_primitive_trait_method_mut(
        &self,
        typ: &Type,
        method_name: &str,
    ) -> Option<FuncId> {
        let typ = Type::MutableReference(Box::new(typ.clone()));
        self.lookup_primitive_method(&typ, method_name)
    }

    /// Returns what the next trait impl id is expected to be.
    /// Note that this does not actually reserve the slot so care should
    /// be taken that the next trait impl added matches this ID.
    pub fn next_trait_impl_id(&self) -> TraitImplId {
        TraitImplId(self.trait_implementations.len())
    }

    /// Removes all TraitImplKind::Assumed from the list of known impls for the given trait
    pub fn remove_assumed_trait_implementations_for_trait(&mut self, trait_id: TraitId) {
        let entries = self.trait_implementation_map.entry(trait_id).or_default();
        entries.retain(|(_, kind)| matches!(kind, TraitImplKind::Normal(_)));
    }

    /// Tags the given identifier with the selected trait_impl so that monomorphization
    /// can later recover which impl was selected, or alternatively see if it needs to
    /// decide which impl to select (because the impl was Assumed).
    pub fn select_impl_for_expression(&mut self, ident_id: ExprId, trait_impl: TraitImplKind) {
        self.selected_trait_implementations.insert(ident_id, trait_impl);
    }

    /// Retrieves the impl selected for a given ExprId during name resolution.
    pub fn get_selected_impl_for_expression(&self, ident_id: ExprId) -> Option<TraitImplKind> {
        self.selected_trait_implementations.get(&ident_id).cloned()
    }

    /// Returns the [Location] of the definition of the given Ident found at [Span] of the given [FileId].
    /// Returns [None] when definition is not found.
    pub fn get_definition_location_from(&self, location: Location) -> Option<Location> {
        self.find_location_index(location)
            .and_then(|index| self.resolve_location(index))
            .or_else(|| self.try_resolve_trait_impl_location(location))
    }

    /// For a given [Index] we return [Location] to which we resolved to
    /// We currently return None for features not yet implemented
    /// TODO(#3659): LSP goto def should error when Ident at Location could not resolve
    fn resolve_location(&self, index: impl Into<Index>) -> Option<Location> {
        let node = self.nodes.get(index.into())?;

        match node {
            Node::Function(func) => self.resolve_location(func.as_expr()),
            Node::Expression(expression) => self.resolve_expression_location(expression),
            _ => None,
        }
    }

    /// Resolves the [Location] of the definition for a given [HirExpression]
    ///
    /// Note: current the code returns None because some expressions are not yet implemented.
    fn resolve_expression_location(&self, expression: &HirExpression) -> Option<Location> {
        match expression {
            HirExpression::Ident(ident) => {
                let definition_info = self.definition(ident.id);
                match definition_info.kind {
                    DefinitionKind::Function(func_id) => {
                        Some(self.function_meta(&func_id).location)
                    }
                    DefinitionKind::Local(_local_id) => Some(definition_info.location),
                    _ => None,
                }
            }
            HirExpression::Constructor(expr) => {
                let struct_type = &expr.r#type.borrow();
                Some(struct_type.location)
            }
            HirExpression::MemberAccess(expr_member_access) => {
                self.resolve_struct_member_access(expr_member_access)
            }
            HirExpression::Call(expr_call) => {
                let func = expr_call.func;
                self.resolve_location(func)
            }

            _ => None,
        }
    }

    /// Resolves the [Location] of the definition for a given [crate::hir_def::expr::HirMemberAccess]
    /// This is used to resolve the location of a struct member access.
    /// For example, in the expression `foo.bar` we want to resolve the location of `bar`
    /// to the location of the definition of `bar` in the struct `foo`.
    fn resolve_struct_member_access(
        &self,
        expr_member_access: &crate::hir_def::expr::HirMemberAccess,
    ) -> Option<Location> {
        let expr_lhs = &expr_member_access.lhs;
        let expr_rhs = &expr_member_access.rhs;

        let lhs_self_struct = match self.id_type(expr_lhs) {
            Type::Struct(struct_type, _) => struct_type,
            _ => return None,
        };

        let struct_type = lhs_self_struct.borrow();
        let field_names = struct_type.field_names();

        field_names.iter().find(|field_name| field_name.0 == expr_rhs.0).map(|found_field_name| {
            Location::new(found_field_name.span(), struct_type.location.file)
        })
    }

    /// Retrieves the trait id for a given binary operator.
    /// All binary operators correspond to a trait - although multiple may correspond
    /// to the same trait (such as `==` and `!=`).
    /// `self.operator_traits` is expected to be filled before name resolution,
    /// during definition collection.
    pub fn get_operator_trait_method(&self, operator: BinaryOpKind) -> TraitMethodId {
        let trait_id = self.operator_traits[&operator];

        // Assume that the operator's method to be overloaded is the first method of the trait.
        TraitMethodId { trait_id, method_index: 0 }
    }

    /// Add the given trait as an operator trait if its name matches one of the
    /// operator trait names (Add, Sub, ...).
    pub fn try_add_operator_trait(&mut self, trait_id: TraitId) {
        let the_trait = self.get_trait(trait_id);

        let operator = match the_trait.name.0.contents.as_str() {
            "Add" => BinaryOpKind::Add,
            "Sub" => BinaryOpKind::Subtract,
            "Mul" => BinaryOpKind::Multiply,
            "Div" => BinaryOpKind::Divide,
            "Rem" => BinaryOpKind::Modulo,
            "Eq" => BinaryOpKind::Equal,
            "Ord" => BinaryOpKind::Less,
            "BitAnd" => BinaryOpKind::And,
            "BitOr" => BinaryOpKind::Or,
            "BitXor" => BinaryOpKind::Xor,
            "Shl" => BinaryOpKind::ShiftLeft,
            "Shr" => BinaryOpKind::ShiftRight,
            _ => return,
        };

        self.operator_traits.insert(operator, trait_id);

        // Some operators also require we insert a matching entry for related operators
        match operator {
            BinaryOpKind::Equal => {
                self.operator_traits.insert(BinaryOpKind::NotEqual, trait_id);
            }
            BinaryOpKind::Less => {
                self.operator_traits.insert(BinaryOpKind::LessEqual, trait_id);
                self.operator_traits.insert(BinaryOpKind::Greater, trait_id);
                self.operator_traits.insert(BinaryOpKind::GreaterEqual, trait_id);

                let the_trait = self.get_trait(trait_id);
                self.ordering_type = match &the_trait.methods[0].typ {
                    Type::Forall(_, typ) => match typ.as_ref() {
                        Type::Function(_, return_type, _) => Some(return_type.as_ref().clone()),
                        other => unreachable!("Expected function type for `cmp`, found {}", other),
                    },
                    other => unreachable!("Expected Forall type for `cmp`, found {}", other),
                };
            }
            _ => (),
        }
    }

    /// This function is needed when creating a NodeInterner for testing so that calls
    /// to `get_operator_trait` do not panic when the stdlib isn't present.
    #[cfg(test)]
    pub fn populate_dummy_operator_traits(&mut self) {
        let dummy_trait = TraitId(ModuleId::dummy_id());
        self.operator_traits.insert(BinaryOpKind::Add, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Subtract, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Multiply, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Divide, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Modulo, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Equal, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::NotEqual, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Less, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::LessEqual, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Greater, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::GreaterEqual, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::And, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Or, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::Xor, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::ShiftLeft, dummy_trait);
        self.operator_traits.insert(BinaryOpKind::ShiftRight, dummy_trait);
    }

    pub(crate) fn ordering_type(&self) -> Type {
        self.ordering_type.clone().expect("Expected ordering_type to be set in the NodeInterner")
    }

    /// Attempts to resolve [Location] of [Trait] based on [Location] of [TraitImpl]
    /// This is used by LSP to resolve the location of a trait based on the location of a trait impl.
    ///
    /// Example:
    /// impl Foo for Bar { ... } -> trait Foo { ... }
    fn try_resolve_trait_impl_location(&self, location: Location) -> Option<Location> {
        self.trait_implementations
            .iter()
            .find(|shared_trait_impl| {
                let trait_impl = shared_trait_impl.borrow();
                trait_impl.file == location.file && trait_impl.ident.span().contains(&location.span)
            })
            .and_then(|shared_trait_impl| {
                let trait_impl = shared_trait_impl.borrow();
                self.traits.get(&trait_impl.trait_id).map(|trait_| trait_.location)
            })
    }
}

impl Methods {
    /// Get a single, unambiguous reference to a name if one exists.
    /// If not, there may be multiple methods of the same name for a given
    /// type or there may be no methods at all.
    fn get_unambiguous(&self) -> Option<FuncId> {
        if self.direct.len() == 1 {
            Some(self.direct[0])
        } else if self.direct.is_empty() && self.trait_impl_methods.len() == 1 {
            Some(self.trait_impl_methods[0])
        } else {
            None
        }
    }

    fn add_method(&mut self, method: FuncId, is_trait_method: bool) {
        if is_trait_method {
            self.trait_impl_methods.push(method);
        } else {
            self.direct.push(method);
        }
    }

    /// Iterate through each method, starting with the direct methods
    fn iter(&self) -> impl Iterator<Item = FuncId> + '_ {
        self.direct.iter().copied().chain(self.trait_impl_methods.iter().copied())
    }

    /// Select the 1 matching method with an object type matching `typ`
    fn find_matching_method(&self, typ: &Type, interner: &NodeInterner) -> Option<FuncId> {
        // When adding methods we always check they do not overlap, so there should be
        // at most 1 matching method in this list.
        for method in self.iter() {
            match interner.function_meta(&method).typ.instantiate(interner).0 {
                Type::Function(args, _, _) => {
                    if let Some(object) = args.get(0) {
                        let mut bindings = TypeBindings::new();

                        if object.try_unify(typ, &mut bindings).is_ok() {
                            Type::apply_type_bindings(bindings);
                            return Some(method);
                        }
                    }
                }
                Type::Error => (),
                other => unreachable!("Expected function type, found {other}"),
            }
        }
        None
    }
}

/// These are the primitive type variants that we support adding methods to
#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
enum TypeMethodKey {
    /// Fields and integers share methods for ease of use. These methods may still
    /// accept only fields or integers, it is just that their names may not clash.
    FieldOrInt,
    Array,
    Bool,
    String,
    FmtString,
    Unit,
    Tuple,
    Function,
    Generic,
}

fn get_type_method_key(typ: &Type) -> Option<TypeMethodKey> {
    use TypeMethodKey::*;
    let typ = typ.follow_bindings();
    match &typ {
        Type::FieldElement => Some(FieldOrInt),
        Type::Array(_, _) => Some(Array),
        Type::Integer(_, _) => Some(FieldOrInt),
        Type::TypeVariable(_, TypeVariableKind::IntegerOrField) => Some(FieldOrInt),
        Type::Bool => Some(Bool),
        Type::String(_) => Some(String),
        Type::FmtString(_, _) => Some(FmtString),
        Type::Unit => Some(Unit),
        Type::Tuple(_) => Some(Tuple),
        Type::Function(_, _, _) => Some(Function),
        Type::NamedGeneric(_, _) => Some(Generic),
        Type::MutableReference(element) => get_type_method_key(element),

        // We do not support adding methods to these types
        Type::TypeVariable(_, _)
        | Type::Forall(_, _)
        | Type::Constant(_)
        | Type::Error
        | Type::NotConstant
        | Type::Struct(_, _)
        | Type::TraitAsType(..) => None,
    }
}

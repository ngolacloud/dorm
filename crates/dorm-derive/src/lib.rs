//! `#[derive(Model)]` — the macro that makes a plain struct a model.
//!
//! What it emits, for a struct `Tenant`:
//!
//! * `impl Model for Tenant` — the row decoder, the primary key, the column
//!   values for a write.
//! * `const`/`static` metadata, plus one `inventory::submit!` so the model joins
//!   the registry at link time. That submission is the whole mechanism by which
//!   the REPL discovers models: there is no list to maintain, and a model that
//!   compiles is a model the shell can see.
//! * `pub mod tenant` — the typed field handles, so `tenant::slug.eq("acme")`
//!   compiles and `tenant::slug.icontains(..)` only compiles for text.
//! * `pub struct TenantFields` — the relation accessor, so a foreign key on
//!   another model can expose `…tenant.slug()` without that model's macro
//!   knowing `Tenant`'s fields. This is the piece that lets the two derives run
//!   independently and in any order.
//! * `pub struct NewTenant` — the insert shape, carrying only the columns a
//!   caller supplies.
//!
//! One requirement the macro cannot check: a model with a foreign key must be
//! able to *name* `<Target>Fields`, so the target model has to be in scope where
//! the deriving struct is defined. In practice both live in the same models
//! module, which is also where the REPL expects them.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, Data, DeriveInput, Error, Fields, GenericArgument, Ident, LitInt, LitStr,
    PathArguments, Result, Type,
};

#[proc_macro_derive(Model, attributes(model, field, m2m))]
pub fn derive_model(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    expand(&ast)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// What `#[model(..)]` can say.
struct ModelOpts {
    table: Option<String>,
    ordering: Vec<String>,
    tenant_scoped: bool,
    /// Each entry is a composite-unique group of column names.
    unique_together: Vec<Vec<String>>,
    /// Table-level `CHECK` predicates (raw SQL, no `CHECK (...)` wrapper).
    checks: Vec<String>,
}

/// What one `#[m2m(..)]` struct attribute declares: a named accessor onto a
/// many-to-many through an explicit junction model.
struct M2mSpec {
    /// The accessor method to emit — `name = "tags"` gives `post.tags()`.
    name: Ident,
    /// The junction model's registered *name* (not a type), resolved at runtime
    /// through the registry by [`ManyToMany::through`].
    through: String,
    /// The junction field pointing back at *this* model — the "left" key.
    via: String,
    /// The junction field pointing at the *other* model — the "right" key.
    to: String,
}

/// What `#[field(..)]` can say, plus what was inferred from the Rust type.
struct FieldSpec {
    ident: Ident,
    /// The declared Rust type, used to decode the column.
    ty: Type,
    /// The type with any `Option<>` peeled off.
    inner_ty: Type,
    column: String,
    field_type: Ident,
    nullable: bool,
    primary_key: bool,
    unique: bool,
    index: bool,
    max_length: Option<u32>,
    default: Option<String>,
    auto_now_add: bool,
    auto_now: bool,
    foreign_key: Option<String>,
    on_delete: String,
    related_name: Option<String>,
}

impl FieldSpec {
    /// Whether a caller supplies this column, as opposed to the database.
    ///
    /// A serial primary key, a column with a SQL default, and the `auto_now`
    /// pair are all filled in server-side; asking the caller to invent a value
    /// for them is what `NewRow` exists to avoid.
    fn caller_supplied(&self) -> bool {
        // Written as early returns rather than one expression: each clause is a
        // separate reason, and the reasons are what a reader needs.
        if self.auto_now_add || self.auto_now {
            return false;
        }
        if self.default.is_some() {
            return false;
        }
        // A primary key is server-filled only in the serial/identity case: an
        // INTEGER key with no default. A primary key of any other type — a TEXT
        // key like DBaaS `tenant_databases.addon_name` — is the caller's value
        // and must appear in `New<Model>`, or the insert silently drops it.
        if self.primary_key {
            return !matches!(self.field_type.to_string().as_str(), "I32" | "I64");
        }
        true
    }
}

fn expand(ast: &DeriveInput) -> Result<TokenStream2> {
    let name = &ast.ident;
    if !ast.generics.params.is_empty() {
        return Err(Error::new_spanned(
            &ast.generics,
            "a model cannot be generic: its metadata is one `static`, and a generic struct would need one per instantiation",
        ));
    }

    let fields = match &ast.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => &n.named,
            _ => {
                return Err(Error::new_spanned(
                    &s.fields,
                    "a model needs named fields: a column has a name, and a tuple struct has none to give it",
                ))
            }
        },
        _ => {
            return Err(Error::new_spanned(
                name,
                "only a struct can be a model",
            ))
        }
    };

    let opts = parse_model_opts(ast)?;
    let table = opts
        .table
        .clone()
        .unwrap_or_else(|| default_table(&name.to_string()));

    let mut specs = Vec::new();
    for f in fields {
        specs.push(parse_field(f)?);
    }

    if specs.iter().filter(|s| s.primary_key).count() > 1 {
        return Err(Error::new_spanned(
            name,
            "a model can declare only one `primary_key`",
        ));
    }

    let m2m = parse_m2m_specs(ast)?;

    let meta_tokens = build_meta(name, &table, &opts, &specs);
    let impl_tokens = build_model_impl(name, &specs, &m2m)?;
    let fields_struct = build_fields_struct(name, &specs);
    let module = build_module(name, &specs);
    let new_row = build_new_row(name, &specs);

    Ok(quote! {
        #meta_tokens
        #impl_tokens
        #fields_struct
        #module
        #new_row
    })
}

fn parse_model_opts(ast: &DeriveInput) -> Result<ModelOpts> {
    let mut opts = ModelOpts {
        table: None,
        ordering: Vec::new(),
        tenant_scoped: false,
        unique_together: Vec::new(),
        checks: Vec::new(),
    };
    for attr in &ast.attrs {
        if !attr.path().is_ident("model") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match key.as_str() {
                "table" => {
                    let s: LitStr = meta.value()?.parse()?;
                    opts.table = Some(s.value());
                }
                "ordering" => {
                    let s: LitStr = meta.value()?.parse()?;
                    // Comma-separated, so one attribute can order by several
                    // columns: `ordering = "-created_at, slug"`.
                    opts.ordering = s
                        .value()
                        .split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|p| !p.is_empty())
                        .collect();
                }
                "tenant_scoped" => opts.tenant_scoped = true,
                "unique_together" => {
                    // One group per attribute, columns comma-separated:
                    // `unique_together = "tenant, name"`. Repeat the attribute
                    // for several groups.
                    let s: LitStr = meta.value()?.parse()?;
                    let group: Vec<String> = s
                        .value()
                        .split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|p| !p.is_empty())
                        .collect();
                    if !group.is_empty() {
                        opts.unique_together.push(group);
                    }
                }
                "check" => {
                    // Raw table-level CHECK predicate, from the model definition
                    // (compile time), never a caller. Repeat for several.
                    let s: LitStr = meta.value()?.parse()?;
                    opts.checks.push(s.value());
                }
                other => {
                    return Err(meta.error(format!(
                        "unknown `model` option `{other}`; known: table, ordering, \
                         tenant_scoped, unique_together, check"
                    )))
                }
            }
            Ok(())
        })?;
    }
    Ok(opts)
}

/// Parse every `#[m2m(..)]` struct attribute. Each needs all four of `name`,
/// `through`, `via`, `to`; a missing one is an error naming it, since a
/// half-specified association could not resolve.
fn parse_m2m_specs(ast: &DeriveInput) -> Result<Vec<M2mSpec>> {
    let mut specs = Vec::new();
    for attr in &ast.attrs {
        if !attr.path().is_ident("m2m") {
            continue;
        }
        let (mut name, mut through, mut via, mut to) = (None, None, None, None);
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match key.as_str() {
                "name" => {
                    let s: LitStr = meta.value()?.parse()?;
                    // The accessor's method name — an identifier, not a string.
                    name = Some(Ident::new(&s.value(), s.span()));
                }
                "through" => through = Some(meta.value()?.parse::<LitStr>()?.value()),
                "via" => via = Some(meta.value()?.parse::<LitStr>()?.value()),
                "to" => to = Some(meta.value()?.parse::<LitStr>()?.value()),
                other => {
                    return Err(meta.error(format!(
                        "unknown `m2m` option `{other}`; known: name, through, via, to"
                    )))
                }
            }
            Ok(())
        })?;
        let missing = |what: &str| {
            Error::new_spanned(
                attr,
                format!("`#[m2m(..)]` needs `{what}`; all of name, through, via, to are required"),
            )
        };
        specs.push(M2mSpec {
            name: name.ok_or_else(|| missing("name"))?,
            through: through.ok_or_else(|| missing("through"))?,
            via: via.ok_or_else(|| missing("via"))?,
            to: to.ok_or_else(|| missing("to"))?,
        });
    }
    Ok(specs)
}

fn parse_field(f: &syn::Field) -> Result<FieldSpec> {
    let ident = f
        .ident
        .clone()
        .ok_or_else(|| Error::new_spanned(f, "a model field needs a name"))?;

    let (inner_ty, nullable) = peel_option(&f.ty);
    let mut spec = FieldSpec {
        ident: ident.clone(),
        ty: f.ty.clone(),
        inner_ty: inner_ty.clone(),
        column: ident.to_string(),
        // Filled in below: either inferred from the Rust type, or stated.
        field_type: format_ident!("Text"),
        nullable,
        primary_key: false,
        unique: false,
        index: false,
        max_length: None,
        default: None,
        auto_now_add: false,
        auto_now: false,
        foreign_key: None,
        on_delete: "restrict".to_string(),
        related_name: None,
    };

    let mut stated_type: Option<Ident> = None;
    for attr in &f.attrs {
        if !attr.path().is_ident("field") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match key.as_str() {
                "primary_key" => spec.primary_key = true,
                "unique" => spec.unique = true,
                "index" => spec.index = true,
                "auto_now_add" => spec.auto_now_add = true,
                "auto_now" => spec.auto_now = true,
                "column" => {
                    let s: LitStr = meta.value()?.parse()?;
                    spec.column = s.value();
                }
                "default" => {
                    let s: LitStr = meta.value()?.parse()?;
                    spec.default = Some(s.value());
                }
                "max_length" => {
                    let n: LitInt = meta.value()?.parse()?;
                    spec.max_length = Some(n.base10_parse()?);
                }
                "foreign_key" => {
                    let s: LitStr = meta.value()?.parse()?;
                    spec.foreign_key = Some(s.value());
                }
                "on_delete" => {
                    let s: LitStr = meta.value()?.parse()?;
                    let v = s.value();
                    if !matches!(
                        v.as_str(),
                        "cascade" | "restrict" | "set_null" | "do_nothing"
                    ) {
                        return Err(syn::Error::new(
                            s.span(),
                            "on_delete must be one of: cascade, restrict, set_null, do_nothing",
                        ));
                    }
                    spec.on_delete = v;
                }
                "related_name" => {
                    let s: LitStr = meta.value()?.parse()?;
                    spec.related_name = Some(s.value());
                }
                "column_type" => {
                    let s: LitStr = meta.value()?.parse()?;
                    stated_type = Some(column_type_ident(&s)?);
                }
                other => {
                    return Err(meta.error(format!(
                        "unknown `field` option `{other}`; known: primary_key, unique, index, \
                         column, default, max_length, auto_now_add, auto_now, foreign_key, \
                         on_delete, related_name, column_type"
                    )))
                }
            }
            Ok(())
        })?;
    }

    // A `ForeignKey<T>` in the type position says the same thing as
    // `foreign_key = "T"`; accepting both and reconciling them here means the
    // two cannot contradict each other further down.
    if let Some(target) = foreign_key_target(&inner_ty) {
        if spec.foreign_key.is_none() {
            spec.foreign_key = Some(target);
        }
    }

    spec.field_type = match stated_type {
        Some(t) => t,
        None if spec.foreign_key.is_some() => {
            // The column holds the target's primary key. `BIGINT` is the
            // default; a target keyed by text says so with `column_type`.
            format_ident!("I64")
        }
        None => infer_field_type(&inner_ty)?,
    };

    Ok(spec)
}

/// `Option<T>` → `(T, true)`; anything else → `(it, false)`.
fn peel_option(ty: &Type) -> (Type, bool) {
    if let Type::Path(p) = ty {
        if let Some(seg) = p.path.segments.last() {
            if seg.ident == "Option" {
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    if let Some(GenericArgument::Type(inner)) = args.args.first() {
                        return (inner.clone(), true);
                    }
                }
            }
        }
    }
    (ty.clone(), false)
}

/// The `T` of a `ForeignKey<T>`, as a string.
fn foreign_key_target(ty: &Type) -> Option<String> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "ForeignKey" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first()? {
        GenericArgument::Type(Type::Path(t)) => Some(t.path.segments.last()?.ident.to_string()),
        _ => None,
    }
}

fn column_type_ident(s: &LitStr) -> Result<Ident> {
    let v = s.value();
    let name = match v.as_str() {
        "int" | "integer" | "i32" => "I32",
        "bigint" | "i64" => "I64",
        "text" | "string" => "Text",
        "bool" | "boolean" => "Bool",
        "decimal" | "numeric" => "Decimal",
        "timestamp" | "timestamptz" => "Timestamp",
        "uuid" => "Uuid",
        "json" | "jsonb" => "Json",
        other => {
            return Err(Error::new(
                s.span(),
                format!(
                    "unknown column_type `{other}`; known: int, bigint, text, bool, decimal, \
                     timestamp, uuid, json"
                ),
            ))
        }
    };
    Ok(format_ident!("{}", name))
}

/// Map a Rust type to a column type.
///
/// Syntactic, by necessity: the metadata is a `const`, and a trait method could
/// not be called to build one. The upside is that an unsupported type fails here
/// with a message naming the field, rather than deep inside a `ToSql` bound.
fn infer_field_type(ty: &Type) -> Result<Ident> {
    let Type::Path(p) = ty else {
        return Err(Error::new_spanned(
            ty,
            "a model field must be a plain named type",
        ));
    };
    let seg = p
        .path
        .segments
        .last()
        .ok_or_else(|| Error::new_spanned(ty, "a model field must be a plain named type"))?;
    let name = seg.ident.to_string();
    let mapped = match name.as_str() {
        "i32" => "I32",
        "i64" => "I64",
        "String" => "Text",
        "bool" => "Bool",
        "Decimal" => "Decimal",
        "DateTime" => "Timestamp",
        "Uuid" => "Uuid",
        // `serde_json::Value`, however it was imported.
        "Value" | "JsonValue" => "Json",
        other => {
            return Err(Error::new_spanned(
                ty,
                format!(
                    "`{other}` is not a column type the ORM knows. Supported: i32, i64, String, \
                     bool, rust_decimal::Decimal, chrono::DateTime<Utc>, uuid::Uuid, \
                     serde_json::Value, Option<T> of any of those, and ForeignKey<T>. \
                     State it explicitly with `#[field(column_type = \"...\")]` if the type is \
                     an alias."
                ),
            ))
        }
    };
    Ok(format_ident!("{}", mapped))
}

/// `Tenant` → `tenants`, `Application` → `applications`.
///
/// A deliberately minimal pluralisation: a `y` becomes `ies`, an `s`/`x`/`z`/
/// `ch`/`sh` ending takes `es`, everything else takes `s`. Anything irregular
/// is spelled out with `#[model(table = "...")]` rather than guessed at.
fn default_table(name: &str) -> String {
    let snake = to_snake_case(name);
    if let Some(stem) = snake.strip_suffix('y') {
        if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) {
            return format!("{stem}ies");
        }
    }
    if snake.ends_with('s')
        || snake.ends_with('x')
        || snake.ends_with('z')
        || snake.ends_with("ch")
        || snake.ends_with("sh")
    {
        return format!("{snake}es");
    }
    format!("{snake}s")
}

fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.char_indices() {
        if c.is_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn build_meta(name: &Ident, table: &str, opts: &ModelOpts, specs: &[FieldSpec]) -> TokenStream2 {
    let model_name = name.to_string();
    let fields_const = format_ident!("__{}_FIELDS", name.to_string().to_uppercase());
    let meta_const = format_ident!("__{}_META", name.to_string().to_uppercase());
    let meta_static = format_ident!("__{}_META_STATIC", name.to_string().to_uppercase());

    let field_entries = specs.iter().map(|s| {
        let fname = s.ident.to_string();
        let column = &s.column;
        let ft = &s.field_type;
        let nullable = s.nullable;
        let pk = s.primary_key;
        let unique = s.unique;
        let index = s.index;
        let max_length = match s.max_length {
            Some(n) => quote!(::core::option::Option::Some(#n)),
            None => quote!(::core::option::Option::None),
        };
        let default = match &s.default {
            Some(d) => quote!(::core::option::Option::Some(#d)),
            None => quote!(::core::option::Option::None),
        };
        let auto_now_add = s.auto_now_add;
        let auto_now = s.auto_now;
        let relation = match &s.foreign_key {
            Some(target) => {
                let on_delete = match s.on_delete.as_str() {
                    "cascade" => quote!(Cascade),
                    "set_null" => quote!(SetNull),
                    "do_nothing" => quote!(DoNothing),
                    _ => quote!(Restrict),
                };
                let related = match &s.related_name {
                    Some(r) => quote!(::core::option::Option::Some(#r)),
                    None => quote!(::core::option::Option::None),
                };
                quote!(::core::option::Option::Some(::dorm_core::meta::Relation {
                    model: #target,
                    on_delete: ::dorm_core::meta::OnDelete::#on_delete,
                    related_name: #related,
                }))
            }
            None => quote!(::core::option::Option::None),
        };

        quote! {
            ::dorm_core::meta::FieldMeta {
                name: #fname,
                column: #column,
                ty: ::dorm_core::value::FieldType::#ft,
                nullable: #nullable,
                primary_key: #pk,
                unique: #unique,
                index: #index,
                max_length: #max_length,
                default: #default,
                auto_now_add: #auto_now_add,
                auto_now: #auto_now,
                relation: #relation,
            }
        }
    });

    let ordering = opts.ordering.iter().map(|o| quote!(#o));
    let tenant_scoped = opts.tenant_scoped;
    // Each group becomes a `&[&str]`; the whole thing a `&[&[&str]]`.
    let unique_together = opts.unique_together.iter().map(|group| {
        let cols = group.iter().map(|c| quote!(#c));
        quote!(&[#(#cols),*] as &[&'static str])
    });
    let checks = opts.checks.iter().map(|c| quote!(#c));

    quote! {
        #[doc(hidden)]
        const #fields_const: &[::dorm_core::meta::FieldMeta] = &[#(#field_entries),*];

        // A `const` first, because `inventory::submit!` needs a const-evaluable
        // expression and a `static` cannot be read from one. The `static` then
        // borrows it, which is what `Model::meta()` returns.
        #[doc(hidden)]
        const #meta_const: ::dorm_core::meta::ModelMeta =
            ::dorm_core::meta::ModelMeta {
                name: #model_name,
                table: #table,
                fields: #fields_const,
                ordering: &[#(#ordering),*],
                tenant_scoped: #tenant_scoped,
                unique_together: &[#(#unique_together),*],
                checks: &[#(#checks),*],
            };

        #[doc(hidden)]
        static #meta_static: ::dorm_core::meta::ModelMeta = #meta_const;

        // This is the line that makes the model visible to the REPL. The linker
        // gathers every submission; nothing enumerates models by hand.
        ::dorm_core::inventory::submit! { #meta_const }
    }
}

fn build_model_impl(name: &Ident, specs: &[FieldSpec], m2m: &[M2mSpec]) -> Result<TokenStream2> {
    let meta_static = format_ident!("__{}_META_STATIC", name.to_string().to_uppercase());

    // The decoder reads columns by position, in metadata order, offset by where
    // this model's columns start in the row.
    let decoders = specs.iter().enumerate().map(|(i, s)| {
        let ident = &s.ident;
        let inner = &s.inner_ty;
        let column = &s.column;
        if s.foreign_key.is_some() {
            let variant = &s.field_type;
            if s.nullable {
                quote! {
                    #ident: {
                        let raw: ::core::option::Option<_> = row
                            .try_get::<::core::option::Option<i64>>(offset + #i)
                            .map_err(|_| ::dorm_core::Error::UnexpectedColumnType {
                                column: #column.to_string(),
                                want: "bigint foreign key",
                            })?;
                        match raw {
                            ::core::option::Option::Some(v) => ::core::option::Option::Some(
                                ::dorm_core::field::ForeignKey::from_id(
                                    ::dorm_core::value::Value::#variant(v),
                                ),
                            ),
                            ::core::option::Option::None => ::core::option::Option::None,
                        }
                    },
                }
            } else {
                quote! {
                    #ident: ::dorm_core::field::ForeignKey::from_id(
                        ::dorm_core::value::Value::#variant(
                            row.try_get::<i64>(offset + #i).map_err(|_| {
                                ::dorm_core::Error::UnexpectedColumnType {
                                    column: #column.to_string(),
                                    want: "bigint foreign key",
                                }
                            })?,
                        ),
                    ),
                }
            }
        } else if s.nullable {
            quote! {
                #ident: row
                    .try_get::<::core::option::Option<#inner>>(offset + #i)
                    .map_err(|_| ::dorm_core::Error::UnexpectedColumnType {
                        column: #column.to_string(),
                        want: ::core::stringify!(#inner),
                    })?,
            }
        } else {
            quote! {
                #ident: row
                    .try_get::<#inner>(offset + #i)
                    .map_err(|_| ::dorm_core::Error::UnexpectedColumnType {
                        column: #column.to_string(),
                        want: ::core::stringify!(#inner),
                    })?,
            }
        }
    });

    let pk_expr = match specs.iter().find(|s| s.primary_key) {
        Some(s) => {
            let ident = &s.ident;
            quote!(::dorm_core::value::ToValue::to_value(&self.#ident))
        }
        // No primary key declared: the instance operations that need one report
        // it by name. Returning NULL here is what makes `save()` insert.
        None => quote!(::dorm_core::value::Value::Null),
    };

    let column_values = specs.iter().filter(|s| s.caller_supplied()).map(|s| {
        let ident = &s.ident;
        let fname = s.ident.to_string();
        quote! {
            (
                #fname.to_string(),
                ::dorm_core::value::ToValue::to_value(&self.#ident),
            )
        }
    });

    let attach_arms = specs
        .iter()
        .filter(|s| s.foreign_key.is_some() && !s.nullable)
        .map(|s| {
            let ident = &s.ident;
            let fname = s.ident.to_string();
            let target = foreign_key_type(&s.inner_ty);
            quote! {
                #fname => {
                    let loaded = <#target as ::dorm_core::model::Model>::from_row(row, offset)?;
                    self.#ident.attach(loaded);
                    ::core::result::Result::Ok(())
                }
            }
        });

    // One accessor per `#[m2m(..)]`: a method named by `name`, returning the
    // association with this instance's key already bound. Fallible because the
    // junction and its fields resolve through the registry at runtime — a
    // misspelt `through`/`via`/`to` surfaces here, not at compile time.
    let m2m_accessors = m2m.iter().map(|s| {
        let method = &s.name;
        let through = &s.through;
        let via = &s.via;
        let to = &s.to;
        let doc = format!(
            "The `{method}` many-to-many association, through the `{through}` junction \
             (`{via}` → this model, `{to}` → the other).\n\n\
             Returns the association with this instance's key already bound, so \
             `self.{method}()?.add(other_pk)` and `.all_keys()` need no repetition of \
             this key. Fallible: the junction resolves through the registry."
        );
        quote! {
            #[doc = #doc]
            pub fn #method(
                &self,
            ) -> ::dorm_core::Result<::dorm_core::m2m::BoundManyToMany> {
                ::core::result::Result::Ok(
                    ::dorm_core::m2m::ManyToMany::through(#through, #via, #to)?
                        .bind(::dorm_core::model::Model::pk(self)),
                )
            }
        }
    });

    Ok(quote! {
        impl ::dorm_core::model::Model for #name {
            fn meta() -> &'static ::dorm_core::meta::ModelMeta {
                &#meta_static
            }

            fn from_row(
                row: &::dorm_core::__private::Row,
                offset: usize,
            ) -> ::dorm_core::Result<Self> {
                ::core::result::Result::Ok(Self { #(#decoders)* })
            }

            fn pk(&self) -> ::dorm_core::value::Value {
                #pk_expr
            }

            fn column_values(
                &self,
            ) -> ::std::vec::Vec<(::std::string::String, ::dorm_core::value::Value)> {
                ::std::vec![#(#column_values),*]
            }

            fn attach_related(
                &mut self,
                field: &str,
                row: &::dorm_core::__private::Row,
                offset: usize,
            ) -> ::dorm_core::Result<()> {
                match field {
                    #(#attach_arms)*
                    // A name that is not a foreign key of this model is not an
                    // error here: the query layer already rejected it.
                    _ => ::core::result::Result::Ok(()),
                }
            }
        }

        impl #name {
            /// Insert this instance, or update it if it already has a key.
            pub async fn save(&mut self) -> ::dorm_core::Result<()> {
                ::dorm_core::model::save_instance(self).await
            }

            /// Delete this instance by primary key.
            pub async fn delete(&self) -> ::dorm_core::Result<u64> {
                ::dorm_core::model::delete_instance(self).await
            }

            /// Re-read this instance, discarding local changes.
            pub async fn refresh(&mut self) -> ::dorm_core::Result<()> {
                ::dorm_core::model::refresh_instance(self).await
            }

            /// A foreign key pointing at this instance, for use in a `New…`
            /// struct. Saves spelling `ForeignKey::from_id(x.pk())` at every
            /// call site that relates one row to another.
            pub fn as_fk(&self) -> ::dorm_core::field::ForeignKey<Self> {
                ::dorm_core::field::ForeignKey::from_id(
                    ::dorm_core::model::Model::pk(self),
                )
            }

            #(#m2m_accessors)*
        }
    })
}

/// The `T` of a `ForeignKey<T>` as a type, for generated code to name.
fn foreign_key_type(ty: &Type) -> TokenStream2 {
    if let Type::Path(p) = ty {
        if let Some(seg) = p.path.segments.last() {
            if seg.ident == "ForeignKey" {
                if let PathArguments::AngleBracketed(args) = &seg.arguments {
                    if let Some(GenericArgument::Type(inner)) = args.args.first() {
                        return quote!(#inner);
                    }
                }
            }
        }
    }
    quote!(#ty)
}

/// The relation accessor: `TenantFields`, with one method per field.
fn build_fields_struct(name: &Ident, specs: &[FieldSpec]) -> TokenStream2 {
    let struct_name = format_ident!("{}Fields", name);
    let doc = format!(
        "Field accessors for [`{name}`], reached through a foreign key.\n\n\
         A foreign key on another model exposes this, so `…{}.slug()` builds the \
         path `{}__slug` without the other model's macro knowing this one's fields.",
        to_snake_case(&name.to_string()),
        to_snake_case(&name.to_string()),
    );

    let accessors = specs.iter().map(|s| {
        let ident = &s.ident;
        let fname = s.ident.to_string();
        if let Some(target) = &s.foreign_key {
            // Traversing further: hand back the target's accessor with this
            // field appended to the prefix.
            let target_fields = format_ident!("{}Fields", target);
            let doc = format!("Continue the path through the `{fname}` relation.");
            quote! {
                #[doc = #doc]
                pub fn #ident(&self) -> #target_fields {
                    #target_fields::nested(::std::format!("{}__{}", self.prefix, #fname))
                }
            }
        } else {
            let inner = &s.inner_ty;
            let doc = format!("The `{fname}` field, as reached through this relation.");
            quote! {
                #[doc = #doc]
                pub fn #ident(&self) -> ::dorm_core::field::Field<#inner> {
                    ::dorm_core::field::Field::owned(
                        ::std::format!("{}__{}", self.prefix, #fname),
                    )
                }
            }
        }
    });

    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone)]
        pub struct #struct_name {
            // `Cow`, so `at` can be `const` for a literal path while `nested`
            // still builds one at runtime.
            prefix: ::std::borrow::Cow<'static, str>,
        }

        impl #struct_name {
            /// Anchor at a literal path.
            pub const fn at(prefix: &'static str) -> Self {
                Self {
                    prefix: ::std::borrow::Cow::Borrowed(prefix),
                }
            }

            /// Anchor at a path assembled while traversing.
            pub fn nested(prefix: ::std::string::String) -> Self {
                Self {
                    prefix: ::std::borrow::Cow::Owned(prefix),
                }
            }

            /// Compare the foreign key itself against a referenced key.
            pub fn eq(&self, v: impl ::dorm_core::value::ToValue) -> ::dorm_core::expr::Q {
                ::dorm_core::expr::Q::new(self.prefix.to_string(), v)
            }

            /// Whether the foreign key is unset.
            pub fn isnull(&self, yes: bool) -> ::dorm_core::expr::Q {
                ::dorm_core::expr::Q::new(
                    ::std::format!("{}__isnull", self.prefix),
                    yes,
                )
            }

            #(#accessors)*
        }
    }
}

/// The per-model module of typed handles: `tenant::slug`.
fn build_module(name: &Ident, specs: &[FieldSpec]) -> TokenStream2 {
    let module = format_ident!("{}", to_snake_case(&name.to_string()));
    let doc = format!("Typed field handles for [`{name}`].");

    let items = specs.iter().map(|s| {
        let ident = &s.ident;
        let fname = s.ident.to_string();
        if let Some(target) = &s.foreign_key {
            let target_fields = format_ident!("{}Fields", target);
            let doc = format!(
                "The `{fname}` relation. Call a field on it to traverse: \
                 `{fname}.<field>()`."
            );
            quote! {
                #[doc = #doc]
                pub const #ident: super::#target_fields = super::#target_fields::at(#fname);
            }
        } else {
            let inner = &s.inner_ty;
            let doc = format!("The `{fname}` column.");
            quote! {
                #[doc = #doc]
                pub const #ident: ::dorm_core::field::Field<#inner> =
                    ::dorm_core::field::Field::borrowed(#fname);
            }
        }
    });

    quote! {
        #[doc = #doc]
        // Lower case on purpose: `tenant::slug` is the spelling the REPL uses
        // too, and matching it is worth one lint exemption.
        #[allow(non_upper_case_globals)]
        pub mod #module {
            // The handles are typed with the struct's own field types —
            // `DateTime<Utc>`, `Decimal`, a foreign key's target — and none of
            // those names exist inside a nested module. Re-importing the parent
            // scope is what makes the generated types resolve to the same ones
            // the author wrote.
            #[allow(unused_imports)]
            use super::*;

            #(#items)*
        }
    }
}

/// The insert shape: `NewTenant`.
fn build_new_row(name: &Ident, specs: &[FieldSpec]) -> TokenStream2 {
    let new_name = format_ident!("New{}", name);
    let supplied: Vec<&FieldSpec> = specs.iter().filter(|s| s.caller_supplied()).collect();

    let decls = supplied.iter().map(|s| {
        let ident = &s.ident;
        let ty = &s.ty;
        let doc = format!("The `{}` column.", s.ident);
        quote! {
            #[doc = #doc]
            pub #ident: #ty,
        }
    });

    let values = supplied.iter().map(|s| {
        let ident = &s.ident;
        let fname = s.ident.to_string();
        quote! {
            (
                #fname.to_string(),
                ::dorm_core::value::ToValue::to_value(&self.#ident),
            )
        }
    });

    let doc = format!(
        "The columns a caller supplies when creating a [`{name}`].\n\n\
         Columns the database fills in — a serial primary key, anything with a \
         SQL default, `auto_now_add`/`auto_now` stamps — are absent on purpose: \
         there is no value for the caller to invent."
    );

    quote! {
        #[doc = #doc]
        #[derive(Debug, Clone)]
        pub struct #new_name {
            #(#decls)*
        }

        impl ::dorm_core::model::NewRow<#name> for #new_name {
            fn column_values(
                &self,
            ) -> ::std::vec::Vec<(::std::string::String, ::dorm_core::value::Value)> {
                ::std::vec![#(#values),*]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proc_macro2::Span;

    #[test]
    fn camel_case_becomes_snake_case() {
        assert_eq!(to_snake_case("Tenant"), "tenant");
        assert_eq!(to_snake_case("Application"), "application");
        assert_eq!(to_snake_case("DeploymentLog"), "deployment_log");
    }

    #[test]
    fn the_default_table_pluralises_the_regular_cases() {
        assert_eq!(default_table("Tenant"), "tenants");
        assert_eq!(default_table("Application"), "applications");
        assert_eq!(default_table("Deployment"), "deployments");
        // Consonant + y.
        assert_eq!(default_table("Policy"), "policies");
        // Vowel + y stays a plain plural.
        assert_eq!(default_table("Day"), "days");
        // Sibilant endings take `es`.
        assert_eq!(default_table("Address"), "addresses");
        assert_eq!(default_table("Box"), "boxes");
        assert_eq!(default_table("Branch"), "branches");
    }

    #[test]
    fn a_multi_word_model_pluralises_only_its_last_word() {
        assert_eq!(default_table("DeploymentPolicy"), "deployment_policies");
    }

    #[test]
    fn column_type_accepts_both_the_sql_and_the_rust_spelling() {
        let parse = |s: &str| {
            column_type_ident(&LitStr::new(s, Span::call_site()))
                .map(|i| i.to_string())
                .ok()
        };
        assert_eq!(parse("bigint").as_deref(), Some("I64"));
        assert_eq!(parse("i64").as_deref(), Some("I64"));
        assert_eq!(parse("jsonb").as_deref(), Some("Json"));
        assert_eq!(parse("timestamptz").as_deref(), Some("Timestamp"));
        assert_eq!(parse("blob"), None);
    }
}

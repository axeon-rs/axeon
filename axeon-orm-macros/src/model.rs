use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::{Data, DeriveInput, Error, Fields, GenericArgument, Ident, LitStr, Path, PathArguments, Result, Type};

#[derive(Clone, Copy, PartialEq)]
enum RelKind {
    HasMany,
    HasOne,
    BelongsToMany,
}

struct Relation {
    kind: RelKind,
    target: Path,
    foreign_key: Option<LitStr>,
    name: Option<LitStr>,
    pivot: Option<LitStr>,
    related_key: Option<LitStr>,
}

#[derive(Default)]
struct FieldAttrs {
    primary_key: bool,
    column: Option<LitStr>,
    unique: bool,
    index: bool,
    default: Option<Option<LitStr>>,
    created_at: bool,
    updated_at: bool,
    belongs_to: Option<Path>,
    on_delete: Option<LitStr>,
    skip: bool,
    dimensions: Option<syn::LitInt>,
    tenant: bool,
    version: bool,
}

struct Field {
    ident: Ident,
    ty: Type,
    column: String,
    attrs: FieldAttrs,
}

pub fn expand(input: DeriveInput) -> Result<TokenStream> {
    let name = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(&input.generics, "Model can't be derived for generic structs"));
    }
    let Data::Struct(data) = &input.data else {
        return Err(Error::new(Span::call_site(), "Model can only be derived for structs"));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(Error::new(Span::call_site(), "Model needs a struct with named fields"));
    };

    // Struct attributes.
    let mut table = None;
    let mut relations = Vec::new();
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("model")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                table = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("has_many") || meta.path.is_ident("has_one") || meta.path.is_ident("belongs_to_many") {
                let kind = if meta.path.is_ident("has_many") {
                    RelKind::HasMany
                } else if meta.path.is_ident("has_one") {
                    RelKind::HasOne
                } else {
                    RelKind::BelongsToMany
                };
                let mut relation = Relation {
                    kind,
                    target: Path::from(format_ident!("_")),
                    foreign_key: None,
                    name: None,
                    pivot: None,
                    related_key: None,
                };
                let mut target = None;
                meta.parse_nested_meta(|inner| {
                    if inner.path.is_ident("foreign_key") {
                        relation.foreign_key = Some(inner.value()?.parse()?);
                    } else if inner.path.is_ident("name") {
                        relation.name = Some(inner.value()?.parse()?);
                    } else if inner.path.is_ident("pivot") && kind == RelKind::BelongsToMany {
                        relation.pivot = Some(inner.value()?.parse()?);
                    } else if inner.path.is_ident("related_key") && kind == RelKind::BelongsToMany {
                        relation.related_key = Some(inner.value()?.parse()?);
                    } else if target.is_none() && !inner.input.peek(syn::Token![=]) {
                        target = Some(inner.path.clone());
                    } else if kind == RelKind::BelongsToMany {
                        return Err(inner.error("expected `pivot`, `foreign_key`, `related_key` or `name`"));
                    } else {
                        return Err(inner.error("expected `foreign_key = \"...\"` or `name = \"...\"`"));
                    }
                    Ok(())
                })?;
                relation.target = target.ok_or_else(|| meta.error("name the related model, e.g. has_many(Post)"))?;
                relations.push(relation);
                Ok(())
            } else {
                Err(meta.error("expected `table`, `has_many`, `has_one` or `belongs_to_many`"))
            }
        })?;
    }
    let table = table.unwrap_or_else(|| pluralize(&snake_case(&name.to_string())));

    // Fields.
    let mut fields = Vec::new();
    for field in &named.named {
        let ident = field.ident.clone().expect("named field");
        let mut attrs = FieldAttrs::default();
        for attr in field.attrs.iter().filter(|a| a.path().is_ident("model")) {
            attr.parse_nested_meta(|meta| {
                let path = &meta.path;
                if path.is_ident("primary_key") {
                    attrs.primary_key = true;
                } else if path.is_ident("column") {
                    attrs.column = Some(meta.value()?.parse()?);
                } else if path.is_ident("unique") {
                    attrs.unique = true;
                } else if path.is_ident("index") {
                    attrs.index = true;
                } else if path.is_ident("default") {
                    attrs.default = Some(if meta.input.peek(syn::Token![=]) { Some(meta.value()?.parse()?) } else { None });
                } else if path.is_ident("created_at") {
                    attrs.created_at = true;
                } else if path.is_ident("updated_at") {
                    attrs.updated_at = true;
                } else if path.is_ident("belongs_to") {
                    attrs.belongs_to = Some(meta.value()?.parse()?);
                } else if path.is_ident("on_delete") {
                    let action: LitStr = meta.value()?.parse()?;
                    let valid = ["cascade", "restrict", "set null", "set default", "no action"];
                    if !valid.contains(&action.value().to_ascii_lowercase().as_str()) {
                        return Err(Error::new(action.span(), "expected cascade, restrict, set null, set default or no action"));
                    }
                    attrs.on_delete = Some(action);
                } else if path.is_ident("skip") {
                    attrs.skip = true;
                } else if path.is_ident("tenant") {
                    attrs.tenant = true;
                } else if path.is_ident("version") {
                    attrs.version = true;
                } else if path.is_ident("dimensions") {
                    let lit: syn::LitInt = meta.value()?.parse()?;
                    let n: u32 = lit.base10_parse()?;
                    if !(1..=16000).contains(&n) {
                        return Err(Error::new_spanned(&lit, "a vector has 1 to 16000 dimensions"));
                    }
                    attrs.dimensions = Some(lit);
                } else {
                    return Err(meta.error(
                        "expected primary_key, column, unique, index, default, created_at, updated_at, belongs_to, on_delete, skip, dimensions, tenant or version",
                    ));
                }
                Ok(())
            })?;
        }
        if attrs.on_delete.is_some() && attrs.belongs_to.is_none() {
            return Err(Error::new_spanned(&ident, "`on_delete` needs `belongs_to`"));
        }
        if attrs.skip && (attrs.primary_key || attrs.belongs_to.is_some() || attrs.tenant) {
            return Err(Error::new_spanned(&ident, "a `skip` field can't be a key"));
        }
        if attrs.tenant && attrs.primary_key {
            return Err(Error::new_spanned(&ident, "the tenant column can't be the primary key"));
        }
        if attrs.version && (attrs.primary_key || attrs.tenant || attrs.skip) {
            return Err(Error::new_spanned(&ident, "the version column can't be the primary key, the tenant column or skipped"));
        }
        let column = attrs.column.as_ref().map(LitStr::value).unwrap_or_else(|| unraw(&ident));
        fields.push(Field { ident, ty: field.ty.clone(), column, attrs });
    }

    let columns: Vec<&Field> = fields.iter().filter(|f| !f.attrs.skip).collect();
    let mut seen = HashSet::new();
    for field in &columns {
        if !seen.insert(field.column.clone()) {
            return Err(Error::new_spanned(&field.ident, format!("two fields map to column `{}`", field.column)));
        }
    }

    let explicit: Vec<&&Field> = columns.iter().filter(|f| f.attrs.primary_key).collect();
    let pk = match explicit.as_slice() {
        [one] => **one,
        [] => *columns.iter().find(|f| f.ident == "id").ok_or_else(|| {
            Error::new(Span::call_site(), "add an `id` field or mark the primary key with #[model(primary_key)]")
        })?,
        [_, second, ..] => {
            return Err(Error::new_spanned(&second.ident, "only one primary key is supported"));
        }
    };
    let pk_column = &pk.column;
    let pk_ty = &pk.ty;

    let tenants: Vec<&&Field> = columns.iter().filter(|f| f.attrs.tenant).collect();
    let tenant = match tenants.as_slice() {
        [] => quote! { ::core::option::Option::None },
        [one] => {
            let column = &one.column;
            quote! { ::core::option::Option::Some(#column) }
        }
        [_, second, ..] => return Err(Error::new_spanned(&second.ident, "only one tenant column is supported")),
    };

    let versions: Vec<&&Field> = columns.iter().filter(|f| f.attrs.version).collect();
    let (version, version_check) = match versions.as_slice() {
        [] => (quote! { ::core::option::Option::None }, quote! {}),
        [one] => {
            let (column, ty) = (&one.column, &one.ty);
            let check = quote! {
                const _: () = ::core::assert!(
                    <#ty as ::axeon_orm::SqlType>::KIND.is_integer() && !<#ty as ::axeon_orm::SqlType>::NULLABLE,
                    "a #[model(version)] column must be a non-nullable i32 or i64"
                );
            };
            (quote! { ::core::option::Option::Some(#column) }, check)
        }
        [_, second, ..] => return Err(Error::new_spanned(&second.ident, "only one version column is supported")),
    };

    // impl Model.
    let column_defs = columns.iter().map(|f| {
        let (col, ty) = (&f.column, &f.ty);
        let mut def = quote! {
            ::axeon_orm::ColumnDef::new(#col, <#ty as ::axeon_orm::SqlType>::KIND, <#ty as ::axeon_orm::SqlType>::NULLABLE)
                .values(<#ty as ::axeon_orm::SqlType>::VALUES)
        };
        if std::ptr::eq(*f, pk) {
            def = quote! { #def.primary_key() };
        }
        if f.attrs.unique {
            def = quote! { #def.unique() };
        }
        if f.attrs.index {
            def = quote! { #def.index() };
        }
        if let Some(dimensions) = &f.attrs.dimensions {
            def = quote! { #def.dimensions(#dimensions) };
        }
        if let Some(default) = &f.attrs.default {
            let sql = match default {
                Some(sql) => quote! { ::core::option::Option::Some(#sql) },
                None => quote! { ::core::option::Option::None },
            };
            def = quote! { #def.default(#sql) };
        }
        if f.attrs.created_at {
            def = quote! { #def.timestamp(::axeon_orm::Timestamp::CreatedAt) };
        }
        if f.attrs.updated_at {
            def = quote! { #def.timestamp(::axeon_orm::Timestamp::UpdatedAt) };
        }
        if let Some(target) = &f.attrs.belongs_to {
            let on_delete = match &f.attrs.on_delete {
                Some(action) => quote! { ::core::option::Option::Some(#action) },
                None => quote! { ::core::option::Option::None },
            };
            def = quote! {
                #def.references(
                    <#target as ::axeon_orm::Model>::TABLE,
                    <#target as ::axeon_orm::Model>::PRIMARY_KEY,
                    #on_delete,
                )
            };
        }
        def
    });
    let decode = fields.iter().map(|f| {
        let (ident, ty, col) = (&f.ident, &f.ty, &f.column);
        if f.attrs.skip {
            quote! { #ident: ::core::default::Default::default() }
        } else {
            quote! {
                #ident: <#ty as ::axeon_orm::SqlType>::decode(row, &::axeon_orm::__private::column_name(prefix, #col))?
            }
        }
    });
    let get_arms = columns.iter().map(|f| {
        let (ident, col) = (&f.ident, &f.column);
        quote! { #col => ::core::option::Option::Some(::axeon_orm::SqlType::to_value(&self.#ident)) }
    });

    // Column constants.
    let column_consts = columns.iter().map(|f| {
        let const_name = format_ident!("{}", unraw(&f.ident).to_uppercase());
        let col = &f.column;
        let ty = option_inner(&f.ty).unwrap_or(&f.ty);
        let doc = format!("The `{}` column.", col);
        quote! {
            #[doc = #doc]
            pub const #const_name: ::axeon_orm::Column<Self, #ty> = ::axeon_orm::Column::new(#col);
        }
    });

    // Relations.
    let mut relation_items = Vec::new();
    let mut assertions = Vec::new();
    let parent_snake = snake_case(&name.to_string());
    for relation in &relations {
        let target = &relation.target;
        let target_name = target.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
        let foreign_key = relation.foreign_key.as_ref().map(LitStr::value).unwrap_or_else(|| format!("{}_id", parent_snake));
        let accessor = relation.name.as_ref().map(LitStr::value).unwrap_or_else(|| match relation.kind {
            RelKind::HasMany | RelKind::BelongsToMany => pluralize(&snake_case(&target_name)),
            RelKind::HasOne => snake_case(&target_name),
        });
        let accessor_ident = Ident::new(&accessor, Span::call_site());
        let const_ident = format_ident!("{}", accessor.to_uppercase());
        if relation.kind == RelKind::BelongsToMany {
            let target_snake = snake_case(&target_name);
            let pivot = relation.pivot.as_ref().map(LitStr::value).unwrap_or_else(|| {
                let mut names = [parent_snake.clone(), target_snake.clone()];
                names.sort();
                names.join("_")
            });
            let related_key =
                relation.related_key.as_ref().map(LitStr::value).unwrap_or_else(|| format!("{}_id", target_snake));
            let doc = format!("The `{}` records linked to this one through `{}`.", target_name, pivot);
            relation_items.push(quote! {
                #[doc = #doc]
                pub const #const_ident: ::axeon_orm::BelongsToMany<Self, #target> =
                    ::axeon_orm::BelongsToMany::new(#pivot, #foreign_key, #related_key);
                #[doc = #doc]
                pub fn #accessor_ident(&self) -> ::axeon_orm::Query<#target> {
                    Self::#const_ident.of(self)
                }
            });
            continue;
        }
        let (rel_ty, doc) = match relation.kind {
            RelKind::HasMany => (quote! { HasMany }, format!("The `{}` this record has (by `{}.{}`).", target_name, target_name, foreign_key)),
            _ => (quote! { HasOne }, format!("The `{}` this record has (by `{}.{}`).", target_name, target_name, foreign_key)),
        };
        relation_items.push(quote! {
            #[doc = #doc]
            pub const #const_ident: ::axeon_orm::#rel_ty<Self, #target> = ::axeon_orm::#rel_ty::new(#foreign_key);
            #[doc = #doc]
            pub fn #accessor_ident(&self) -> ::axeon_orm::Query<#target> {
                Self::#const_ident.of(self)
            }
        });
        let message = format!("{}::{}: `{}` has no column `{}`", name, accessor, target_name, foreign_key);
        assertions.push(quote! {
            assert!(::axeon_orm::__private::has_column(<#target as ::axeon_orm::Model>::COLUMNS, #foreign_key), #message);
        });
    }
    for field in columns.iter().filter(|f| f.attrs.belongs_to.is_some()) {
        let target = field.attrs.belongs_to.as_ref().unwrap();
        let field_name = unraw(&field.ident);
        let Some(accessor) = field_name.strip_suffix("_id").filter(|s| !s.is_empty()) else {
            return Err(Error::new_spanned(&field.ident, "a `belongs_to` field should be named `<relation>_id`, e.g. `user_id`"));
        };
        let accessor_ident = Ident::new(accessor, Span::call_site());
        let const_ident = format_ident!("{}", accessor.to_uppercase());
        let col = &field.column;
        let doc = format!("The record `{}` points to.", col);
        relation_items.push(quote! {
            #[doc = #doc]
            pub const #const_ident: ::axeon_orm::BelongsTo<Self, #target> = ::axeon_orm::BelongsTo::new(#col);
            #[doc = #doc]
            pub fn #accessor_ident(&self) -> ::axeon_orm::Query<#target> {
                Self::#const_ident.of(self)
            }
        });
    }

    Ok(quote! {
        #[automatically_derived]
        impl ::axeon_orm::Model for #name {
            const TABLE: &'static str = #table;
            const PRIMARY_KEY: &'static str = #pk_column;
            const COLUMNS: &'static [::axeon_orm::ColumnDef] = &[#(#column_defs),*];
            const TENANT: ::core::option::Option<&'static str> = #tenant;
            const VERSION: ::core::option::Option<&'static str> = #version;

            fn from_row_prefixed(row: &::axeon_orm::Row, prefix: &str) -> ::axeon_orm::Result<Self> {
                ::core::result::Result::Ok(Self { #(#decode),* })
            }

            fn get(&self, column: &str) -> ::core::option::Option<::axeon_orm::Value> {
                match column {
                    #(#get_arms,)*
                    _ => ::core::option::Option::None,
                }
            }
        }

        #version_check

        #[allow(dead_code)]
        impl #name {
            #(#column_consts)*
            #(#relation_items)*

            /// Starts a query on this table.
            pub fn query() -> ::axeon_orm::Query<Self> {
                ::axeon_orm::Query::new()
            }

            /// Fetches the row with this primary key.
            pub async fn find<'c>(
                ex: impl ::axeon_orm::Executor<'c>,
                key: impl ::core::convert::Into<#pk_ty>,
            ) -> ::axeon_orm::Result<::core::option::Option<Self>> {
                ::axeon_orm::__private::find(ex, ::axeon_orm::SqlType::to_value(&key.into())).await
            }

            /// Fetches the row with this primary key, or fails with `Error::NotFound`.
            pub async fn find_or_fail<'c>(
                ex: impl ::axeon_orm::Executor<'c>,
                key: impl ::core::convert::Into<#pk_ty>,
            ) -> ::axeon_orm::Result<Self> {
                Self::find(ex, key).await?.ok_or(::axeon_orm::Error::NotFound)
            }

            /// Fetches every row.
            pub async fn all<'c>(ex: impl ::axeon_orm::Executor<'c>) -> ::axeon_orm::Result<::std::vec::Vec<Self>> {
                Self::query().all(ex).await
            }

            /// Inserts this record and returns the stored row, with database-assigned values.
            pub async fn insert<'c>(self, ex: impl ::axeon_orm::Executor<'c>) -> ::axeon_orm::Result<Self> {
                ::axeon_orm::__private::insert(ex, &self).await
            }

            /// Inserts `rows` with as few statements as possible (one per 1000 rows or so),
            /// returning the stored rows, with database-assigned values, in the same order.
            /// Several statements on a pool run in one transaction: all rows or none.
            pub async fn insert_many<'c>(
                ex: impl ::axeon_orm::Executor<'c>,
                rows: &[Self],
            ) -> ::axeon_orm::Result<::std::vec::Vec<Self>> {
                ::axeon_orm::__private::insert_many(ex, rows).await
            }

            /// Like `insert_many`, updating rows whose `conflict` columns already exist (see
            /// `upsert`). A key may appear only once per call.
            pub async fn upsert_many<'c>(
                ex: impl ::axeon_orm::Executor<'c>,
                rows: &[Self],
                conflict: ::axeon_orm::OnConflict<Self>,
            ) -> ::axeon_orm::Result<::std::vec::Vec<Self>> {
                ::axeon_orm::__private::upsert_many(ex, rows, &conflict).await
            }

            /// Inserts this record or, if a row with the same `conflict` columns exists, updates
            /// it; returns the stored row.
            pub async fn upsert<'c>(
                self,
                ex: impl ::axeon_orm::Executor<'c>,
                conflict: ::axeon_orm::OnConflict<Self>,
            ) -> ::axeon_orm::Result<Self> {
                ::axeon_orm::__private::upsert(ex, &self, &conflict).await
            }

            /// Inserts this record unless a row with the same `conflict` columns exists; returns
            /// the stored row, or `None` if nothing was inserted.
            pub async fn insert_or_ignore<'c>(
                self,
                ex: impl ::axeon_orm::Executor<'c>,
                conflict: ::axeon_orm::OnConflict<Self>,
            ) -> ::axeon_orm::Result<::core::option::Option<Self>> {
                ::axeon_orm::__private::insert_or_ignore(ex, &self, &conflict).await
            }

            /// Saves every column by primary key and reloads the row.
            pub async fn update<'c>(&mut self, ex: impl ::axeon_orm::Executor<'c>) -> ::axeon_orm::Result<()> {
                *self = ::axeon_orm::__private::update(ex, &*self).await?;
                ::core::result::Result::Ok(())
            }

            /// Deletes this record by primary key.
            pub async fn delete<'c>(self, ex: impl ::axeon_orm::Executor<'c>) -> ::axeon_orm::Result<()> {
                ::axeon_orm::__private::delete(ex, &self).await
            }
        }

        const _: () = { #(#assertions)* };
    })
}

/// `Option<T>` → `T`.
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else { return None };
    match args.args.first()? {
        GenericArgument::Type(inner) if args.args.len() == 1 => Some(inner),
        _ => None,
    }
}

fn unraw(ident: &Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").map(str::to_string).unwrap_or(name)
}

/// `UserProfile` → `user_profile`, `HTTPLog` → `http_log`.
pub(crate) fn snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_lower = i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if i > 0 && (prev_lower || (chars[i - 1].is_uppercase() && next_lower)) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

/// English plural for table names: `user` → `users`, `category` →
/// `categories`, `box` → `boxes`. Use `#[model(table = "...")]` for others.
fn pluralize(word: &str) -> String {
    if let Some(stem) = word.strip_suffix('y') {
        if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) {
            return format!("{}ies", stem);
        }
    }
    if word.ends_with('s') || word.ends_with('x') || word.ends_with('z') || word.ends_with("ch") || word.ends_with("sh") {
        return format!("{}es", word);
    }
    format!("{}s", word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(snake_case("User"), "user");
        assert_eq!(snake_case("UserProfile"), "user_profile");
        assert_eq!(snake_case("HTTPLog"), "http_log");
        assert_eq!(snake_case("Order2Item"), "order2_item");
        assert_eq!(pluralize("user"), "users");
        assert_eq!(pluralize("category"), "categories");
        assert_eq!(pluralize("day"), "days");
        assert_eq!(pluralize("box"), "boxes");
        assert_eq!(pluralize("status"), "statuses");
        assert_eq!(pluralize("user_profile"), "user_profiles");
    }
}

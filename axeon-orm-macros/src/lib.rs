//! Derive macros for [axeon-orm](https://docs.rs/axeon-orm). Use them
//! through that crate, which documents them.

use proc_macro::TokenStream;
use syn::{parse_macro_input, DeriveInput, LitStr};

mod migrations;
mod model;
mod sql_enum;

/// Derives `axeon_orm::Model`; see the axeon-orm crate docs.
#[proc_macro_derive(Model, attributes(model))]
pub fn derive_model(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    model::expand(input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Derives `axeon_orm::SqlType` for a fieldless enum; see the axeon-orm
/// crate docs.
#[proc_macro_derive(SqlEnum, attributes(sql_enum))]
pub fn derive_sql_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    sql_enum::expand(input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Embeds a migrations folder; see `axeon_orm::embed_migrations`.
#[proc_macro]
pub fn embed_migrations(input: TokenStream) -> TokenStream {
    let dir = if input.is_empty() {
        LitStr::new("migrations", proc_macro2::Span::call_site())
    } else {
        parse_macro_input!(input as LitStr)
    };
    migrations::expand(dir).unwrap_or_else(syn::Error::into_compile_error).into()
}

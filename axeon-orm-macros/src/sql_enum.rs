use crate::model::snake_case;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use std::collections::HashSet;
use syn::{Data, DeriveInput, Error, Fields, LitStr, Result};

/// How variant names become stored values.
#[derive(Clone, Copy)]
enum Case {
    Snake,
    ScreamingSnake,
    Kebab,
    Lower,
    Upper,
    Camel,
    Pascal,
}

impl Case {
    fn parse(lit: &LitStr) -> Result<Self> {
        Ok(match lit.value().as_str() {
            "snake_case" => Case::Snake,
            "SCREAMING_SNAKE_CASE" => Case::ScreamingSnake,
            "kebab-case" => Case::Kebab,
            "lowercase" => Case::Lower,
            "UPPERCASE" => Case::Upper,
            "camelCase" => Case::Camel,
            "PascalCase" => Case::Pascal,
            _ => {
                return Err(Error::new_spanned(
                    lit,
                    "expected \"snake_case\", \"SCREAMING_SNAKE_CASE\", \"kebab-case\", \"lowercase\", \"UPPERCASE\", \"camelCase\" or \"PascalCase\"",
                ))
            }
        })
    }

    fn apply(self, variant: &str) -> String {
        match self {
            Case::Snake => snake_case(variant),
            Case::ScreamingSnake => snake_case(variant).to_uppercase(),
            Case::Kebab => snake_case(variant).replace('_', "-"),
            Case::Lower => variant.to_lowercase(),
            Case::Upper => variant.to_uppercase(),
            Case::Camel => {
                let mut chars = variant.chars();
                chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
            }
            Case::Pascal => variant.to_string(),
        }
    }
}

pub fn expand(input: DeriveInput) -> Result<TokenStream> {
    let name = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(Error::new_spanned(&input.generics, "SqlEnum can't be derived for generic enums"));
    }
    let Data::Enum(data) = &input.data else {
        return Err(Error::new(Span::call_site(), "SqlEnum can only be derived for enums"));
    };
    if data.variants.is_empty() {
        return Err(Error::new(Span::call_site(), "SqlEnum needs at least one variant"));
    }

    let mut case = Case::Snake;
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("sql_enum")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                case = Case::parse(&meta.value()?.parse()?)?;
                Ok(())
            } else {
                Err(meta.error("expected `rename_all = \"...\"`"))
            }
        })?;
    }

    let mut idents = Vec::new();
    let mut values = Vec::new();
    let mut seen = HashSet::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(Error::new_spanned(variant, "SqlEnum variants can't hold data"));
        }
        let mut value = case.apply(&variant.ident.to_string());
        for attr in variant.attrs.iter().filter(|a| a.path().is_ident("sql_enum")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    value = meta.value()?.parse::<LitStr>()?.value();
                    Ok(())
                } else {
                    Err(meta.error("expected `rename = \"...\"`"))
                }
            })?;
        }
        if !seen.insert(value.clone()) {
            return Err(Error::new_spanned(variant, format!("two variants are stored as {:?}", value)));
        }
        idents.push(&variant.ident);
        values.push(value);
    }

    let type_name = name.to_string();
    Ok(quote! {
        impl #name {
            /// Every value this enum is stored as, in variant order.
            pub const VALUES: &'static [&'static str] = &[#(#values),*];

            /// The value this variant is stored as.
            pub const fn as_str(&self) -> &'static str {
                match self {
                    #(Self::#idents => #values,)*
                }
            }
        }

        impl ::core::fmt::Display for #name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::core::str::FromStr for #name {
            type Err = ::axeon_orm::UnknownVariant;
            fn from_str(value: &str) -> ::core::result::Result<Self, Self::Err> {
                match value {
                    #(#values => ::core::result::Result::Ok(Self::#idents),)*
                    _ => ::core::result::Result::Err(::axeon_orm::UnknownVariant::new(#type_name, value)),
                }
            }
        }

        impl ::axeon_orm::SqlType for #name {
            const KIND: ::axeon_orm::Kind = ::axeon_orm::Kind::Text;
            const VALUES: ::core::option::Option<&'static [&'static str]> = ::core::option::Option::Some(Self::VALUES);
            fn to_value(&self) -> ::axeon_orm::Value {
                ::axeon_orm::Value::Text(self.as_str().into())
            }
            fn decode(row: &::axeon_orm::Row, column: &str) -> ::axeon_orm::Result<Self> {
                let value = <::std::string::String as ::axeon_orm::SqlType>::decode(row, column)?;
                value.parse().map_err(|e| ::axeon_orm::__private::decode_error(column, e))
            }
        }

        impl ::core::convert::From<#name> for ::axeon_orm::Value {
            fn from(value: #name) -> Self {
                ::axeon_orm::Value::Text(value.as_str().into())
            }
        }
    })
}

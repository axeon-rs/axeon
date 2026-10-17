use proc_macro2::TokenStream;
use quote::quote;
use std::collections::HashSet;
use std::path::PathBuf;
use syn::{Error, LitStr, Result};

pub fn expand(dir: LitStr) -> Result<TokenStream> {
    let root = std::env::var("CARGO_MANIFEST_DIR").map_err(|_| Error::new(dir.span(), "CARGO_MANIFEST_DIR is not set"))?;
    let path = PathBuf::from(root).join(dir.value());
    let entries = std::fs::read_dir(&path)
        .map_err(|e| Error::new(dir.span(), format!("cannot read {}: {}", path.display(), e)))?;

    let mut migrations = Vec::new();
    for entry in entries {
        let file = entry.map_err(|e| Error::new(dir.span(), e.to_string()))?.path();
        if file.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let parsed = stem.split_once('_').and_then(|(version, name)| {
            let valid = !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit());
            Some((version.parse::<i64>().ok().filter(|_| valid)?, name.to_string()))
        });
        let Some((version, name)) = parsed else {
            return Err(Error::new(dir.span(), format!("{}: expected `<version>_<name>.sql`", file.display())));
        };
        let file = file.to_str().ok_or_else(|| Error::new(dir.span(), "migration path is not UTF-8"))?.to_string();
        migrations.push((version, name, file));
    }
    migrations.sort();

    let mut seen = HashSet::new();
    for (version, _, _) in &migrations {
        if !seen.insert(*version) {
            return Err(Error::new(dir.span(), format!("two migrations have version {}", version)));
        }
    }

    let items = migrations.iter().map(|(version, name, file)| {
        quote! { ::axeon_orm::migrate::Migration::new(#version, #name, include_str!(#file)) }
    });
    Ok(quote! { ::axeon_orm::migrate::Migrator::new(vec![#(#items),*]) })
}

//! Derive macros for `paddock-sdk`. Use them through `paddock_sdk`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{DeriveInput, Expr, ExprLit, Lit, Meta, Path, parse_macro_input};

/// Derive `paddock_sdk::Snapshot` for a serde type.
///
/// ```ignore
/// #[derive(Serialize, Deserialize, Snapshot)]
/// #[snapshot(version = 2, migrate = upgrade_state)]
/// struct State { count: u64, history: Vec<u64> }
///
/// fn upgrade_state(from: u32, old: paddock_sdk::snapshot::Value) -> Result<State, String> { ... }
/// ```
///
/// `version` (default 1) is the format version this type writes. `migrate`
/// names a function that reads older versions; without it, loading another
/// version fails.
#[proc_macro_derive(Snapshot, attributes(snapshot))]
pub fn derive_snapshot(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let mut version: u32 = 1;
    let mut migrate: Option<Path> = None;
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("snapshot")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("version") {
                let value: Expr = meta.value()?.parse()?;
                let Expr::Lit(ExprLit {
                    lit: Lit::Int(n), ..
                }) = value
                else {
                    return Err(meta.error("version must be an integer"));
                };
                version = n.base10_parse()?;
                Ok(())
            } else if meta.path.is_ident("migrate") {
                migrate = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error("expected `version = N` or `migrate = path`"))
            }
        })?;
    }
    if version == 0 {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "snapshot version must be at least 1",
        ));
    }
    // Reject a stray `#[snapshot]` with no arguments early, for a clearer error.
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("snapshot")) {
        if matches!(attr.meta, Meta::Path(_)) {
            return Err(syn::Error::new_spanned(
                attr,
                "expected #[snapshot(version = N, ...)]",
            ));
        }
    }
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let migrate_fn = migrate.map(|path| {
        quote! {
            fn migrate(from_version: u32, old: ::paddock_sdk::snapshot::Value) -> ::core::result::Result<Self, ::std::string::String> {
                #path(from_version, old)
            }
        }
    });
    Ok(quote! {
        impl #impl_generics ::paddock_sdk::Snapshot for #name #ty_generics #where_clause {
            const VERSION: u32 = #version;
            #migrate_fn
        }
    })
}

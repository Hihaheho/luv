use proc_macro::TokenStream;

use syn::{Data, DeriveInput, Ident, Token};

/// Adds the cross-platform binding derives to a serde type.
///
/// Emits `#[cfg_attr(feature = "ts", derive(::ts_rs::TS), ts(export))]` and
/// a shape-aware `#[cfg_attr(feature = "uniffi", derive(::uniffi::Record))]`
/// (structs) / `derive(::uniffi::Enum)` (enums). The consuming crate decides
/// which of the `ts` and `uniffi` features exist; `cfg_attr` keeps the
/// dependencies out of the build when a feature is off. TypeScript files are
/// written to `TS_RS_EXPORT_DIR` (ts-rs' default is `./bindings`).
///
/// Pass `no_uniffi` to skip the UniFFI derive for types UniFFI cannot lower:
/// `#[luv_ffi(no_uniffi)]`.
///
/// ```ignore
/// #[luv_ffi]
/// #[derive(serde::Serialize, serde::Deserialize)]
/// pub struct Foo { /* ... */ }
/// ```
#[proc_macro_attribute]
pub fn luv_ffi(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input: proc_macro2::TokenStream = item.clone().into();
    let parsed = match syn::parse::<DeriveInput>(item) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error().into(),
    };
    let no_uniffi = match parse_options(attr) {
        Ok(no_uniffi) => no_uniffi,
        Err(error) => return error.to_compile_error().into(),
    };

    let uniffi_derive = if no_uniffi {
        proc_macro2::TokenStream::new()
    } else {
        match &parsed.data {
            Data::Struct(_) => quote::quote! {
                #[cfg_attr(feature = "uniffi", derive(::uniffi::Record))]
            },
            Data::Enum(_) => quote::quote! {
                #[cfg_attr(feature = "uniffi", derive(::uniffi::Enum))]
            },
            Data::Union(_) => {
                return syn::Error::new_spanned(&parsed.ident, "luv_ffi does not support unions")
                    .to_compile_error()
                    .into();
            }
        }
    };

    quote::quote! {
        #[cfg_attr(feature = "ts", derive(::ts_rs::TS))]
        #[cfg_attr(feature = "ts", ts(export))]
        #uniffi_derive
        #input
    }
    .into()
}

fn parse_options(attr: TokenStream) -> syn::Result<bool> {
    if attr.is_empty() {
        return Ok(false);
    }
    let parser = syn::punctuated::Punctuated::<Ident, Token![,]>::parse_terminated;
    let mut no_uniffi = false;
    for ident in syn::parse::Parser::parse(parser, attr)? {
        if ident == "no_uniffi" {
            no_uniffi = true;
        } else {
            return Err(syn::Error::new_spanned(
                &ident,
                format!("unknown luv_ffi option: `{ident}`"),
            ));
        }
    }
    Ok(no_uniffi)
}

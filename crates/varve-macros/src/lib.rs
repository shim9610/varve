use proc_macro::TokenStream;
use proc_macro2::{Group, Span, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Data, DeriveInput, Fields, Ident, LitBool, LitByteStr, LitInt, LitStr, Result, Token, Type,
    Visibility, braced, bracketed, parenthesized, parse_macro_input,
};

/// Derives [`VarveBlock`] — and the codec impls it needs — for one stored
/// record type.
///
/// Re-exported by the `varve` facade as `varve::VarveBlock`; depend on `varve`,
/// not on this crate directly.
///
/// # Attributes
///
/// The block itself is configured with `#[varve(...)]` on the struct:
///
/// | Key | Meaning |
/// | --- | --- |
/// | `id = <u32>` | block id, below `0xFFFF_FF00` (ids at or above that are reserved for internal records) |
/// | `version = <u16>` | block version; defaults to `1` |
/// | `kind = "fixed" \| "variable" \| "matrix"` | encoding shape |
/// | `key = "<field>"` | marks the block keyed and names the key field |
/// | `endian = "little" \| "big"` | per-block byte-order override; otherwise the format's byte order applies |
///
/// Fields of a `variable` block take `#[varve(field_id = <u32>)]`, which is the
/// wire identity that lets readers skip unknown fields. `fixed` blocks are
/// positional and take no field ids.
///
/// ```no_run
/// #[derive(Clone, Debug, PartialEq, varve::VarveBlock)]
/// #[varve(id = 2, version = 1, kind = "variable", key = "id")]
/// struct User {
///     #[varve(field_id = 1)]
///     id: u64,
///     #[varve(field_id = 2)]
///     name: String,
/// }
/// ```
///
/// # Generated schema fingerprint
///
/// The derive computes `VarveBlock::SCHEMA_FINGERPRINT` deterministically
/// (FNV-1a 64 over the canonical schema: id, version, kind, endian, keyedness,
/// ordered field name/type identities, and each field's resolved codec
/// `SCHEMA_ID`). A manual block that mirrors a derived one must reuse that
/// block's fingerprint constant.
///
/// # Field codec identity
///
/// Every field type must declare a **non-zero** `VarveEncode::SCHEMA_ID` and
/// `VarveDecode::SCHEMA_ID`. A field whose codec leaves the trait default `0`
/// fails to compile with a message naming the field. Wrapping the codec in
/// `Option`, `Vec`, an array, a map, or a tuple does not satisfy the rule:
/// container identities fold their elements, so a missing element identity
/// propagates outward. Every built-in codec — including `ChunkedBytes` and
/// `PackedBitmap` — declares one. See `docs/custom-codec-guide.md`.
///
/// # Matrix blocks
///
/// `kind = "matrix"` additionally requires every field to have a width fixed by
/// its type, because a matrix slot needs a compile-time stride. Scalars and
/// fixed arrays of them qualify; anything that owns heap storage does not, and
/// is rejected during const evaluation of the generated `SLOT_STRIDE`.
///
/// [`VarveBlock`]: https://docs.rs/varve/latest/varve/trait.VarveBlock.html
#[proc_macro_derive(VarveBlock, attributes(varve))]
pub fn derive_varve_block(input: TokenStream) -> TokenStream {
    match expand_varve_block(parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => rebrand_facade(tokens).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Declares a whole binary format: its header identity, resource limits,
/// policies, block set, and a typed API generated from them.
///
/// Re-exported by the `varve` facade as `varve::varve_format!`; depend on
/// `varve`, not on this crate directly. This is the format-first entry point —
/// the declaration is the single source of truth for the on-disk contract and
/// for the Rust API that reads and writes it.
///
/// ```no_run
/// varve::varve_format! {
///     pub format AppFormat {
///         magic: b"APPF";
///         version: 1;
///         limits {
///             file_len: 8_589_934_592;
///             records: 4_000_000;
///             index_bytes: 536_870_912;
///             scan_bytes: 8_589_934_592;
///             record_payload: 67_108_864;
///             logical_payload: 268_435_456;
///             materialized_bytes: 1_073_741_824;
///             segments: 4_000_000;
///             sidecar: 268_435_456;
///             mmap: 8_589_934_592;
///         }
///         endian: little;
///         schema_hash: computed;
///         extension: "appf";
///         blocks {
///             fixed Point(id = 1, version = 1) { x: u32, y: u32 }
///             variable User(id = 2, version = 1, key = [id]) { id: u64, name: String }
///         }
///     }
/// }
///
/// # fn main() {
/// let spec = AppFormat::spec();
/// assert_eq!(spec.magic, *b"APPF");
/// # }
/// ```
///
/// # Sections
///
/// | Section | Meaning |
/// | --- | --- |
/// | `magic`, `version`, `extension` | file header identity |
/// | `limits { … }` | the resource ceilings every read is checked against |
/// | `endian` | format byte order; a block may override it |
/// | `schema_hash` | `computed` (derived from the declaration) or a pinned literal |
/// | `index`, `commit`, `manifest`, `integrity`, `compression` | policies |
/// | `dims`, `commit: cell_bitmap`, `aux` | preallocated-matrix declarations |
/// | `layout { … }` | custom physical layout, for non-Varve-native byte shapes |
/// | `blocks { … }` | `fixed` / `variable` / `matrix` block declarations |
///
/// # What is generated
///
/// A `FormatSpec` constructor (`AppFormat::spec()`), one block type per
/// declaration with its `VarveBlock` impl, typed reader/writer handles
/// (`AppFormat::open_reader`, `create_writer_with_dims`, …), and per-block
/// accessors: `push_point`, `points()`, `delete_user`, and for matrix blocks
/// `write_cell`/`commit_cell`/`cell`/`cell_status` plus a key struct such as
/// `CellKey { scan, ch }`.
///
/// Generated code refers to the facade as `::varve::…` and rewrites that path
/// when the dependency is renamed (`vv = { package = "varve", … }`), so no
/// `extern crate` alias is needed downstream.
///
/// See `docs/spec.md` ("Macro Contract") for the normative grammar and
/// `docs/format-author-guide.md` for a clause-by-clause walkthrough of it.
#[proc_macro]
pub fn varve_format(input: TokenStream) -> TokenStream {
    match syn::parse::<FormatInput>(input) {
        Ok(input) => rebrand_facade(expand_format(input)).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Name under which the calling crate depends on the `varve` facade, when it
/// differs from the literal `varve` (API2-05).
///
/// Generated code references the facade as `::varve::__core::…`, which fails
/// with E0433 when the dependency is renamed (`vv = { package = "varve", … }`).
/// `proc-macro-crate` resolves the rename from the calling crate's manifest.
/// `FoundCrate::Itself` (the facade's own integration tests, where `::varve`
/// resolves normally) and resolution errors both fall back to the literal
/// `varve` name.
fn renamed_facade_ident() -> Option<Ident> {
    match proc_macro_crate::crate_name("varve") {
        Ok(proc_macro_crate::FoundCrate::Name(name)) if name != "varve" => {
            Some(Ident::new(&name, proc_macro2::Span::call_site()))
        }
        Ok(proc_macro_crate::FoundCrate::Name(_) | proc_macro_crate::FoundCrate::Itself)
        | Err(_) => None,
    }
}

/// Rewrites path-leading `::varve` segments in generated tokens to the
/// resolved facade name. No-op in the common non-renamed case.
fn rebrand_facade(tokens: TokenStream2) -> TokenStream2 {
    match renamed_facade_ident() {
        Some(facade) => rebrand_facade_tokens(tokens, &facade),
        None => tokens,
    }
}

/// Whether `ident` can be a path segment immediately preceding a `::`
/// separator, i.e. whether `ident::…` continues a path rather than beginning
/// an absolute one. Path-prefix keywords (`crate`, `super`, `self`, `Self`)
/// and every non-keyword ident qualify; other keywords (`impl`, `as`, `dyn`,
/// `for`, …) cannot precede a path segment, so a `::` after them is
/// path-leading.
fn ident_can_end_path_prefix(ident: &Ident) -> bool {
    let name = ident.to_string();
    matches!(name.as_str(), "crate" | "super" | "self" | "Self")
        || !matches!(
            name.as_str(),
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "static"
                | "struct"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        )
}

/// Replaces every ident spelled `varve` that begins an absolute path
/// (`::varve`) with `facade`, recursing into groups.
///
/// Only generated code produces absolute `::varve` paths: a user crate that
/// renamed the dependency cannot name `::varve` in the field types or
/// attributes the macros embed, so the rewrite never touches user tokens.
/// Idents preceded by a path prefix (`crate::varve`, `<T as Tr>::varve`) or
/// with no leading `::` (`self.varve`) are left alone.
///
/// A leading `::` is recognized in two shapes:
/// - a 2-colon run whose first colon does not follow a path prefix
///   (statement/expression position: `= ::varve::…`, `(::varve::…)`), and
/// - the tail of a 3-or-more-colon run: Rust has no `:::` token, so a run of
///   three or more colons is always a lone `:` (type ascription, struct
///   field, trait bound, …) followed by an absolute-path `::` — the pervasive
///   generated shape `IDENT: ::varve::__core::…`. `run_is_leading` is latched
///   at the run's first colon (the ascription colon, which follows an ident),
///   so this shape must be accepted by run length rather than by the latch.
fn rebrand_facade_tokens(tokens: TokenStream2, facade: &Ident) -> TokenStream2 {
    let mut output: Vec<TokenTree> = Vec::new();
    // Consecutive `:` puncts seen immediately before the current token, and
    // whether that colon run begins a path rather than continuing one.
    let mut colon_run = 0usize;
    let mut run_is_leading = false;
    // Whether the previous non-colon token can end a path prefix (an ident
    // or the closing `>` of a qualified path).
    let mut prev_ends_path = false;
    // Char of the immediately preceding punct token, to tell the `>` of a
    // qualified path apart from the second half of `->` / `=>`.
    let mut prev_punct: Option<char> = None;
    for tree in tokens {
        match tree {
            TokenTree::Punct(punct) if punct.as_char() == ':' => {
                if colon_run == 0 {
                    run_is_leading = !prev_ends_path;
                }
                colon_run += 1;
                prev_punct = Some(':');
                output.push(TokenTree::Punct(punct));
            }
            TokenTree::Ident(ident) => {
                prev_ends_path = ident_can_end_path_prefix(&ident);
                if colon_run >= 2 && (run_is_leading || colon_run >= 3) && ident == "varve" {
                    let mut renamed = facade.clone();
                    renamed.set_span(ident.span());
                    output.push(TokenTree::Ident(renamed));
                } else {
                    output.push(TokenTree::Ident(ident));
                }
                colon_run = 0;
                prev_punct = None;
            }
            TokenTree::Group(group) => {
                let rebranded = rebrand_facade_tokens(group.stream(), facade);
                let mut rebuilt = Group::new(group.delimiter(), rebranded);
                rebuilt.set_span(group.span());
                output.push(TokenTree::Group(rebuilt));
                colon_run = 0;
                prev_ends_path = false;
                prev_punct = None;
            }
            TokenTree::Punct(punct) => {
                let char = punct.as_char();
                prev_ends_path = char == '>' && !matches!(prev_punct, Some('-') | Some('='));
                prev_punct = Some(char);
                colon_run = 0;
                output.push(TokenTree::Punct(punct));
            }
            literal @ TokenTree::Literal(_) => {
                colon_run = 0;
                prev_ends_path = false;
                prev_punct = None;
                output.push(literal);
            }
        }
    }
    output.into_iter().collect()
}

fn expand_varve_block(input: DeriveInput) -> Result<TokenStream2> {
    let ident = input.ident.clone();
    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            input.generics,
            "VarveBlock derive does not support generic parameters yet; declare a concrete block type or implement VarveBlock manually",
        ));
    }
    let mut block_id = None;
    let mut version = quote!(1u16);
    let mut version_value: u16 = 1;
    let mut kind = quote!(::varve::__core::BlockKind::Fixed);
    let mut kind_name = "fixed";
    let mut variable_block = false;
    let mut endian = quote!(::core::option::Option::None);
    let mut endian_name = "default";
    let mut key_fields: Vec<Ident> = Vec::new();

    for attr in &input.attrs {
        if !attr.path().is_ident("varve") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("id") {
                let value: LitInt = meta.value()?.parse()?;
                block_id = Some(value.base10_parse::<u32>()?);
                Ok(())
            } else if meta.path.is_ident("version") {
                let value: LitInt = meta.value()?.parse()?;
                let parsed = value.base10_parse::<u16>()?;
                version = quote!(#parsed);
                version_value = parsed;
                Ok(())
            } else if meta.path.is_ident("kind") {
                let value: LitStr = meta.value()?.parse()?;
                kind = match value.value().as_str() {
                    "fixed" => {
                        variable_block = false;
                        kind_name = "fixed";
                        quote!(::varve::__core::BlockKind::Fixed)
                    }
                    "matrix" => {
                        variable_block = false;
                        kind_name = "matrix";
                        quote!(::varve::__core::BlockKind::Matrix)
                    }
                    "variable" => {
                        variable_block = true;
                        kind_name = "variable";
                        quote!(::varve::__core::BlockKind::Variable)
                    }
                    other => {
                        return Err(meta.error(format!(
                            "unsupported varve kind {other:?}; use \"fixed\", \"variable\", or \"matrix\""
                        )));
                    }
                };
                Ok(())
            } else if meta.path.is_ident("endian") {
                let value: LitStr = meta.value()?.parse()?;
                endian = match value.value().as_str() {
                    "little" => {
                        endian_name = "little";
                        quote!(::core::option::Option::Some(::varve::__core::Endian::Little))
                    }
                    "big" => {
                        endian_name = "big";
                        quote!(::core::option::Option::Some(::varve::__core::Endian::Big))
                    }
                    other => {
                        return Err(meta.error(format!(
                            "unsupported endian {other:?}; use \"little\" or \"big\""
                        )));
                    }
                };
                Ok(())
            } else if meta.path.is_ident("key") {
                let value: LitStr = meta.value()?.parse()?;
                key_fields = parse_key_fields(&value)?;
                Ok(())
            } else {
                Err(meta.error("unsupported varve block attribute"))
            }
        })?;
    }

    let block_id =
        block_id.ok_or_else(|| syn::Error::new_spanned(&ident, "missing #[varve(id = ...)]"))?;
    if block_id >= 0xFFFF_FF00 {
        return Err(syn::Error::new_spanned(
            &ident,
            "varve block id is reserved; use an id below 0xFFFF_FF00",
        ));
    }
    for (index, key) in key_fields.iter().enumerate() {
        if key_fields[(index + 1)..].iter().any(|other| other == key) {
            return Err(syn::Error::new_spanned(key, "duplicate varve key field"));
        }
    }
    let fields = match input.data {
        Data::Struct(data) => match data.fields {
            Fields::Named(fields) => fields.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    ident,
                    "VarveBlock only supports structs with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                ident,
                "VarveBlock only supports structs",
            ));
        }
    };

    let mut descriptors = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let name = field.ident.clone().expect("named field");
        let ty = field.ty.clone();
        let mut field_id = (index as u32) + 1;
        let mut default = false;
        for attr in &field.attrs {
            if !attr.path().is_ident("varve") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("field_id") {
                    let value: LitInt = meta.value()?.parse()?;
                    field_id = value.base10_parse::<u32>()?;
                    Ok(())
                } else if meta.path.is_ident("default") {
                    default = true;
                    Ok(())
                } else {
                    Err(meta.error("unsupported varve field attribute"))
                }
            })?;
        }
        if default && !variable_block {
            return Err(syn::Error::new_spanned(
                &name,
                "#[varve(default)] is only supported on variable blocks",
            ));
        }
        descriptors.push(FieldDescriptor {
            name,
            ty,
            field_id,
            default,
        });
    }
    for (index, field) in descriptors.iter().enumerate() {
        if field.field_id == 0 {
            return Err(syn::Error::new_spanned(
                &field.name,
                "varve field_id must be non-zero",
            ));
        }
        if descriptors[(index + 1)..]
            .iter()
            .any(|other| other.field_id == field.field_id)
        {
            return Err(syn::Error::new_spanned(
                &field.name,
                "duplicate varve field_id",
            ));
        }
    }

    let fixed_encode = descriptors.iter().map(|field| {
        let name = &field.name;
        quote!(::varve::__core::VarveEncode::encode_varve(&self.#name, encoder)?;)
    });
    let field_descriptors = descriptors.iter().map(|field| {
        let name = field.name.to_string();
        let ty = &field.ty;
        let field_id = field.field_id;
        let presence = if field.default {
            quote!(::varve::__core::FieldPresence::Defaulted)
        } else {
            quote!(::varve::__core::FieldPresence::Required)
        };
        quote! {
            ::varve::__core::FieldDescriptor {
                id: #field_id,
                name: #name,
                wire_type: <#ty as ::varve::__core::VarveEncode>::WIRE_TYPE,
                presence: #presence,
            }
        }
    });
    let fixed_decode = descriptors.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        quote!(#name: <#ty as ::varve::__core::VarveDecode>::decode_varve(decoder)?)
    });

    let variable_encode = descriptors.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        let field_id = field.field_id;
        quote! {
            // DEF-02: the nested field encoder inherits the parent encoder's
            // remaining output budget, so a limit-bounded writer entry point
            // surfaces its typed limit error before an oversized field is
            // ever fully buffered, instead of after staging the whole child
            // encoding.
            let payload = encoder.encode_nested_to_vec(&self.#name)?;
            ::varve::__core::write_field(
                encoder,
                #field_id,
                <#ty as ::varve::__core::VarveEncode>::WIRE_TYPE,
                &payload,
            )?;
        }
    });

    let option_vars = descriptors.iter().map(|field| {
        let var = option_ident(&field.name);
        let ty = &field.ty;
        quote!(let mut #var: ::core::option::Option<#ty> = ::core::option::Option::None;)
    });
    let match_arms = descriptors.iter().map(|field| {
        let var = option_ident(&field.name);
        let ty = &field.ty;
        let field_id = field.field_id;
        let field_name = field.name.to_string();
        quote! {
            #field_id => {
                if header.wire_type != <#ty as ::varve::__core::VarveDecode>::WIRE_TYPE {
                    return Err(::varve::__core::Error::WireTypeMismatch {
                        field: #field_name,
                        expected: <#ty as ::varve::__core::VarveDecode>::WIRE_TYPE,
                        actual: header.wire_type,
                    });
                }
                #var = ::core::option::Option::Some(
                    decoder.decode_nested::<#ty>(payload)?
                );
            }
        }
    });
    let build_fields = descriptors.iter().map(|field| {
        let name = &field.name;
        let var = option_ident(name);
        let field_id = field.field_id;
        let field_name = name.to_string();
        if field.default {
            quote!(#name: #var.unwrap_or_default())
        } else {
            quote! {
                #name: #var.ok_or(::varve::__core::Error::MissingField {
                    field: #field_name,
                    field_id: #field_id,
                })?
            }
        }
    });

    let encode_body = quote! {
        match <Self as ::varve::__core::VarveBlock>::KIND {
            ::varve::__core::BlockKind::Fixed | ::varve::__core::BlockKind::Matrix => {
                #(#fixed_encode)*
            }
            ::varve::__core::BlockKind::Variable => {
                #(#variable_encode)*
            }
            ::varve::__core::BlockKind::Internal => {}
        }
        ::core::result::Result::Ok(())
    };

    let decode_body = quote! {
        match <Self as ::varve::__core::VarveBlock>::KIND {
            ::varve::__core::BlockKind::Fixed | ::varve::__core::BlockKind::Matrix => {
                ::core::result::Result::Ok(Self { #(#fixed_decode,)* })
            }
            ::varve::__core::BlockKind::Variable => {
                #(#option_vars)*
                while decoder.remaining() > 0 {
                    let header = ::varve::__core::read_field_header(decoder)?;
                    let payload = decoder.read_exact(header.payload_len as usize)?;
                    match header.field_id {
                        #(#match_arms)*
                        _ => {}
                    }
                }
                ::core::result::Result::Ok(Self { #(#build_fields,)* })
            }
            ::varve::__core::BlockKind::Internal => unreachable!("user blocks cannot be internal"),
        }
    };

    let keyed_impl = if key_fields.is_empty() {
        quote!()
    } else {
        let mut key_types = Vec::new();
        for key in &key_fields {
            let ty = descriptors
                .iter()
                .find(|field| field.name == *key)
                .map(|field| field.ty.clone())
                .ok_or_else(|| syn::Error::new_spanned(key, "key field does not exist"))?;
            key_types.push(ty);
        }
        let key_type = if key_types.len() == 1 {
            let ty = &key_types[0];
            quote!(#ty)
        } else {
            quote!((#(#key_types,)*))
        };
        let key_value = if key_fields.len() == 1 {
            let key = &key_fields[0];
            quote!(self.#key.clone())
        } else {
            quote!((#(self.#key_fields.clone(),)*))
        };
        quote! {
            impl ::varve::__core::VarveKeyedBlock for #ident {
                type Key = #key_type;

                fn key(&self) -> Self::Key {
                    #key_value
                }
            }
        }
    };
    let is_keyed = !key_fields.is_empty();
    let codec_identity_asserts = field_codec_identity_asserts(&descriptors);
    let schema_fingerprint = schema_fingerprint(
        block_id,
        version_value,
        kind_name,
        endian_name,
        is_keyed,
        &descriptors,
    );
    let replace_impl = if key_fields.is_empty() {
        quote! {
            impl ::varve::__core::VarveReplaceBlock for #ident {
                fn validate_replacement(
                    _old: &Self,
                    _new: &Self,
                ) -> ::varve::__core::Result<()> {
                    ::core::result::Result::Ok(())
                }
            }
        }
    } else {
        quote! {
            impl ::varve::__core::VarveReplaceBlock for #ident {
                fn validate_replacement(
                    old: &Self,
                    new: &Self,
                ) -> ::varve::__core::Result<()> {
                    if <Self as ::varve::__core::VarveKeyedBlock>::key(old)
                        != <Self as ::varve::__core::VarveKeyedBlock>::key(new)
                    {
                        return ::core::result::Result::Err(
                            ::varve::__core::Error::ReplacementKeyMismatch,
                        );
                    }
                    ::core::result::Result::Ok(())
                }
            }
        }
    };

    Ok(quote! {
        #(#codec_identity_asserts)*

        impl ::varve::__core::VarveEncode for #ident {
            const WIRE_TYPE: ::varve::__core::WireType = ::varve::__core::WireType::Nested;
            const SCHEMA_ID: u64 = <Self as ::varve::__core::VarveBlock>::SCHEMA_FINGERPRINT;

            fn encode_varve(&self, encoder: &mut ::varve::__core::Encoder) -> ::varve::__core::Result<()> {
                #encode_body
            }
        }

        impl ::varve::__core::VarveDecode for #ident {
            const WIRE_TYPE: ::varve::__core::WireType = ::varve::__core::WireType::Nested;
            const SCHEMA_ID: u64 = <Self as ::varve::__core::VarveBlock>::SCHEMA_FINGERPRINT;

            fn decode_varve(decoder: &mut ::varve::__core::Decoder<'_>) -> ::varve::__core::Result<Self> {
                #decode_body
            }
        }

        impl ::varve::__core::VarveBlock for #ident {
            const ID: u32 = #block_id;
            const VERSION: u16 = #version;
            const KIND: ::varve::__core::BlockKind = #kind;
            const ENDIAN: ::core::option::Option<::varve::__core::Endian> = #endian;
            const IS_KEYED: bool = #is_keyed;
            const SCHEMA_FINGERPRINT: u64 = #schema_fingerprint;
            const FIELDS: &'static [::varve::__core::FieldDescriptor] = &[
                #(#field_descriptors,)*
            ];
        }

        #keyed_impl
        #replace_impl
    })
}

fn option_ident(name: &Ident) -> Ident {
    format_ident!("__varve_field_{}", name)
}

/// Deterministic fingerprint of the canonical block schema, emitted as a const
/// expression the compiler folds in the caller's crate.
///
/// FNV-1a 64 over a canonical byte string is expanded inline because the
/// standard `DefaultHasher` output is not stability-guaranteed. The value is a
/// process-local schema identity, never part of the wire format.
///
/// API-04: each field contributes `<FieldTy as VarveEncode>::SCHEMA_ID` and
/// its decode counterpart — resolved codec identities — alongside the source
/// spelling, so two custom nested codecs that spell the field type identically
/// but emit different bytes cannot share a fingerprint. That is why the
/// fingerprint can no longer be a literal: the field identities are known only
/// after type resolution.
fn schema_fingerprint(
    block_id: u32,
    version: u16,
    kind: &str,
    endian: &str,
    keyed: bool,
    fields: &[FieldDescriptor],
) -> TokenStream2 {
    let header = LitByteStr::new(
        format!(
            "varve:block-schema:v2|id={block_id}|version={version}|kind={kind}|endian={endian}|keyed={keyed}"
        )
        .as_bytes(),
        Span::call_site(),
    );
    let field_steps = fields.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        let type_identity = quote!(#ty).to_string();
        let field_id = field.field_id;
        let presence = if field.default {
            "defaulted"
        } else {
            "required"
        };
        let literal = LitByteStr::new(
            format!(
                "|field:id={field_id},name={name},type={type_identity},presence={presence},codec="
            )
            .as_bytes(),
            Span::call_site(),
        );
        quote! {
            __varve_acc = __varve_schema_bytes(__varve_acc, #literal);
            __varve_acc = __varve_schema_u64(
                __varve_acc,
                <#ty as ::varve::__core::VarveEncode>::SCHEMA_ID,
            );
            __varve_acc = __varve_schema_u64(
                __varve_acc,
                <#ty as ::varve::__core::VarveDecode>::SCHEMA_ID,
            );
            __varve_acc = __varve_schema_u64(
                __varve_acc,
                <#ty as ::varve::__core::VarveEncode>::WIRE_TYPE as u16 as u64,
            );
        }
    });
    quote! {
        {
            const fn __varve_schema_bytes(acc: u64, bytes: &[u8]) -> u64 {
                let mut acc = acc;
                let mut index = 0;
                while index < bytes.len() {
                    acc ^= bytes[index] as u64;
                    acc = acc.wrapping_mul(0x0000_0100_0000_01b3u64);
                    index += 1;
                }
                acc
            }
            const fn __varve_schema_u64(acc: u64, value: u64) -> u64 {
                __varve_schema_bytes(acc, &value.to_le_bytes())
            }
            let mut __varve_acc = __varve_schema_bytes(0xcbf2_9ce4_8422_2325u64, #header);
            #(#field_steps)*
            __varve_acc
        }
    }
}

/// Compile-time enforcement of the `VarveEncode::SCHEMA_ID` trust boundary for
/// every field codec: a field type whose codec declares no identity would
/// fingerprint by source spelling alone, which is exactly the API-04 collision.
///
/// The rule is deliberately wire-type agnostic. Restricting it to
/// `WireType::Nested` left the same collision reachable through any other wire
/// type - two `Packed` codecs spelled identically, both `WireType::U64`, both
/// identity-less, emitting different bytes - so the assertion demands an
/// identity from every field type. Built-in scalars, built-in containers and
/// derived blocks all declare one, so this only fires on hand-written codecs,
/// including ones reached through a container (`container_schema_id`
/// propagates the missing identity outward as zero).
fn field_codec_identity_asserts(fields: &[FieldDescriptor]) -> Vec<TokenStream2> {
    fields
        .iter()
        .map(|field| {
            let ty = &field.ty;
            let name = field.name.to_string();
            let message = format!(
                "varve field `{name}` uses a custom codec that declares no SCHEMA_ID; a hand-written codec must declare a schema identity that changes whenever its encoded bytes change (wrapping it in Option/Vec/array/map/tuple does not supply one)"
            );
            quote! {
                const _: () = ::core::assert!(
                    <#ty as ::varve::__core::VarveEncode>::SCHEMA_ID != 0,
                    #message
                );
                const _: () = ::core::assert!(
                    <#ty as ::varve::__core::VarveDecode>::SCHEMA_ID != 0,
                    #message
                );
            }
        })
        .collect()
}

fn parse_key_fields(value: &LitStr) -> Result<Vec<Ident>> {
    let raw = value.value();
    if raw.trim().is_empty() {
        return Err(syn::Error::new(
            value.span(),
            "varve key must name at least one field",
        ));
    }
    let mut fields = Vec::new();
    for part in raw.split(',') {
        let name = part.trim();
        if name.is_empty() {
            return Err(syn::Error::new(
                value.span(),
                "varve key contains an empty field name",
            ));
        }
        let ident = syn::parse_str::<Ident>(name).map_err(|_| {
            syn::Error::new(
                value.span(),
                format!("invalid varve key field {name:?}; expected a Rust identifier"),
            )
        })?;
        fields.push(Ident::new(&ident.to_string(), value.span()));
    }
    Ok(fields)
}

struct FieldDescriptor {
    name: Ident,
    ty: Type,
    field_id: u32,
    default: bool,
}

struct FormatInput {
    vis: Visibility,
    name: Ident,
    magic: LitByteStr,
    version: u16,
    endian: EndianChoice,
    schema_hash: SchemaHashChoice,
    extension: Option<LitStr>,
    index: IndexChoice,
    commit: CommitChoice,
    integrity: IntegrityChoice,
    recovery: RecoveryChoice,
    manifest: ManifestChoice,
    compression: CompressionChoice,
    limits: LimitsChoice,
    dims: Vec<MatrixDim>,
    matrix_commit: Option<MatrixCommit>,
    matrix_aux: Vec<MatrixAux>,
    registry_blocks: Vec<Type>,
    inline_blocks: Vec<InlineBlock>,
    layout_preset: Option<LayoutPresetChoice>,
    layout_file_header: Option<LayoutFileHeader>,
    layout_segments: Vec<LayoutSegment>,
    typed_api: bool,
}

enum EndianChoice {
    Little,
    Big,
}

enum SchemaHashChoice {
    Literal(u64),
    Computed,
}

#[derive(Clone)]
struct IndexChoice {
    scan_on_open: bool,
    checkpoint_on_flush: bool,
    block_offset_chain: bool,
    keyed_offset_chain: bool,
}

impl IndexChoice {
    fn empty() -> Self {
        Self {
            scan_on_open: false,
            checkpoint_on_flush: false,
            block_offset_chain: false,
            keyed_offset_chain: false,
        }
    }

    fn scan_on_open() -> Self {
        Self {
            scan_on_open: true,
            ..Self::empty()
        }
    }

    fn with_block_offset_chain(mut self) -> Self {
        self.scan_on_open = true;
        self.block_offset_chain = true;
        self
    }

    fn with_keyed_offset_chain(mut self) -> Self {
        self.scan_on_open = true;
        self.block_offset_chain = true;
        self.keyed_offset_chain = true;
        self
    }
}

enum CommitChoice {
    None,
    RecordFooter,
    TransactionMarkerOnFlush,
    TransactionMarkerExplicit,
}

enum ParsedCommitChoice {
    Append(CommitChoice),
    Matrix(MatrixCommit),
}

enum IntegrityChoice {
    None,
    Crc32,
    Crc32WithHeader,
}

enum RecoveryChoice {
    Strict,
    TruncateTail,
}

enum ManifestChoice {
    None,
    Embedded,
}

enum CompressionChoice {
    None,
    VariableBlocks(VariableCompressionChoice),
}

enum LimitsChoice {
    Finite(Vec<LimitEntry>),
    TrustedUnbounded,
}

struct LimitEntry {
    key: Ident,
    value: u64,
}

struct VariableCompressionChoice {
    algorithm: CompressionAlgorithmChoice,
    level: CompressionLevelChoice,
    header: CompressionHeaderChoice,
    min_len: u64,
    only_if_smaller: bool,
    max_len: u64,
}

enum CompressionAlgorithmChoice {
    Zstd,
}

enum CompressionLevelChoice {
    Fast,
    Default,
    Best,
    Exact(i32),
}

enum CompressionHeaderChoice {
    RecordExplicit,
    FileExplicit,
    FormatContract,
}

#[derive(Clone, Copy)]
enum LayoutPresetChoice {
    VarveNative,
    None,
    Custom,
}

struct LayoutFileHeader {
    name: Ident,
    fields: Vec<LayoutField>,
}

struct LayoutSegment {
    name: Ident,
    repeat: SegmentRepeatChoice,
    lead_in: LayoutLeadIn,
    metadata: Ident,
    raw_region: Ident,
    footer: Option<LayoutFooter>,
}

#[derive(Clone, Copy)]
enum SegmentRepeatChoice {
    Once,
    UntilEof,
}

struct LayoutLeadIn {
    name: Ident,
    fields: Vec<LayoutField>,
}

struct LayoutFooter {
    name: Ident,
    fields: Vec<LayoutField>,
}

struct LayoutField {
    name: Ident,
    ty: LayoutFieldTypeChoice,
    source: LayoutFieldSourceChoice,
}

enum LayoutFieldTypeChoice {
    Bytes(u64),
    U8,
    U16,
    U32,
    U64,
    I64,
}

enum LayoutFieldSourceChoice {
    LiteralBytes(LitByteStr),
    LiteralU64(u64),
    LiteralI64(i64),
    Caller,
    Finalize(LayoutFinalizeChoice),
}

struct LayoutFinalizeChoice {
    target: LayoutAnchorChoice,
    relative_to: LayoutAnchorChoice,
}

#[derive(Clone, Copy)]
enum LayoutAnchorChoice {
    SegmentStart,
    AfterLeadIn,
    MetadataStart,
    RawRegionStart,
    SegmentEnd,
    FooterStart,
    FooterEnd,
}

enum InlineBlockKind {
    Fixed,
    Variable,
    Matrix(MatrixBlockMeta),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyIndexChoice {
    Memory,
    Disk,
}

struct InlineBlock {
    kind: InlineBlockKind,
    name: Ident,
    id: u32,
    version: u16,
    key_fields: Vec<Ident>,
    key_index: KeyIndexChoice,
    fields: Vec<InlineField>,
}

struct MatrixDim {
    name: Ident,
    ty: Type,
}

struct MatrixCommit {
    keyspace: Vec<Ident>,
    categories: Vec<Ident>,
    singles: Vec<Ident>,
    per_channel: Vec<Ident>,
}

struct MatrixAux {
    name: Ident,
    byte_len: u64,
}

struct MatrixBlockMeta {
    dims: Vec<Ident>,
    category: Ident,
}

struct InlineField {
    name: Ident,
    ty: Type,
    field_id: u32,
    default: bool,
}

impl Parse for FormatInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let vis: Visibility = input.parse()?;
        let typed_api = if input.peek(Token![struct]) {
            input.parse::<Token![struct]>()?;
            false
        } else {
            let keyword: Ident = input.parse()?;
            if keyword != "format" {
                return Err(syn::Error::new_spanned(
                    keyword,
                    "expected struct or format",
                ));
            }
            true
        };
        let name: Ident = input.parse()?;
        let content;
        braced!(content in input);

        let mut magic = None;
        let mut version = None;
        let mut endian = EndianChoice::Little;
        let mut schema_hash = SchemaHashChoice::Literal(0);
        let mut extension = None;
        let mut index = IndexChoice::scan_on_open();
        let mut commit = CommitChoice::None;
        let mut integrity = IntegrityChoice::None;
        let mut recovery = RecoveryChoice::Strict;
        let mut manifest = ManifestChoice::None;
        let mut compression = CompressionChoice::None;
        let mut limits = None;
        let mut dims: Option<Vec<MatrixDim>> = None;
        let mut matrix_commit = None;
        let mut matrix_aux: Option<Vec<MatrixAux>> = None;
        let mut registry_blocks: Option<Vec<Type>> = None;
        let mut inline_blocks: Option<Vec<InlineBlock>> = None;
        let mut layout_preset = None;
        let mut layout_file_header = None;
        let mut layout_segments: Option<Vec<LayoutSegment>> = None;
        let mut seen_keys = Vec::new();

        while !content.is_empty() {
            let key: Ident = content.parse()?;
            if key == "limits" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                limits = Some(LimitsChoice::Finite(parse_read_limits(&inner)?));
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "dims" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                dims = Some(parse_matrix_dims(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "aux" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                matrix_aux = Some(parse_matrix_aux(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "blocks" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                inline_blocks = Some(parse_inline_blocks(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "layout" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                let layout = parse_layout(&inner)?;
                layout_file_header = layout.file_header;
                layout_segments = Some(layout.segments);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            content.parse::<Token![:]>()?;
            note_format_key(&mut seen_keys, &key)?;
            if key == "magic" {
                magic = Some(content.parse()?);
            } else if key == "version" {
                let value: LitInt = content.parse()?;
                version = Some(value.base10_parse::<u16>()?);
            } else if key == "endian" {
                let value: Ident = content.parse()?;
                endian = match value.to_string().as_str() {
                    "little" => EndianChoice::Little,
                    "big" => EndianChoice::Big,
                    _ => return Err(syn::Error::new_spanned(value, "expected little or big")),
                };
            } else if key == "schema_hash" {
                if content.peek(LitInt) {
                    let value: LitInt = content.parse()?;
                    schema_hash = SchemaHashChoice::Literal(value.base10_parse::<u64>()?);
                } else {
                    let value: Ident = content.parse()?;
                    if value == "computed" {
                        schema_hash = SchemaHashChoice::Computed;
                    } else {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected computed or a u64 schema hash",
                        ));
                    }
                }
            } else if key == "extension" {
                extension = Some(content.parse()?);
            } else if key == "index" {
                index = parse_index_choice(&content)?;
            } else if key == "commit" {
                match parse_commit_choice(&content)? {
                    ParsedCommitChoice::Append(choice) => commit = choice,
                    ParsedCommitChoice::Matrix(choice) => matrix_commit = Some(choice),
                }
            } else if key == "integrity" {
                let value: Ident = content.parse()?;
                integrity = match value.to_string().as_str() {
                    "none" => IntegrityChoice::None,
                    "crc32" => IntegrityChoice::Crc32,
                    "crc32_with_header" => IntegrityChoice::Crc32WithHeader,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected none, crc32, or crc32_with_header",
                        ));
                    }
                };
            } else if key == "recovery" {
                let value: Ident = content.parse()?;
                recovery = match value.to_string().as_str() {
                    "strict" => RecoveryChoice::Strict,
                    "truncate_tail" => RecoveryChoice::TruncateTail,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected strict or truncate_tail",
                        ));
                    }
                };
            } else if key == "manifest" {
                let value: Ident = content.parse()?;
                manifest = match value.to_string().as_str() {
                    "none" => ManifestChoice::None,
                    "embedded" => ManifestChoice::Embedded,
                    _ => return Err(syn::Error::new_spanned(value, "expected none or embedded")),
                };
            } else if key == "compression" {
                compression = parse_compression_choice(&content)?;
            } else if key == "limits" {
                let value: Ident = content.parse()?;
                if value != "trusted_unbounded" {
                    return Err(syn::Error::new_spanned(
                        value,
                        "expected trusted_unbounded or limits { ... }",
                    ));
                }
                limits = Some(LimitsChoice::TrustedUnbounded);
            } else if key == "preset" {
                let value: Ident = content.parse()?;
                layout_preset = Some(match value.to_string().as_str() {
                    "varve_native" => LayoutPresetChoice::VarveNative,
                    "none" => LayoutPresetChoice::None,
                    "custom" => LayoutPresetChoice::Custom,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected varve_native, none, or custom",
                        ));
                    }
                });
            } else if key == "blocks" {
                let inner;
                bracketed!(inner in content);
                let parsed = Punctuated::<Type, Token![,]>::parse_terminated(&inner)?;
                registry_blocks = Some(parsed.into_iter().collect());
            } else if key == "dims" {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected dims { name: type, ... }",
                ));
            } else if key == "aux" {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected aux { name: byte_len, ... }",
                ));
            } else if key == "layout" {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected layout { file_header ... segment ... }",
                ));
            } else {
                return Err(syn::Error::new_spanned(key, "unsupported varve_format key"));
            }
            content.parse::<Token![;]>()?;
        }

        let dims = dims.unwrap_or_default();
        let matrix_aux = matrix_aux.unwrap_or_default();
        let registry_blocks = registry_blocks.unwrap_or_default();
        let inline_blocks = inline_blocks.unwrap_or_default();
        let layout_segments = layout_segments.unwrap_or_default();
        let limits = limits.unwrap_or_else(|| LimitsChoice::Finite(Vec::new()));
        if typed_api && inline_blocks.is_empty() && layout_segments.is_empty() {
            return Err(content.error("format syntax requires inline blocks"));
        }
        if !typed_api && registry_blocks.is_empty() {
            return Err(content.error("missing blocks"));
        }
        validate_matrix_format(&dims, matrix_commit.as_ref(), &matrix_aux, &inline_blocks)?;

        Ok(Self {
            vis,
            name,
            magic: magic.ok_or_else(|| content.error("missing magic"))?,
            version: version.ok_or_else(|| content.error("missing version"))?,
            endian,
            schema_hash,
            extension,
            index,
            commit,
            integrity,
            recovery,
            manifest,
            compression,
            limits,
            dims,
            matrix_commit,
            matrix_aux,
            registry_blocks,
            inline_blocks,
            layout_preset,
            layout_file_header,
            layout_segments,
            typed_api,
        })
    }
}

const READ_LIMIT_KEYS: &[&str] = &[
    "file_len",
    "records",
    "index_bytes",
    "scan_bytes",
    "record_payload",
    "logical_payload",
    "materialized_bytes",
    "segments",
    "matrix_dimension",
    "matrix_cells",
    "matrix_bitmap",
    "matrix_crc",
    "matrix_metadata",
    "matrix_slot_region",
    "sidecar",
    "mmap",
    "keyed_tail",
];

fn parse_read_limits(input: ParseStream<'_>) -> Result<Vec<LimitEntry>> {
    let mut entries = Vec::new();
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        let key_text = key.to_string();
        if !READ_LIMIT_KEYS.contains(&key_text.as_str()) {
            return Err(syn::Error::new_spanned(key, "unknown Varve read limit key"));
        }
        if entries.iter().any(|entry: &LimitEntry| entry.key == key) {
            return Err(syn::Error::new_spanned(
                key,
                format!("duplicate Varve read limit key {key_text:?}"),
            ));
        }
        input.parse::<Token![:]>()?;
        let value: LitInt = input.parse()?;
        entries.push(LimitEntry {
            key,
            value: value.base10_parse::<u64>()?,
        });
        input.parse::<Token![;]>()?;
    }

    Ok(entries)
}

fn note_format_key(seen: &mut Vec<String>, key: &Ident) -> Result<()> {
    let key_text = key.to_string();
    if seen.iter().any(|seen| seen == &key_text) {
        return Err(syn::Error::new_spanned(
            key,
            format!("duplicate varve_format key {key_text:?}"),
        ));
    }
    seen.push(key_text);
    Ok(())
}

fn parse_index_choice(input: ParseStream<'_>) -> Result<IndexChoice> {
    if input.peek(syn::token::Bracket) {
        let inner;
        bracketed!(inner in input);
        let mut choice = IndexChoice::empty();
        let mut seen_any = false;
        while !inner.is_empty() {
            choice = parse_index_entry(&inner, choice)?;
            seen_any = true;
            if inner.is_empty() {
                break;
            }
            inner.parse::<Token![,]>()?;
        }
        if !seen_any {
            return Err(inner.error("index list must not be empty"));
        }
        return Ok(choice);
    }
    parse_index_entry(input, IndexChoice::empty())
}

/// One entry of the `index: [...]` list.
///
/// Entries are bare idents (`scan_on_open`) or call-shaped
/// (`offset_sidecar(per_block)`); the list previously parsed as
/// `Punctuated<Ident, Comma>`, which cannot express the second form.
fn parse_index_entry(input: ParseStream<'_>, choice: IndexChoice) -> Result<IndexChoice> {
    let value: Ident = input.parse()?;
    apply_index_ident(choice, value)
}

fn apply_index_ident(mut choice: IndexChoice, value: Ident) -> Result<IndexChoice> {
    match value.to_string().as_str() {
        "scan_on_open" => {
            choice.scan_on_open = true;
            Ok(choice)
        }
        "checkpoint_on_flush" => {
            choice.scan_on_open = true;
            choice.checkpoint_on_flush = true;
            Ok(choice)
        }
        "block_offset_chain" => Ok(choice.with_block_offset_chain()),
        "keyed_offset_chain" => Ok(choice.with_keyed_offset_chain()),
        _ => Err(syn::Error::new_spanned(
            value,
            "expected scan_on_open, checkpoint_on_flush, block_offset_chain, or keyed_offset_chain",
        )),
    }
}

fn parse_commit_choice(input: ParseStream<'_>) -> Result<ParsedCommitChoice> {
    let value: Ident = input.parse()?;
    match value.to_string().as_str() {
        "none" => Ok(ParsedCommitChoice::Append(CommitChoice::None)),
        "record_footer" => Ok(ParsedCommitChoice::Append(CommitChoice::RecordFooter)),
        "transaction_marker" => {
            let args;
            parenthesized!(args in input);
            let mode: Ident = args.parse()?;
            if !args.is_empty() {
                return Err(args.error("transaction_marker accepts exactly one mode"));
            }
            match mode.to_string().as_str() {
                "on_flush" => Ok(ParsedCommitChoice::Append(
                    CommitChoice::TransactionMarkerOnFlush,
                )),
                "explicit" => Ok(ParsedCommitChoice::Append(
                    CommitChoice::TransactionMarkerExplicit,
                )),
                _ => Err(syn::Error::new_spanned(
                    mode,
                    "expected on_flush or explicit",
                )),
            }
        }
        "cell_bitmap" => {
            let body;
            braced!(body in input);
            parse_matrix_commit(&body).map(ParsedCommitChoice::Matrix)
        }
        _ => Err(syn::Error::new_spanned(
            value,
            "expected none, record_footer, transaction_marker(...), or cell_bitmap { ... }",
        )),
    }
}

struct LayoutDecl {
    file_header: Option<LayoutFileHeader>,
    segments: Vec<LayoutSegment>,
}

fn parse_layout(input: ParseStream<'_>) -> Result<LayoutDecl> {
    let mut file_header = None;
    let mut segments = Vec::new();
    while !input.is_empty() {
        let keyword: Ident = input.parse()?;
        match keyword.to_string().as_str() {
            "file_header" => {
                if file_header.is_some() {
                    return Err(syn::Error::new_spanned(
                        keyword,
                        "layout can declare at most one file_header",
                    ));
                }
                let name: Ident = input.parse()?;
                let inner;
                braced!(inner in input);
                file_header = Some(LayoutFileHeader {
                    name,
                    fields: parse_layout_fields(&inner)?,
                });
            }
            "segment" => {
                let name: Ident = input.parse()?;
                let mut repeat = SegmentRepeatChoice::Once;
                if input.peek(Ident) {
                    let repeat_keyword: Ident = input.parse()?;
                    if repeat_keyword != "repeat" {
                        return Err(syn::Error::new_spanned(repeat_keyword, "expected repeat"));
                    }
                    let repeat_value: Ident = input.parse()?;
                    repeat = match repeat_value.to_string().as_str() {
                        "once" => SegmentRepeatChoice::Once,
                        "until_eof" => SegmentRepeatChoice::UntilEof,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                repeat_value,
                                "expected once or until_eof",
                            ));
                        }
                    };
                }
                let inner;
                braced!(inner in input);
                segments.push(parse_layout_segment_body(name, repeat, &inner)?);
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    keyword,
                    "expected file_header or segment",
                ));
            }
        }
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
    }
    Ok(LayoutDecl {
        file_header,
        segments,
    })
}

fn parse_layout_segment_body(
    name: Ident,
    repeat: SegmentRepeatChoice,
    input: ParseStream<'_>,
) -> Result<LayoutSegment> {
    let mut lead_in = None;
    let mut metadata = None;
    let mut raw_region = None;
    let mut footer = None;
    while !input.is_empty() {
        let keyword: Ident = input.parse()?;
        match keyword.to_string().as_str() {
            "lead_in" => {
                let lead_name: Ident = input.parse()?;
                let inner;
                braced!(inner in input);
                lead_in = Some(LayoutLeadIn {
                    name: lead_name,
                    fields: parse_layout_fields(&inner)?,
                });
            }
            "metadata" => {
                metadata = Some(input.parse()?);
                input.parse::<Token![;]>()?;
            }
            "raw_region" => {
                raw_region = Some(input.parse()?);
                input.parse::<Token![;]>()?;
            }
            "footer" => {
                let footer_name: Ident = input.parse()?;
                let inner;
                braced!(inner in input);
                footer = Some(LayoutFooter {
                    name: footer_name,
                    fields: parse_layout_fields(&inner)?,
                });
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    keyword,
                    "expected lead_in, metadata, raw_region, or footer",
                ));
            }
        }
    }
    Ok(LayoutSegment {
        name,
        repeat,
        lead_in: lead_in.ok_or_else(|| input.error("layout segment requires lead_in"))?,
        metadata: metadata.ok_or_else(|| input.error("layout segment requires metadata"))?,
        raw_region: raw_region.ok_or_else(|| input.error("layout segment requires raw_region"))?,
        footer,
    })
}

fn parse_layout_fields(input: ParseStream<'_>) -> Result<Vec<LayoutField>> {
    let mut fields = Vec::new();
    while !input.is_empty() {
        let ty_ident: Ident = input.parse()?;
        let name: Ident = input.parse()?;
        let mut source = LayoutFieldSourceChoice::Caller;
        let ty = match ty_ident.to_string().as_str() {
            "bytes" => {
                input.parse::<Token![=]>()?;
                let bytes: LitByteStr = input.parse()?;
                let len = bytes.value().len() as u64;
                source = LayoutFieldSourceChoice::LiteralBytes(bytes);
                LayoutFieldTypeChoice::Bytes(len)
            }
            "u8" => LayoutFieldTypeChoice::U8,
            "u16" => LayoutFieldTypeChoice::U16,
            "u32" => LayoutFieldTypeChoice::U32,
            "u64" => LayoutFieldTypeChoice::U64,
            "i64" => LayoutFieldTypeChoice::I64,
            _ => {
                return Err(syn::Error::new_spanned(
                    ty_ident,
                    "expected bytes, u8, u16, u32, u64, or i64",
                ));
            }
        };
        if !matches!(ty, LayoutFieldTypeChoice::Bytes(_)) && input.peek(Token![=]) {
            input.parse::<Token![=]>()?;
            if input.peek(Ident) {
                source = LayoutFieldSourceChoice::Finalize(parse_layout_finalize(input)?);
            } else {
                let value: LitInt = input.parse()?;
                source = match ty {
                    LayoutFieldTypeChoice::I64 => {
                        LayoutFieldSourceChoice::LiteralI64(value.base10_parse::<i64>()?)
                    }
                    _ => LayoutFieldSourceChoice::LiteralU64(value.base10_parse::<u64>()?),
                };
            }
        }
        input.parse::<Token![;]>()?;
        fields.push(LayoutField { name, ty, source });
    }
    Ok(fields)
}

fn parse_layout_finalize(input: ParseStream<'_>) -> Result<LayoutFinalizeChoice> {
    let function: Ident = input.parse()?;
    if function != "finalize" {
        return Err(syn::Error::new_spanned(function, "expected finalize"));
    }
    let args;
    parenthesized!(args in input);
    let mut target = None;
    let mut relative_to = None;
    while !args.is_empty() {
        let key: Ident = args.parse()?;
        args.parse::<Token![=]>()?;
        let value: Ident = args.parse()?;
        match key.to_string().as_str() {
            "target" => target = Some(parse_layout_anchor(value)?),
            "relative_to" => relative_to = Some(parse_layout_anchor(value)?),
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected target or relative_to",
                ));
            }
        }
        if args.peek(Token![,]) {
            args.parse::<Token![,]>()?;
        }
    }
    Ok(LayoutFinalizeChoice {
        target: target.ok_or_else(|| args.error("finalize requires target"))?,
        relative_to: relative_to.ok_or_else(|| args.error("finalize requires relative_to"))?,
    })
}

fn parse_layout_anchor(value: Ident) -> Result<LayoutAnchorChoice> {
    Ok(match value.to_string().as_str() {
        "segment_start" => LayoutAnchorChoice::SegmentStart,
        "after_lead_in" => LayoutAnchorChoice::AfterLeadIn,
        "metadata_start" => LayoutAnchorChoice::MetadataStart,
        "raw_region_start" => LayoutAnchorChoice::RawRegionStart,
        "segment_end" => LayoutAnchorChoice::SegmentEnd,
        "footer_start" => LayoutAnchorChoice::FooterStart,
        "footer_end" => LayoutAnchorChoice::FooterEnd,
        _ => {
            return Err(syn::Error::new_spanned(
                value,
                "expected a known layout anchor",
            ));
        }
    })
}

fn parse_matrix_dims(input: ParseStream<'_>) -> Result<Vec<MatrixDim>> {
    let mut dims = Vec::new();
    while !input.is_empty() {
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let ty: Type = input.parse()?;
        dims.push(MatrixDim { name, ty });
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(dims)
}

fn parse_matrix_commit(input: ParseStream<'_>) -> Result<MatrixCommit> {
    let mut keyspace = None;
    let mut categories = None;
    let mut singles = None;
    let mut per_channel = None;
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        let inner;
        bracketed!(inner in input);
        let parsed = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
        let values: Vec<Ident> = parsed.into_iter().collect();
        match key.to_string().as_str() {
            "keyspace" => keyspace = Some(values),
            "categories" => categories = Some(values),
            "singles" => singles = Some(values),
            "per_channel" => per_channel = Some(values),
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected keyspace, categories, singles, or per_channel",
                ));
            }
        }
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        } else if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(MatrixCommit {
        keyspace: keyspace.unwrap_or_default(),
        categories: categories.unwrap_or_default(),
        singles: singles.unwrap_or_default(),
        per_channel: per_channel.unwrap_or_default(),
    })
}

fn parse_matrix_aux(input: ParseStream<'_>) -> Result<Vec<MatrixAux>> {
    let mut aux = Vec::new();
    while !input.is_empty() {
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let len: LitInt = input.parse()?;
        aux.push(MatrixAux {
            name,
            byte_len: len.base10_parse::<u64>()?,
        });
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(aux)
}

fn parse_inline_blocks(input: ParseStream<'_>) -> Result<Vec<InlineBlock>> {
    let mut blocks = Vec::new();
    while !input.is_empty() {
        blocks.push(parse_inline_block(input)?);
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        } else if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
    }
    Ok(blocks)
}

fn parse_inline_block(input: ParseStream<'_>) -> Result<InlineBlock> {
    let kind_ident: Ident = input.parse()?;
    let kind_name = kind_ident.to_string();
    let mut matrix_dims = Vec::new();
    let mut matrix_category = None;
    let kind = match kind_name.as_str() {
        "fixed" => InlineBlockKind::Fixed,
        "variable" => InlineBlockKind::Variable,
        "matrix" => InlineBlockKind::Matrix(MatrixBlockMeta {
            dims: Vec::new(),
            category: Ident::new("__varve_missing_category", kind_ident.span()),
        }),
        _ => {
            return Err(syn::Error::new_spanned(
                kind_ident,
                "expected fixed, variable, or matrix",
            ));
        }
    };
    let name: Ident = input.parse()?;
    let meta;
    parenthesized!(meta in input);
    let mut id = None;
    let mut version = 1u16;
    let mut key_fields = Vec::new();
    let mut key_index = KeyIndexChoice::Memory;
    let mut key_index_span = None;
    while !meta.is_empty() {
        let key: Ident = meta.parse()?;
        meta.parse::<Token![=]>()?;
        match key.to_string().as_str() {
            "id" => {
                let value: LitInt = meta.parse()?;
                id = Some(value.base10_parse::<u32>()?);
            }
            "version" => {
                let value: LitInt = meta.parse()?;
                version = value.base10_parse::<u16>()?;
            }
            "key" => {
                let inner;
                bracketed!(inner in meta);
                let parsed = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
                if parsed.is_empty() {
                    return Err(syn::Error::new_spanned(key, "key list must not be empty"));
                }
                key_fields = parsed.into_iter().collect();
            }
            "key_index" => {
                if key_index_span.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate key_index"));
                }
                let value: Ident = meta.parse()?;
                key_index = match value.to_string().as_str() {
                    "memory" => KeyIndexChoice::Memory,
                    "disk" => KeyIndexChoice::Disk,
                    _ => {
                        return Err(syn::Error::new_spanned(value, "expected memory or disk"));
                    }
                };
                key_index_span = Some(key.span());
            }
            "dims" => {
                let inner;
                bracketed!(inner in meta);
                let parsed = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
                if parsed.is_empty() {
                    return Err(syn::Error::new_spanned(key, "dims list must not be empty"));
                }
                matrix_dims = parsed.into_iter().collect();
            }
            "category" => {
                matrix_category = Some(meta.parse()?);
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected id, version, key, key_index, dims, or category",
                ));
            }
        }
        if meta.peek(Token![,]) {
            meta.parse::<Token![,]>()?;
        }
    }
    let kind = if matches!(&kind, InlineBlockKind::Matrix(_)) {
        if let Some(span) = key_index_span {
            return Err(syn::Error::new(
                span,
                "key_index is not supported on matrix blocks",
            ));
        }
        if !key_fields.is_empty() {
            return Err(syn::Error::new_spanned(
                &name,
                "matrix blocks use dims for their keyspace, not key",
            ));
        }
        InlineBlockKind::Matrix(MatrixBlockMeta {
            dims: matrix_dims,
            category: matrix_category
                .ok_or_else(|| syn::Error::new_spanned(&name, "missing matrix category"))?,
        })
    } else {
        if !matrix_dims.is_empty() {
            return Err(syn::Error::new_spanned(
                &name,
                "dims is only supported on matrix blocks",
            ));
        }
        if matrix_category.is_some() {
            return Err(syn::Error::new_spanned(
                &name,
                "category is only supported on matrix blocks",
            ));
        }
        kind
    };
    if let Some(span) = key_index_span {
        if key_fields.is_empty() {
            return Err(syn::Error::new(
                span,
                "key_index requires a keyed fixed or variable block",
            ));
        }
        if !cfg!(feature = "high-cardinality-dev") {
            return Err(syn::Error::new(
                span,
                "key_index requires the `high-cardinality-dev` feature",
            ));
        }
    }
    let body;
    braced!(body in input);
    let mut fields = Vec::new();
    while !body.is_empty() {
        let name: Ident = body.parse()?;
        body.parse::<Token![:]>()?;
        let ty: Type = body.parse()?;
        let mut default = false;
        if body.peek(Token![=]) {
            body.parse::<Token![=]>()?;
            let value: Ident = body.parse()?;
            if value != "default" {
                return Err(syn::Error::new_spanned(value, "expected default"));
            }
            default = true;
        }
        if matches!(&kind, InlineBlockKind::Fixed | InlineBlockKind::Matrix(_)) && default {
            return Err(syn::Error::new_spanned(
                &name,
                "field defaults are only supported on variable blocks",
            ));
        }
        let field_id = fields.len() as u32 + 1;
        fields.push(InlineField {
            name,
            ty,
            field_id,
            default,
        });
        if body.peek(Token![,]) {
            body.parse::<Token![,]>()?;
        }
    }
    let id = id.ok_or_else(|| syn::Error::new_spanned(&name, "missing block id"))?;
    if id >= 0xFFFF_FF00 {
        return Err(syn::Error::new_spanned(
            &name,
            "varve block id is reserved; use an id below 0xFFFF_FF00",
        ));
    }
    if version == 0 {
        return Err(syn::Error::new_spanned(
            &name,
            "block version must be non-zero",
        ));
    }
    for (index, key) in key_fields.iter().enumerate() {
        if key_fields[(index + 1)..].iter().any(|other| other == key) {
            return Err(syn::Error::new_spanned(key, "duplicate key field"));
        }
        if !fields.iter().any(|field| field.name == *key) {
            return Err(syn::Error::new_spanned(key, "key field does not exist"));
        }
    }
    if let InlineBlockKind::Matrix(meta) = &kind {
        for (index, dim) in meta.dims.iter().enumerate() {
            if meta.dims[(index + 1)..].iter().any(|other| other == dim) {
                return Err(syn::Error::new_spanned(dim, "duplicate matrix dimension"));
            }
        }
    }
    Ok(InlineBlock {
        kind,
        name,
        id,
        version,
        key_fields,
        key_index,
        fields,
    })
}

fn parse_compression_choice(input: ParseStream<'_>) -> Result<CompressionChoice> {
    let value: Ident = input.parse()?;
    match value.to_string().as_str() {
        "none" => Ok(CompressionChoice::None),
        "variable_blocks" => {
            let args;
            parenthesized!(args in input);
            parse_variable_compression_choice(&args).map(CompressionChoice::VariableBlocks)
        }
        _ => Err(syn::Error::new_spanned(
            value,
            "expected none or variable_blocks(...)",
        )),
    }
}

fn validate_matrix_format(
    dims: &[MatrixDim],
    commit: Option<&MatrixCommit>,
    aux: &[MatrixAux],
    blocks: &[InlineBlock],
) -> Result<()> {
    for (index, dim) in dims.iter().enumerate() {
        if dims[(index + 1)..]
            .iter()
            .any(|other| other.name == dim.name)
        {
            return Err(syn::Error::new_spanned(
                &dim.name,
                "duplicate matrix dimension",
            ));
        }
    }

    let matrix_blocks: Vec<_> = blocks
        .iter()
        .filter_map(|block| match &block.kind {
            InlineBlockKind::Matrix(meta) => Some((block, meta)),
            _ => None,
        })
        .collect();
    if matrix_blocks.is_empty() {
        if !dims.is_empty() || commit.is_some() || !aux.is_empty() {
            return Err(syn::Error::new_spanned(
                dims.first()
                    .map(|dim| dim.name.clone())
                    .or_else(|| commit.and_then(|commit| commit.keyspace.first().cloned()))
                    .or_else(|| aux.first().map(|aux| aux.name.clone()))
                    .unwrap_or_else(|| Ident::new("commit", proc_macro2::Span::call_site())),
                "matrix dims/commit/aux require at least one matrix block",
            ));
        }
        return Ok(());
    }

    for (index, item) in aux.iter().enumerate() {
        if item.byte_len == 0 {
            return Err(syn::Error::new_spanned(
                &item.name,
                "matrix aux byte_len must be non-zero",
            ));
        }
        if aux[(index + 1)..]
            .iter()
            .any(|other| other.name == item.name)
        {
            return Err(syn::Error::new_spanned(&item.name, "duplicate matrix aux"));
        }
    }

    let commit = commit.ok_or_else(|| {
        syn::Error::new_spanned(
            &matrix_blocks[0].0.name,
            "matrix blocks require commit: cell_bitmap { ... }",
        )
    })?;
    if dims.is_empty() {
        return Err(syn::Error::new_spanned(
            &matrix_blocks[0].0.name,
            "matrix blocks require dims { ... }",
        ));
    }
    if commit.keyspace.is_empty() {
        return Err(syn::Error::new_spanned(
            &matrix_blocks[0].0.name,
            "matrix commit keyspace must not be empty",
        ));
    }
    if commit.keyspace.len() != 2 {
        return Err(syn::Error::new_spanned(
            &matrix_blocks[0].0.name,
            "P0 matrix commit keyspace must contain exactly two dimensions",
        ));
    }
    validate_ident_list("matrix commit keyspace", &commit.keyspace)?;
    validate_ident_list("matrix commit categories", &commit.categories)?;
    validate_ident_list("matrix commit singles", &commit.singles)?;
    validate_ident_list("matrix commit per_channel", &commit.per_channel)?;

    for key_dim in &commit.keyspace {
        if !dims
            .iter()
            .any(|dim| matrix_dimension_matches_key(&dim.name, key_dim))
        {
            return Err(syn::Error::new_spanned(
                key_dim,
                "matrix commit keyspace dimension is not declared in dims",
            ));
        }
    }
    for (index, (_, meta)) in matrix_blocks.iter().enumerate() {
        if meta.dims != commit.keyspace {
            return Err(syn::Error::new_spanned(
                &meta.category,
                "P0 matrix block dims must match commit keyspace order",
            ));
        }
        if meta.dims.len() != 2 {
            return Err(syn::Error::new_spanned(
                &meta.category,
                "P0 matrix block dims must contain exactly two dimensions",
            ));
        }
        if !commit
            .categories
            .iter()
            .any(|category| category == &meta.category)
        {
            return Err(syn::Error::new_spanned(
                &meta.category,
                "matrix block category is not declared in commit categories",
            ));
        }
        for (_, other) in &matrix_blocks[(index + 1)..] {
            if meta.category == other.category {
                return Err(syn::Error::new_spanned(
                    &other.category,
                    "matrix cell commit category must be unique per block",
                ));
            }
        }
    }
    for (block, _) in &matrix_blocks {
        for field in &block.fields {
            if !matrix_field_is_fixed_width(&field.ty) {
                return Err(syn::Error::new_spanned(
                    &field.name,
                    "P0 matrix fields must be a named type (or an array of one) whose \
                     codec has a fixed encoded width; references, raw pointers, slices, \
                     tuples, trait objects, `impl Trait` and function pointers can never \
                     occupy a matrix slot",
                ));
            }
        }
    }
    Ok(())
}

/// Permissive syntactic shape check for matrix field types (API-04, F-10).
///
/// A matrix slot needs a stride the compiler knows. Deciding *which* types have
/// one is a question about types, and a proc macro sees only spellings, so this
/// function deliberately does **not** answer it. It rejects exactly the source
/// shapes that can never denote a `VarveEncode` type with a fixed encoded width
/// no matter what they resolve to — references, raw pointers, slices, tuples,
/// trait objects, `impl Trait`, function pointers, `dyn`/`!`/`_` and macro
/// invocations — and admits every named path plus arrays of an admitted
/// element.
///
/// The authority on the classification is the generated `SLOT_STRIDE`, which
/// resolves each element type's `VarveEncode::WIRE_TYPE` and fails to compile
/// for anything without a fixed encoded width — see
/// [`matrix_slot_stride_tokens`].
///
/// The previous revision whitelisted the literal primitive spellings
/// (`"u32"`, `"f64"`, …). That made a *spelling* authoritative over a *type*
/// in both directions: `type Word = u32;` was rejected even though the alias
/// resolves to a perfectly good 4-byte slot (contradicting
/// `docs/api-reference.md`), while a user struct named `u32` in scope would
/// have been waved through to the const check anyway. Only the const check can
/// tell those apart, so only the const check decides. `PackedBitmap` — built-in
/// or user-defined — now fails there rather than here: the built-in owns a
/// `Vec<u8>` and encodes a `bit_len` plus a variable byte string, so it has no
/// fixed encoded width, and a same-named user type has no `VarveEncode` impl at
/// all. It remains usable as an ordinary variable field.
fn matrix_field_is_fixed_width(ty: &Type) -> bool {
    match ty {
        // A named path may be a primitive, an alias for one, or a user type
        // with a fixed-width codec. `SLOT_STRIDE` decides.
        Type::Path(_) => true,
        Type::Array(array) => matrix_field_is_fixed_width(&array.elem),
        Type::Group(group) => matrix_field_is_fixed_width(&group.elem),
        Type::Paren(paren) => matrix_field_is_fixed_width(&paren.elem),
        _ => false,
    }
}

/// The compile-time slot stride of an inline matrix block (API-04).
///
/// Every field contributes the encoded width of its own codec, resolved through
/// `VarveEncode::WIRE_TYPE` rather than taken from `size_of`. The two disagree
/// for any type that owns heap storage — `size_of::<PackedBitmap>()` is the
/// width of a `Vec` plus a `u64`, not the width of the bytes it emits — so
/// deriving the stride from the Rust object size could publish a matrix whose
/// slots do not match the values written into them. A wire type with no fixed
/// width has no stride, and the const evaluation of this expression is what
/// rejects it, whatever the field type is spelled.
fn matrix_slot_stride_tokens(fields: &[InlineField]) -> TokenStream2 {
    let terms = fields.iter().map(|field| {
        let mut ty = &field.ty;
        let mut count = quote!(1u64);
        while let Type::Array(array) = ty {
            let len = &array.len;
            count = quote!(#count * ((#len) as u64));
            ty = &array.elem;
        }
        quote!(#count * __varve_matrix_encoded_width(
            <#ty as ::varve::__core::VarveEncode>::WIRE_TYPE
        ))
    });
    quote! {
        {
            const fn __varve_matrix_encoded_width(wire: ::varve::__core::WireType) -> u64 {
                match wire {
                    ::varve::__core::WireType::Bool
                    | ::varve::__core::WireType::U8
                    | ::varve::__core::WireType::I8 => 1,
                    ::varve::__core::WireType::U16 | ::varve::__core::WireType::I16 => 2,
                    ::varve::__core::WireType::U32
                    | ::varve::__core::WireType::I32
                    | ::varve::__core::WireType::F32 => 4,
                    ::varve::__core::WireType::U64
                    | ::varve::__core::WireType::I64
                    | ::varve::__core::WireType::F64 => 8,
                    ::varve::__core::WireType::U128 | ::varve::__core::WireType::I128 => 16,
                    _ => panic!(
                        "matrix fields must have a width fixed by their type; this codec does not"
                    ),
                }
            }
            0u64 #( + #terms )*
        }
    }
}

fn matrix_dimension_matches_key(dim: &Ident, key: &Ident) -> bool {
    matrix_storage_dimension_name_from_str(key.to_string().as_str(), &[dim.to_string()]).is_some()
}

fn matrix_storage_dimension_name(key: &Ident, dims: &[MatrixDim]) -> Option<String> {
    let names = dims
        .iter()
        .map(|dim| dim.name.to_string())
        .collect::<Vec<_>>();
    matrix_storage_dimension_name_from_str(&key.to_string(), &names)
}

fn matrix_storage_dimension_name_from_str(key: &str, dims: &[String]) -> Option<String> {
    let candidates = [
        key.to_string(),
        format!("n_{key}"),
        format!("n_{key}s"),
        if key == "ch" {
            "n_channels".to_string()
        } else {
            String::new()
        },
    ];
    candidates
        .into_iter()
        .filter(|candidate| !candidate.is_empty())
        .find(|candidate| dims.iter().any(|dim| dim == candidate))
}

fn validate_ident_list(label: &str, values: &[Ident]) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        if values[(index + 1)..].iter().any(|other| other == value) {
            return Err(syn::Error::new_spanned(
                value,
                format!("duplicate {label} entry"),
            ));
        }
    }
    Ok(())
}

fn parse_variable_compression_choice(input: ParseStream<'_>) -> Result<VariableCompressionChoice> {
    let algorithm: Ident = input.parse()?;
    let algorithm = match algorithm.to_string().as_str() {
        "zstd" => CompressionAlgorithmChoice::Zstd,
        _ => return Err(syn::Error::new_spanned(algorithm, "expected zstd")),
    };
    let mut level = CompressionLevelChoice::Default;
    let mut header = CompressionHeaderChoice::RecordExplicit;
    let mut min_len = 0u64;
    let mut only_if_smaller = true;
    let mut max_len = 64 * 1024 * 1024;

    while !input.is_empty() {
        input.parse::<Token![,]>()?;
        if input.is_empty() {
            break;
        }
        let key: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        match key.to_string().as_str() {
            "level" => {
                level = if input.peek(LitInt) {
                    let value: LitInt = input.parse()?;
                    CompressionLevelChoice::Exact(value.base10_parse::<i32>()?)
                } else {
                    let value: Ident = input.parse()?;
                    match value.to_string().as_str() {
                        "fast" => CompressionLevelChoice::Fast,
                        "default" => CompressionLevelChoice::Default,
                        "best" => CompressionLevelChoice::Best,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                value,
                                "expected fast, default, best, or an integer level",
                            ));
                        }
                    }
                };
            }
            "header" => {
                let value: Ident = input.parse()?;
                header = match value.to_string().as_str() {
                    "record_explicit" => CompressionHeaderChoice::RecordExplicit,
                    "file_explicit" => CompressionHeaderChoice::FileExplicit,
                    "format_contract" => CompressionHeaderChoice::FormatContract,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected record_explicit, file_explicit, or format_contract",
                        ));
                    }
                };
            }
            "min_len" => {
                let value: LitInt = input.parse()?;
                min_len = value.base10_parse::<u64>()?;
            }
            "only_if_smaller" => {
                let value: LitBool = input.parse()?;
                only_if_smaller = value.value;
            }
            "max_len" => {
                let value: LitInt = input.parse()?;
                max_len = value.base10_parse::<u64>()?;
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected level, header, min_len, only_if_smaller, or max_len",
                ));
            }
        }
    }

    Ok(VariableCompressionChoice {
        algorithm,
        level,
        header,
        min_len,
        only_if_smaller,
        max_len,
    })
}

fn expand_format(input: FormatInput) -> TokenStream2 {
    let vis = input.vis;
    let name = input.name;
    let magic = input.magic;
    let version = input.version;
    let (schema_hash, schema_hash_step) = match input.schema_hash {
        SchemaHashChoice::Literal(value) => (value, quote!()),
        SchemaHashChoice::Computed => (0u64, quote!(.with_computed_schema_hash())),
    };
    let extension = input
        .extension
        .map(|extension| quote!(::core::option::Option::Some(#extension)))
        .unwrap_or_else(|| quote!(::core::option::Option::None));
    let endian = match input.endian {
        EndianChoice::Little => quote!(::varve::__core::Endian::Little),
        EndianChoice::Big => quote!(::varve::__core::Endian::Big),
    };
    let keyed_offset_chain = input.index.keyed_offset_chain;
    let index = index_tokens(input.index);
    let commit = commit_tokens(input.commit);
    let integrity = match input.integrity {
        IntegrityChoice::None => quote!(::varve::__core::IntegrityPolicy::None),
        IntegrityChoice::Crc32 => quote!(::varve::__core::IntegrityPolicy::Crc32),
        IntegrityChoice::Crc32WithHeader => {
            quote!(::varve::__core::IntegrityPolicy::Crc32WithHeader)
        }
    };
    let recovery = match input.recovery {
        RecoveryChoice::Strict => quote!(::varve::__core::RecoveryPolicy::Strict),
        RecoveryChoice::TruncateTail => quote!(::varve::__core::RecoveryPolicy::TruncateTail),
    };
    let manifest = match input.manifest {
        ManifestChoice::None => quote!(::varve::__core::ManifestPolicy::None),
        ManifestChoice::Embedded => quote!(::varve::__core::ManifestPolicy::Embedded),
    };
    let compression = compression_tokens(input.compression);
    let read_limits = read_limits_tokens(input.limits);
    let dims = input.dims;
    let matrix_commit = input.matrix_commit;
    let matrix_aux = input.matrix_aux;
    let layout_file_header = input.layout_file_header;
    let layout_segments = input.layout_segments;
    let typed_api_enabled = input.typed_api;
    let layout = layout_tokens(
        input.layout_preset,
        layout_file_header.as_ref(),
        &layout_segments,
    );
    let registry_blocks = input.registry_blocks;
    let inline_blocks = input.inline_blocks;
    let inline_block_defs = inline_blocks
        .iter()
        .map(|block| inline_block_tokens(&vis, block, &dims));
    let matrix_type_defs = matrix_type_tokens(&vis, &name, &dims, &inline_blocks);
    let inline_block_types = inline_blocks.iter().map(|block| {
        let block = &block.name;
        syn::parse_quote!(#block)
    });
    let blocks: Vec<Type> = registry_blocks
        .into_iter()
        .chain(inline_block_types)
        .collect();

    let descriptors = blocks.iter().map(|block| {
        quote! {
            ::varve::__core::BlockDescriptor {
                id: <#block as ::varve::__core::VarveBlock>::ID,
                name: stringify!(#block),
                version: <#block as ::varve::__core::VarveBlock>::VERSION,
                kind: <#block as ::varve::__core::VarveBlock>::KIND,
                fields: <#block as ::varve::__core::VarveBlock>::FIELDS,
            }
        }
    });
    // Per-block schema identities folded into the computed schema hash:
    // endian override, keyedness, and the generated codec fingerprint
    // (API2-01). `BlockDescriptor` alone does not carry these.
    let block_identities = blocks.iter().map(|block| {
        quote! {
            (
                <#block as ::varve::__core::VarveBlock>::ID,
                <#block as ::varve::__core::VarveBlock>::ENDIAN,
                <#block as ::varve::__core::VarveBlock>::IS_KEYED,
                <#block as ::varve::__core::VarveBlock>::SCHEMA_FINGERPRINT,
            )
        }
    });

    let duplicate_asserts = pairwise(&blocks).into_iter().map(|(left, right)| {
        quote! {
            const _: () = assert!(
                <#left as ::varve::__core::VarveBlock>::ID
                    != <#right as ::varve::__core::VarveBlock>::ID
            );
        }
    });
    let typed_api = if typed_api_enabled {
        typed_api_tokens(&name, &inline_blocks, matrix_commit.as_ref(), &matrix_aux)
    } else {
        quote!()
    };
    let (high_cardinality_constructors, high_cardinality_api) =
        if typed_api_enabled && cfg!(feature = "high-cardinality-dev") {
            high_cardinality_api_tokens(&name, &inline_blocks, keyed_offset_chain)
        } else {
            (quote!(), quote!())
        };
    let layout_typed_api = if typed_api_enabled && !layout_segments.is_empty() {
        layout_typed_api_tokens(&name, layout_file_header.as_ref(), &layout_segments)
    } else {
        quote!()
    };
    let writer_return = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name)
    } else {
        quote!(::varve::__core::VarveWriter)
    };
    let reader_return = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(#reader_name)
    } else {
        quote!(::varve::__core::VarveReader)
    };
    let has_typed_layout_api = typed_api_enabled && !layout_segments.is_empty();
    let layout_writer_return = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(#writer_name)
    } else {
        quote!(::varve::__core::LayoutWriter)
    };
    let layout_reader_return = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(#reader_name)
    } else {
        quote!(::varve::__core::LayoutReader)
    };
    let create_layout_writer_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer(path)?)))
    } else {
        quote!(Self::spec().create_layout_writer(path))
    };
    let create_layout_writer_with_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_limits(path, limits)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_limits(path, limits))
    };
    let create_layout_writer_with_resource_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_resource_limits(path, limits)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_resource_limits(path, limits))
    };
    let create_layout_writer_trusted_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_trusted_unbounded(path)?)))
    } else {
        quote!(Self::spec().create_layout_writer_trusted_unbounded(path))
    };
    let create_layout_writer_with_header_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_header(path, fields)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_header(path, fields))
    };
    let create_layout_writer_with_header_and_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_header_and_limits(path, fields, limits)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_header_and_limits(path, fields, limits))
    };
    let create_layout_writer_with_header_and_resource_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_header_and_resource_limits(path, fields, limits)?)))
    } else {
        quote!(
            Self::spec().create_layout_writer_with_header_and_resource_limits(path, fields, limits)
        )
    };
    let create_layout_writer_with_header_trusted_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_header_trusted_unbounded(path, fields)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_header_trusted_unbounded(path, fields))
    };
    let open_layout_writer_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().open_layout_writer(path)?)))
    } else {
        quote!(Self::spec().open_layout_writer(path))
    };
    let open_layout_writer_with_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().open_layout_writer_with_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_layout_writer_with_limits(path, limits))
    };
    let open_layout_writer_with_resource_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().open_layout_writer_with_resource_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_layout_writer_with_resource_limits(path, limits))
    };
    let open_layout_writer_trusted_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().open_layout_writer_trusted_unbounded(path)?)))
    } else {
        quote!(Self::spec().open_layout_writer_trusted_unbounded(path))
    };
    let open_layout_reader_body = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_layout_reader(path)?)))
    } else {
        quote!(Self::spec().open_layout_reader(path))
    };
    let open_layout_reader_with_limits_body = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_layout_reader_with_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_layout_reader_with_limits(path, limits))
    };
    let open_layout_reader_with_resource_limits_body = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_layout_reader_with_resource_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_layout_reader_with_resource_limits(path, limits))
    };
    let open_layout_reader_trusted_body = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_layout_reader_trusted_unbounded(path)?)))
    } else {
        quote!(Self::spec().open_layout_reader_trusted_unbounded(path))
    };
    let create_writer_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer(path)?))
    } else {
        quote!(Self::spec().create_writer(path))
    };
    let create_writer_with_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer_with_limits(path, limits)?))
    } else {
        quote!(Self::spec().create_writer_with_limits(path, limits))
    };
    let create_writer_with_resource_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer_with_resource_limits(path, limits)?))
    } else {
        quote!(Self::spec().create_writer_with_resource_limits(path, limits))
    };
    let create_writer_trusted_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer_trusted_unbounded(path)?))
    } else {
        quote!(Self::spec().create_writer_trusted_unbounded(path))
    };
    let open_writer_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer(path)?))
    } else {
        quote!(Self::spec().open_writer(path))
    };
    let open_writer_with_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer_with_limits(path, limits)?))
    } else {
        quote!(Self::spec().open_writer_with_limits(path, limits))
    };
    let open_writer_with_resource_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer_with_resource_limits(path, limits)?))
    } else {
        quote!(Self::spec().open_writer_with_resource_limits(path, limits))
    };
    let open_writer_trusted_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer_trusted_unbounded(path)?))
    } else {
        quote!(Self::spec().open_writer_trusted_unbounded(path))
    };
    let open_recover_writer_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer(path)?))
    } else {
        quote!(Self::spec().open_recover_writer(path))
    };
    let open_recover_writer_with_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer_with_limits(path, limits)?))
    } else {
        quote!(Self::spec().open_recover_writer_with_limits(path, limits))
    };
    let open_recover_writer_with_resource_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer_with_resource_limits(path, limits)?))
    } else {
        quote!(Self::spec().open_recover_writer_with_resource_limits(path, limits))
    };
    let open_recover_writer_trusted_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer_trusted_unbounded(path)?))
    } else {
        quote!(Self::spec().open_recover_writer_trusted_unbounded(path))
    };
    let open_recover_writer_report_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec().open_recover_writer_with_report(path)?;
                ::core::result::Result::Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report(path))
    };
    let open_recover_writer_report_with_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec()
                    .open_recover_writer_with_report_and_limits(path, limits)?;
                ::core::result::Result::Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report_and_limits(path, limits))
    };
    let open_recover_writer_report_with_resource_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec()
                    .open_recover_writer_with_report_and_resource_limits(path, limits)?;
                ::core::result::Result::Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report_and_resource_limits(path, limits))
    };
    let open_recover_writer_report_trusted_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec()
                    .open_recover_writer_with_report_trusted_unbounded(path)?;
                ::core::result::Result::Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report_trusted_unbounded(path))
    };
    let open_reader_body = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_reader(path)?)))
    } else {
        quote!(Self::spec().open_reader(path))
    };
    let open_reader_with_limits_body = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_reader_with_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_reader_with_limits(path, limits))
    };
    let open_reader_with_resource_limits_body = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_reader_with_resource_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_reader_with_resource_limits(path, limits))
    };
    let open_reader_trusted_body = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_reader_trusted_unbounded(path)?)))
    } else {
        quote!(Self::spec().open_reader_trusted_unbounded(path))
    };
    let matrix_spec_step =
        matrix_spec_tokens(&dims, matrix_commit.as_ref(), &matrix_aux, &inline_blocks);
    let create_writer_with_dims_method = create_writer_with_dims_tokens(
        &name,
        &dims,
        matrix_commit.as_ref(),
        typed_api_enabled,
        &writer_return,
    );

    quote! {
        #matrix_type_defs
        #(#inline_block_defs)*

        #vis struct #name;

        impl #name {
            pub fn spec() -> ::varve::__core::FormatSpec {
                const BLOCKS: &[::varve::__core::BlockDescriptor] = &[
                    #(#descriptors,)*
                ];
                const BLOCK_IDENTITIES: &[(
                    u32,
                    ::core::option::Option<::varve::__core::Endian>,
                    bool,
                    u64,
                )] = &[
                    #(#block_identities,)*
                ];
                ::varve::__core::FormatSpec::new(
                    #magic,
                    #version,
                    #endian,
                    #schema_hash,
                    #index,
                    #integrity,
                    #recovery,
                    #manifest,
                    BLOCKS,
                )
                .with_extension(#extension)
                .with_commit_policy(#commit)
                .with_compression_policy(#compression)
                .with_read_limits(#read_limits)
                #matrix_spec_step
                .with_block_identities(BLOCK_IDENTITIES)
                .with_layout(#layout)
                #schema_hash_step
            }

            pub fn clear_stale_writer_lock<P: AsRef<::std::path::Path>>(
                path: P,
                policy: ::varve::__core::WriterLockBreakPolicy,
            ) -> ::varve::__core::Result<()> {
                Self::spec().clear_stale_writer_lock(path, policy)
            }

            pub fn create<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().create(path)
            }

            pub fn create_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().create_with_limits(path, limits)
            }

            pub fn create_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().create_with_resource_limits(path, limits)
            }

            pub fn create_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().create_trusted_unbounded(path)
            }

            pub fn create_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#writer_return> {
                #create_writer_body
            }

            pub fn create_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<#writer_return> {
                #create_writer_with_limits_body
            }

            pub fn create_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<#writer_return> {
                #create_writer_with_resource_limits_body
            }

            pub fn create_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<#writer_return> {
                #create_writer_trusted_body
            }

            pub fn create_layout_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#layout_writer_return> {
                #create_layout_writer_body
            }

            pub fn create_layout_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_limits_body
            }

            pub fn create_layout_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_resource_limits_body
            }

            pub fn create_layout_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #create_layout_writer_trusted_body
            }

            pub fn create_layout_writer_with_header<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::varve::__core::LayoutFieldValue],
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_header_body
            }

            pub fn create_layout_writer_with_header_and_limits<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::varve::__core::LayoutFieldValue],
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_header_and_limits_body
            }

            pub fn create_layout_writer_with_header_and_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::varve::__core::LayoutFieldValue],
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_header_and_resource_limits_body
            }

            pub fn create_layout_writer_with_header_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::varve::__core::LayoutFieldValue],
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_header_trusted_body
            }

            #create_writer_with_dims_method

            pub fn open<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open(path)
            }

            pub fn open_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_with_limits(path, limits)
            }

            pub fn open_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_with_resource_limits(path, limits)
            }

            pub fn open_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_trusted_unbounded(path)
            }

            pub fn open_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#writer_return> {
                #open_writer_body
            }

            pub fn open_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<#writer_return> {
                #open_writer_with_limits_body
            }

            pub fn open_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<#writer_return> {
                #open_writer_with_resource_limits_body
            }

            pub fn open_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<#writer_return> {
                #open_writer_trusted_body
            }

            pub fn open_layout_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#layout_writer_return> {
                #open_layout_writer_body
            }

            pub fn open_layout_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #open_layout_writer_with_limits_body
            }

            pub fn open_layout_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #open_layout_writer_with_resource_limits_body
            }

            pub fn open_layout_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<#layout_writer_return> {
                #open_layout_writer_trusted_body
            }

            pub fn open_readonly<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_readonly(path)
            }

            pub fn open_readonly_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_readonly_with_limits(path, limits)
            }

            pub fn open_readonly_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_readonly_with_resource_limits(path, limits)
            }

            pub fn open_readonly_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_readonly_trusted_unbounded(path)
            }

            pub fn open_reader<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#reader_return> {
                #open_reader_body
            }

            pub fn open_reader_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<#reader_return> {
                #open_reader_with_limits_body
            }

            pub fn open_reader_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<#reader_return> {
                #open_reader_with_resource_limits_body
            }

            pub fn open_reader_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<#reader_return> {
                #open_reader_trusted_body
            }

            #high_cardinality_constructors

            pub fn open_layout_reader<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#layout_reader_return> {
                #open_layout_reader_body
            }

            pub fn open_layout_reader_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<#layout_reader_return> {
                #open_layout_reader_with_limits_body
            }

            pub fn open_layout_reader_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<#layout_reader_return> {
                #open_layout_reader_with_resource_limits_body
            }

            pub fn open_layout_reader_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<#layout_reader_return> {
                #open_layout_reader_trusted_body
            }

            pub fn inspect_layout_file<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file(path)
            }

            pub fn inspect_layout_file_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<::varve::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file_with_limits(path, limits)
            }

            pub fn inspect_layout_file_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<::varve::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file_with_resource_limits(path, limits)
            }

            pub fn inspect_layout_file_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<::varve::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file_trusted_unbounded(path)
            }

            pub fn inspect_layout_file_report<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::LayoutScanReport> {
                Self::spec().inspect_layout_file_report(path)
            }

            pub fn inspect_layout_file_report_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<::varve::__core::LayoutScanReport> {
                Self::spec().inspect_layout_file_report_with_limits(path, limits)
            }

            pub fn inspect_layout_file_report_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<::varve::__core::LayoutScanReport> {
                Self::spec().inspect_layout_file_report_with_resource_limits(path, limits)
            }

            pub fn inspect_layout_file_report_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<::varve::__core::LayoutScanReport> {
                Self::spec().inspect_layout_file_report_trusted_unbounded(path)
            }

            pub fn open_recover<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_recover(path)
            }

            pub fn open_recover_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_recover_with_limits(path, limits)
            }

            pub fn open_recover_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_recover_with_resource_limits(path, limits)
            }

            pub fn open_recover_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_recover_trusted_unbounded(path)
            }

            pub fn open_recover_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#writer_return> {
                #open_recover_writer_body
            }

            pub fn open_recover_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<#writer_return> {
                #open_recover_writer_with_limits_body
            }

            pub fn open_recover_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<#writer_return> {
                #open_recover_writer_with_resource_limits_body
            }

            pub fn open_recover_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<#writer_return> {
                #open_recover_writer_trusted_body
            }

            pub fn open_recover_with_report<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<(::varve::__core::VarveFile, ::varve::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report(path)
            }

            pub fn open_recover_with_report_and_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<(::varve::__core::VarveFile, ::varve::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report_and_limits(path, limits)
            }

            pub fn open_recover_with_report_and_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<(::varve::__core::VarveFile, ::varve::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report_and_resource_limits(path, limits)
            }

            pub fn open_recover_with_report_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<(::varve::__core::VarveFile, ::varve::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report_trusted_unbounded(path)
            }

            pub fn open_recover_writer_with_report<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<(#writer_return, ::varve::__core::RecoveryReport)> {
                #open_recover_writer_report_body
            }

            pub fn open_recover_writer_with_report_and_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ReadLimits,
            ) -> ::varve::__core::Result<(#writer_return, ::varve::__core::RecoveryReport)> {
                #open_recover_writer_report_with_limits_body
            }

            pub fn open_recover_writer_with_report_and_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::varve::__core::ResourceLimits,
            ) -> ::varve::__core::Result<(#writer_return, ::varve::__core::RecoveryReport)> {
                #open_recover_writer_report_with_resource_limits_body
            }

            pub fn open_recover_writer_with_report_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<(#writer_return, ::varve::__core::RecoveryReport)> {
                #open_recover_writer_report_trusted_body
            }

            pub fn diagnostics() -> ::varve::__core::FormatDiagnostics {
                Self::spec().diagnostics()
            }

            pub fn diagnose_file<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::FormatDiagnostics {
                Self::spec().diagnose_file(path)
            }

            pub fn self_test<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::FormatSelfTest {
                Self::spec().self_test(path)
            }
        }

        #(#duplicate_asserts)*
        #typed_api
        #high_cardinality_api
        #layout_typed_api
    }
}

fn layout_tokens(
    preset: Option<LayoutPresetChoice>,
    file_header: Option<&LayoutFileHeader>,
    segments: &[LayoutSegment],
) -> TokenStream2 {
    let has_parts = file_header.is_some() || !segments.is_empty();
    let preset = preset.unwrap_or(if !has_parts {
        LayoutPresetChoice::VarveNative
    } else {
        LayoutPresetChoice::None
    });
    let preset_tokens = layout_preset_tokens(preset);
    if !has_parts {
        return match preset {
            LayoutPresetChoice::VarveNative => quote!(::varve::__core::LayoutSpec::varve_native()),
            LayoutPresetChoice::None => quote!(::varve::__core::LayoutSpec::none()),
            LayoutPresetChoice::Custom => {
                quote!(::varve::__core::LayoutSpec::custom(&[]))
            }
        };
    }

    let header_fields_const = format_ident!("__VARVE_LAYOUT_FILE_HEADER_FIELDS");
    let header_field_const = file_header
        .map(|header| {
            let fields = header.fields.iter().map(layout_field_tokens);
            quote! {
                const #header_fields_const: &[::varve::__core::LayoutFieldDescriptor] = &[
                    #(#fields,)*
                ];
            }
        })
        .unwrap_or_else(|| quote!());
    let header_part = file_header
        .map(|header| {
            let name = &header.name;
            quote! {
                ::varve::__core::LayoutPartDescriptor {
                    name: stringify!(#name),
                    kind: ::varve::__core::LayoutPartKind::FileHeader(
                        ::varve::__core::FileHeaderDescriptor {
                            name: stringify!(#name),
                            fields: #header_fields_const,
                        }
                    ),
                },
            }
        })
        .unwrap_or_else(|| quote!());

    let lead_field_const_names: Vec<_> = (0..segments.len())
        .map(|index| format_ident!("__VARVE_LAYOUT_LEAD_IN_FIELDS_{index}"))
        .collect();
    let footer_field_const_names: Vec<_> = (0..segments.len())
        .map(|index| format_ident!("__VARVE_LAYOUT_FOOTER_FIELDS_{index}"))
        .collect();
    let lead_field_consts =
        segments
            .iter()
            .zip(lead_field_const_names.iter())
            .map(|(segment, const_name)| {
                let fields = segment.lead_in.fields.iter().map(layout_field_tokens);
                quote! {
                    const #const_name: &[::varve::__core::LayoutFieldDescriptor] = &[
                        #(#fields,)*
                    ];
                }
            });
    let footer_field_consts = segments
        .iter()
        .zip(footer_field_const_names.iter())
        .filter_map(|(segment, const_name)| {
            segment.footer.as_ref().map(|footer| {
                let fields = footer.fields.iter().map(layout_field_tokens);
                quote! {
                    const #const_name: &[::varve::__core::LayoutFieldDescriptor] = &[
                        #(#fields,)*
                    ];
                }
            })
        });
    let parts = segments
        .iter()
        .zip(lead_field_const_names.iter())
        .zip(footer_field_const_names.iter())
        .map(|((segment, fields), footer_fields)| {
            let name = &segment.name;
            let repeat = segment_repeat_tokens(segment.repeat);
            let lead_in = &segment.lead_in.name;
            let metadata = &segment.metadata;
            let raw_region = &segment.raw_region;
            let footer = segment
                .footer
                .as_ref()
                .map(|footer| {
                    let footer_name = &footer.name;
                    quote! {
                        ::core::option::Option::Some(::varve::__core::FooterDescriptor {
                            name: stringify!(#footer_name),
                            fields: #footer_fields,
                        })
                    }
                })
                .unwrap_or_else(|| quote!(::core::option::Option::None));
            quote! {
                ::varve::__core::LayoutPartDescriptor {
                    name: stringify!(#name),
                    kind: ::varve::__core::LayoutPartKind::Segment(
                        ::varve::__core::SegmentDescriptor {
                            name: stringify!(#name),
                            repeat: #repeat,
                            lead_in: ::varve::__core::LeadInDescriptor {
                                name: stringify!(#lead_in),
                                fields: #fields,
                            },
                            metadata: ::varve::__core::MetadataDescriptor {
                                name: stringify!(#metadata),
                                source: ::varve::__core::LayoutBytesSource::Caller,
                            },
                            raw_region: ::varve::__core::RawRegionDescriptor {
                                name: stringify!(#raw_region),
                                source: ::varve::__core::LayoutBytesSource::Caller,
                            },
                            footer: #footer,
                        }
                    ),
                }
            }
        });

    quote! {
        {
            #header_field_const
            #(#lead_field_consts)*
            #(#footer_field_consts)*
            const __VARVE_LAYOUT_PARTS: &[::varve::__core::LayoutPartDescriptor] = &[
                #header_part
                #(#parts,)*
            ];
            ::varve::__core::LayoutSpec {
                preset: #preset_tokens,
                parts: __VARVE_LAYOUT_PARTS,
            }
        }
    }
}

fn layout_field_tokens(field: &LayoutField) -> TokenStream2 {
    let name = &field.name;
    let ty = layout_field_type_tokens(&field.ty);
    let source = layout_field_source_tokens(&field.source);
    quote! {
        ::varve::__core::LayoutFieldDescriptor {
            name: stringify!(#name),
            ty: #ty,
            source: #source,
            endian: None,
        }
    }
}

fn layout_preset_tokens(preset: LayoutPresetChoice) -> TokenStream2 {
    match preset {
        LayoutPresetChoice::VarveNative => quote!(::varve::__core::LayoutPreset::VarveNative),
        LayoutPresetChoice::None => quote!(::varve::__core::LayoutPreset::None),
        LayoutPresetChoice::Custom => quote!(::varve::__core::LayoutPreset::Custom),
    }
}

fn segment_repeat_tokens(repeat: SegmentRepeatChoice) -> TokenStream2 {
    match repeat {
        SegmentRepeatChoice::Once => quote!(::varve::__core::SegmentRepeat::Once),
        SegmentRepeatChoice::UntilEof => quote!(::varve::__core::SegmentRepeat::UntilEof),
    }
}

fn layout_field_type_tokens(ty: &LayoutFieldTypeChoice) -> TokenStream2 {
    match ty {
        LayoutFieldTypeChoice::Bytes(len) => {
            quote!(::varve::__core::LayoutFieldType::Bytes { len: #len })
        }
        LayoutFieldTypeChoice::U8 => quote!(::varve::__core::LayoutFieldType::U8),
        LayoutFieldTypeChoice::U16 => quote!(::varve::__core::LayoutFieldType::U16),
        LayoutFieldTypeChoice::U32 => quote!(::varve::__core::LayoutFieldType::U32),
        LayoutFieldTypeChoice::U64 => quote!(::varve::__core::LayoutFieldType::U64),
        LayoutFieldTypeChoice::I64 => quote!(::varve::__core::LayoutFieldType::I64),
    }
}

fn layout_field_source_tokens(source: &LayoutFieldSourceChoice) -> TokenStream2 {
    match source {
        LayoutFieldSourceChoice::LiteralBytes(bytes) => {
            quote!(::varve::__core::LayoutFieldSource::LiteralBytes(#bytes))
        }
        LayoutFieldSourceChoice::LiteralU64(value) => {
            quote!(::varve::__core::LayoutFieldSource::LiteralU64(#value))
        }
        LayoutFieldSourceChoice::LiteralI64(value) => {
            quote!(::varve::__core::LayoutFieldSource::LiteralI64(#value))
        }
        LayoutFieldSourceChoice::Caller => {
            quote!(::varve::__core::LayoutFieldSource::Caller)
        }
        LayoutFieldSourceChoice::Finalize(finalize) => {
            let target = layout_anchor_tokens(finalize.target);
            let relative_to = layout_anchor_tokens(finalize.relative_to);
            quote! {
                ::varve::__core::LayoutFieldSource::Finalize(
                    ::varve::__core::LayoutFinalize {
                        target: #target,
                        relative_to: #relative_to,
                    }
                )
            }
        }
    }
}

fn layout_anchor_tokens(anchor: LayoutAnchorChoice) -> TokenStream2 {
    match anchor {
        LayoutAnchorChoice::SegmentStart => quote!(::varve::__core::LayoutAnchor::SegmentStart),
        LayoutAnchorChoice::AfterLeadIn => quote!(::varve::__core::LayoutAnchor::AfterLeadIn),
        LayoutAnchorChoice::MetadataStart => quote!(::varve::__core::LayoutAnchor::MetadataStart),
        LayoutAnchorChoice::RawRegionStart => {
            quote!(::varve::__core::LayoutAnchor::RawRegionStart)
        }
        LayoutAnchorChoice::SegmentEnd => quote!(::varve::__core::LayoutAnchor::SegmentEnd),
        LayoutAnchorChoice::FooterStart => quote!(::varve::__core::LayoutAnchor::FooterStart),
        LayoutAnchorChoice::FooterEnd => quote!(::varve::__core::LayoutAnchor::FooterEnd),
    }
}

fn index_tokens(choice: IndexChoice) -> TokenStream2 {
    let scan_on_open = choice.scan_on_open;
    let checkpoint_on_flush = choice.checkpoint_on_flush;
    let block_offset_chain = choice.block_offset_chain;
    let keyed_offset_chain = choice.keyed_offset_chain;
    quote! {
        ::varve::__core::IndexPolicy::new(
            #scan_on_open,
            #checkpoint_on_flush,
            #block_offset_chain,
            #keyed_offset_chain,
        )
    }
}

fn commit_tokens(choice: CommitChoice) -> TokenStream2 {
    match choice {
        CommitChoice::None => quote!(::varve::__core::CommitPolicy::None),
        CommitChoice::RecordFooter => quote!(::varve::__core::CommitPolicy::RecordFooter),
        CommitChoice::TransactionMarkerOnFlush => quote! {
            ::varve::__core::CommitPolicy::TransactionMarker(
                ::varve::__core::TransactionMarkerMode::OnFlush
            )
        },
        CommitChoice::TransactionMarkerExplicit => quote! {
            ::varve::__core::CommitPolicy::TransactionMarker(
                ::varve::__core::TransactionMarkerMode::Explicit
            )
        },
    }
}

fn matrix_type_tokens(
    vis: &Visibility,
    format_name: &Ident,
    dims: &[MatrixDim],
    blocks: &[InlineBlock],
) -> TokenStream2 {
    let dims_tokens = if dims.is_empty() {
        quote!()
    } else {
        let dims_name = format_ident!("{}Dims", format_name);
        let fields = dims.iter().map(|dim| {
            let name = &dim.name;
            let ty = &dim.ty;
            quote!(#vis #name: #ty,)
        });
        let pairs = dims.iter().map(|dim| {
            let name = dim.name.to_string();
            let field = &dim.name;
            quote!((#name, self.#field as u64))
        });
        quote! {
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #vis struct #dims_name {
                #(#fields)*
            }

            impl #dims_name {
                pub fn into_matrix_dims(self) -> ::varve::__core::MatrixDimensions {
                    ::varve::__core::MatrixDimensions::from_pairs([
                        #(#pairs,)*
                    ])
                }
            }
        }
    };

    let key_tokens = blocks.iter().filter_map(|block| {
        let InlineBlockKind::Matrix(meta) = &block.kind else {
            return None;
        };
        let key_name = format_ident!("{}Key", block.name);
        let fields = meta.dims.iter().map(|dim| quote!(#vis #dim: u64,));
        let first = &meta.dims[0];
        let second = &meta.dims[1];
        Some(quote! {
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
            #vis struct #key_name {
                #(#fields)*
            }

            impl From<#key_name> for ::varve::__core::MatrixKey {
                fn from(key: #key_name) -> Self {
                    ::varve::__core::MatrixKey::new(key.#first, key.#second)
                }
            }
        })
    });

    quote! {
        #dims_tokens
        #(#key_tokens)*
    }
}

fn matrix_spec_tokens(
    dims: &[MatrixDim],
    commit: Option<&MatrixCommit>,
    aux: &[MatrixAux],
    blocks: &[InlineBlock],
) -> TokenStream2 {
    let matrix_blocks: Vec<_> = blocks
        .iter()
        .filter_map(|block| match &block.kind {
            InlineBlockKind::Matrix(meta) => Some((block, meta)),
            _ => None,
        })
        .collect();
    if matrix_blocks.is_empty() {
        return quote!();
    }
    let Some(commit) = commit else {
        return quote!();
    };
    let dimensions = dims.iter().map(|dim| {
        let name = dim.name.to_string();
        quote!(::varve::__core::MatrixDimensionDescriptor { name: #name })
    });
    let cell_commits = commit.categories.iter().map(|ident| {
        let name = ident.to_string();
        quote! {
            ::varve::__core::MatrixCommitDescriptor {
                name: #name,
                kind: ::varve::__core::MatrixCommitKind::Cell,
            }
        }
    });
    let single_commits = commit.singles.iter().map(|ident| {
        let name = ident.to_string();
        quote! {
            ::varve::__core::MatrixCommitDescriptor {
                name: #name,
                kind: ::varve::__core::MatrixCommitKind::Single,
            }
        }
    });
    let channel_commits = commit.per_channel.iter().map(|ident| {
        let name = ident.to_string();
        quote! {
            ::varve::__core::MatrixCommitDescriptor {
                name: #name,
                kind: ::varve::__core::MatrixCommitKind::PerChannel,
            }
        }
    });
    let block_specs = matrix_blocks.iter().map(|(block, meta)| {
        let ty = &block.name;
        let category = meta.category.to_string();
        let first = matrix_storage_dimension_name(&meta.dims[0], dims)
            .unwrap_or_else(|| meta.dims[0].to_string());
        let second = matrix_storage_dimension_name(&meta.dims[1], dims)
            .unwrap_or_else(|| meta.dims[1].to_string());
        quote! {
            ::varve::__core::MatrixBlockDescriptor {
                block_id: <#ty as ::varve::__core::VarveBlock>::ID,
                dimensions: [#first, #second],
                category: #category,
                slot_stride: <#ty as ::varve::__core::VarveMatrixBlock>::SLOT_STRIDE,
            }
        }
    });
    let aux_step = if aux.is_empty() {
        quote!()
    } else {
        let aux_specs = aux.iter().map(|aux| {
            let name = aux.name.to_string();
            let byte_len = aux.byte_len;
            quote! {
                ::varve::__core::MatrixAuxDescriptor {
                    name: #name,
                    byte_len: #byte_len,
                }
            }
        });
        quote! {
            .with_matrix_aux(&[
                #(#aux_specs,)*
            ])
        }
    };
    quote! {
        .with_matrix_spec(
            &[
                #(#dimensions,)*
            ],
            &[
                #(#cell_commits,)*
                #(#single_commits,)*
                #(#channel_commits,)*
            ],
            &[
                #(#block_specs,)*
            ],
        )
        #aux_step
    }
}

fn create_writer_with_dims_tokens(
    format_name: &Ident,
    dims: &[MatrixDim],
    commit: Option<&MatrixCommit>,
    typed_api: bool,
    writer_return: &TokenStream2,
) -> TokenStream2 {
    if dims.is_empty() || commit.is_none() {
        return quote!();
    }
    let dims_name = format_ident!("{}Dims", format_name);
    let body = if typed_api {
        let writer_name = format_ident!("{}Writer", format_name);
        quote!(#writer_name::from_inner(
            Self::spec().create_writer_with_dims(path, dims.into_matrix_dims())?
        ))
    } else {
        quote!(Self::spec().create_writer_with_dims(path, dims.into_matrix_dims()))
    };
    let with_limits_body = if typed_api {
        let writer_name = format_ident!("{}Writer", format_name);
        quote!(#writer_name::from_inner(
            Self::spec().create_writer_with_dims_and_limits(
                path,
                dims.into_matrix_dims(),
                limits,
            )?
        ))
    } else {
        quote!(Self::spec().create_writer_with_dims_and_limits(
            path,
            dims.into_matrix_dims(),
            limits,
        ))
    };
    let with_resource_limits_body = if typed_api {
        let writer_name = format_ident!("{}Writer", format_name);
        quote!(#writer_name::from_inner(
            Self::spec().create_writer_with_dims_and_resource_limits(
                path,
                dims.into_matrix_dims(),
                limits,
            )?
        ))
    } else {
        quote!(Self::spec().create_writer_with_dims_and_resource_limits(
            path,
            dims.into_matrix_dims(),
            limits,
        ))
    };
    let trusted_body = if typed_api {
        let writer_name = format_ident!("{}Writer", format_name);
        quote!(#writer_name::from_inner(
            Self::spec().create_writer_with_dims_trusted_unbounded(
                path,
                dims.into_matrix_dims(),
            )?
        ))
    } else {
        quote!(
            Self::spec().create_writer_with_dims_trusted_unbounded(path, dims.into_matrix_dims(),)
        )
    };
    quote! {
        pub fn create_writer_with_dims<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
        ) -> ::varve::__core::Result<#writer_return> {
            #body
        }

        pub fn create_writer_with_dims_and_limits<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
            limits: ::varve::__core::ReadLimits,
        ) -> ::varve::__core::Result<#writer_return> {
            #with_limits_body
        }

        pub fn create_writer_with_dims_and_resource_limits<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
            limits: ::varve::__core::ResourceLimits,
        ) -> ::varve::__core::Result<#writer_return> {
            #with_resource_limits_body
        }

        pub fn create_writer_with_dims_trusted_unbounded<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
        ) -> ::varve::__core::Result<#writer_return> {
            #trusted_body
        }
    }
}

fn inline_block_tokens(vis: &Visibility, block: &InlineBlock, dims: &[MatrixDim]) -> TokenStream2 {
    let name = &block.name;
    let id = block.id;
    let version = block.version;
    let kind = match &block.kind {
        InlineBlockKind::Fixed => LitStr::new("fixed", name.span()),
        InlineBlockKind::Variable => LitStr::new("variable", name.span()),
        InlineBlockKind::Matrix(_) => LitStr::new("matrix", name.span()),
    };
    let key_attr = if block.key_fields.is_empty() {
        quote!()
    } else {
        let key = block
            .key_fields
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let key = LitStr::new(&key, name.span());
        quote!(#[varve(key = #key)])
    };
    let fields = block.fields.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        let field_id = field.field_id;
        let default = if field.default {
            quote!(#[varve(default)])
        } else {
            quote!()
        };
        quote! {
            #[varve(field_id = #field_id)]
            #default
            #vis #name: #ty,
        }
    });
    let matrix_impl = if let InlineBlockKind::Matrix(meta) = &block.kind {
        let matrix_dims = meta
            .dims
            .iter()
            .map(|dim| matrix_storage_dimension_name(dim, dims).unwrap_or_else(|| dim.to_string()));
        let category = meta.category.to_string();
        let stride = matrix_slot_stride_tokens(&block.fields);
        quote! {
            impl ::varve::__core::VarveMatrixBlock for #name {
                const DIMENSIONS: [&'static str; 2] = [#(#matrix_dims,)*];
                const CATEGORY: &'static str = #category;
                const SLOT_STRIDE: u64 = #stride;
            }
        }
    } else {
        quote!()
    };
    quote! {
        #[derive(Clone, Debug, PartialEq, ::varve::VarveBlock)]
        #[varve(id = #id, version = #version, kind = #kind)]
        #key_attr
        #vis struct #name {
            #(#fields)*
        }

        #matrix_impl
    }
}

fn layout_typed_api_tokens(
    format_name: &Ident,
    file_header: Option<&LayoutFileHeader>,
    segments: &[LayoutSegment],
) -> TokenStream2 {
    let reader_name = format_ident!("{}LayoutReader", format_name);
    let writer_name = format_ident!("{}LayoutWriter", format_name);
    let header_tokens = file_header
        .map(|header| layout_header_typed_tokens(format_name, &writer_name, header))
        .unwrap_or_else(|| quote!());
    let reader_header_methods = file_header
        .map(|header| layout_reader_header_method_tokens(format_name, header))
        .unwrap_or_else(|| quote!());
    let segment_types = segments
        .iter()
        .map(|segment| layout_segment_typed_tokens(format_name, segment));
    let writer_segment_methods = segments
        .iter()
        .map(|segment| layout_writer_segment_method_tokens(format_name, segment));
    let reader_segment_methods = segments
        .iter()
        .map(|segment| layout_reader_segment_methods_tokens(format_name, segment));

    quote! {
        #header_tokens
        #(#segment_types)*

        #[derive(Debug)]
        pub struct #reader_name {
            inner: ::varve::__core::LayoutReader,
        }

        impl #reader_name {
            pub fn from_inner(inner: ::varve::__core::LayoutReader) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::varve::__core::LayoutReader {
                self.inner
            }

            pub fn spec(&self) -> ::varve::__core::FormatSpec {
                self.inner.spec()
            }

            pub fn path(&self) -> &::std::path::Path {
                self.inner.path()
            }

            pub fn file_header_len(&self) -> u64 {
                self.inner.file_header_len()
            }

            pub fn segments(&self) -> &[::varve::__core::LayoutSegmentInfo] {
                self.inner.segments()
            }

            pub fn read_metadata(&self, index: usize) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
                self.inner.read_metadata(index)
            }

            pub fn read_metadata_range(
                &self,
                index: usize,
                offset: u64,
                len: u64,
            ) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
                self.inner.read_metadata_range(index, offset, len)
            }

            pub fn read_raw(&self, index: usize) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
                self.inner.read_raw(index)
            }

            pub fn read_raw_range(
                &self,
                index: usize,
                offset: u64,
                len: u64,
            ) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
                self.inner.read_raw_range(index, offset, len)
            }

            fn __varve_layout_segment_index(
                &self,
                name: &'static str,
                ordinal: usize,
            ) -> ::varve::__core::Result<usize> {
                self.inner
                    .segments()
                    .iter()
                    .enumerate()
                    .filter(|(_, segment)| segment.name == name)
                    .nth(ordinal)
                    .map(|(index, _)| index)
                    .ok_or_else(|| ::varve::__core::Error::LayoutSegmentIndexOutOfBounds {
                        segment: name.to_string(),
                        index: ordinal,
                    })
            }

            #reader_header_methods
            #(#reader_segment_methods)*
        }

        #[derive(Debug)]
        pub struct #writer_name {
            inner: ::varve::__core::LayoutWriter,
        }

        impl #writer_name {
            pub fn from_inner(inner: ::varve::__core::LayoutWriter) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::varve::__core::LayoutWriter {
                self.inner
            }

            pub fn spec(&self) -> ::varve::__core::FormatSpec {
                self.inner.spec()
            }

            pub fn path(&self) -> &::std::path::Path {
                self.inner.path()
            }

            pub fn write_segment(
                &mut self,
                segment: ::varve::__core::SegmentWrite<'_>,
            ) -> ::varve::__core::Result<::varve::__core::LayoutSegmentInfo> {
                self.inner.write_segment(segment)
            }

            pub fn write_segment_streamed<M, R>(
                &mut self,
                segment: ::varve::__core::SegmentWriteStream<'_, M, R>,
            ) -> ::varve::__core::Result<::varve::__core::LayoutSegmentInfo>
            where
                M: FnOnce(&mut dyn ::std::io::Write) -> ::varve::__core::Result<()>,
                R: FnOnce(&mut dyn ::std::io::Write) -> ::varve::__core::Result<()>,
            {
                self.inner.write_segment_streamed(segment)
            }

            pub fn flush(&mut self) -> ::varve::__core::Result<()> {
                self.inner.flush()
            }

            pub fn sync(&mut self) -> ::varve::__core::Result<()> {
                self.inner.sync()
            }

            #(#writer_segment_methods)*
        }
    }
}

fn layout_header_typed_tokens(
    format_name: &Ident,
    writer_name: &Ident,
    header: &LayoutFileHeader,
) -> TokenStream2 {
    let fields_name = format_ident!("{}{}LayoutFields", format_name, header.name);
    let info_name = format_ident!("{}{}LayoutInfo", format_name, header.name);
    let field_struct = layout_field_struct_tokens(&fields_name, &header.fields);
    let getters = header
        .fields
        .iter()
        .map(layout_file_header_info_field_getter);
    quote! {
        #field_struct

        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct #info_name {
            fields: ::std::vec::Vec<::varve::__core::LayoutFieldValue>,
        }

        impl #info_name {
            pub fn from_fields(fields: ::std::vec::Vec<::varve::__core::LayoutFieldValue>) -> Self {
                Self { fields }
            }

            pub fn fields(&self) -> &[::varve::__core::LayoutFieldValue] {
                &self.fields
            }

            pub fn field(&self, name: &str) -> ::core::option::Option<&::varve::__core::LayoutValue> {
                self.fields
                    .iter()
                    .find(|field| field.name == name)
                    .map(|field| &field.value)
            }

            #(#getters)*
        }

        impl #format_name {
            pub fn create_layout_writer_with_typed_header<P: AsRef<::std::path::Path>>(
                path: P,
                header: #fields_name,
            ) -> ::varve::__core::Result<#writer_name> {
                let fields = header.__varve_layout_values();
                ::core::result::Result::Ok(#writer_name::from_inner(
                    Self::spec().create_layout_writer_with_header(path, &fields)?
                ))
            }
        }
    }
}

fn layout_reader_header_method_tokens(
    format_name: &Ident,
    header: &LayoutFileHeader,
) -> TokenStream2 {
    let info_name = format_ident!("{}{}LayoutInfo", format_name, header.name);
    quote! {
        pub fn file_header(&self) -> #info_name {
            #info_name::from_fields(self.inner.file_header_fields().to_vec())
        }

        pub fn file_header_fields(&self) -> &[::varve::__core::LayoutFieldValue] {
            self.inner.file_header_fields()
        }

        pub fn file_header_field(
            &self,
            name: &str,
        ) -> ::core::option::Option<&::varve::__core::LayoutValue> {
            self.inner.file_header_field(name)
        }
    }
}

fn layout_segment_typed_tokens(format_name: &Ident, segment: &LayoutSegment) -> TokenStream2 {
    let field_type = format_ident!("{}{}LayoutFields", format_name, segment.name);
    let footer_field_type = format_ident!("{}{}LayoutFooterFields", format_name, segment.name);
    let write_type = format_ident!("{}{}LayoutWrite", format_name, segment.name);
    let info_type = format_ident!("{}{}LayoutInfo", format_name, segment.name);
    let field_struct = layout_field_struct_tokens(&field_type, &segment.lead_in.fields);
    let footer_fields = segment
        .footer
        .as_ref()
        .map(|footer| footer.fields.as_slice())
        .unwrap_or(&[]);
    let footer_field_struct = layout_field_struct_tokens(&footer_field_type, footer_fields);
    let lead_in_getters = segment.lead_in.fields.iter().map(layout_info_field_getter);
    let footer_getters = footer_fields.iter().map(layout_info_footer_field_getter);

    quote! {
        #field_struct
        #footer_field_struct

        #[derive(Clone, Debug)]
        pub struct #write_type<'a> {
            pub fields: #field_type,
            pub footer_fields: #footer_field_type,
            pub metadata: &'a [u8],
            pub raw: &'a [u8],
        }

        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct #info_type {
            inner: ::varve::__core::LayoutSegmentInfo,
        }

        impl #info_type {
            pub fn from_inner(inner: ::varve::__core::LayoutSegmentInfo) -> Self {
                Self { inner }
            }

            pub fn as_layout_segment_info(&self) -> &::varve::__core::LayoutSegmentInfo {
                &self.inner
            }

            pub fn into_layout_segment_info(self) -> ::varve::__core::LayoutSegmentInfo {
                self.inner
            }

            pub fn segment_start(&self) -> u64 {
                self.inner.segment_start
            }

            pub fn metadata_len(&self) -> u64 {
                self.inner.metadata_len
            }

            pub fn raw_len(&self) -> u64 {
                self.inner.raw_len
            }

            pub fn segment_end(&self) -> u64 {
                self.inner.segment_end
            }

            #(#lead_in_getters)*
            #(#footer_getters)*
        }
    }
}

fn layout_writer_segment_method_tokens(
    format_name: &Ident,
    segment: &LayoutSegment,
) -> TokenStream2 {
    let method = format_ident!("write_{}", singular_method_name(&segment.name));
    let streamed_method = format_ident!("write_{}_streamed", singular_method_name(&segment.name));
    let write_type = format_ident!("{}{}LayoutWrite", format_name, segment.name);
    let field_type = format_ident!("{}{}LayoutFields", format_name, segment.name);
    let footer_field_type = format_ident!("{}{}LayoutFooterFields", format_name, segment.name);
    let info_type = format_ident!("{}{}LayoutInfo", format_name, segment.name);
    let segment_name = segment.name.to_string();
    quote! {
        pub fn #method(
            &mut self,
            segment: #write_type<'_>,
        ) -> ::varve::__core::Result<#info_type> {
            let fields = segment.fields.__varve_layout_values();
            let footer_fields = segment.footer_fields.__varve_layout_values();
            let info = self.inner.write_segment(::varve::__core::SegmentWrite {
                name: #segment_name,
                fields: &fields,
                footer_fields: &footer_fields,
                metadata: segment.metadata,
                raw: segment.raw,
            })?;
            ::core::result::Result::Ok(#info_type::from_inner(info))
        }

        pub fn #streamed_method<M, R>(
            &mut self,
            fields: #field_type,
            footer_fields: #footer_field_type,
            write_metadata: M,
            write_raw: R,
        ) -> ::varve::__core::Result<#info_type>
        where
            M: FnOnce(&mut dyn ::std::io::Write) -> ::varve::__core::Result<()>,
            R: FnOnce(&mut dyn ::std::io::Write) -> ::varve::__core::Result<()>,
        {
            let fields = fields.__varve_layout_values();
            let footer_fields = footer_fields.__varve_layout_values();
            let info = self.inner.write_segment_streamed(::varve::__core::SegmentWriteStream {
                name: #segment_name,
                fields: &fields,
                footer_fields: &footer_fields,
                write_metadata,
                write_raw,
            })?;
            ::core::result::Result::Ok(#info_type::from_inner(info))
        }
    }
}

fn layout_reader_segment_methods_tokens(
    format_name: &Ident,
    segment: &LayoutSegment,
) -> TokenStream2 {
    let plural = plural_method_ident(&segment.name);
    let singular = format_ident!("{}", singular_method_name(&segment.name));
    let read_metadata = format_ident!("read_{}_metadata", singular_method_name(&segment.name));
    let read_metadata_range = format_ident!(
        "read_{}_metadata_range",
        singular_method_name(&segment.name)
    );
    let read_raw = format_ident!("read_{}_raw", singular_method_name(&segment.name));
    let read_raw_range = format_ident!("read_{}_raw_range", singular_method_name(&segment.name));
    let info_type = format_ident!("{}{}LayoutInfo", format_name, segment.name);
    let segment_name = segment.name.to_string();
    quote! {
        pub fn #plural(&self) -> ::varve::__core::Result<::std::vec::Vec<#info_type>> {
            self.inner
                .segments()
                .iter()
                .filter(|segment| segment.name == #segment_name)
                .cloned()
                .map(#info_type::from_inner)
                .map(::core::result::Result::Ok)
                .collect()
        }

        pub fn #singular(&self, index: usize) -> ::varve::__core::Result<::core::option::Option<#info_type>> {
            let ::core::result::Result::Ok(index) = self.__varve_layout_segment_index(#segment_name, index) else {
                return ::core::result::Result::Ok(::core::option::Option::None);
            };
            let ::core::option::Option::Some(segment) = self.inner.segments().get(index) else {
                return ::core::result::Result::Ok(::core::option::Option::None);
            };
            ::core::result::Result::Ok(::core::option::Option::Some(#info_type::from_inner(segment.clone())))
        }

        pub fn #read_metadata(&self, index: usize) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
            let index = self.__varve_layout_segment_index(#segment_name, index)?;
            self.inner.read_metadata(index)
        }

        pub fn #read_metadata_range(
            &self,
            index: usize,
            offset: u64,
            len: u64,
        ) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
            let index = self.__varve_layout_segment_index(#segment_name, index)?;
            self.inner.read_metadata_range(index, offset, len)
        }

        pub fn #read_raw(&self, index: usize) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
            let index = self.__varve_layout_segment_index(#segment_name, index)?;
            self.inner.read_raw(index)
        }

        pub fn #read_raw_range(
            &self,
            index: usize,
            offset: u64,
            len: u64,
        ) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
            let index = self.__varve_layout_segment_index(#segment_name, index)?;
            self.inner.read_raw_range(index, offset, len)
        }
    }
}

fn layout_field_struct_tokens(name: &Ident, fields: &[LayoutField]) -> TokenStream2 {
    let caller_fields: Vec<_> = fields
        .iter()
        .filter(|field| matches!(&field.source, LayoutFieldSourceChoice::Caller))
        .collect();
    if caller_fields.is_empty() {
        return quote! {
            #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
            pub struct #name;

            impl #name {
                fn __varve_layout_values(&self) -> ::std::vec::Vec<::varve::__core::LayoutFieldValue> {
                    ::std::vec::Vec::new()
                }
            }
        };
    }

    let struct_fields = caller_fields.iter().map(|field| {
        let field_name = &field.name;
        let ty = layout_field_rust_type_tokens(&field.ty);
        quote!(pub #field_name: #ty,)
    });
    let value_entries = caller_fields.iter().map(|field| {
        let field_name = &field.name;
        let field_name_string = field.name.to_string();
        let value = layout_field_to_value_tokens(field, quote!(self.#field_name));
        quote! {
            ::varve::__core::LayoutFieldValue {
                name: #field_name_string,
                value: #value,
            }
        }
    });

    quote! {
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        pub struct #name {
            #(#struct_fields)*
        }

        impl #name {
            fn __varve_layout_values(&self) -> ::std::vec::Vec<::varve::__core::LayoutFieldValue> {
                vec![
                    #(#value_entries,)*
                ]
            }
        }
    }
}

fn layout_info_field_getter(field: &LayoutField) -> TokenStream2 {
    let method = &field.name;
    let field_name = field.name.to_string();
    let ty = layout_field_rust_type_tokens(&field.ty);
    let conversion = layout_value_conversion_tokens(&field.ty, quote!(value), &field_name);
    quote! {
        pub fn #method(&self) -> ::varve::__core::Result<#ty> {
            let value = self
                .inner
                .field(#field_name)
                .ok_or(::varve::__core::Error::LayoutFieldMissing(#field_name))?;
            #conversion
        }
    }
}

fn layout_file_header_info_field_getter(field: &LayoutField) -> TokenStream2 {
    let method = &field.name;
    let field_name = field.name.to_string();
    let ty = layout_field_rust_type_tokens(&field.ty);
    let conversion = layout_value_conversion_tokens(&field.ty, quote!(value), &field_name);
    quote! {
        pub fn #method(&self) -> ::varve::__core::Result<#ty> {
            let value = self
                .field(#field_name)
                .ok_or(::varve::__core::Error::LayoutFieldMissing(#field_name))?;
            #conversion
        }
    }
}

fn layout_info_footer_field_getter(field: &LayoutField) -> TokenStream2 {
    let method = format_ident!("footer_{}", field.name);
    let field_name = field.name.to_string();
    let ty = layout_field_rust_type_tokens(&field.ty);
    let conversion = layout_value_conversion_tokens(&field.ty, quote!(value), &field_name);
    quote! {
        pub fn #method(&self) -> ::varve::__core::Result<#ty> {
            let value = self
                .inner
                .footer_field(#field_name)
                .ok_or(::varve::__core::Error::LayoutFieldMissing(#field_name))?;
            #conversion
        }
    }
}

fn layout_field_rust_type_tokens(ty: &LayoutFieldTypeChoice) -> TokenStream2 {
    match ty {
        LayoutFieldTypeChoice::Bytes(_) => quote!(::std::vec::Vec<u8>),
        LayoutFieldTypeChoice::U8 => quote!(u8),
        LayoutFieldTypeChoice::U16 => quote!(u16),
        LayoutFieldTypeChoice::U32 => quote!(u32),
        LayoutFieldTypeChoice::U64 => quote!(u64),
        LayoutFieldTypeChoice::I64 => quote!(i64),
    }
}

fn layout_field_to_value_tokens(field: &LayoutField, access: TokenStream2) -> TokenStream2 {
    match field.ty {
        LayoutFieldTypeChoice::Bytes(_) => {
            quote!(::varve::__core::LayoutValue::Bytes(#access.clone()))
        }
        LayoutFieldTypeChoice::U8 => quote!(::varve::__core::LayoutValue::U8(#access)),
        LayoutFieldTypeChoice::U16 => quote!(::varve::__core::LayoutValue::U16(#access)),
        LayoutFieldTypeChoice::U32 => quote!(::varve::__core::LayoutValue::U32(#access)),
        LayoutFieldTypeChoice::U64 => quote!(::varve::__core::LayoutValue::U64(#access)),
        LayoutFieldTypeChoice::I64 => quote!(::varve::__core::LayoutValue::I64(#access)),
    }
}

fn layout_value_conversion_tokens(
    ty: &LayoutFieldTypeChoice,
    value: TokenStream2,
    field_name: &str,
) -> TokenStream2 {
    match ty {
        LayoutFieldTypeChoice::Bytes(_) => quote!(#value.to_bytes(#field_name)),
        LayoutFieldTypeChoice::U8 => quote!(#value.to_u8(#field_name)),
        LayoutFieldTypeChoice::U16 => quote!(#value.to_u16(#field_name)),
        LayoutFieldTypeChoice::U32 => quote!(#value.to_u32(#field_name)),
        LayoutFieldTypeChoice::U64 => quote!(#value.to_u64(#field_name)),
        LayoutFieldTypeChoice::I64 => quote!(#value.to_i64(#field_name)),
    }
}

fn high_cardinality_api_tokens(
    format_name: &Ident,
    blocks: &[InlineBlock],
    keyed_offset_chain: bool,
) -> (TokenStream2, TokenStream2) {
    let stream_reader_name = format_ident!("{}StreamReader", format_name);
    let stream_writer_name = format_ident!("{}StreamWriter", format_name);
    let indexed_reader_name = format_ident!("{}IndexedReader", format_name);
    let indexed_writer_name = format_ident!("{}IndexedWriter", format_name);
    let append_blocks: Vec<_> = blocks
        .iter()
        .filter(|block| !matches!(&block.kind, InlineBlockKind::Matrix(_)))
        .collect();
    let mut disk_blocks: Vec<_> = append_blocks
        .iter()
        .copied()
        .filter(|block| block.key_index == KeyIndexChoice::Disk)
        .collect();
    disk_blocks.sort_by_key(|block| block.id);

    let stream_reader_methods = append_blocks.iter().map(|block| {
        let ty = &block.name;
        let plural = plural_method_ident(ty);
        quote! {
            pub fn #plural(
                &self,
            ) -> ::varve::__core::Result<::varve::__core::StreamingBlocks<#ty>> {
                self.inner.blocks::<#ty>()
            }
        }
    });
    let stream_writer_methods = append_blocks.iter().flat_map(|block| {
        if keyed_offset_chain && !block.key_fields.is_empty() {
            return Vec::new();
        }
        let ty = &block.name;
        let push = format_ident!("push_{}", singular_method_name(ty));
        let push_many = format_ident!("push_{}", plural_method_name(ty));
        let mut methods = vec![
            quote! {
                pub fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    self.inner.push_info(value)
                }
            },
            quote! {
                pub fn #push_many<I>(
                    &mut self,
                    values: I,
                    options: ::varve::__core::BatchOptions,
                ) -> ::core::result::Result<
                    ::varve::__core::BatchAppendInfo,
                    ::varve::__core::BatchAppendError,
                >
                where
                    I: ::core::iter::IntoIterator,
                    I::Item: ::core::borrow::Borrow<#ty>,
                {
                    self.inner.push_iter::<#ty, _>(values, options)
                }
            },
        ];
        if !block.key_fields.is_empty() {
            let delete = format_ident!("delete_{}", singular_method_name(ty));
            methods.push(quote! {
                pub fn #delete(
                    &mut self,
                    key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    self.inner.delete_with_prev_key_info::<#ty>(
                        key,
                        ::core::option::Option::None,
                    )
                }
            });
        }
        methods
    });

    let stream_constructors = quote! {
        pub fn open_stream_reader<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::StreamOptions,
        ) -> ::varve::__core::Result<#stream_reader_name> {
            ::core::result::Result::Ok(#stream_reader_name::from_inner(
                ::varve::__core::VarveStreamReader::open(Self::spec(), path, options)?,
            ))
        }

        pub fn create_stream_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::StreamOptions,
        ) -> ::varve::__core::Result<#stream_writer_name> {
            ::core::result::Result::Ok(#stream_writer_name::from_inner(
                ::varve::__core::VarveStreamWriter::create(Self::spec(), path, options)?,
            ))
        }

        pub fn open_stream_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::StreamOptions,
        ) -> ::varve::__core::Result<#stream_writer_name> {
            ::core::result::Result::Ok(#stream_writer_name::from_inner(
                ::varve::__core::VarveStreamWriter::open(Self::spec(), path, options)?,
            ))
        }

        pub fn restore_stream_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::StreamOptions,
        ) -> ::varve::__core::Result<#stream_writer_name> {
            ::core::result::Result::Ok(#stream_writer_name::from_inner(
                ::varve::__core::VarveStreamWriter::restore_checkpoint_and_open(
                    Self::spec(),
                    path,
                    options,
                )?,
            ))
        }

        pub fn bootstrap_stream_checkpoint<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::StreamOptions,
        ) -> ::varve::__core::Result<::varve::__core::StreamBootstrapReport> {
            ::varve::__core::bootstrap_stream_checkpoint(Self::spec(), path, options)
        }

        pub fn bootstrap_stream_checkpoint_with_progress<P, F>(
            path: P,
            options: ::varve::__core::StreamOptions,
            scan: ::varve::__core::ScanOptions<'_>,
            observer: F,
        ) -> ::varve::__core::Result<::varve::__core::StreamBootstrapReport>
        where
            P: AsRef<::std::path::Path>,
            F: ::core::ops::FnMut(::varve::__core::ScanProgress),
        {
            ::varve::__core::bootstrap_stream_checkpoint_with_progress(
                Self::spec(), path, options, scan, observer,
            )
        }
    };
    let stream_api = quote! {
        pub struct #stream_reader_name {
            inner: ::varve::__core::VarveStreamReader,
        }

        impl #stream_reader_name {
            pub fn from_inner(inner: ::varve::__core::VarveStreamReader) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::varve::__core::VarveStreamReader {
                self.inner
            }

            pub fn events(&self) -> ::varve::__core::Result<::varve::__core::StreamEvents> {
                self.inner.events()
            }

            pub fn verify_all(&self) -> ::varve::__core::Result<u64> {
                self.inner.verify_all()
            }

            pub fn verify_all_with_progress<F>(
                &self,
                scan: ::varve::__core::ScanOptions<'_>,
                observer: F,
            ) -> ::varve::__core::Result<u64>
            where
                F: ::core::ops::FnMut(::varve::__core::ScanProgress),
            {
                self.inner.verify_all_with_progress(scan, observer)
            }

            pub fn resident_state(&self) -> ::varve::__core::StreamResidentState {
                self.inner.resident_state()
            }

            #(#stream_reader_methods)*
        }

        pub struct #stream_writer_name {
            inner: ::varve::__core::VarveStreamWriter,
        }

        impl #stream_writer_name {
            pub fn from_inner(inner: ::varve::__core::VarveStreamWriter) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::varve::__core::VarveStreamWriter {
                self.inner
            }

            pub fn flush(&mut self) -> ::varve::__core::Result<()> {
                self.inner.flush()
            }

            pub fn sync(&mut self) -> ::varve::__core::Result<()> {
                self.inner.sync()
            }

            pub fn resident_state(&self) -> ::varve::__core::StreamResidentState {
                self.inner.resident_state()
            }

            #(#stream_writer_methods)*
        }
    };

    if disk_blocks.is_empty() {
        return (stream_constructors, stream_api);
    }

    let indexed_reader_methods = disk_blocks.iter().map(|block| {
        let ty = &block.name;
        let get = format_ident!("get_{}", singular_method_name(ty));
        quote! {
            pub fn #get(
                &self,
                key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
            ) -> ::varve::__core::Result<::core::option::Option<#ty>> {
                self.inner.get::<#ty>(key)
            }
        }
    });
    let indexed_scan_methods = append_blocks.iter().map(|block| {
        let ty = &block.name;
        let plural = plural_method_ident(ty);
        quote! {
            pub fn #plural(
                &self,
            ) -> ::varve::__core::Result<::varve::__core::StreamingBlocks<#ty>> {
                self.inner.blocks::<#ty>()
            }
        }
    });
    let indexed_writer_methods = append_blocks.iter().flat_map(|block| {
        if keyed_offset_chain
            && !block.key_fields.is_empty()
            && block.key_index != KeyIndexChoice::Disk
        {
            return Vec::new();
        }
        let ty = &block.name;
        let push = format_ident!("push_{}", singular_method_name(ty));
        let push_many = format_ident!("push_{}", plural_method_name(ty));
        let mut methods = if block.key_index == KeyIndexChoice::Disk {
            vec![
                quote! {
                    pub fn #push(
                        &mut self,
                        value: &#ty,
                    ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                        self.inner.push_info(value)
                    }
                },
                quote! {
                    pub fn #push_many<I>(
                        &mut self,
                        values: I,
                        options: ::varve::__core::BatchOptions,
                    ) -> ::core::result::Result<
                        ::varve::__core::BatchAppendInfo,
                        ::varve::__core::BatchAppendError,
                    >
                    where
                        I: ::core::iter::IntoIterator,
                        I::Item: ::core::borrow::Borrow<#ty>,
                    {
                        self.inner.push_iter::<#ty, _>(values, options)
                    }
                },
            ]
        } else {
            vec![
                quote! {
                    pub fn #push(
                        &mut self,
                        value: &#ty,
                    ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                        self.inner.push_unindexed_info(value)
                    }
                },
                quote! {
                    pub fn #push_many<I>(
                        &mut self,
                        values: I,
                        options: ::varve::__core::BatchOptions,
                    ) -> ::core::result::Result<
                        ::varve::__core::BatchAppendInfo,
                        ::varve::__core::BatchAppendError,
                    >
                    where
                        I: ::core::iter::IntoIterator,
                        I::Item: ::core::borrow::Borrow<#ty>,
                    {
                        self.inner
                            .push_unindexed_iter::<#ty, _>(values, options)
                    }
                },
            ]
        };
        if !block.key_fields.is_empty() {
            let delete = format_ident!("delete_{}", singular_method_name(ty));
            methods.push(if block.key_index == KeyIndexChoice::Disk {
                quote! {
                    pub fn #delete(
                        &mut self,
                        key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
                    ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                        self.inner.delete_info::<#ty>(key)
                    }
                }
            } else {
                quote! {
                pub fn #delete(
                    &mut self,
                    key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    self.inner.delete_unindexed_info::<#ty>(key)
                }
                }
            });
        }
        methods
    });
    let indexed_block_descriptors = disk_blocks.iter().map(|block| {
        let ty = &block.name;
        quote!(::varve::__core::DiskIndexedBlock::of::<#ty>())
    });
    let indexed_block_descriptors: Vec<_> = indexed_block_descriptors.collect();
    let indexed_constructors = quote! {
        pub fn disk_index_plan() -> ::varve::__core::Result<::varve::__core::DiskIndexPlan> {
            const BLOCKS: &[::varve::__core::DiskIndexedBlock] = &[
                #(#indexed_block_descriptors),*
            ];
            ::varve::__core::DiskIndexPlan::canonical(Self::spec(), BLOCKS)
                .map_err(|error| ::varve::__core::Error::DiskIndex(::std::boxed::Box::new(error)))
        }

        pub fn open_indexed_reader<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::DiskIndexOptions,
        ) -> ::varve::__core::Result<#indexed_reader_name> {
            ::core::result::Result::Ok(#indexed_reader_name::from_inner(
                ::varve::__core::VarveIndexedReader::open(
                    Self::spec(),
                    path,
                    options,
                    Self::disk_index_plan()?,
                )?,
            ))
        }

        pub fn create_indexed_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::DiskIndexOptions,
        ) -> ::varve::__core::Result<#indexed_writer_name> {
            ::core::result::Result::Ok(#indexed_writer_name::from_inner(
                ::varve::__core::VarveIndexedWriter::create(
                    Self::spec(),
                    path,
                    options,
                    Self::disk_index_plan()?,
                )?,
            ))
        }

        pub fn open_indexed_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::DiskIndexOptions,
        ) -> ::varve::__core::Result<#indexed_writer_name> {
            ::core::result::Result::Ok(#indexed_writer_name::from_inner(
                ::varve::__core::VarveIndexedWriter::open(
                    Self::spec(),
                    path,
                    options,
                    Self::disk_index_plan()?,
                )?,
            ))
        }


        pub fn restore_indexed_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::DiskIndexOptions,
        ) -> ::varve::__core::Result<#indexed_writer_name> {
            ::core::result::Result::Ok(#indexed_writer_name::from_inner(
                ::varve::__core::VarveIndexedWriter::restore_checkpoint_and_open(
                    Self::spec(),
                    path,
                    options,
                    Self::disk_index_plan()?,
                )?,
            ))
        }

        pub fn rebuild_disk_index<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::varve::__core::DiskIndexOptions,
        ) -> ::varve::__core::Result<::varve::__core::DiskIndexRebuildReport> {
            ::varve::__core::rebuild_disk_index(
                Self::spec(),
                path,
                options,
                Self::disk_index_plan()?,
            )
        }

        pub fn rebuild_disk_index_with_progress<P, F>(
            path: P,
            options: ::varve::__core::DiskIndexOptions,
            scan: ::varve::__core::ScanOptions<'_>,
            observer: F,
        ) -> ::varve::__core::Result<::varve::__core::DiskIndexRebuildReport>
        where
            P: AsRef<::std::path::Path>,
            F: ::core::ops::FnMut(::varve::__core::ScanProgress),
        {
            ::varve::__core::rebuild_disk_index_with_progress(
                Self::spec(),
                path,
                options,
                Self::disk_index_plan()?,
                scan,
                observer,
            )
        }
    };
    let indexed_api = quote! {
        pub struct #indexed_reader_name {
            inner: ::varve::__core::VarveIndexedReader,
        }

        impl #indexed_reader_name {
            pub fn from_inner(inner: ::varve::__core::VarveIndexedReader) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::varve::__core::VarveIndexedReader {
                self.inner
            }

            pub fn resident_state(&self) -> ::varve::__core::StreamResidentState {
                self.inner.resident_state()
            }

            pub fn events(&self) -> ::varve::__core::Result<::varve::__core::StreamEvents> {
                self.inner.events()
            }

            pub fn verify_all(&self) -> ::varve::__core::Result<u64> {
                self.inner.verify_all()
            }

            pub fn verify_all_with_progress<F>(
                &self,
                scan: ::varve::__core::ScanOptions<'_>,
                observer: F,
            ) -> ::varve::__core::Result<u64>
            where
                F: ::core::ops::FnMut(::varve::__core::ScanProgress),
            {
                self.inner.verify_all_with_progress(scan, observer)
            }

            #(#indexed_scan_methods)*
            #(#indexed_reader_methods)*
        }

        pub struct #indexed_writer_name {
            inner: ::varve::__core::VarveIndexedWriter,
        }

        impl #indexed_writer_name {
            pub fn from_inner(inner: ::varve::__core::VarveIndexedWriter) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::varve::__core::VarveIndexedWriter {
                self.inner
            }

            pub fn flush(&mut self) -> ::varve::__core::Result<()> {
                self.inner.flush()
            }

            pub fn sync(&mut self) -> ::varve::__core::Result<()> {
                self.inner.sync()
            }

            pub fn resident_state(&self) -> ::varve::__core::StreamResidentState {
                self.inner.resident_state()
            }

            #(#indexed_writer_methods)*
        }
    };

    (
        quote!(#stream_constructors #indexed_constructors),
        quote!(#stream_api #indexed_api),
    )
}

/// Rustdoc for a generated typed writer (P-01).
///
/// The resident writer is the convenient default, and for a format whose
/// distinct key count is large it is the wrong tool. The published pages are
/// where a caller decides that, so the cost model and the alternative are
/// stated on the type itself rather than only in `docs/known-limitations.md`
/// (§2.2) and `docs/api-reference.md`.
fn generated_writer_doc(format_name: &Ident, keyed_blocks: &[&InlineBlock]) -> String {
    if keyed_blocks.is_empty() {
        return format!(
            "Typed resident writer for the `{format_name}` format.\n\
             \n\
             This format declares no keyed block, so construction primes no \
             keyed-tail cache. The writer holds the whole record index in \
             memory, which is the resident (non-petabyte) path: for a file \
             whose record count is chosen by an untrusted producer, bound it \
             with `ReadLimits` or use the streaming/indexed APIs."
        );
    }
    let names: Vec<String> = keyed_blocks
        .iter()
        .map(|block| format!("`{}`", block.name))
        .collect();
    let count = keyed_blocks.len();
    format!(
        "Typed resident writer for the `{format_name}` format.\n\
         \n\
         # Construction cost and cache size\n\
         \n\
         Constructing this writer primes one resident keyed-tail map per keyed \
         block type. This format declares {count} ({names}), and each priming \
         pass walks the resident record index, so construction costs \
         `Theta(M*N)` for `M` keyed block types and `N` resident index \
         entries.\n\
         \n\
         The retained cache is bounded per block id, not per file: \
         `ReadLimits::max_keyed_tail_bytes` is checked against each block's own \
         map, so the aggregate a single writer can retain is about `M` times \
         that ceiling. Both facts are the documented behaviour of the resident \
         path, not a limit violation.\n\
         \n\
         # High-cardinality formats should not use this writer\n\
         \n\
         If the number of distinct keys is large, unbounded, or chosen by an \
         untrusted producer, declare `key_index = disk` on the keyed blocks and \
         use the generated disk-indexed API (`{format_name}IndexedWriter` \
         through `create_indexed_writer` / `open_indexed_writer`, and \
         `{format_name}IndexedReader` through `open_indexed_reader`) instead. \
         Those keep key state in the redb sidecar rather than in this process, \
         they perform no per-construction index walk, and they are the APIs the \
         petabyte-scale contract covers. The resident writer is explicitly not \
         that path.",
        names = names.join(", "),
    )
}

/// Rustdoc for the generated writer's `from_inner` (P-01).
fn generated_writer_construction_doc(keyed_blocks: &[&InlineBlock]) -> String {
    if keyed_blocks.is_empty() {
        return "Wraps an open `VarveWriter`. This format declares no keyed \
                block, so nothing is primed and this cannot fail on cache \
                budget."
            .to_string();
    }
    format!(
        "Wraps an open `VarveWriter`, priming the resident keyed-tail map of \
         each of this format's {count} keyed block types.\n\
         \n\
         The priming is fallible and happens **before** any mutation: a file \
         whose distinct key count exceeds `ReadLimits::max_keyed_tail_bytes` \
         for one of those blocks is refused here, not at a later append. Each \
         pass walks the resident record index, so this is the `Theta(M*N)` step \
         described on the type; a format with high key cardinality should use \
         the disk-indexed API instead.",
        count = keyed_blocks.len(),
    )
}

fn typed_api_tokens(
    format_name: &Ident,
    blocks: &[InlineBlock],
    matrix_commit: Option<&MatrixCommit>,
    matrix_aux: &[MatrixAux],
) -> TokenStream2 {
    let reader_name = format_ident!("{}Reader", format_name);
    let writer_name = format_ident!("{}Writer", format_name);
    let read_trait = format_ident!("{}Read", format_name);
    let write_trait = format_ident!("{}Write", format_name);

    let append_blocks: Vec<_> = blocks
        .iter()
        .filter(|block| !matches!(&block.kind, InlineBlockKind::Matrix(_)))
        .collect();
    let matrix_blocks: Vec<_> = blocks
        .iter()
        .filter(|block| matches!(&block.kind, InlineBlockKind::Matrix(_)))
        .collect();

    let keyed_blocks: Vec<_> = append_blocks
        .iter()
        .filter(|block| !block.key_fields.is_empty())
        .copied()
        .collect();
    // F-01: the writer owns no keyed tail state of its own. Every keyed
    // mutation routes through the byte-keyed resident cache inside
    // `VarveFile`, so the only step after an authoritative append is an
    // insert into a `HashMap<Vec<u8>, u64>` slot reserved before it - no
    // user-defined `Hash`, `Eq` or `Clone` can run after publication, and no
    // per-writer map has to be translated after a record replacement moves
    // offsets (the file invalidates its own cache there).
    //
    // The map is still *built* at writer construction, which is where the
    // keyed-tail budget has always been enforced for generated writers: a file
    // whose distinct key count exceeds `keyed_tail` is refused by
    // `open_writer`, not by a later append.
    let writer_tail_primes = keyed_blocks.iter().map(|block| {
        let ty = &block.name;
        quote!(inner.prime_keyed_tails::<#ty>()?;)
    });
    // A format with no keyed block primes nothing, so the binding must not be
    // `mut` there.
    let writer_inner_binding = if keyed_blocks.is_empty() {
        quote!(inner)
    } else {
        quote!(mut inner)
    };
    // P-01, documentation only. Construction primes one keyed-tail map per
    // keyed block type and each priming pass walks the whole resident index,
    // so the cost is Theta(M*N) and the retained cache can reach roughly
    // M * `keyed_tail` because that ceiling is per block id, not per file.
    // That is the documented behaviour of the resident (non-PB) path rather
    // than a contract violation, so the generated pages say it out loud and
    // name the disk-indexed API a high-cardinality format should use instead.
    let writer_doc = generated_writer_doc(format_name, &keyed_blocks);
    let writer_construction_doc = generated_writer_construction_doc(&keyed_blocks);

    let reader_inherent_methods = append_blocks.iter().flat_map(|block| reader_methods(block));
    let reader_trait_methods = append_blocks
        .iter()
        .flat_map(|block| reader_trait_methods(block));
    let reader_trait_impl_methods = append_blocks
        .iter()
        .flat_map(|block| reader_trait_impl_methods(block));
    let writer_inherent_methods = append_blocks.iter().flat_map(|block| writer_methods(block));
    let writer_trait_methods = append_blocks
        .iter()
        .flat_map(|block| writer_trait_methods(block));
    let writer_trait_impl_methods = append_blocks
        .iter()
        .flat_map(|block| writer_trait_impl_methods(block));

    let matrix_reader_inherent_methods =
        matrix_reader_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::Inherent);
    let matrix_reader_trait_methods =
        matrix_reader_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::Trait);
    let matrix_reader_trait_impl_methods =
        matrix_reader_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::TraitImpl);
    let matrix_writer_inherent_methods =
        matrix_writer_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::Inherent);
    let matrix_writer_trait_methods =
        matrix_writer_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::Trait);
    let matrix_writer_trait_impl_methods =
        matrix_writer_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::TraitImpl);
    let matrix_reader_aux_inherent_methods =
        matrix_reader_aux_methods(matrix_aux, MatrixMethodTarget::Inherent);
    let matrix_reader_aux_trait_methods =
        matrix_reader_aux_methods(matrix_aux, MatrixMethodTarget::Trait);
    let matrix_reader_aux_trait_impl_methods =
        matrix_reader_aux_methods(matrix_aux, MatrixMethodTarget::TraitImpl);
    let matrix_writer_aux_inherent_methods =
        matrix_writer_aux_methods(matrix_aux, MatrixMethodTarget::Inherent);
    let matrix_writer_aux_trait_methods =
        matrix_writer_aux_methods(matrix_aux, MatrixMethodTarget::Trait);
    let matrix_writer_aux_trait_impl_methods =
        matrix_writer_aux_methods(matrix_aux, MatrixMethodTarget::TraitImpl);

    quote! {
        pub trait #read_trait {
            #(#reader_trait_methods)*
            #matrix_reader_trait_methods
            #matrix_reader_aux_trait_methods
        }

        pub trait #write_trait {
            #(#writer_trait_methods)*
            #matrix_writer_trait_methods
            #matrix_writer_aux_trait_methods

            fn commit(&mut self) -> ::varve::__core::Result<::varve::__core::AppendInfo>;
            fn commit_durable(&mut self) -> ::varve::__core::Result<::varve::__core::AppendInfo>;
            fn flush(&mut self) -> ::varve::__core::Result<()>;
            fn sync(&mut self) -> ::varve::__core::Result<()>;
        }

        #[derive(Debug)]
        pub struct #reader_name {
            inner: ::varve::__core::VarveReader,
        }

        impl #reader_name {
            pub fn from_inner(inner: ::varve::__core::VarveReader) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::varve::__core::VarveReader {
                self.inner
            }

            pub fn spec(&self) -> ::varve::__core::FormatSpec {
                self.inner.spec()
            }

            pub fn path(&self) -> &::std::path::Path {
                self.inner.path()
            }

            #(#reader_inherent_methods)*
            #matrix_reader_inherent_methods
            #matrix_reader_aux_inherent_methods
        }

        impl #read_trait for #reader_name {
            #(#reader_trait_impl_methods)*
            #matrix_reader_trait_impl_methods
            #matrix_reader_aux_trait_impl_methods
        }

        #[doc = #writer_doc]
        #[derive(Debug)]
        pub struct #writer_name {
            inner: ::varve::__core::VarveWriter,
        }

        impl #writer_name {
            #[doc = #writer_construction_doc]
            pub fn from_inner(
                #writer_inner_binding: ::varve::__core::VarveWriter,
            ) -> ::varve::__core::Result<Self> {
                #(#writer_tail_primes)*
                ::core::result::Result::Ok(Self { inner })
            }

            pub fn into_inner(self) -> ::varve::__core::VarveWriter {
                self.inner
            }

            pub fn spec(&self) -> ::varve::__core::FormatSpec {
                self.inner.spec()
            }

            pub fn path(&self) -> &::std::path::Path {
                self.inner.path()
            }

            pub fn commit(&mut self) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                self.inner.commit()
            }

            pub fn commit_durable(&mut self) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                self.inner.commit_durable()
            }

            pub fn flush(&mut self) -> ::varve::__core::Result<()> {
                self.inner.flush()
            }

            pub fn sync(&mut self) -> ::varve::__core::Result<()> {
                self.inner.sync()
            }

            #(#writer_inherent_methods)*
            #matrix_writer_inherent_methods
            #matrix_writer_aux_inherent_methods
        }

        impl #write_trait for #writer_name {
            #(#writer_trait_impl_methods)*
            #matrix_writer_trait_impl_methods
            #matrix_writer_aux_trait_impl_methods

            fn commit(&mut self) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                self.commit()
            }

            fn commit_durable(&mut self) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                self.commit_durable()
            }

            fn flush(&mut self) -> ::varve::__core::Result<()> {
                self.flush()
            }

            fn sync(&mut self) -> ::varve::__core::Result<()> {
                self.sync()
            }
        }
    }
}

#[derive(Clone, Copy)]
enum MatrixMethodTarget {
    Inherent,
    Trait,
    TraitImpl,
}

fn matrix_reader_aux_methods(aux: &[MatrixAux], target: MatrixMethodTarget) -> TokenStream2 {
    let methods = aux.iter().map(|aux| {
        let name = aux.name.to_string();
        let len_method = format_ident!("{}_aux_len", aux.name);
        let read_method = format_ident!("read_{}_aux", aux.name);
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #len_method(&self) -> ::varve::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                pub fn #read_method(
                    &mut self,
                    offset: u64,
                    len: u64,
                ) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #len_method(&self) -> ::varve::__core::Result<u64>;

                fn #read_method(
                    &mut self,
                    offset: u64,
                    len: u64,
                ) -> ::varve::__core::Result<::std::vec::Vec<u8>>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #len_method(&self) -> ::varve::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                fn #read_method(
                    &mut self,
                    offset: u64,
                    len: u64,
                ) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }
            },
        }
    });
    quote! {
        #(#methods)*
    }
}

fn matrix_writer_aux_methods(aux: &[MatrixAux], target: MatrixMethodTarget) -> TokenStream2 {
    let methods = aux.iter().map(|aux| {
        let name = aux.name.to_string();
        let len_method = format_ident!("{}_aux_len", aux.name);
        let read_method = format_ident!("read_{}_aux", aux.name);
        let write_method = format_ident!("write_{}_aux", aux.name);
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #len_method(&self) -> ::varve::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                pub fn #read_method(
                    &mut self,
                    offset: u64,
                    len: u64,
                ) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }

                pub fn #write_method(
                    &mut self,
                    offset: u64,
                    payload: &[u8],
                ) -> ::varve::__core::Result<()> {
                    self.inner.write_matrix_aux(#name, offset, payload)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #len_method(&self) -> ::varve::__core::Result<u64>;

                fn #read_method(
                    &mut self,
                    offset: u64,
                    len: u64,
                ) -> ::varve::__core::Result<::std::vec::Vec<u8>>;

                fn #write_method(
                    &mut self,
                    offset: u64,
                    payload: &[u8],
                ) -> ::varve::__core::Result<()>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #len_method(&self) -> ::varve::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                fn #read_method(
                    &mut self,
                    offset: u64,
                    len: u64,
                ) -> ::varve::__core::Result<::std::vec::Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }

                fn #write_method(
                    &mut self,
                    offset: u64,
                    payload: &[u8],
                ) -> ::varve::__core::Result<()> {
                    self.inner.write_matrix_aux(#name, offset, payload)
                }
            },
        }
    });
    quote! {
        #(#methods)*
    }
}

fn matrix_reader_methods(
    blocks: &[&InlineBlock],
    commit: Option<&MatrixCommit>,
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let cell_methods = blocks.iter().map(|block| {
        let ty = &block.name;
        let key_ty = format_ident!("{}Key", ty);
        let method = format_ident!("{}", singular_method_name(ty));
        let status = format_ident!("{}_status", singular_method_name(ty));
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #method(&mut self, key: #key_ty) -> ::varve::__core::Result<#ty> {
                    self.inner.read_matrix_cell::<#ty>(key.into())
                }

                pub fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::varve::__core::Result<::varve::__core::MatrixCellStatus> {
                    self.inner.matrix_cell_status::<#ty>(key.into())
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #method(&mut self, key: #key_ty) -> ::varve::__core::Result<#ty>;

                fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::varve::__core::Result<::varve::__core::MatrixCellStatus>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #method(&mut self, key: #key_ty) -> ::varve::__core::Result<#ty> {
                    self.inner.read_matrix_cell::<#ty>(key.into())
                }

                fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::varve::__core::Result<::varve::__core::MatrixCellStatus> {
                    self.inner.matrix_cell_status::<#ty>(key.into())
                }
            },
        }
    });
    let flag_methods = matrix_reader_flag_methods(commit, target);
    quote! {
        #(#cell_methods)*
        #flag_methods
    }
}

fn matrix_writer_methods(
    blocks: &[&InlineBlock],
    commit: Option<&MatrixCommit>,
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let cell_methods = blocks.iter().map(|block| {
        let ty = &block.name;
        let key_ty = format_ident!("{}Key", ty);
        let name = singular_method_name(ty);
        let write = format_ident!("write_{}", name);
        let commit_method = format_ident!("commit_{}", name);
        let clear = format_ident!("clear_{}", name);
        let rebuild = format_ident!("rebuild_{}_commit_from_crc", name);
        let status = format_ident!("{}_status", name);
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #write(
                    &mut self,
                    key: #key_ty,
                    value: &#ty,
                ) -> ::varve::__core::Result<()> {
                    self.inner.write_matrix_cell::<#ty>(key.into(), value)
                }

                pub fn #commit_method(&mut self, key: #key_ty) -> ::varve::__core::Result<()> {
                    self.inner.commit_matrix_cell::<#ty>(key.into())
                }

                pub fn #clear(&mut self, key: #key_ty) -> ::varve::__core::Result<()> {
                    self.inner.clear_matrix_cell::<#ty>(key.into())
                }

                pub fn #rebuild(&mut self) -> ::varve::__core::Result<u64> {
                    self.inner.rebuild_matrix_commit_from_crc::<#ty>()
                }

                pub fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::varve::__core::Result<::varve::__core::MatrixCellStatus> {
                    self.inner.matrix_cell_status::<#ty>(key.into())
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #write(
                    &mut self,
                    key: #key_ty,
                    value: &#ty,
                ) -> ::varve::__core::Result<()>;

                fn #commit_method(&mut self, key: #key_ty) -> ::varve::__core::Result<()>;

                fn #clear(&mut self, key: #key_ty) -> ::varve::__core::Result<()>;

                fn #rebuild(&mut self) -> ::varve::__core::Result<u64>;

                fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::varve::__core::Result<::varve::__core::MatrixCellStatus>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #write(
                    &mut self,
                    key: #key_ty,
                    value: &#ty,
                ) -> ::varve::__core::Result<()> {
                    self.inner.write_matrix_cell::<#ty>(key.into(), value)
                }

                fn #commit_method(&mut self, key: #key_ty) -> ::varve::__core::Result<()> {
                    self.inner.commit_matrix_cell::<#ty>(key.into())
                }

                fn #clear(&mut self, key: #key_ty) -> ::varve::__core::Result<()> {
                    self.inner.clear_matrix_cell::<#ty>(key.into())
                }

                fn #rebuild(&mut self) -> ::varve::__core::Result<u64> {
                    self.inner.rebuild_matrix_commit_from_crc::<#ty>()
                }

                fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::varve::__core::Result<::varve::__core::MatrixCellStatus> {
                    self.inner.matrix_cell_status::<#ty>(key.into())
                }
            },
        }
    });
    let flag_methods = matrix_writer_flag_methods(commit, target);
    quote! {
        #(#cell_methods)*
        #flag_methods
    }
}

fn matrix_reader_flag_methods(
    commit: Option<&MatrixCommit>,
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let Some(commit) = commit else {
        return quote!();
    };
    let single_methods = commit.singles.iter().map(|flag| {
        let method = format_ident!("is_{}_committed", flag);
        let name = flag.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #method(&self) -> ::varve::__core::Result<bool> {
                    self.inner.is_matrix_single_committed(#name)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #method(&self) -> ::varve::__core::Result<bool>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #method(&self) -> ::varve::__core::Result<bool> {
                    self.inner.is_matrix_single_committed(#name)
                }
            },
        }
    });
    let channel_methods = commit.per_channel.iter().map(|flag| {
        let method = format_ident!("is_{}_committed", flag);
        let name = flag.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #method(&self, channel: u64) -> ::varve::__core::Result<bool> {
                    self.inner.is_matrix_channel_committed(#name, channel)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #method(&self, channel: u64) -> ::varve::__core::Result<bool>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #method(&self, channel: u64) -> ::varve::__core::Result<bool> {
                    self.inner.is_matrix_channel_committed(#name, channel)
                }
            },
        }
    });
    quote! {
        #(#single_methods)*
        #(#channel_methods)*
    }
}

fn matrix_writer_flag_methods(
    commit: Option<&MatrixCommit>,
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let Some(commit) = commit else {
        return quote!();
    };
    let category_methods = commit.categories.iter().map(|category| {
        let method = format_ident!("clear_{}_category", category);
        let name = category.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #method(&mut self) -> ::varve::__core::Result<u64> {
                    self.inner.clear_matrix_category(#name)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #method(&mut self) -> ::varve::__core::Result<u64>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #method(&mut self) -> ::varve::__core::Result<u64> {
                    self.inner.clear_matrix_category(#name)
                }
            },
        }
    });
    let single_methods = commit.singles.iter().map(|flag| {
        let get_method = format_ident!("is_{}_committed", flag);
        let set_method = format_ident!("set_{}_committed", flag);
        let name = flag.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #get_method(&self) -> ::varve::__core::Result<bool> {
                    self.inner.is_matrix_single_committed(#name)
                }

                pub fn #set_method(&mut self, value: bool) -> ::varve::__core::Result<()> {
                    self.inner.set_matrix_single_committed(#name, value)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #get_method(&self) -> ::varve::__core::Result<bool>;

                fn #set_method(&mut self, value: bool) -> ::varve::__core::Result<()>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #get_method(&self) -> ::varve::__core::Result<bool> {
                    self.inner.is_matrix_single_committed(#name)
                }

                fn #set_method(&mut self, value: bool) -> ::varve::__core::Result<()> {
                    self.inner.set_matrix_single_committed(#name, value)
                }
            },
        }
    });
    let channel_methods = commit.per_channel.iter().map(|flag| {
        let get_method = format_ident!("is_{}_committed", flag);
        let set_method = format_ident!("set_{}_committed", flag);
        let name = flag.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #get_method(&self, channel: u64) -> ::varve::__core::Result<bool> {
                    self.inner.is_matrix_channel_committed(#name, channel)
                }

                pub fn #set_method(
                    &mut self,
                    channel: u64,
                    value: bool,
                ) -> ::varve::__core::Result<()> {
                    self.inner.set_matrix_channel_committed(#name, channel, value)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #get_method(&self, channel: u64) -> ::varve::__core::Result<bool>;

                fn #set_method(
                    &mut self,
                    channel: u64,
                    value: bool,
                ) -> ::varve::__core::Result<()>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #get_method(&self, channel: u64) -> ::varve::__core::Result<bool> {
                    self.inner.is_matrix_channel_committed(#name, channel)
                }

                fn #set_method(
                    &mut self,
                    channel: u64,
                    value: bool,
                ) -> ::varve::__core::Result<()> {
                    self.inner.set_matrix_channel_committed(#name, channel, value)
                }
            },
        }
    });
    quote! {
        #(#category_methods)*
        #(#single_methods)*
        #(#channel_methods)*
    }
}

fn reader_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let plural = plural_method_ident(ty);
    if block.key_fields.is_empty() {
        vec![quote! {
            pub fn #plural(&self) -> ::varve::__core::Result<::varve::__core::BlockVec<#ty>> {
                self.inner.blocks::<#ty>()
            }
        }]
    } else {
        vec![quote! {
            pub fn #plural(
                &self,
            ) -> ::varve::__core::Result<
                ::varve::__core::KeyedBlockVec<
                    <#ty as ::varve::__core::VarveKeyedBlock>::Key,
                    #ty,
                >
            > {
                self.inner.keyed_blocks::<#ty>()
            }
        }]
    }
}

fn reader_trait_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let plural = plural_method_ident(ty);
    if block.key_fields.is_empty() {
        vec![quote! {
            fn #plural(&self) -> ::varve::__core::Result<::varve::__core::BlockVec<#ty>>;
        }]
    } else {
        vec![quote! {
            fn #plural(
                &self,
            ) -> ::varve::__core::Result<
                ::varve::__core::KeyedBlockVec<
                    <#ty as ::varve::__core::VarveKeyedBlock>::Key,
                    #ty,
                >
            >;
        }]
    }
}

fn reader_trait_impl_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let plural = plural_method_ident(ty);
    if block.key_fields.is_empty() {
        vec![quote! {
            fn #plural(&self) -> ::varve::__core::Result<::varve::__core::BlockVec<#ty>> {
                self.inner.blocks::<#ty>()
            }
        }]
    } else {
        vec![quote! {
            fn #plural(
                &self,
            ) -> ::varve::__core::Result<
                ::varve::__core::KeyedBlockVec<
                    <#ty as ::varve::__core::VarveKeyedBlock>::Key,
                    #ty,
                >
            > {
                self.inner.keyed_blocks::<#ty>()
            }
        }]
    }
}

fn writer_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let push = format_ident!("push_{}", singular_method_name(ty));
    let replace = format_ident!("replace_{}", singular_method_name(ty));
    let replace_method = quote! {
        pub fn #replace(
            &mut self,
            index: usize,
            value: &#ty,
        ) -> ::varve::__core::Result<::varve::__core::ReplacementInfo> {
            // F-01: a successful replacement republishes the file generation
            // and moves record offsets, and the file invalidates its own
            // keyed-tail cache when it rebinds. There is nothing left to
            // translate here, so no fallible step follows the publication.
            self.inner.replace_block::<#ty>(index, value)
        }
    };
    if block.key_fields.is_empty() {
        vec![
            quote! {
                pub fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    self.inner.push_info(value)
                }
            },
            replace_method,
        ]
    } else {
        let delete = format_ident!("delete_{}", singular_method_name(ty));
        vec![
            quote! {
                pub fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    // F-01: the maintained keyed append. The predecessor comes
                    // from - and the new tail goes into - the byte-keyed
                    // resident cache, whose slot is charged and reserved
                    // before the append and filled afterwards by an insert
                    // that runs no user code and cannot allocate.
                    self.inner.push_keyed_info(value)
                }
            },
            quote! {
                pub fn #delete(
                    &mut self,
                    key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    // F-01: see the push path. The key is encoded once, before
                    // the tombstone, and those same canonical bytes are both
                    // the record payload and the cache key, so nothing owned
                    // by the caller's key type is cloned, hashed or compared
                    // after publication.
                    self.inner.delete_info::<#ty>(key)
                }
            },
            replace_method,
        ]
    }
}

fn writer_trait_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let push = format_ident!("push_{}", singular_method_name(ty));
    let replace = format_ident!("replace_{}", singular_method_name(ty));
    let replace_method = quote! {
        fn #replace(
            &mut self,
            index: usize,
            value: &#ty,
        ) -> ::varve::__core::Result<::varve::__core::ReplacementInfo>;
    };
    if block.key_fields.is_empty() {
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo>;
            },
            replace_method,
        ]
    } else {
        let delete = format_ident!("delete_{}", singular_method_name(ty));
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo>;
            },
            quote! {
                fn #delete(
                    &mut self,
                    key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo>;
            },
            replace_method,
        ]
    }
}

fn writer_trait_impl_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let push = format_ident!("push_{}", singular_method_name(ty));
    let replace = format_ident!("replace_{}", singular_method_name(ty));
    let replace_method = quote! {
        fn #replace(
            &mut self,
            index: usize,
            value: &#ty,
        ) -> ::varve::__core::Result<::varve::__core::ReplacementInfo> {
            self.#replace(index, value)
        }
    };
    if block.key_fields.is_empty() {
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    self.inner.push_info(value)
                }
            },
            replace_method,
        ]
    } else {
        let delete = format_ident!("delete_{}", singular_method_name(ty));
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    // F-01: the trait route carries its own copy of the body,
                    // so it routes through the same single implementation as
                    // the inherent method.
                    self.inner.push_keyed_info(value)
                }
            },
            quote! {
                fn #delete(
                    &mut self,
                    key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    // F-01: see the inherent `delete_<block>` method.
                    self.inner.delete_info::<#ty>(key)
                }
            },
            replace_method,
        ]
    }
}

fn plural_method_ident(name: &Ident) -> Ident {
    format_ident!("{}", plural_method_name(name))
}

fn plural_method_name(name: &Ident) -> String {
    let singular = singular_method_name(name);
    if let Some(prefix) = singular.strip_suffix('y') {
        format!("{prefix}ies")
    } else if singular.ends_with('s') {
        format!("{singular}es")
    } else {
        format!("{singular}s")
    }
}

fn singular_method_name(name: &Ident) -> String {
    let raw = name.to_string();
    let mut out = String::new();
    for (index, ch) in raw.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn compression_tokens(choice: CompressionChoice) -> TokenStream2 {
    match choice {
        CompressionChoice::None => quote!(::varve::__core::CompressionPolicy::None),
        CompressionChoice::VariableBlocks(compression) => {
            let algorithm = match compression.algorithm {
                CompressionAlgorithmChoice::Zstd => {
                    quote!(::varve::__core::CompressionAlgorithm::Zstd)
                }
            };
            let level = match compression.level {
                CompressionLevelChoice::Fast => quote!(::varve::__core::CompressionLevel::Fast),
                CompressionLevelChoice::Default => {
                    quote!(::varve::__core::CompressionLevel::Default)
                }
                CompressionLevelChoice::Best => quote!(::varve::__core::CompressionLevel::Best),
                CompressionLevelChoice::Exact(level) => {
                    quote!(::varve::__core::CompressionLevel::Exact(#level))
                }
            };
            let header = match compression.header {
                CompressionHeaderChoice::RecordExplicit => {
                    quote!(::varve::__core::CompressionHeaderMode::RecordExplicit)
                }
                CompressionHeaderChoice::FileExplicit => {
                    quote!(::varve::__core::CompressionHeaderMode::FileExplicit)
                }
                CompressionHeaderChoice::FormatContract => {
                    quote!(::varve::__core::CompressionHeaderMode::FormatContract)
                }
            };
            let min_len = compression.min_len;
            let only_if_smaller = compression.only_if_smaller;
            let max_len = compression.max_len;
            quote! {
                ::varve::__core::CompressionPolicy::VariableBlocks(
                    ::varve::__core::VariableCompression {
                        algorithm: #algorithm,
                        level: #level,
                        header_mode: #header,
                        min_uncompressed_len: #min_len,
                        only_if_smaller: #only_if_smaller,
                        max_uncompressed_len: #max_len,
                    }
                )
            }
        }
    }
}

fn read_limits_tokens(choice: LimitsChoice) -> TokenStream2 {
    match choice {
        LimitsChoice::TrustedUnbounded => {
            quote!(::varve::__core::ReadLimits::trusted_unbounded())
        }
        LimitsChoice::Finite(entries) => {
            let setters = entries.into_iter().map(|entry| {
                let method = match entry.key.to_string().as_str() {
                    "file_len" => format_ident!("with_max_file_len"),
                    "records" => format_ident!("with_max_records"),
                    "index_bytes" => format_ident!("with_max_index_bytes"),
                    "scan_bytes" => format_ident!("with_max_scan_bytes"),
                    "record_payload" => format_ident!("with_max_record_payload_len"),
                    "logical_payload" => format_ident!("with_max_logical_payload_len"),
                    "materialized_bytes" => format_ident!("with_max_materialized_bytes"),
                    "segments" => format_ident!("with_max_segments"),
                    "matrix_dimension" => format_ident!("with_max_matrix_dimension"),
                    "matrix_cells" => format_ident!("with_max_matrix_cells"),
                    "matrix_bitmap" => format_ident!("with_max_matrix_bitmap_bytes"),
                    "matrix_crc" => format_ident!("with_max_matrix_crc_bytes"),
                    "matrix_metadata" => format_ident!("with_max_matrix_metadata_bytes"),
                    "matrix_slot_region" => format_ident!("with_max_matrix_slot_region_len"),
                    "sidecar" => format_ident!("with_max_sidecar_len"),
                    "mmap" => format_ident!("with_max_mmap_len"),
                    "keyed_tail" => format_ident!("with_max_keyed_tail_bytes"),
                    _ => unreachable!("read limit keys are validated while parsing"),
                };
                let value = entry.value;
                quote!(.#method(#value))
            });
            quote! {
                ::varve::__core::ReadLimits::missing()
                #(#setters)*
            }
        }
    }
}

fn pairwise<T>(items: &[T]) -> Vec<(&T, &T)> {
    let mut pairs = Vec::new();
    for left in 0..items.len() {
        for right in (left + 1)..items.len() {
            pairs.push((&items[left], &items[right]));
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn parse_inline_test_block(block: TokenStream2) -> Result<InlineBlock> {
        let parser = |input: ParseStream<'_>| parse_inline_block(input);
        syn::parse::Parser::parse2(parser, block)
    }

    /// API2-05: absolute facade paths are rewritten to the renamed dependency.
    #[test]
    fn rebrand_rewrites_absolute_facade_paths() {
        let facade = Ident::new("vv", proc_macro2::Span::call_site());
        let tokens = quote! {
            impl ::varve::__core::VarveBlock for Foo {
                const ID: u32 = <Bar as ::varve::__core::VarveBlock>::ID;
            }
        };
        let rebranded = rebrand_facade_tokens(tokens, &facade).to_string();
        assert!(rebranded.contains(":: vv :: __core :: VarveBlock"));
        assert!(!rebranded.contains("varve"));
    }

    /// API2-05 regression: `IDENT: ::varve::…` merges the type-ascription
    /// colon with the leading `::` into a 3-colon run, whose leading-ness the
    /// first-colon latch misjudges (it follows an ident). Rust has no `:::`
    /// token, so the final `::` of a 3+ colon run always begins an absolute
    /// path and must be rewritten. This is the pervasive generated shape
    /// (`const WIRE_TYPE: ::varve::__core::WireType`, typed fn params, trait
    /// bounds, struct-literal fields) that the original fix missed.
    #[test]
    fn rebrand_rewrites_absolute_paths_after_type_ascription_colon() {
        let facade = Ident::new("vv", proc_macro2::Span::call_site());
        let tokens = quote! {
            const WIRE_TYPE: ::varve::__core::WireType =
                ::varve::__core::WireType::Fixed;
            static SPEC: ::varve::__core::FormatSpec = make();
            fn probe<T: ::varve::__core::VarveBlock>(
                spec: ::varve::__core::FormatSpec,
            ) -> u32
            where
                T: ::varve::__core::VarveKeyedBlock,
            {
                let descriptor = Descriptor {
                    kind: ::varve::__core::BlockKind::Fixed,
                };
                let bound: ::varve::__core::WireType = descriptor.kind.wire();
                <T as ::varve::__core::VarveBlock>::ID
            }
        };
        let rebranded = rebrand_facade_tokens(tokens, &facade).to_string();
        assert!(
            !rebranded.contains("varve"),
            "unrewritten facade path survived: {rebranded}"
        );
        assert!(rebranded.contains("WIRE_TYPE : :: vv :: __core :: WireType"));
        assert!(rebranded.contains("spec : :: vv :: __core :: FormatSpec"));
        assert!(rebranded.contains("T : :: vv :: __core :: VarveKeyedBlock"));
        assert!(rebranded.contains("kind : :: vv :: __core :: BlockKind"));
        assert!(rebranded.contains("< T as :: vv :: __core :: VarveBlock > :: ID"));
    }

    /// API2-05: user tokens that merely contain the ident `varve` without a
    /// leading `::` (field access, relative paths, qualified-path members,
    /// strings) survive the rewrite unchanged.
    #[test]
    fn rebrand_leaves_non_facade_tokens_alone() {
        let facade = Ident::new("vv", proc_macro2::Span::call_site());
        let tokens = quote! {
            fn probe(value: crate::varve::Local, other: some::varve::Path) {
                let _ = value.varve;
                let _ = <T as Trait>::varve;
                let _ = "::varve::__core";
            }
        };
        let rebranded = rebrand_facade_tokens(tokens.clone(), &facade).to_string();
        assert_eq!(rebranded, tokens.to_string());
    }

    /// API2-05: the rewrite recurses into delimited groups.
    #[test]
    fn rebrand_recurses_into_groups() {
        let facade = Ident::new("renamed_facade", proc_macro2::Span::call_site());
        let tokens = quote! {
            fn body() -> ::varve::__core::Result<()> {
                ::core::result::Result::Ok(::varve::__core::noop())
            }
        };
        let rebranded = rebrand_facade_tokens(tokens, &facade).to_string();
        assert!(rebranded.contains(":: renamed_facade :: __core :: Result"));
        assert!(rebranded.contains(":: renamed_facade :: __core :: noop"));
        assert!(rebranded.contains(":: core :: result :: Result :: Ok"));
        assert!(!rebranded.contains(":: varve"));
    }

    #[test]
    fn inline_key_index_defaults_to_memory() {
        let block = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id]) { id: u64 }
        })
        .expect("keyed inline block should parse");

        assert!(block.key_index == KeyIndexChoice::Memory);
    }

    #[test]
    fn inline_key_index_rejects_unkeyed_blocks() {
        let error = parse_inline_test_block(quote! {
            fixed Item(id = 1, key_index = memory) { id: u64 }
        })
        .err()
        .expect("unkeyed key_index should fail");

        assert!(error.to_string().contains("key_index requires a keyed"));
    }

    #[test]
    fn inline_key_index_rejects_matrix_blocks() {
        let error = parse_inline_test_block(quote! {
            matrix Cell(
                id = 1,
                dims = [row, column],
                category = data,
                key_index = disk,
            ) { value: u64 }
        })
        .err()
        .expect("matrix key_index should fail");

        assert!(error.to_string().contains("matrix blocks"));
    }

    #[test]
    fn inline_key_index_rejects_unknown_value() {
        let error = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id], key_index = cached) { id: u64 }
        })
        .err()
        .expect("unknown key_index should fail");

        assert_eq!(error.to_string(), "expected memory or disk");
    }

    #[cfg(not(feature = "high-cardinality-dev"))]
    #[test]
    fn explicit_key_index_requires_feature() {
        let error = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id], key_index = disk) { id: u64 }
        })
        .err()
        .expect("key_index without the feature should fail");

        assert!(error.to_string().contains("high-cardinality-dev"));
    }

    #[cfg(feature = "high-cardinality-dev")]
    #[test]
    fn disk_key_index_generates_indexed_api() {
        let block = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id], key_index = disk) { id: u64 }
        })
        .expect("disk key_index should parse with the feature");
        let (constructors, api) =
            high_cardinality_api_tokens(&format_ident!("Test"), &[block], false);
        let tokens = format!("{constructors} {api}");

        assert!(tokens.contains("TestStreamReader"));
        assert!(tokens.contains("TestIndexedReader"));
        assert!(tokens.contains("get_item"));
        assert!(tokens.contains("push_item"));
        assert!(tokens.contains("push_items"));
        assert!(tokens.contains("delete_item"));
        assert!(tokens.contains("DiskIndexPlan"));
        assert!(tokens.contains("disk_index_plan"));
        assert!(tokens.contains("BatchAppendInfo"));
        assert!(tokens.contains("BatchAppendError"));
        assert!(
            !tokens.contains("high-cardinality-dev"),
            "dependency features must not become downstream cfg predicates"
        );
    }

    #[cfg(feature = "high-cardinality-dev")]
    #[test]
    fn disk_index_plan_is_sorted_by_block_id() {
        let later = parse_inline_test_block(quote! {
            variable Later(id = 9, key = [id], key_index = disk) { id: u64 }
        })
        .expect("later disk block should parse");
        let earlier = parse_inline_test_block(quote! {
            variable Earlier(id = 2, key = [id], key_index = disk) { id: u64 }
        })
        .expect("earlier disk block should parse");
        let (constructors, _) =
            high_cardinality_api_tokens(&format_ident!("Test"), &[later, earlier], false);
        let tokens = constructors.to_string();
        let plan_start = tokens
            .find("pub fn disk_index_plan")
            .expect("generated plan function");
        let plan_end = tokens[plan_start..]
            .find("pub fn open_indexed_reader")
            .expect("reader constructor after plan")
            + plan_start;
        let plan = &tokens[plan_start..plan_end];

        assert!(plan.contains("DiskIndexPlan :: canonical"));
        assert!(
            plan.find("Earlier").expect("earlier descriptor")
                < plan.find("Later").expect("later descriptor")
        );
        assert!(tokens.contains("Self :: disk_index_plan ()"));
    }

    #[test]
    fn generated_batch_method_uses_plural_and_borrowed_iterator() {
        let block = parse_inline_test_block(quote! {
            variable Frame(id = 1) { payload: Vec<u8> }
        })
        .expect("variable block should parse");
        let (_, api) = high_cardinality_api_tokens(&format_ident!("Test"), &[block], false);
        let tokens = api.to_string();

        assert!(tokens.contains("push_frames"));
        assert!(tokens.contains("BatchOptions"));
        assert!(tokens.contains("BatchAppendInfo"));
        assert!(tokens.contains("BatchAppendError"));
        assert!(tokens.contains("Borrow < Frame >"));
        assert!(tokens.contains("push_iter :: < Frame , _ >"));
    }

    #[cfg(feature = "high-cardinality-dev")]
    #[test]
    fn keyed_chain_exposes_only_plan_backed_keyed_mutations() {
        let account = parse_inline_test_block(quote! {
            variable Account(id = 1, key = [id]) { id: u64 }
        })
        .expect("memory-keyed block should parse");
        let frame = parse_inline_test_block(quote! {
            variable Frame(id = 2, key = [id], key_index = disk) { id: u64 }
        })
        .expect("disk-keyed block should parse");
        let metadata = parse_inline_test_block(quote! {
            fixed Metadata(id = 3) { value: u64 }
        })
        .expect("unkeyed block should parse");
        let (_, api) =
            high_cardinality_api_tokens(&format_ident!("Test"), &[account, frame, metadata], true);
        let tokens = api.to_string();
        let stream_start = tokens
            .find("impl TestStreamWriter")
            .expect("stream writer impl");
        let stream_end = tokens[stream_start..]
            .find("pub struct TestIndexedReader")
            .expect("indexed reader after stream writer")
            + stream_start;
        let stream_writer = &tokens[stream_start..stream_end];
        let indexed_start = tokens
            .find("impl TestIndexedWriter")
            .expect("indexed writer impl");
        let indexed_writer = &tokens[indexed_start..];

        assert!(!stream_writer.contains("push_account"));
        assert!(!stream_writer.contains("delete_account"));
        assert!(!stream_writer.contains("push_frame"));
        assert!(!stream_writer.contains("delete_frame"));
        assert!(stream_writer.contains("push_metadata"));
        assert!(indexed_writer.contains("push_frame"));
        assert!(indexed_writer.contains("push_frames"));
        assert!(indexed_writer.contains("delete_frame"));
        assert!(!indexed_writer.contains("push_account"));
        assert!(!indexed_writer.contains("delete_account"));
        assert!(indexed_writer.contains("push_metadata"));
    }

    #[test]
    fn memory_key_index_generates_only_stream_api() {
        let block = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id]) { id: u64 }
        })
        .expect("default key index should parse");
        let (constructors, api) =
            high_cardinality_api_tokens(&format_ident!("Test"), &[block], false);
        let tokens = format!("{constructors} {api}");

        assert!(tokens.contains("TestStreamReader"));
        assert!(!tokens.contains("TestIndexedReader"));
    }

    #[test]
    fn parses_matrix_format_syntax() {
        let input = syn::parse2::<FormatInput>(quote! {
            pub format AnalysisFormat {
                magic: b"ANALYSIS";
                version: 1;
                limits: trusted_unbounded;
                endian: little;
                schema_hash: computed;
                extension: "vrv";

                dims {
                    n_scans: u32,
                    n_channels: u32,
                }

                commit: cell_bitmap {
                    keyspace = [scan, ch];
                    categories = [analysis];
                    singles = [master_grid];
                    per_channel = [threshold];
                };

                aux {
                    thumbnail: 64,
                }

                blocks {
                    matrix AnalysisCell(id = 10, dims = [scan, ch], category = analysis) {
                        value: u32,
                    }
                }
            }
        })
        .expect("matrix format syntax should parse");

        assert_eq!(input.dims.len(), 2);
        assert!(input.matrix_commit.is_some());
        assert_eq!(input.matrix_aux.len(), 1);
        assert_eq!(input.inline_blocks.len(), 1);
        assert!(matches!(
            input.inline_blocks[0].kind,
            InlineBlockKind::Matrix(_)
        ));
    }
}

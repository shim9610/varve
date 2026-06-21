use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Data, DeriveInput, Fields, Ident, LitBool, LitByteStr, LitInt, LitStr, Result, Token, Type,
    Visibility, braced, bracketed, parenthesized, parse_macro_input,
};

#[proc_macro_derive(VarveBlock, attributes(varve))]
pub fn derive_varve_block(input: TokenStream) -> TokenStream {
    match expand_varve_block(parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro]
pub fn varve_format(input: TokenStream) -> TokenStream {
    match syn::parse::<FormatInput>(input) {
        Ok(input) => expand_format(input).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_varve_block(input: DeriveInput) -> Result<TokenStream2> {
    let ident = input.ident;
    let mut block_id = None;
    let mut version = quote!(1u16);
    let mut kind = quote!(::varve::__core::BlockKind::Fixed);
    let mut variable_block = false;
    let mut endian = quote!(None);
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
                Ok(())
            } else if meta.path.is_ident("kind") {
                let value: LitStr = meta.value()?.parse()?;
                kind = match value.value().as_str() {
                    "fixed" => {
                        variable_block = false;
                        quote!(::varve::__core::BlockKind::Fixed)
                    }
                    "matrix" => {
                        variable_block = false;
                        quote!(::varve::__core::BlockKind::Matrix)
                    }
                    "variable" => {
                        variable_block = true;
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
                    "little" => quote!(Some(::varve::__core::Endian::Little)),
                    "big" => quote!(Some(::varve::__core::Endian::Big)),
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
            let payload = ::varve::__core::encode_to_vec(&self.#name, encoder.endian())?;
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
                    ::varve::__core::decode_from_slice::<#ty>(payload, decoder.endian())?
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
        match Self::KIND {
            ::varve::__core::BlockKind::Fixed | ::varve::__core::BlockKind::Matrix => {
                #(#fixed_encode)*
            }
            ::varve::__core::BlockKind::Variable => {
                #(#variable_encode)*
            }
            ::varve::__core::BlockKind::Internal => {}
        }
        Ok(())
    };

    let decode_body = quote! {
        match Self::KIND {
            ::varve::__core::BlockKind::Fixed | ::varve::__core::BlockKind::Matrix => {
                Ok(Self { #(#fixed_decode,)* })
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
                Ok(Self { #(#build_fields,)* })
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

    Ok(quote! {
        impl ::varve::__core::VarveEncode for #ident {
            const WIRE_TYPE: ::varve::__core::WireType = ::varve::__core::WireType::Nested;

            fn encode_varve(&self, encoder: &mut ::varve::__core::Encoder) -> ::varve::__core::Result<()> {
                #encode_body
            }
        }

        impl ::varve::__core::VarveDecode for #ident {
            const WIRE_TYPE: ::varve::__core::WireType = ::varve::__core::WireType::Nested;

            fn decode_varve(decoder: &mut ::varve::__core::Decoder<'_>) -> ::varve::__core::Result<Self> {
                #decode_body
            }
        }

        impl ::varve::__core::VarveBlock for #ident {
            const ID: u32 = #block_id;
            const VERSION: u16 = #version;
            const KIND: ::varve::__core::BlockKind = #kind;
            const ENDIAN: ::core::option::Option<::varve::__core::Endian> = #endian;
            const FIELDS: &'static [::varve::__core::FieldDescriptor] = &[
                #(#field_descriptors,)*
            ];
        }

        #keyed_impl
    })
}

fn option_ident(name: &Ident) -> Ident {
    format_ident!("__varve_field_{}", name)
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

#[derive(Clone, Copy)]
struct IndexChoice {
    scan_on_open: bool,
    checkpoint_on_flush: bool,
    block_offset_chain: bool,
    keyed_offset_chain: bool,
}

impl IndexChoice {
    const fn scan_on_open() -> Self {
        Self {
            scan_on_open: true,
            checkpoint_on_flush: false,
            block_offset_chain: false,
            keyed_offset_chain: false,
        }
    }

    const fn with_block_offset_chain(mut self) -> Self {
        self.scan_on_open = true;
        self.block_offset_chain = true;
        self
    }

    const fn with_keyed_offset_chain(mut self) -> Self {
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

struct InlineBlock {
    kind: InlineBlockKind,
    name: Ident,
    id: u32,
    version: u16,
    key_fields: Vec<Ident>,
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
        let mut dims: Option<Vec<MatrixDim>> = None;
        let mut matrix_commit = None;
        let mut matrix_aux: Option<Vec<MatrixAux>> = None;
        let mut registry_blocks: Option<Vec<Type>> = None;
        let mut inline_blocks: Option<Vec<InlineBlock>> = None;
        let mut layout_preset = None;
        let mut layout_file_header = None;
        let mut layout_segments: Option<Vec<LayoutSegment>> = None;

        while !content.is_empty() {
            let key: Ident = content.parse()?;
            if key == "dims" && content.peek(syn::token::Brace) {
                let inner;
                braced!(inner in content);
                dims = Some(parse_matrix_dims(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "aux" && content.peek(syn::token::Brace) {
                let inner;
                braced!(inner in content);
                matrix_aux = Some(parse_matrix_aux(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "blocks" && content.peek(syn::token::Brace) {
                let inner;
                braced!(inner in content);
                inline_blocks = Some(parse_inline_blocks(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "layout" && content.peek(syn::token::Brace) {
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
                    _ => return Err(syn::Error::new_spanned(value, "expected none or crc32")),
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

fn parse_index_choice(input: ParseStream<'_>) -> Result<IndexChoice> {
    if input.peek(syn::token::Bracket) {
        let inner;
        bracketed!(inner in input);
        let mut choice = IndexChoice {
            scan_on_open: false,
            checkpoint_on_flush: false,
            block_offset_chain: false,
            keyed_offset_chain: false,
        };
        let values = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
        if values.is_empty() {
            return Err(inner.error("index list must not be empty"));
        }
        for value in values {
            choice = apply_index_ident(choice, value)?;
        }
        return Ok(choice);
    }
    let value: Ident = input.parse()?;
    apply_index_ident(
        IndexChoice {
            scan_on_open: false,
            checkpoint_on_flush: false,
            block_offset_chain: false,
            keyed_offset_chain: false,
        },
        value,
    )
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
                    "expected id, version, key, dims, or category",
                ));
            }
        }
        if meta.peek(Token![,]) {
            meta.parse::<Token![,]>()?;
        }
    }
    let kind = if matches!(&kind, InlineBlockKind::Matrix(_)) {
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
                    "P0 matrix fields must be fixed-width scalar or fixed-array types",
                ));
            }
        }
    }
    Ok(())
}

fn matrix_field_is_fixed_width(ty: &Type) -> bool {
    match ty {
        Type::Path(path) => path.path.segments.last().is_some_and(|segment| {
            matches!(
                segment.ident.to_string().as_str(),
                "bool"
                    | "u8"
                    | "i8"
                    | "u16"
                    | "i16"
                    | "u32"
                    | "i32"
                    | "u64"
                    | "i64"
                    | "u128"
                    | "i128"
                    | "f32"
                    | "f64"
                    | "PackedBitmap"
            )
        }),
        Type::Array(array) => matrix_field_is_fixed_width(&array.elem),
        _ => false,
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
        .map(|extension| quote!(Some(#extension)))
        .unwrap_or_else(|| quote!(None));
    let endian = match input.endian {
        EndianChoice::Little => quote!(::varve::__core::Endian::Little),
        EndianChoice::Big => quote!(::varve::__core::Endian::Big),
    };
    let index = index_tokens(input.index);
    let commit = commit_tokens(input.commit);
    let integrity = match input.integrity {
        IntegrityChoice::None => quote!(::varve::__core::IntegrityPolicy::None),
        IntegrityChoice::Crc32 => quote!(::varve::__core::IntegrityPolicy::Crc32),
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
    let dims = input.dims;
    let matrix_commit = input.matrix_commit;
    let matrix_aux = input.matrix_aux;
    let layout = layout_tokens(
        input.layout_preset,
        input.layout_file_header.as_ref(),
        &input.layout_segments,
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

    let duplicate_asserts = pairwise(&blocks).into_iter().map(|(left, right)| {
        quote! {
            const _: () = assert!(
                <#left as ::varve::__core::VarveBlock>::ID
                    != <#right as ::varve::__core::VarveBlock>::ID
            );
        }
    });
    let typed_api = if input.typed_api {
        typed_api_tokens(&name, &inline_blocks, matrix_commit.as_ref(), &matrix_aux)
    } else {
        quote!()
    };
    let writer_return = if input.typed_api {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name)
    } else {
        quote!(::varve::__core::VarveWriter)
    };
    let reader_return = if input.typed_api {
        let reader_name = format_ident!("{}Reader", name);
        quote!(#reader_name)
    } else {
        quote!(::varve::__core::VarveReader)
    };
    let create_writer_body = if input.typed_api {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer(path)?))
    } else {
        quote!(Self::spec().create_writer(path))
    };
    let open_writer_body = if input.typed_api {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer(path)?))
    } else {
        quote!(Self::spec().open_writer(path))
    };
    let open_recover_writer_body = if input.typed_api {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer(path)?))
    } else {
        quote!(Self::spec().open_recover_writer(path))
    };
    let open_recover_writer_report_body = if input.typed_api {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec().open_recover_writer_with_report(path)?;
                Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report(path))
    };
    let open_reader_body = if input.typed_api {
        let reader_name = format_ident!("{}Reader", name);
        quote!(Ok(#reader_name::from_inner(Self::spec().open_reader(path)?)))
    } else {
        quote!(Self::spec().open_reader(path))
    };
    let matrix_spec_step =
        matrix_spec_tokens(&dims, matrix_commit.as_ref(), &matrix_aux, &inline_blocks);
    let create_writer_with_dims_method = create_writer_with_dims_tokens(
        &name,
        &dims,
        matrix_commit.as_ref(),
        input.typed_api,
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
                #matrix_spec_step
                .with_layout(#layout)
                #schema_hash_step
            }

            pub fn create<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().create(path)
            }

            pub fn create_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#writer_return> {
                #create_writer_body
            }

            pub fn create_layout_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::LayoutWriter> {
                Self::spec().create_layout_writer(path)
            }

            pub fn create_layout_writer_with_header<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::varve::__core::LayoutFieldValue],
            ) -> ::varve::__core::Result<::varve::__core::LayoutWriter> {
                Self::spec().create_layout_writer_with_header(path, fields)
            }

            #create_writer_with_dims_method

            pub fn open<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open(path)
            }

            pub fn open_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#writer_return> {
                #open_writer_body
            }

            pub fn open_layout_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::LayoutWriter> {
                Self::spec().open_layout_writer(path)
            }

            pub fn open_readonly<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_readonly(path)
            }

            pub fn open_reader<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#reader_return> {
                #open_reader_body
            }

            pub fn open_layout_reader<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::LayoutReader> {
                Self::spec().open_layout_reader(path)
            }

            pub fn inspect_layout_file<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file(path)
            }

            pub fn open_recover<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<::varve::__core::VarveFile> {
                Self::spec().open_recover(path)
            }

            pub fn open_recover_writer<P: AsRef<::std::path::Path>>(path: P) -> ::varve::__core::Result<#writer_return> {
                #open_recover_writer_body
            }

            pub fn open_recover_with_report<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<(::varve::__core::VarveFile, ::varve::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report(path)
            }

            pub fn open_recover_writer_with_report<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::varve::__core::Result<(#writer_return, ::varve::__core::RecoveryReport)> {
                #open_recover_writer_report_body
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
        LayoutPresetChoice::Custom
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
                        Some(::varve::__core::FooterDescriptor {
                            name: stringify!(#footer_name),
                            fields: #footer_fields,
                        })
                    }
                })
                .unwrap_or_else(|| quote!(None));
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
            ::varve::__core::VarveWriter::create_with_dims(
                Self::spec(),
                path,
                dims.into_matrix_dims(),
            )?
        ))
    } else {
        quote!(::varve::__core::VarveWriter::create_with_dims(
            Self::spec(),
            path,
            dims.into_matrix_dims(),
        ))
    };
    quote! {
        pub fn create_writer_with_dims<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
        ) -> ::varve::__core::Result<#writer_return> {
            #body
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
        let stride_terms = block.fields.iter().map(|field| {
            let ty = &field.ty;
            quote!(::core::mem::size_of::<#ty>() as u64)
        });
        quote! {
            impl ::varve::__core::VarveMatrixBlock for #name {
                const DIMENSIONS: [&'static str; 2] = [#(#matrix_dims,)*];
                const CATEGORY: &'static str = #category;
                const SLOT_STRIDE: u64 = 0u64 #( + #stride_terms )*;
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
    let writer_tail_fields = keyed_blocks.iter().map(|block| {
        let field = tail_map_ident(&block.name);
        let ty = &block.name;
        quote! {
            #field: ::std::collections::HashMap<
                <#ty as ::varve::__core::VarveKeyedBlock>::Key,
                u64,
            >,
        }
    });
    let writer_tail_inits = keyed_blocks.iter().map(|block| {
        let field = tail_map_ident(&block.name);
        let ty = &block.name;
        quote!(let #field = inner.key_tail_offsets::<#ty>()?;)
    });
    let writer_tail_values = keyed_blocks.iter().map(|block| {
        let field = tail_map_ident(&block.name);
        quote!(#field,)
    });

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

        #[derive(Debug)]
        pub struct #writer_name {
            inner: ::varve::__core::VarveWriter,
            #(#writer_tail_fields)*
        }

        impl #writer_name {
            pub fn from_inner(inner: ::varve::__core::VarveWriter) -> ::varve::__core::Result<Self> {
                #(#writer_tail_inits)*
                Ok(Self {
                    inner,
                    #(#writer_tail_values)*
                })
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
                ) -> ::varve::__core::Result<Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #len_method(&self) -> ::varve::__core::Result<u64>;

                fn #read_method(
                    &mut self,
                    offset: u64,
                    len: u64,
                ) -> ::varve::__core::Result<Vec<u8>>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #len_method(&self) -> ::varve::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                fn #read_method(
                    &mut self,
                    offset: u64,
                    len: u64,
                ) -> ::varve::__core::Result<Vec<u8>> {
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
                ) -> ::varve::__core::Result<Vec<u8>> {
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
                ) -> ::varve::__core::Result<Vec<u8>>;

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
                ) -> ::varve::__core::Result<Vec<u8>> {
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
    if block.key_fields.is_empty() {
        vec![quote! {
            pub fn #push(
                &mut self,
                value: &#ty,
            ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                self.inner.push_info(value)
            }
        }]
    } else {
        let delete = format_ident!("delete_{}", singular_method_name(ty));
        let tails = tail_map_ident(ty);
        vec![
            quote! {
                pub fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    let key = <#ty as ::varve::__core::VarveKeyedBlock>::key(value);
                    let prev = self.#tails.get(&key).copied();
                    let info = self.inner.push_with_prev_key_info(value, prev)?;
                    self.#tails.insert(key, info.record_offset);
                    Ok(info)
                }
            },
            quote! {
                pub fn #delete(
                    &mut self,
                    key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    let prev = self.#tails.get(key).copied();
                    let info = self.inner.delete_with_prev_key_info::<#ty>(key, prev)?;
                    self.#tails.insert(key.clone(), info.record_offset);
                    Ok(info)
                }
            },
        ]
    }
}

fn writer_trait_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let push = format_ident!("push_{}", singular_method_name(ty));
    if block.key_fields.is_empty() {
        vec![quote! {
            fn #push(
                &mut self,
                value: &#ty,
            ) -> ::varve::__core::Result<::varve::__core::AppendInfo>;
        }]
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
        ]
    }
}

fn writer_trait_impl_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let push = format_ident!("push_{}", singular_method_name(ty));
    if block.key_fields.is_empty() {
        vec![quote! {
            fn #push(
                &mut self,
                value: &#ty,
            ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                self.inner.push_info(value)
            }
        }]
    } else {
        let delete = format_ident!("delete_{}", singular_method_name(ty));
        let tails = tail_map_ident(ty);
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    let key = <#ty as ::varve::__core::VarveKeyedBlock>::key(value);
                    let prev = self.#tails.get(&key).copied();
                    let info = self.inner.push_with_prev_key_info(value, prev)?;
                    self.#tails.insert(key, info.record_offset);
                    Ok(info)
                }
            },
            quote! {
                fn #delete(
                    &mut self,
                    key: &<#ty as ::varve::__core::VarveKeyedBlock>::Key,
                ) -> ::varve::__core::Result<::varve::__core::AppendInfo> {
                    let prev = self.#tails.get(key).copied();
                    let info = self.inner.delete_with_prev_key_info::<#ty>(key, prev)?;
                    self.#tails.insert(key.clone(), info.record_offset);
                    Ok(info)
                }
            },
        ]
    }
}

fn tail_map_ident(name: &Ident) -> Ident {
    format_ident!("__varve_{}_tails", singular_method_name(name))
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

    #[test]
    fn parses_matrix_format_syntax() {
        let input = syn::parse2::<FormatInput>(quote! {
            pub format AnalysisFormat {
                magic: b"ANALYSIS";
                version: 1;
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

//! Finite, schema-owned value combinations. No runtime interning or dictionary.
use super::*;
use std::collections::HashSet;

pub(super) const MAX_COMBINATIONS: usize = 4096;

pub(super) struct Entry {
    name: Ident,
    values: Vec<syn::Expr>,
}

pub(super) enum Declaration {
    Values(Vec<Entry>),
    Domain(Vec<(Ident, Vec<syn::Expr>)>),
}

pub(super) struct FiniteKey {
    fields: Vec<(Ident, Type)>,
    entries: Vec<Entry>,
}

fn fail<T>(span: Span, message: &str) -> Result<T> {
    Err(syn::Error::new(span, message))
}

fn integer(expr: &syn::Expr) -> Option<i128> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(n),
            ..
        }) => n.base10_parse().ok(),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => integer(expr)?.checked_neg(),
        _ => None,
    }
}

fn literal_identity(expr: &syn::Expr) -> Result<String> {
    if let Some(n) = integer(expr) {
        return Ok(format!("i:{n}"));
    }
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Bool(v),
            ..
        }) => Ok(format!("b:{}", v.value)),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(v),
            ..
        }) => Ok(format!("s:{:?}", v.value())),
        _ => Err(syn::Error::new_spanned(
            expr,
            "finite keys require integer, bool, or string literals",
        )),
    }
}

pub(super) fn parse_values(input: ParseStream<'_>) -> Result<Declaration> {
    let inner;
    bracketed!(inner in input);
    let mut entries = Vec::new();
    while !inner.is_empty() {
        if entries.len() == MAX_COMBINATIONS {
            return fail(
                inner.span(),
                "finite key exceeds 4096 combinations per block",
            );
        }
        let name = inner.parse()?;
        inner.parse::<Token![=]>()?;
        let expr: syn::Expr = inner.parse()?;
        let values = match expr {
            syn::Expr::Tuple(tuple) => tuple.elems.into_iter().collect(),
            value => vec![value],
        };
        entries.push(Entry { name, values });
        if !inner.is_empty() {
            inner.parse::<Token![,]>()?;
        }
    }
    Ok(Declaration::Values(entries))
}

pub(super) fn parse_domain(input: ParseStream<'_>) -> Result<Declaration> {
    let inner;
    bracketed!(inner in input);
    let mut domains = Vec::new();
    let mut product = 1usize;
    while !inner.is_empty() {
        let field: Ident = inner.parse()?;
        inner.parse::<Token![=]>()?;
        let expr: syn::Expr = inner.parse()?;
        let values: Vec<syn::Expr> = match expr {
            syn::Expr::Array(array) => array.elems.into_iter().collect(),
            syn::Expr::Range(range) => {
                let (Some(start), Some(end)) = (&range.start, &range.end) else {
                    return fail(field.span(), "finite key ranges need both endpoints");
                };
                let (Some(start), Some(end)) = (integer(start), integer(end)) else {
                    return fail(
                        field.span(),
                        "finite key range endpoints must be integer literals",
                    );
                };
                let count = end
                    .checked_sub(start)
                    .and_then(|n| {
                        n.checked_add(i128::from(matches!(
                            range.limits,
                            syn::RangeLimits::Closed(_)
                        )))
                    })
                    .filter(|n| *n > 0 && *n <= MAX_COMBINATIONS as i128)
                    .ok_or_else(|| {
                        syn::Error::new(
                            field.span(),
                            "finite key range must contain 1..=4096 values",
                        )
                    })?;
                (0..count)
                    .map(|i| syn::parse_str(&(start + i).to_string()).unwrap())
                    .collect()
            }
            _ => {
                return fail(
                    field.span(),
                    "key_domain expects a literal list or finite integer range",
                );
            }
        };
        if values.is_empty() || values.len() > MAX_COMBINATIONS {
            return fail(
                field.span(),
                "finite key domain must contain 1..=4096 values",
            );
        }
        product = product
            .checked_mul(values.len())
            .filter(|n| *n <= MAX_COMBINATIONS)
            .ok_or_else(|| {
                syn::Error::new(
                    field.span(),
                    "finite key exceeds 4096 combinations per block",
                )
            })?;
        let mut seen = HashSet::new();
        for value in &values {
            if !seen.insert(literal_identity(value)?) {
                return Err(syn::Error::new_spanned(
                    value,
                    "duplicate finite key domain value",
                ));
            }
        }
        domains.push((field, values));
        if !inner.is_empty() {
            inner.parse::<Token![,]>()?;
        }
    }
    Ok(Declaration::Domain(domains))
}

impl FiniteKey {
    pub(super) fn build(
        decl: Declaration,
        keys: &[Ident],
        fields: &[InlineField],
        span: Span,
    ) -> Result<Self> {
        if keys.is_empty() {
            return fail(span, "finite keys require key = [fields]");
        }
        let fields: Vec<_> = keys
            .iter()
            .map(|key| {
                let field = fields.iter().find(|f| f.name == *key).unwrap();
                if field.default {
                    return fail(key.span(), "finite key fields cannot be defaulted");
                }
                Ok((key.clone(), field.ty.clone()))
            })
            .collect::<Result<_>>()?;
        let entries = match decl {
            Declaration::Values(entries) => entries,
            Declaration::Domain(domains) => {
                if domains.len() != keys.len()
                    || domains
                        .iter()
                        .zip(keys)
                        .any(|((field, _), key)| field != key)
                {
                    return fail(
                        span,
                        "key_domain must list every key field once, in key declaration order",
                    );
                }
                let mut combinations = vec![Vec::new()];
                for (_, values) in domains {
                    combinations = combinations
                        .into_iter()
                        .flat_map(|prefix| {
                            values.iter().map(move |value| {
                                let mut row = prefix.clone();
                                row.push(value.clone());
                                row
                            })
                        })
                        .collect();
                }
                combinations
                    .into_iter()
                    .enumerate()
                    .map(|(i, values)| Entry {
                        name: format_ident!("K{i}"),
                        values,
                    })
                    .collect()
            }
        };
        if entries.is_empty() || entries.len() > MAX_COMBINATIONS {
            return fail(span, "finite keys require 1..=4096 combinations per block");
        }
        let mut names = HashSet::new();
        let mut combinations = HashSet::new();
        for entry in &entries {
            if !names.insert(entry.name.to_string()) {
                return fail(entry.name.span(), "duplicate finite key variant");
            }
            if entry.values.len() != fields.len() {
                return fail(
                    entry.name.span(),
                    "finite key tuple arity differs from key declaration",
                );
            }
            let mut identity = Vec::new();
            for ((_, ty), value) in fields.iter().zip(&entry.values) {
                let type_name = quote!(#ty).to_string();
                let identity_value = literal_identity(value)?;
                let valid = match type_name.as_str() {
                    "bool" => identity_value.starts_with("b:"),
                    "String" => identity_value.starts_with("s:"),
                    "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64" => {
                        let bits: u32 = type_name[1..].parse().unwrap();
                        integer(value).is_some_and(|n| {
                            if type_name.starts_with('u') {
                                n >= 0 && n < (1i128 << bits)
                            } else {
                                n >= -(1i128 << (bits - 1)) && n < (1i128 << (bits - 1))
                            }
                        })
                    }
                    _ => {
                        return Err(syn::Error::new_spanned(
                            ty,
                            "finite keys support u8/u16/u32/u64, i8/i16/i32/i64, bool, and String",
                        ));
                    }
                };
                if !valid {
                    return Err(syn::Error::new_spanned(
                        value,
                        "finite key value does not fit its declared field type",
                    ));
                }
                identity.push(identity_value);
            }
            if !combinations.insert(identity) {
                return fail(entry.name.span(), "duplicate finite key value combination");
            }
        }
        Ok(Self { fields, entries })
    }

    pub(super) fn tokens(&self, vis: &Visibility, block: &Ident) -> TokenStream2 {
        let name = format_ident!("{block}Key");
        let variants: Vec<_> = self.entries.iter().map(|entry| &entry.name).collect();
        let codes: Vec<_> = (0..self.entries.len() as u16).collect();
        let count = self.entries.len();
        let types: Vec<_> = self
            .fields
            .iter()
            .map(|(_, ty)| {
                if quote!(#ty).to_string() == "String" {
                    quote!(&str)
                } else {
                    quote!(#ty)
                }
            })
            .collect();
        let static_types: Vec<_> = self
            .fields
            .iter()
            .map(|(_, ty)| {
                if quote!(#ty).to_string() == "String" {
                    quote!(&'static str)
                } else {
                    quote!(#ty)
                }
            })
            .collect();
        let value_type = if types.len() == 1 {
            quote!(#(#types)*)
        } else {
            quote!((#(#types,)*))
        };
        let static_type = if types.len() == 1 {
            quote!(#(#static_types)*)
        } else {
            quote!((#(#static_types,)*))
        };
        let values: Vec<_> = self
            .entries
            .iter()
            .map(|entry| {
                // Normalize numeric suffixes/representations after checking the schema type.
                let vals: Vec<_> = entry
                    .values
                    .iter()
                    .map(|expr| {
                        if let Some(n) = integer(expr) {
                            syn::parse_str::<syn::Expr>(&n.to_string()).unwrap()
                        } else {
                            expr.clone()
                        }
                    })
                    .collect();
                if vals.len() == 1 {
                    quote!(#(#vals)*)
                } else {
                    quote!((#(#vals,)*))
                }
            })
            .collect();
        let identity = format!(
            "varve/finite-key/u16/v1/{}/{:?}/{:?}",
            quote!(#(#static_types),*),
            self.fields
                .iter()
                .map(|(field, _)| field.to_string())
                .collect::<Vec<_>>(),
            self.entries
                .iter()
                .map(|entry| entry
                    .values
                    .iter()
                    .map(|v| literal_identity(v).unwrap())
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
        let schema_id = identity
            .as_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325u64, |acc, byte| {
                (acc ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        let disk_impl = {
            quote! {
                impl ::varve::__core::VarveDiskKey for #name {
                    const FINITE_COUNT: u32 = #count as u32;
                    fn disk_codec_identity() -> ::std::string::String {
                        ::std::format!("varve/finite-key/u16/v1/{:016x}", #schema_id)
                    }
                }
            }
        };
        quote! {
            /// A schema-declared value combination, encoded as two bytes.
            #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
            #[repr(u16)]
            #vis enum #name { #(#variants = #codes,)* }
            impl #name {
                pub const COUNT: usize = #count;
                pub const ALL: &'static [Self] = &[#(Self::#variants,)*];
                pub const fn code(self) -> u16 { self as u16 }
                pub fn from_code(code: u16) -> ::varve::__core::Result<Self> {
                    Self::ALL.get(usize::from(code)).copied().ok_or(
                        ::varve::__core::Error::InvalidCanonicalEncoding("undeclared finite key code"))
                }
                #[allow(unreachable_patterns)] // A declared domain may cover an entire scalar type.
                pub fn from_values(values: #value_type) -> ::varve::__core::Result<Self> {
                    match values {
                        #(#values => ::core::result::Result::Ok(Self::#variants),)*
                        _ => ::core::result::Result::Err(::varve::__core::Error::InvalidCanonicalEncoding("undeclared finite key combination")),
                    }
                }
                pub const fn values(self) -> #static_type {
                    match self { #(Self::#variants => #values,)* }
                }
            }
            impl ::varve::__core::VarveEncode for #name {
                const WIRE_TYPE: ::varve::__core::WireType = ::varve::__core::WireType::U16;
                const SCHEMA_ID: u64 = #schema_id;
                fn encode_varve(&self, encoder: &mut ::varve::__core::Encoder) -> ::varve::__core::Result<()> {
                    ::varve::__core::VarveEncode::encode_varve(&self.code(), encoder)
                }
            }
            impl ::varve::__core::VarveDecode for #name {
                const WIRE_TYPE: ::varve::__core::WireType = ::varve::__core::WireType::U16;
                const SCHEMA_ID: u64 = #schema_id;
                fn decode_varve(decoder: &mut ::varve::__core::Decoder<'_>) -> ::varve::__core::Result<Self> {
                    Self::from_code(<u16 as ::varve::__core::VarveDecode>::decode_varve(decoder)?)
                }
            }
            #disk_impl
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse::Parser;

    fn parse(source: &str) -> Result<InlineBlock> {
        super::super::parse_inline_block.parse_str(source)
    }

    fn rejection(source: &str, expected: &str) {
        let error = parse(source).err().expect("invalid schema accepted");
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test]
    fn combination_limit_is_checked_before_cartesian_expansion() {
        let block =
            parse("fixed Item(id=1, key=[a,b], key_domain=[a=0..64,b=0..64]) {a:u16,b:u16}")
                .unwrap();
        assert_eq!(block.finite_key.unwrap().entries.len(), 4096);
        rejection(
            "fixed Item(id=1, key=[a,b], key_domain=[a=0..64,b=0..65]) {a:u16,b:u16}",
            "4096",
        );
        rejection(
            "fixed Item(id=1, key=[a], key_domain=[a=0..=4096]) {a:u16}",
            "4096",
        );
        rejection(
            "fixed Item(id=1, key=[a], key_domain=[a=0..=18446744073709551615]) {a:u64}",
            "4096",
        );
        let entries = (0..4097)
            .map(|i| format!("K{i}={i}"))
            .collect::<Vec<_>>()
            .join(",");
        rejection(
            &format!("fixed Item(id=1,key=[a],key_values=[{entries}]){{a:u16}}"),
            "4096",
        );
    }

    #[test]
    fn invalid_and_ambiguous_schemas_fail_at_compile_time() {
        for (source, expected) in [
            ("fixed I(id=1,key_values=[A=1]){a:u8}", "require key"),
            ("fixed I(id=1,key=[a],key_values=[]){a:u8}", "1..=4096"),
            (
                "fixed I(id=1,key=[a],key_values=[A=1,B=0x1]){a:u8}",
                "duplicate finite key value",
            ),
            (
                "fixed I(id=1,key=[a],key_values=[A=1,A=2]){a:u8}",
                "duplicate finite key variant",
            ),
            (
                "fixed I(id=1,key=[a,b],key_values=[A=1]){a:u8,b:u8}",
                "arity",
            ),
            (
                "fixed I(id=1,key=[a],key_values=[A=256]){a:u8}",
                "does not fit",
            ),
            (
                "fixed I(id=1,key=[a],key_values=[A=-1]){a:u8}",
                "does not fit",
            ),
            ("fixed I(id=1,key=[a],key_values=[A=1]){a:f64}", "support"),
            (
                "fixed I(id=1,key=[a],key_values=[A=1]){a:bool}",
                "does not fit",
            ),
            (
                "fixed I(id=1,key=[a],key_values=[A=1]){a:u8,key:u64}",
                "reserve",
            ),
            ("fixed I(id=1,key=[a],key_domain=[a=[]]){a:u8}", "1..=4096"),
            (
                "fixed I(id=1,key=[a],key_domain=[a=[1,1]]){a:u8}",
                "duplicate",
            ),
            (
                "fixed I(id=1,key=[a],key_domain=[a=2..1]){a:u8}",
                "1..=4096",
            ),
            (
                "fixed I(id=1,key=[a],key_domain=[a=0..]){a:u8}",
                "both endpoints",
            ),
            (
                "fixed I(id=1,key=[a],key_domain=[b=0..2]){a:u8,b:u8}",
                "every key field",
            ),
            (
                "fixed I(id=1,key=[a,b],key_domain=[b=0..2,a=0..2]){a:u8,b:u8}",
                "declaration order",
            ),
            (
                "fixed I(id=1,key=[a],key_values=[A=1],key_domain=[a=0..2]){a:u8}",
                "exactly once",
            ),
            (
                "variable I(id=1,key=[a],key_values=[A=1]){a:u8=default}",
                "cannot be defaulted",
            ),
        ] {
            rejection(source, expected);
        }
    }
}

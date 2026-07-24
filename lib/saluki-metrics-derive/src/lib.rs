//! Procedural macros for `saluki-metrics`.
//!
//! This crate provides the [`static_metrics`] attribute macro. It is an implementation detail of `saluki-metrics` and
//! should be used through the re-export there (`saluki_metrics::static_metrics`) rather than depended on directly.

#![deny(missing_docs)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    parse::{Parse, ParseStream, Parser},
    parse_quote, Fields, Ident, ItemStruct, LitStr, Token, Type,
};

/// Defines a container struct of statically defined metrics.
///
/// Applied to a struct whose fields are metric handles, this generates the registration, accessors, and metadata for
/// the whole group. It is re-exported (and intended to be used) as `saluki_metrics::static_metrics`.
///
/// Because it can rewrite field storage (see "Mapped metrics" below), it must appear **before** any `#[derive(...)]`
/// on the struct.
///
/// # Fields
///
/// Every field must be typed as `Counter`, `Gauge`, or `Histogram` (from the `metrics` crate, re-exported by
/// `saluki-metrics`). Each field becomes one registered metric whose name is `"<prefix>_<field_name>"`.
///
/// # Arguments
///
/// - `prefix = "..."` (**required**): the string prefixed to every metric name.
/// - `labels(a, b, ...)` (optional): the names of labels applied to every metric in the group. The label *values*
///   are supplied to the generated `new()` and may be of any type implementing `saluki_metrics::Stringable`.
///
/// # Field attributes
///
/// - `#[metric(level = info | debug | trace)]` (optional, default `info`): the metric's verbosity level.
/// - `#[metric(mapped(...))]` (optional): see below.
///
/// # Generated code
///
/// For a struct `Foo`, this generates `Foo::new(<label>, ...) -> Foo` (generic over each label value's type), a
/// `Foo::<field>(&self) -> &<Handle>` accessor per metric, a `Foo::<field>_name() -> &'static str` helper, and a
/// `Debug` implementation that prints only the struct name. `Clone` is not generated; derive it directly where needed.
///
/// # Mapped metrics
///
/// A field can be *mapped* by one or more labels whose values are supplied at emission time rather than at
/// construction:
///
/// ```ignore
/// #[static_metrics(prefix = "component", labels(component_id))]
/// #[derive(Clone)]
/// struct Metrics {
///     events_sent_total: Counter,
///     // one label, sourced from any `Stringable`:
///     #[metric(mapped(reason))]
///     events_discarded_total: Counter,
///     // or pinned to a concrete type for misuse resistance (and, potentially, cheaper conversion):
///     #[metric(mapped(reason: DiscardReason))]
///     other_total: Counter,
/// }
/// ```
///
/// A mapped field's storage is rewritten to hold a concurrent map of the dynamic label values to lazily registered
/// handles (the source keeps writing `Counter`/`Gauge`/`Histogram`). Its accessor takes the label values by
/// reference and returns an owned handle:
///
/// ```ignore
/// // bare label -> generic parameter; typed label -> the concrete type
/// metrics.events_discarded_total(&"queue_full").increment(1);
/// metrics.other_total(&DiscardReason::QueueFull).increment(1);
/// ```
///
/// Each mapped handle is registered with the fixed struct labels plus the mapped labels. One or many labels may be
/// given, comma-separated, mixing bare and typed forms.
#[proc_macro_attribute]
pub fn static_metrics(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand(attr.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// The metric kind, inferred from a field's declared type.
#[derive(Clone, Copy)]
enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    /// Returns the `metrics` registration macro name for this kind (`counter`/`gauge`/`histogram`).
    fn macro_ident(self) -> Ident {
        match self {
            MetricKind::Counter => format_ident!("counter"),
            MetricKind::Gauge => format_ident!("gauge"),
            MetricKind::Histogram => format_ident!("histogram"),
        }
    }
}

/// The verbosity level of a metric.
#[derive(Clone, Copy)]
enum Level {
    Info,
    Debug,
    Trace,
}

impl Level {
    /// Returns the fully qualified `metrics::Level` variant for this level.
    fn to_tokens(self) -> TokenStream2 {
        match self {
            Level::Info => quote! { ::saluki_metrics::reexport::metrics::Level::INFO },
            Level::Debug => quote! { ::saluki_metrics::reexport::metrics::Level::DEBUG },
            Level::Trace => quote! { ::saluki_metrics::reexport::metrics::Level::TRACE },
        }
    }
}

/// A single mapped label: a bare name (`reason`), optionally pinned to a concrete type (`reason: DiscardReason`).
struct MappedLabel {
    name: Ident,
    ty: Option<Type>,
}

impl Parse for MappedLabel {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name = input.parse()?;
        let ty = if input.peek(Token![:]) {
            input.parse::<Token![:]>()?;
            Some(input.parse()?)
        } else {
            None
        };
        Ok(Self { name, ty })
    }
}

/// A single metric parsed from a struct field.
struct MetricField {
    ident: Ident,
    ty: Type,
    kind: MetricKind,
    level: Level,
    mapped: Vec<MappedLabel>,
}

impl MetricField {
    fn is_mapped(&self) -> bool {
        !self.mapped.is_empty()
    }
}

/// The struct-level configuration parsed from the attribute arguments.
struct Container {
    prefix: LitStr,
    labels: Vec<Ident>,
}

fn parse_container(attr: TokenStream2) -> syn::Result<Container> {
    let mut prefix = None;
    let mut labels = Vec::new();

    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("prefix") {
            prefix = Some(meta.value()?.parse()?);
            Ok(())
        } else if meta.path.is_ident("labels") {
            meta.parse_nested_meta(|label| {
                let ident = label
                    .path
                    .get_ident()
                    .ok_or_else(|| label.error("label names must be simple identifiers"))?
                    .clone();
                labels.push(ident);
                Ok(())
            })
        } else {
            Err(meta.error("unknown `static_metrics` argument; expected `prefix` or `labels`"))
        }
    });
    parser.parse2(attr)?;

    let prefix = prefix.ok_or_else(|| {
        syn::Error::new(
            Span::call_site(),
            "`static_metrics` requires a `prefix = \"...\"` argument",
        )
    })?;

    Ok(Container { prefix, labels })
}

/// Infers the metric kind from a field's type, matching on the last path segment so both `Counter` and
/// `metrics::Counter` resolve.
fn metric_kind(ty: &Type) -> Option<MetricKind> {
    let Type::Path(type_path) = ty else {
        return None;
    };

    match type_path.path.segments.last()?.ident.to_string().as_str() {
        "Counter" => Some(MetricKind::Counter),
        "Gauge" => Some(MetricKind::Gauge),
        "Histogram" => Some(MetricKind::Histogram),
        _ => None,
    }
}

fn parse_field(field: &syn::Field) -> syn::Result<MetricField> {
    let ident = field
        .ident
        .clone()
        .ok_or_else(|| syn::Error::new_spanned(field, "metric fields must be named"))?;
    let ty = field.ty.clone();
    let kind = metric_kind(&ty).ok_or_else(|| {
        syn::Error::new_spanned(
            &field.ty,
            "metric field type must be `Counter`, `Gauge`, or `Histogram`",
        )
    })?;

    let mut level = Level::Info;
    let mut mapped = Vec::new();
    for attr in &field.attrs {
        if !attr.path().is_ident("metric") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("level") {
                let value: Ident = meta.value()?.parse()?;
                level = match value.to_string().as_str() {
                    "info" => Level::Info,
                    "debug" => Level::Debug,
                    "trace" => Level::Trace,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &value,
                            "metric level must be `info`, `debug`, or `trace`",
                        ))
                    }
                };
                Ok(())
            } else if meta.path.is_ident("mapped") {
                let content;
                syn::parenthesized!(content in meta.input);
                let parsed = content.parse_terminated(MappedLabel::parse, Token![,])?;
                if parsed.is_empty() {
                    return Err(meta.error("`mapped(...)` requires at least one label name"));
                }
                mapped.extend(parsed);
                Ok(())
            } else {
                Err(meta.error("unknown `metric` key; expected `level` or `mapped`"))
            }
        })?;
    }

    Ok(MetricField {
        ident,
        ty,
        kind,
        level,
        mapped,
    })
}

fn expand(attr: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    let mut item = syn::parse2::<ItemStruct>(item)?;
    let container = parse_container(attr)?;

    let named = match &item.fields {
        Fields::Named(named) => named,
        _ => {
            return Err(syn::Error::new_spanned(
                &item,
                "`static_metrics` requires a struct with named fields",
            ))
        }
    };
    if named.named.is_empty() {
        return Err(syn::Error::new_spanned(
            &item,
            "`static_metrics` requires at least one metric field",
        ));
    }

    // Parse every field, accumulating errors so multiple problems surface at once.
    let mut errors = Vec::new();
    let mut metrics = Vec::new();
    for field in &named.named {
        match parse_field(field) {
            Ok(metric) => metrics.push(metric),
            Err(error) => errors.push(error),
        }
    }
    if let Some(error) = combine_errors(errors) {
        return Err(error);
    }

    let prefix = container.prefix.value();
    let any_mapped = metrics.iter().any(MetricField::is_mapped);

    // The struct-level labels become generic, `Stringable`-bounded parameters on `new()`.
    let label_generics: Vec<Ident> = (0..container.labels.len()).map(|i| format_ident!("L{}", i)).collect();
    let new_params = container
        .labels
        .iter()
        .zip(&label_generics)
        .map(|(key, generic)| quote! { #key: #generic });
    let new_generics = if label_generics.is_empty() {
        quote! {}
    } else {
        quote! { < #(#label_generics),* > }
    };
    let new_where = if label_generics.is_empty() {
        quote! {}
    } else {
        quote! { where #(#label_generics: ::saluki_metrics::Stringable,)* }
    };
    let label_entries = container.labels.iter().map(|key| {
        let key_str = LitStr::new(&key.to_string(), key.span());
        quote! {
            ::saluki_metrics::reexport::metrics::Label::new(
                #key_str,
                ::saluki_metrics::Stringable::to_shared_string(&#key),
            )
        }
    });
    // When any field is mapped, the fixed label set is shared (via `Arc`) into each mapped field's storage.
    let labels_binding = if any_mapped {
        quote! {
            let labels: ::std::vec::Vec<::saluki_metrics::reexport::metrics::Label> = ::std::vec![ #(#label_entries,)* ];
            let labels = ::std::sync::Arc::new(labels);
        }
    } else {
        quote! {
            let labels: ::std::vec::Vec<::saluki_metrics::reexport::metrics::Label> = ::std::vec![ #(#label_entries,)* ];
        }
    };

    let field_inits = metrics.iter().map(|metric| field_init(metric, &prefix));
    let methods = metrics.iter().map(|metric| field_methods(metric, &prefix));

    // Rewrite the struct: strip the `#[metric(...)]` field attributes (they are not inert for an attribute macro) and
    // rewrite mapped fields' storage to `MappedMetric<Handle>`. Everything else (visibility, other attributes such as
    // `#[derive(Clone)]`, generics) is preserved.
    if let Fields::Named(named) = &mut item.fields {
        for (field, metric) in named.named.iter_mut().zip(&metrics) {
            field.attrs.retain(|attr| !attr.path().is_ident("metric"));
            if metric.is_mapped() {
                let handle_ty = &metric.ty;
                field.ty = parse_quote! { ::saluki_metrics::MappedMetric<#handle_ty> };
            }
        }
    }

    let struct_name = &item.ident;
    let struct_name_str = struct_name.to_string();
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();

    Ok(quote! {
        #item

        impl #impl_generics #struct_name #ty_generics #where_clause {
            pub fn new #new_generics ( #(#new_params),* ) -> Self #new_where {
                #labels_binding

                Self {
                    #(#field_inits,)*
                }
            }

            #(#methods)*
        }

        impl #impl_generics ::std::fmt::Debug for #struct_name #ty_generics #where_clause {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(#struct_name_str)
            }
        }
    })
}

/// Generates the code that sets a field inside `new()`.
fn field_init(metric: &MetricField, prefix: &str) -> TokenStream2 {
    let ident = &metric.ident;
    if metric.is_mapped() {
        quote! {
            #ident: ::saluki_metrics::MappedMetric::new(::std::sync::Arc::clone(&labels))
        }
    } else {
        let macro_ident = metric.kind.macro_ident();
        let level = metric.level.to_tokens();
        let name = LitStr::new(&format!("{}_{}", prefix, ident), ident.span());
        quote! {
            #ident: ::saluki_metrics::reexport::metrics::#macro_ident!(level: #level, #name, labels.iter())
        }
    }
}

/// Generates the accessors and `<field>_name()` helper for a metric field.
fn field_methods(metric: &MetricField, prefix: &str) -> TokenStream2 {
    let ident = &metric.ident;
    let ty = &metric.ty;
    let name_fn = format_ident!("{}_name", ident);
    let name = LitStr::new(&format!("{}_{}", prefix, ident), ident.span());
    let name_doc = format!("Gets the full name of the `{}` metric as it will be registered.", ident);
    let name_helper = quote! {
        #[doc = #name_doc]
        #[doc = ""]
        #[doc = "This can be useful when testing metrics, as it ensures you can grab the correct metric name to search for."]
        pub fn #name_fn() -> &'static str {
            #name
        }
    };

    if !metric.is_mapped() {
        return quote! {
            pub fn #ident(&self) -> &#ty {
                &self.#ident
            }

            #name_helper
        };
    }

    // Mapped accessor: bare labels contribute a generic `Stringable` parameter; typed labels use their concrete type.
    let mut generics = Vec::new();
    let mut params = Vec::new();
    let mut key_lits = Vec::new();
    let mut value_exprs = Vec::new();
    for label in &metric.mapped {
        let label_name = &label.name;
        key_lits.push(LitStr::new(&label_name.to_string(), label_name.span()));
        value_exprs.push(quote! { ::saluki_metrics::Stringable::to_shared_string(#label_name) });
        match &label.ty {
            Some(ty) => params.push(quote! { #label_name: &#ty }),
            None => {
                let generic = format_ident!("L{}", generics.len());
                params.push(quote! { #label_name: &#generic });
                generics.push(generic);
            }
        }
    }

    let getter_generics = if generics.is_empty() {
        quote! {}
    } else {
        quote! { < #(#generics),* > }
    };
    let getter_where = if generics.is_empty() {
        quote! {}
    } else {
        quote! { where #(#generics: ::saluki_metrics::Stringable,)* }
    };

    let macro_ident = metric.kind.macro_ident();
    let level = metric.level.to_tokens();

    quote! {
        pub fn #ident #getter_generics (&self, #(#params),*) -> #ty #getter_where {
            let values = [ #(#value_exprs,)* ];
            self.#ident.get_or_register(
                &[ #(#key_lits,)* ],
                &values,
                |labels| ::saluki_metrics::reexport::metrics::#macro_ident!(level: #level, #name, labels.iter()),
            )
        }

        #name_helper
    }
}

/// Combines a collection of errors into a single error, if any are present.
fn combine_errors(errors: Vec<syn::Error>) -> Option<syn::Error> {
    let mut iter = errors.into_iter();
    let mut combined = iter.next()?;
    for error in iter {
        combined.combine(error);
    }
    Some(combined)
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::*;

    /// Expands the input and returns the generated tokens with all whitespace removed, so assertions can match token
    /// sequences without depending on `TokenStream`'s inter-token spacing.
    fn expand_str(attr: TokenStream2, item: TokenStream2) -> String {
        expand(attr, item)
            .expect("expansion failed")
            .to_string()
            .replace(char::is_whitespace, "")
    }

    fn expand_err(attr: TokenStream2, item: TokenStream2) -> String {
        expand(attr, item)
            .expect_err("expansion should have failed")
            .to_string()
    }

    #[test]
    fn generates_names_levels_accessors_and_debug() {
        let out = expand_str(
            quote! { prefix = "cache", labels(id) },
            quote! {
                struct Telemetry {
                    hits_total: Counter,
                    #[metric(level = debug)]
                    items_inserted_total: Counter,
                }
            },
        );

        assert!(out.contains("\"cache_hits_total\""));
        assert!(out.contains("\"cache_items_inserted_total\""));
        assert!(out.contains("Level::INFO"));
        assert!(out.contains("Level::DEBUG"));
        assert!(out.contains("fnnew<L0>"));
        assert!(out.contains("hits_total_name"));
        assert!(out.contains("write_str(\"Telemetry\")"));
        // Nothing is mapped, so no map storage is emitted.
        assert!(!out.contains("MappedMetric"));
    }

    #[test]
    fn mapped_bare_label_generates_generic_getter() {
        let out = expand_str(
            quote! { prefix = "component", labels(component_id) },
            quote! {
                struct Telemetry {
                    #[metric(mapped(reason))]
                    events_discarded_total: Counter,
                }
            },
        );

        // The field storage is rewritten to a `MappedMetric`, and the fixed labels are shared via `Arc`.
        assert!(out.contains("MappedMetric<Counter>"));
        assert!(out.contains("Arc::new(labels)"));
        // The accessor is generic over the bare label's value type and resolves the handle lazily.
        assert!(out.contains("fnevents_discarded_total<L0>"));
        assert!(out.contains("reason:&L0"));
        assert!(out.contains("get_or_register"));
        assert!(out.contains("\"reason\""));
    }

    #[test]
    fn mapped_typed_label_uses_concrete_type() {
        let out = expand_str(
            quote! { prefix = "component" },
            quote! {
                struct Telemetry {
                    #[metric(mapped(reason: DiscardReason))]
                    events_discarded_total: Counter,
                }
            },
        );

        // A typed-only mapped label pins the parameter to the concrete type and emits no getter generic.
        assert!(out.contains("fnevents_discarded_total(&self,reason:&DiscardReason)"));
    }

    #[test]
    fn mapped_mixed_multi_label() {
        let out = expand_str(
            quote! { prefix = "component" },
            quote! {
                struct Telemetry {
                    #[metric(mapped(origin, reason: DiscardReason))]
                    events_discarded_total: Counter,
                }
            },
        );

        assert!(out.contains("fnevents_discarded_total<L0>"));
        assert!(out.contains("origin:&L0"));
        assert!(out.contains("reason:&DiscardReason"));
        assert!(out.contains("\"origin\""));
        assert!(out.contains("\"reason\""));
    }

    #[test]
    fn rejects_warn_level() {
        let err = expand_err(
            quote! { prefix = "p" },
            quote! { struct M { #[metric(level = warn)] bar: Counter } },
        );
        assert_eq!(err, "metric level must be `info`, `debug`, or `trace`");
    }

    #[test]
    fn rejects_unknown_field_type() {
        let err = expand_err(quote! { prefix = "p" }, quote! { struct M { bar: String } });
        assert_eq!(err, "metric field type must be `Counter`, `Gauge`, or `Histogram`");
    }

    #[test]
    fn requires_prefix() {
        let err = expand_err(quote! { labels(id) }, quote! { struct M { bar: Counter } });
        assert_eq!(err, "`static_metrics` requires a `prefix = \"...\"` argument");
    }

    #[test]
    fn rejects_non_struct() {
        let err = expand_err(quote! { prefix = "p" }, quote! { enum E { A } });
        assert_eq!(err, "expected `struct`");
    }

    #[test]
    fn requires_at_least_one_field() {
        let err = expand_err(quote! { prefix = "p" }, quote! { struct M {} });
        assert_eq!(err, "`static_metrics` requires at least one metric field");
    }

    #[test]
    fn rejects_empty_mapped() {
        let err = expand_err(
            quote! { prefix = "p" },
            quote! { struct M { #[metric(mapped())] bar: Counter } },
        );
        assert_eq!(err, "`mapped(...)` requires at least one label name");
    }
}

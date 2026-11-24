//! Procedural macros for `saluki-metrics`.
//!
//! This crate provides the [`static_metrics`] attribute macro. It is an implementation detail of `saluki-metrics` and
//! should be used through the re-export there (`saluki_metrics::static_metrics`) rather than depended on directly.

#![deny(missing_docs)]

use proc_macro::TokenStream;
use proc_macro2::{Literal, Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    parse::{Parse, ParseStream, Parser},
    parse_quote,
    spanned::Spanned,
    Fields, Ident, ItemStruct, LitStr, Token, Type,
};

/// Defines a container struct of statically defined metrics.
///
/// Applied to a struct whose fields are metric handles, this generates the registration, accessors, and metadata for
/// the whole group. It is re-exported (and intended to be used) as `saluki_metrics::static_metrics`.
///
/// Because it can rewrite field storage and add hidden fields (see "Mapped metrics" and "Derived gauges" below), it
/// must appear **before** any `#[derive(...)]` on the struct.
///
/// # Fields
///
/// Every field must be typed as `Counter`, `Gauge`, or `Histogram` (from the `metrics` crate, re-exported by
/// `saluki-metrics`). Each field becomes one registered metric whose name is `"<prefix>_<field_name>"`. A derived
/// histogram also registers one gauge for each derived value.
///
/// # Arguments
///
/// - `prefix = <ident>` (**required**): a bare identifier prefixed to every metric name, for example `prefix = cache`.
/// - `labels(a, b, ...)` (optional): the names of labels applied to every metric in the group. The label *values*
///   are supplied to the generated `new()` and may be of any type implementing `saluki_metrics::Stringable`.
///
/// # Field attributes
///
/// - `#[metric(level = info | debug | trace)]` (optional, default `info`): the metric's verbosity level.
/// - `#[metric(mapped(...))]` (optional): see "Mapped metrics" below.
/// - `#[metric(derived(...))]` (optional, `Histogram` fields only): see "Derived gauges" below.
///
/// # Generated code
///
/// For a struct `Foo`, this generates `Foo::new(<label>, ...) -> Foo` (generic over each label value's type), a
/// `Foo::<field>(&self) -> &<Handle>` accessor per metric, a `Foo::<field>_name() -> &'static str` helper, and a
/// `Debug` implementation that prints only the struct name. `Clone` is not generated; derive it directly where needed.
/// Mapped and derived fields get different accessors, which their sections below describe.
///
/// # Mapped metrics
///
/// A field can be *mapped* by one or more labels whose values are supplied at emission time rather than at
/// construction:
///
/// <!-- vale off -->
/// ```ignore
/// #[static_metrics(prefix = component, labels(component_id))]
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
/// <!-- vale on -->
///
/// A mapped field's storage is rewritten to hold a concurrent map of the dynamic label values to lazily registered
/// handles (the source keeps writing `Counter`/`Gauge`/`Histogram`). Its accessor accepts each label value either by
/// value or by reference and returns an owned handle. A typed label still only accepts its concrete type (or a
/// reference to it), so lightweight `Copy` values can be passed directly without borrowing:
///
/// ```ignore
/// // bare label -> generic `Stringable` parameter; typed label -> `Borrow<T>`, so `T` or `&T` are both accepted
/// metrics.events_discarded_total("queue_full").increment(1);
/// metrics.other_total(DiscardReason::QueueFull).increment(1);
/// metrics.other_total(&DiscardReason::QueueFull).increment(1);
/// ```
///
/// Each mapped handle is registered with the fixed struct labels plus the mapped labels. One or many labels may be
/// given, comma-separated, mixing bare and typed forms.
///
/// # Derived gauges
///
/// A `Histogram` field can track the lowest value, the highest value, or both, that it records:
///
/// <!-- vale off -->
/// ```ignore
/// #[static_metrics(prefix = component, labels(component_id))]
/// #[derive(Clone)]
/// struct Metrics {
///     #[metric(derived(max))]
///     payload_size_bytes: Histogram,
///     #[metric(derived(min, max))]
///     latency_seconds: Histogram,
/// }
/// ```
/// <!-- vale on -->
///
/// Each derived value registers a gauge named `"<prefix>_<field_name>_min"` or `"<prefix>_<field_name>_max"`, with the
/// same level and labels as the histogram. The `Foo::<field>_min_name()` and `Foo::<field>_max_name()` helpers return
/// these names. The macro adds one hidden field, `__<field_name>_derived`, at the end of the struct to hold the gauges.
///
/// The accessor returns a `saluki_metrics::DerivedHistogram<'_>`. Its `record` and `record_many` methods record the
/// value to the histogram, then set each gauge only if the value is a new lowest or highest value:
///
/// ```ignore
/// metrics.payload_size_bytes().record(body_len as f64);
/// ```
///
/// - The gauges track values for the life of the struct, and never reset.
/// - Clones share the tracked values. Separate `new()` calls with the same labels do not, and can move the shared
///   gauges back. Create the struct once for each label set, and clone it.
/// - `new()` registers the gauges. With the internal recorder, a gauge reads 0 until the histogram records a value.
/// - `NaN` values go to the histogram, but never change a gauge.
/// - Always record through the accessor. Code that can see the `Histogram` field can record to it directly, and that
///   skips the gauges.
/// - A field can't be both mapped and derived.
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

/// A gauge derived from a histogram: the lowest or highest value recorded to it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DerivedKind {
    Min,
    Max,
}

impl DerivedKind {
    /// Returns the gauge name suffix, which is also the name of the `DerivedGauge` constructor.
    fn as_str(self) -> &'static str {
        match self {
            DerivedKind::Min => "min",
            DerivedKind::Max => "max",
        }
    }
}

/// A single metric parsed from a struct field.
struct MetricField {
    ident: Ident,
    ty: Type,
    kind: MetricKind,
    level: Level,
    mapped: Vec<MappedLabel>,
    /// Derived gauges, in declaration order, with the span of each `min`/`max` token.
    derived: Vec<(DerivedKind, Span)>,
}

impl MetricField {
    fn is_mapped(&self) -> bool {
        !self.mapped.is_empty()
    }

    fn is_derived(&self) -> bool {
        !self.derived.is_empty()
    }

    /// Returns the name of the hidden field that holds this field's derived gauges.
    fn derived_storage_ident(&self) -> Ident {
        format_ident!("__{}_derived", self.ident)
    }

    /// Returns the field-relative name of a derived gauge, such as `payload_size_bytes_max`.
    fn derived_suffix(&self, kind: DerivedKind) -> String {
        format!("{}_{}", self.ident, kind.as_str())
    }
}

/// The struct-level configuration parsed from the attribute arguments.
struct Container {
    prefix: Ident,
    labels: Vec<Ident>,
}

fn parse_container(attr: TokenStream2) -> syn::Result<Container> {
    let mut prefix = None;
    let mut labels = Vec::new();

    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("prefix") {
            // The prefix is a bare identifier, matching the declarative `static_metrics!` macro (`prefix => cache`).
            // Parsing it as an `Ident` deliberately rejects a quoted `prefix = "cache"`.
            let value: Ident = meta.value()?.parse()?;
            prefix = Some(value);
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
            "`static_metrics` requires a `prefix = <ident>` argument",
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
    let mut derived: Vec<(DerivedKind, Span)> = Vec::new();
    let mut derived_span = None;
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
            } else if meta.path.is_ident("derived") {
                if !matches!(kind, MetricKind::Histogram) {
                    return Err(meta.error("`derived(...)` is only supported on `Histogram` fields"));
                }
                derived_span.get_or_insert(meta.path.span());

                // `parse_nested_meta` reports a bare key or an empty list as a generic error, so check for both first.
                let empty = !meta.input.peek(syn::token::Paren) || {
                    let fork = meta.input.fork();
                    let content;
                    syn::parenthesized!(content in fork);
                    content.is_empty()
                };
                if empty {
                    return Err(meta.error("`derived(...)` requires `min`, `max`, or both"));
                }

                meta.parse_nested_meta(|entry| {
                    let derived_kind = if entry.path.is_ident("min") {
                        DerivedKind::Min
                    } else if entry.path.is_ident("max") {
                        DerivedKind::Max
                    } else {
                        return Err(entry.error("unknown derived value; expected `min` or `max`"));
                    };
                    if derived.iter().any(|(existing, _)| *existing == derived_kind) {
                        return Err(entry.error(format!("duplicate derived value `{}`", derived_kind.as_str())));
                    }
                    derived.push((derived_kind, entry.path.span()));
                    Ok(())
                })
            } else {
                Err(meta.error("unknown `metric` key; expected `level`, `mapped`, or `derived`"))
            }
        })?;
    }

    if let Some(span) = derived_span {
        if !mapped.is_empty() {
            return Err(syn::Error::new(
                span,
                "`derived(...)` cannot be combined with `mapped(...)`",
            ));
        }
    }

    Ok(MetricField {
        ident,
        ty,
        kind,
        level,
        mapped,
        derived,
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
    check_derived_collisions(&container.prefix, &metrics, &mut errors);
    if let Some(error) = combine_errors(errors) {
        return Err(error);
    }

    let prefix = container.prefix.to_string();
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
    let derived_inits = metrics
        .iter()
        .filter(|metric| metric.is_derived())
        .map(|metric| derived_init(metric, &prefix));
    let methods = metrics.iter().map(|metric| field_methods(metric, &prefix));

    // Rewrite the struct: strip the `#[metric(...)]` field attributes (they are not inert for an attribute macro),
    // rewrite mapped fields' storage to `MappedMetric<Handle>`, and add one hidden field per derived histogram to hold
    // its gauges. Everything else (visibility, other attributes such as `#[derive(Clone)]`, generics) is preserved.
    if let Fields::Named(named) = &mut item.fields {
        for (field, metric) in named.named.iter_mut().zip(&metrics) {
            field.attrs.retain(|attr| !attr.path().is_ident("metric"));
            if metric.is_mapped() {
                let handle_ty = &metric.ty;
                field.ty = parse_quote! { ::saluki_metrics::MappedMetric<#handle_ty> };
            }
        }
        for metric in metrics.iter().filter(|metric| metric.is_derived()) {
            let storage = metric.derived_storage_ident();
            let len = Literal::usize_unsuffixed(metric.derived.len());
            named
                .named
                .push(parse_quote! { #storage: [::saluki_metrics::DerivedGauge; #len] });
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
                    #(#derived_inits,)*
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

/// Generates the code that sets the hidden derived-gauge field of a derived histogram inside `new()`.
fn derived_init(metric: &MetricField, prefix: &str) -> TokenStream2 {
    let storage = metric.derived_storage_ident();
    let level = metric.level.to_tokens();
    let gauges = metric.derived.iter().map(|(kind, _)| {
        let ctor = format_ident!("{}", kind.as_str());
        let name = LitStr::new(
            &format!("{}_{}", prefix, metric.derived_suffix(*kind)),
            metric.ident.span(),
        );
        quote! {
            ::saluki_metrics::DerivedGauge::#ctor(
                ::saluki_metrics::reexport::metrics::gauge!(level: #level, #name, labels.iter())
            )
        }
    });
    quote! {
        #storage: [ #(#gauges),* ]
    }
}

/// Reports fields whose names clash with the names that derived gauges add: the gauge metric name and its
/// `<field>_<kind>_name()` helper, and the hidden field that holds the gauges.
fn check_derived_collisions(prefix: &Ident, metrics: &[MetricField], errors: &mut Vec<syn::Error>) {
    let field_names: Vec<String> = metrics.iter().map(|metric| metric.ident.to_string()).collect();
    for metric in metrics.iter().filter(|metric| metric.is_derived()) {
        for (kind, span) in &metric.derived {
            let suffix = metric.derived_suffix(*kind);
            let helper = format!("{}_name", suffix);
            if field_names.contains(&suffix) {
                errors.push(syn::Error::new(
                    *span,
                    format!(
                        "derived gauge `{}_{}` has the same name as the metric for field `{}`",
                        prefix, suffix, suffix
                    ),
                ));
            } else if field_names.contains(&helper) {
                errors.push(syn::Error::new(
                    *span,
                    format!(
                        "derived name helper `{}()` has the same name as the accessor for field `{}`",
                        helper, helper
                    ),
                ));
            }
        }

        let storage = metric.derived_storage_ident().to_string();
        if let Some(clash) = metrics.iter().find(|other| other.ident == storage) {
            errors.push(syn::Error::new(
                clash.ident.span(),
                format!(
                    "field `{}` has the same name as the hidden field that holds the gauges derived from `{}`",
                    storage, metric.ident
                ),
            ));
        }
    }
}

/// Generates the accessors and `<field>_name()` helper for a metric field.
fn field_methods(metric: &MetricField, prefix: &str) -> TokenStream2 {
    let ident = &metric.ident;
    let ty = &metric.ty;
    let name_fn = format_ident!("{}_name", ident);
    let name = LitStr::new(&format!("{}_{}", prefix, ident), ident.span());
    let name_doc = format!(
        "Returns the full name of the `{}` metric that `new()` registers.",
        ident
    );
    let name_helper = quote! {
        #[doc = #name_doc]
        #[doc = ""]
        #[doc = "This can be useful when testing metrics, as it ensures you can grab the correct metric name to search for."]
        pub fn #name_fn() -> &'static str {
            #name
        }
    };

    if metric.is_derived() {
        let storage = metric.derived_storage_ident();
        let derived_helpers = metric.derived.iter().map(|(kind, _)| {
            let suffix = metric.derived_suffix(*kind);
            let helper_fn = format_ident!("{}_name", suffix);
            let gauge_name = LitStr::new(&format!("{}_{}", prefix, suffix), ident.span());
            let helper_doc = format!(
                "Returns the full name of the `{}` gauge that `new()` registers for the `{}` metric.",
                kind.as_str(),
                ident
            );
            quote! {
                #[doc = #helper_doc]
                pub fn #helper_fn() -> &'static str {
                    #gauge_name
                }
            }
        });

        return quote! {
            pub fn #ident(&self) -> ::saluki_metrics::DerivedHistogram<'_> {
                ::saluki_metrics::DerivedHistogram::new(&self.#ident, &self.#storage)
            }

            #name_helper

            #(#derived_helpers)*
        };
    }

    if !metric.is_mapped() {
        return quote! {
            pub fn #ident(&self) -> &#ty {
                &self.#ident
            }

            #name_helper
        };
    }

    // Mapped accessor: every label is accepted by value *or* by reference through a generic parameter, so lightweight
    // `Copy` values (enums, integers) need not be borrowed at the call site. A bare label's parameter is bounded by
    // `Stringable` directly; a typed label's parameter is bounded by `Borrow<T>`, which keeps the concrete type pinned
    // (for misuse resistance) while accepting both `T` and `&T` (`T: Borrow<T>` and `&T: Borrow<T>` both hold). In
    // either case we only ever need a shared reference to stringify the value via its `Display` impl.
    let mut generics = Vec::new();
    let mut bounds = Vec::new();
    let mut params = Vec::new();
    let mut key_lits = Vec::new();
    let mut value_exprs = Vec::new();
    for label in &metric.mapped {
        let label_name = &label.name;
        key_lits.push(LitStr::new(&label_name.to_string(), label_name.span()));

        let generic = format_ident!("L{}", generics.len());
        params.push(quote! { #label_name: #generic });
        match &label.ty {
            Some(ty) => {
                bounds.push(quote! { #generic: ::std::borrow::Borrow<#ty> });
                value_exprs.push(quote! {
                    ::saluki_metrics::Stringable::to_shared_string(::std::borrow::Borrow::borrow(&#label_name))
                });
            }
            None => {
                bounds.push(quote! { #generic: ::saluki_metrics::Stringable });
                value_exprs.push(quote! { ::saluki_metrics::Stringable::to_shared_string(&#label_name) });
            }
        }
        generics.push(generic);
    }

    let getter_generics = if generics.is_empty() {
        quote! {}
    } else {
        quote! { < #(#generics),* > }
    };
    let getter_where = if bounds.is_empty() {
        quote! {}
    } else {
        quote! { where #(#bounds,)* }
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
            quote! { prefix = cache, labels(id) },
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
            quote! { prefix = component, labels(component_id) },
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
        // The accessor is generic over the bare label's value type (accepted by value or reference) and resolves the
        // handle lazily.
        assert!(out.contains("fnevents_discarded_total<L0>"));
        assert!(out.contains("reason:L0"));
        assert!(out.contains("L0:::saluki_metrics::Stringable"));
        assert!(out.contains("get_or_register"));
        assert!(out.contains("\"reason\""));
    }

    #[test]
    fn mapped_typed_label_uses_concrete_type() {
        let out = expand_str(
            quote! { prefix = component },
            quote! {
                struct Telemetry {
                    #[metric(mapped(reason: DiscardReason))]
                    events_discarded_total: Counter,
                }
            },
        );

        // A typed mapped label pins the parameter through `Borrow<T>`, so both `T` and `&T` are accepted at the call
        // site while the concrete type stays fixed.
        assert!(out.contains("fnevents_discarded_total<L0>(&self,reason:L0)"));
        assert!(out.contains("L0:::std::borrow::Borrow<DiscardReason>"));
    }

    #[test]
    fn mapped_mixed_multi_label() {
        let out = expand_str(
            quote! { prefix = component },
            quote! {
                struct Telemetry {
                    #[metric(mapped(origin, reason: DiscardReason))]
                    events_discarded_total: Counter,
                }
            },
        );

        assert!(out.contains("fnevents_discarded_total<L0,L1>"));
        assert!(out.contains("origin:L0"));
        assert!(out.contains("reason:L1"));
        assert!(out.contains("L0:::saluki_metrics::Stringable"));
        assert!(out.contains("L1:::std::borrow::Borrow<DiscardReason>"));
        assert!(out.contains("\"origin\""));
        assert!(out.contains("\"reason\""));
    }

    #[test]
    fn rejects_warn_level() {
        let err = expand_err(
            quote! { prefix = p },
            quote! { struct M { #[metric(level = warn)] bar: Counter } },
        );
        assert_eq!(err, "metric level must be `info`, `debug`, or `trace`");
    }

    #[test]
    fn rejects_unknown_field_type() {
        let err = expand_err(quote! { prefix = p }, quote! { struct M { bar: String } });
        assert_eq!(err, "metric field type must be `Counter`, `Gauge`, or `Histogram`");
    }

    #[test]
    fn requires_prefix() {
        let err = expand_err(quote! { labels(id) }, quote! { struct M { bar: Counter } });
        assert_eq!(err, "`static_metrics` requires a `prefix = <ident>` argument");
    }

    #[test]
    fn rejects_string_literal_prefix() {
        // The prefix must be a bare identifier (`prefix = cache`), mirroring the declarative `static_metrics!` macro; a
        // quoted `prefix = "cache"` is rejected during parsing.
        let err = expand_err(quote! { prefix = "cache" }, quote! { struct M { bar: Counter } });
        assert!(err.contains("expected identifier"), "unexpected error message: {err}");
    }

    #[test]
    fn rejects_non_struct() {
        let err = expand_err(quote! { prefix = p }, quote! { enum E { A } });
        assert_eq!(err, "expected `struct`");
    }

    #[test]
    fn requires_at_least_one_field() {
        let err = expand_err(quote! { prefix = p }, quote! { struct M {} });
        assert_eq!(err, "`static_metrics` requires at least one metric field");
    }

    #[test]
    fn rejects_empty_mapped() {
        let err = expand_err(
            quote! { prefix = p },
            quote! { struct M { #[metric(mapped())] bar: Counter } },
        );
        assert_eq!(err, "`mapped(...)` requires at least one label name");
    }

    #[test]
    fn derived_max_generates_gauge_hidden_field_and_wrapper_accessor() {
        let out = expand_str(
            quote! { prefix = component, labels(component_id) },
            quote! {
                struct Telemetry {
                    #[metric(derived(max))]
                    payload_size_bytes: Histogram,
                }
            },
        );

        // The histogram keeps its type and registration, and a hidden field holds the derived gauge.
        assert!(out.contains("payload_size_bytes:Histogram,"));
        assert!(out.contains("__payload_size_bytes_derived:[::saluki_metrics::DerivedGauge;1]"));
        assert!(out.contains("histogram!(level:::saluki_metrics::reexport::metrics::Level::INFO,\"component_payload_size_bytes\",labels.iter())"));
        assert!(out.contains("__payload_size_bytes_derived:[::saluki_metrics::DerivedGauge::max(::saluki_metrics::reexport::metrics::gauge!(level:::saluki_metrics::reexport::metrics::Level::INFO,\"component_payload_size_bytes_max\",labels.iter()))]"));
        // The accessor returns the wrapper. The explicit `'_` avoids `mismatched_lifetime_syntaxes`.
        assert!(out.contains("pubfnpayload_size_bytes(&self)->::saluki_metrics::DerivedHistogram<'_>{::saluki_metrics::DerivedHistogram::new(&self.payload_size_bytes,&self.__payload_size_bytes_derived)}"));
        assert!(out.contains("pubfnpayload_size_bytes_name()->&'staticstr{\"component_payload_size_bytes\"}"));
        assert!(out.contains("pubfnpayload_size_bytes_max_name()->&'staticstr{\"component_payload_size_bytes_max\"}"));
        assert!(!out.contains("payload_size_bytes_min"));
        assert!(!out.contains("#[metric"));
    }

    #[test]
    fn derived_min_and_max_share_the_histogram_level() {
        let out = expand_str(
            quote! { prefix = p },
            quote! {
                struct Telemetry {
                    #[metric(level = trace, derived(min, max))]
                    latency_seconds: Histogram,
                }
            },
        );

        assert_eq!(out.matches("Level::TRACE").count(), 3);
        assert!(out.contains("__latency_seconds_derived:[::saluki_metrics::DerivedGauge;2]"));
        assert!(out.contains("DerivedGauge::min(::saluki_metrics::reexport::metrics::gauge!(level:::saluki_metrics::reexport::metrics::Level::TRACE,\"p_latency_seconds_min\""));
        assert!(out.contains("DerivedGauge::max(::saluki_metrics::reexport::metrics::gauge!(level:::saluki_metrics::reexport::metrics::Level::TRACE,\"p_latency_seconds_max\""));
        assert!(out.contains("pubfnlatency_seconds_min_name()->&'staticstr{\"p_latency_seconds_min\"}"));
        assert!(out.contains("pubfnlatency_seconds_max_name()->&'staticstr{\"p_latency_seconds_max\"}"));
    }

    #[test]
    fn derived_next_to_mapped_field_uses_shared_labels() {
        let out = expand_str(
            quote! { prefix = p, labels(id) },
            quote! {
                struct Telemetry {
                    #[metric(mapped(reason))]
                    dropped_total: Counter,
                    #[metric(derived(max))]
                    size_bytes: Histogram,
                }
            },
        );

        assert!(out.contains("Arc::new(labels)"));
        assert!(out.contains("MappedMetric<Counter>"));
        assert!(!out.contains("MappedMetric<Histogram>"));
        assert!(out.contains("\"p_size_bytes_max\",labels.iter()"));
    }

    #[test]
    fn plain_histogram_accessor_is_unchanged() {
        let out = expand_str(quote! { prefix = p }, quote! { struct M { h: Histogram } });

        assert!(out.contains("pubfnh(&self)->&Histogram{&self.h}"));
        assert!(!out.contains("Derived"));
    }

    #[test]
    fn rejects_invalid_derived() {
        let cases = [
            (
                "empty list",
                quote! { struct M { #[metric(derived())] h: Histogram } },
                "`derived(...)` requires `min`, `max`, or both",
            ),
            (
                "bare key",
                quote! { struct M { #[metric(derived)] h: Histogram } },
                "`derived(...)` requires `min`, `max`, or both",
            ),
            (
                "unknown value",
                quote! { struct M { #[metric(derived(avg))] h: Histogram } },
                "unknown derived value; expected `min` or `max`",
            ),
            (
                "duplicate in one attribute",
                quote! { struct M { #[metric(derived(max, max))] h: Histogram } },
                "duplicate derived value `max`",
            ),
            (
                "duplicate across attributes",
                quote! { struct M { #[metric(derived(max))] #[metric(derived(min, max))] h: Histogram } },
                "duplicate derived value `max`",
            ),
            (
                "counter field",
                quote! { struct M { #[metric(derived(max))] c: Counter } },
                "`derived(...)` is only supported on `Histogram` fields",
            ),
            (
                "gauge field",
                quote! { struct M { #[metric(derived(min))] g: Gauge } },
                "`derived(...)` is only supported on `Histogram` fields",
            ),
            (
                "mapped after derived",
                quote! { struct M { #[metric(derived(max), mapped(reason))] h: Histogram } },
                "`derived(...)` cannot be combined with `mapped(...)`",
            ),
            (
                "mapped before derived",
                quote! { struct M { #[metric(mapped(reason))] #[metric(derived(max))] h: Histogram } },
                "`derived(...)` cannot be combined with `mapped(...)`",
            ),
            (
                "gauge name collision",
                quote! { struct M { #[metric(derived(max))] size: Histogram, size_max: Gauge } },
                "derived gauge `p_size_max` has the same name as the metric for field `size_max`",
            ),
            (
                "name helper collision",
                quote! { struct M { #[metric(derived(max))] size: Histogram, size_max_name: Counter } },
                "derived name helper `size_max_name()` has the same name as the accessor for field `size_max_name`",
            ),
            (
                "hidden field collision",
                quote! { struct M { #[metric(derived(max))] size: Histogram, __size_derived: Counter } },
                "field `__size_derived` has the same name as the hidden field that holds the gauges derived from `size`",
            ),
            (
                "unknown key",
                quote! { struct M { #[metric(foo)] h: Histogram } },
                "unknown `metric` key; expected `level`, `mapped`, or `derived`",
            ),
        ];

        for (case, item, expected) in cases {
            let err = expand_err(quote! { prefix = p }, item);
            assert_eq!(err, expected, "case: {case}");
        }
    }
}

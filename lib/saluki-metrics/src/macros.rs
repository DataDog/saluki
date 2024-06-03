#[doc(hidden)]
#[macro_export]
macro_rules! metric_type_from_lower {
    (counter) => {
        ::metrics::Counter
    };
    (gauge) => {
        ::metrics::Gauge
    };
    (histogram) => {
        ::metrics::Histogram
    };
    ($($other:tt)*) => {
        ()
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! register_metric {
    (counter, $metric_name:expr, $labels:expr) => {
        ::metrics::counter!($metric_name, $labels)
    };
    (gauge, $metric_name:expr, $labels:expr) => {
        ::metrics::gauge!($metric_name, $labels)
    };
    (histogram, $metric_name:expr, $labels:expr) => {
        ::metrics::histogram!($metric_name, $labels)
    };
    ($($other:tt)*) => {
        compile_error!("metric type must be `counter`, `gauge`, or `histogram`");
    };
}

/// Creates statically-defined metrics within a dedicated container struct.
///
/// In some cases, the metrics needed for a component are well-established and do not require a
/// high-level of dynamism: perhaps only a single label is needed for all metrics, and the value is
/// known ahead of time. In these cases, it can be useful to declare the metrics up front, and in a
/// contained way, to avoid having to deal with the string-y metric names (and any labels) at each
/// and every callsite.
///
/// `static_metrics!` allows defining a number of metrics contained within a single struct. The
/// struct can be initialized with a fixed set of labels, which is used when registering the
/// metrics. The metrics can then be accessed with simple accessors on the struct, allowing for
/// ergonomic access from the calling code.
///
/// ## Labels
///
/// A fixed set of labels can be configured for all metrics that are registered. These labels have their definition
/// defined when calling `static_metrics!`, and the label value is provided when initializing the generated struct.
///
/// This allows for quickly applying the same set of labels to all metrics defined within the container struct, and
/// being able to handle them in a strong-typed way right up until the moment where they need to be rendered as strings.
///
/// ## Example
///
/// ```rust
/// # use saluki_metrics::static_metrics;
/// // We are required to provide a name for the struct, as well as the metric prefix to apply to each of the defined metrics.
/// //
/// // Naturally, we also have to define metrics, but labels are optionally and can be excluded from the macro usage entirely.
/// static_metrics!(
///    name => FrobulatorMetrics,
///    prefix => frobulator,
///    labels => [process_id: u32],
///    metrics => [
///        counter(successful_frobulations),
///    ],
/// );
///
/// struct Frobulator {
///     metrics: FrobulatorMetrics,
/// }
///
/// impl Frobulator {
///     fn new(process_id: u32) -> Self {
///         Self {
///            metrics: FrobulatorMetrics::new(process_id),
///         }
///     }
///
///     fn frobulate(&self) {
///         /* Do the frobulation...*/
///         self.metrics.successful_frobulations().increment(1)
///     }
/// }
/// ```
#[macro_export]
macro_rules! static_metrics {
    (name => $name:ident, prefix => $prefix:ident, metrics => [$($metric_type:ident($metric_name:ident)),+ $(,)?] $(,)?) => {
        static_metrics!(name => $name, prefix => $prefix, labels => [], metrics => [$($metric_type($metric_name)),+]);
    };
    (name => $name:ident, prefix => $prefix:ident, labels => [$($label_key:ident: $label_ty:ty),*], metrics => [$($metric_type:ident($metric_name:ident)),+ $(,)?] $(,)?) => {
        #[derive(Clone)]
        struct $name {
            $(
                $metric_name: $crate::metric_type_from_lower!($metric_type),
            )*
        }

        impl $name {
            pub fn new($($label_key: $label_ty,)*) -> Self
            where
                Self: Sized,
            $(
                $label_ty: $crate::Stringable,
            )*
            {
                #[allow(unused_imports)]
                use $crate::Stringable;

                let labels = vec![
                    $(
                        ::metrics::Label::new(stringify!($label_key), $label_key.to_shared_string()),
                    )*
                ];

                Self {
                $(
                    $metric_name: $crate::register_metric!($metric_type, concat!(stringify!($prefix), "_", stringify!($metric_name)), labels.iter()),
                )*
                }
            }

            $(
                pub fn $metric_name(&self) -> &$crate::metric_type_from_lower!($metric_type) {
                    &self.$metric_name
                }
            )*
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, stringify!($name))
            }
        }
    };
}

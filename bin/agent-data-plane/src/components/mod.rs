pub mod apm_onboarding;
// The legacy metric transforms, replaced by kernels and routing operators that regions of the plan-driven topology run.
// They are compiled only for tests, as differential references for the plan-driven metrics path.
#[cfg(test)]
pub mod dogstatsd_no_agg_split;
#[cfg(test)]
pub mod dogstatsd_post_aggregate_filter;
#[cfg(test)]
pub mod dogstatsd_prefix_filter;
pub mod host_tags;
pub mod liveness;
#[cfg(test)]
pub mod metric_filter;
pub mod ottl_filter_processor;
pub mod ottl_transform_processor;
pub(crate) mod static_tags;
pub mod tag_filterlist;

#[cfg(test)]
mod test_support;

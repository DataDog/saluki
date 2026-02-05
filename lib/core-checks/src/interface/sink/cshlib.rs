// use std::collections::HashMap;
// use std::ffi::{CStr, CString};
// use std::str::FromStr;

// use anyhow::{Context, anyhow};
// use async_trait::async_trait;
// use libc::{c_char, c_double, c_float, c_int, c_longlong, c_ulong};

// use super::*;

// #[repr(C)]
// pub struct Event {
//     pub title: *const c_char,
//     pub text: *const c_char,
//     pub ts: c_ulong,
//     pub priority: *const c_char,
//     pub host: *const c_char,
//     pub tags: *const *const c_char,
//     pub alert_type: *const c_char,
//     pub aggregation_key: *const c_char,
//     pub source_type_name: *const c_char,
//     pub event_type: *const c_char,
// }

// pub mod callback {
//     use super::*;

//     // (id, metric_type, metric_name, value, tags, hostname, flush_first_value)
//     // void (*cb_submit_metric_t)(char *, metric_type_t, char *, double, char **, char *, bool)
//     pub type SubmitMetric = unsafe extern "C" fn(
//         id: *const c_char,
//         metric_type: c_int,
//         metric_name: *const c_char,
//         value: c_double,
//         tags: *const *const c_char,
//         hostname: *const c_char,
//         flush_first: c_int,
//     );

//     // (id, sc_name, status, tags, hostname, message)
//     // void (*cb_submit_service_check_t)(char *, char *, int, char **, char *, char *)
//     pub type SubmitServiceCheck = unsafe extern "C" fn(
//         id: *const c_char,
//         name: *const c_char,
//         status: c_int,
//         tags: *const *const c_char,
//         hostname: *const c_char,
//         message: *const c_char,
//     );

//     // (id, event)
//     // void (*cb_submit_event_t)(char *, event_t *)
//     pub type SubmitEvent = unsafe extern "C" fn(id: *const c_char, event: *const Event);

//     // (id, metric_name, value, lower_bound, upper_bound, monotonic, hostname, tags, flush_first_value)
//     // void (*cb_submit_histogram_bucket_t)(char *, char *, long long, float, float, int, char *, char **, bool)
//     pub type SubmitHistogram = unsafe extern "C" fn(
//         id: *const c_char,
//         metric_name: *const c_char,
//         value: c_longlong,
//         lower_bound: c_float,
//         upper_bound: c_float,
//         monotonic: c_int,
//         hostname: *const c_char,
//         tags: *const *const c_char,
//         flush_first: c_int,
//     );

//     // (id, event, event_type)
//     // void (*cb_submit_event_platform_event_t)(char *, char *, int, char *)
//     pub type SubmitEventPlatformEvent = unsafe extern "C" fn(
//         id: *const c_char,
//         event: *const c_char,
//         event_len: c_int,
//         event_type: *const c_char,
//     );

//     // rtloader/include/rtloader_types.h
//     // pkg/collector/aggregator/aggregator.go
//     #[repr(C)]
//     #[derive(Debug, Copy, Clone)]
//     pub struct Callback {
//         pub submit_metric: SubmitMetric,
//         pub submit_service_check: SubmitServiceCheck,
//         pub submit_event: SubmitEvent,
//         pub submit_histogram: SubmitHistogram,
//         pub submit_event_platform_event: SubmitEventPlatformEvent,
//     }
// }

// // TODO:
// //  - Try to avoid reallocating tags C array
// //  - Restore Callback taken as ref
// pub struct SharedLibrary {
//     check_id: CString,
//     callback: callback::Callback,
// }

// impl SharedLibrary {
//     pub fn new(check_id: &str, callback: &callback::Callback) -> Self {
//         SharedLibrary {
//             check_id: CString::new(check_id).unwrap(), // unwrap: no NULL, created from &str
//             callback: callback.clone(),
//         }
//     }
// }

// #[async_trait]
// impl Sink for SharedLibrary {
//     async fn submit_metric(&self, metric: metric::Metric, flush_first: bool) {
//         let name = to_cstring(metric.name);
//         let hostname = to_cstring("");
//         let tags = map_to_cstring_array(&metric.tags);
//         let tags = to_cchar_array(&tags);

//         unsafe {
//             (self.callback.submit_metric)(
//                 self.check_id.as_c_str().as_ptr(),
//                 metric.metric_type as c_int,
//                 name.as_ptr(),
//                 metric.value,
//                 tags.as_ptr(),
//                 hostname.as_ptr(),
//                 flush_first as c_int,
//             );
//         }
//     }

//     async fn submit_service_check(&self, service_check: service_check::ServiceCheck) {
//         let name = to_cstring(service_check.name);
//         let hostname = to_cstring("");
//         let message = to_cstring(service_check.message);
//         let tags = map_to_cstring_array(&service_check.tags);
//         let tags = to_cchar_array(&tags);

//         unsafe {
//             (self.callback.submit_service_check)(
//                 self.check_id.as_c_str().as_ptr(),
//                 name.as_ptr(),
//                 service_check.status as c_int,
//                 tags.as_ptr(),
//                 hostname.as_ptr(),
//                 message.as_ptr(),
//             );
//         }
//     }

//     async fn submit_event(&self, event: event::Event) {
//         let title = to_cstring(event.title);
//         let text = to_cstring(event.text);
//         let priority = to_cstring(event.priority);
//         let hostname = to_cstring("");
//         let alert_type = to_cstring(event.alert_type);
//         let aggregation_key = to_cstring(event.aggregation_key);
//         let source_type_name = to_cstring(event.source_type_name);
//         let event_type = to_cstring(event.event_type);
//         let tags = map_to_cstring_array(&event.tags);
//         let tags = to_cchar_array(&tags);

//         let c_event = Event {
//             title: title.as_ptr(),
//             text: text.as_ptr(),
//             ts: event.timestamp,
//             priority: priority.as_ptr(),
//             host: hostname.as_ptr(),
//             tags: tags.as_ptr(),
//             alert_type: alert_type.as_ptr(),
//             aggregation_key: aggregation_key.as_ptr(),
//             source_type_name: source_type_name.as_ptr(),
//             event_type: event_type.as_ptr(),
//         };

//         unsafe {
//             (self.callback.submit_event)(self.check_id.as_c_str().as_ptr(), &c_event);
//         }
//     }

//     async fn submit_histogram(&self, histogram: histogram::Histrogram, flush_first: bool) {
//         let metric_name = to_cstring(histogram.metric_name);
//         let hostname = to_cstring("");
//         let tags = map_to_cstring_array(&histogram.tags);
//         let tags = to_cchar_array(&tags);

//         unsafe {
//             (self.callback.submit_histogram)(
//                 self.check_id.as_c_str().as_ptr(),
//                 metric_name.as_ptr(),
//                 histogram.value as c_longlong,
//                 histogram.lower_bound as c_float,
//                 histogram.upper_bound as c_float,
//                 histogram.monotonic as c_int,
//                 hostname.as_ptr(),
//                 tags.as_ptr(),
//                 flush_first as c_int,
//             );
//         }
//     }

//     async fn submit_event_platform_event(&self, event: event_platform::Event) {
//         let event_data = to_cstring(event.event);
//         let event_type = to_cstring(event.event_type);

//         unsafe {
//             (self.callback.submit_event_platform_event)(
//                 self.check_id.as_c_str().as_ptr(),
//                 event_data.as_ptr(),
//                 event_data.as_bytes().len() as c_int,
//                 event_type.as_ptr(),
//             );
//         }
//     }

//     async fn log(&self, level: log::Level, message: String) {
//         println!("[{level:?}] {message}")
//     }
// }

// #[tokio::main(flavor = "current_thread")]
// async fn async_run_check<C>(check: &C) -> Result<()>
// where
//     C: Check<Snk = SharedLibrary>,
// {
//     check.run().await
// }

// fn run_check<C>(
//     callback: &callback::Callback,
//     check_id: &str,
//     init_config: &str,
//     instance_config: &str,
// ) -> Result<()>
// where
//     C: Check<Snk = SharedLibrary>,
// {
//     let init_config: serde_yaml::Mapping =
//         serde_yaml::from_str(init_config).with_context(|| "Failed to parse init configuration")?;
//     let instance_config: serde_yaml::Mapping = serde_yaml::from_str(instance_config)
//         .with_context(|| "Failed to parse instance configuration")?;

//     let shlib = SharedLibrary::new(check_id, callback);
//     let check = C::build(shlib, init_config, instance_config)?;
//     async_run_check(&check)
// }

// fn to_cstring<S>(str: S) -> CString
// where
//     S: AsRef<str>,
// {
//     CString::new(str.as_ref()).expect("bad alloc")
// }

// // fn to_cstring_array(arr: &Vec<String>) -> Vec<CString> {
// //     arr.iter().map(to_cstring).collect()
// // }

// // FIXME name; trait?
// fn map_to_cstring_array(map: &HashMap<String, String>) -> Vec<CString> {
//     map.iter()
//         .map(|(k, v)| to_cstring(format!("{}:{}", k, v)))
//         .collect()
// }

// fn to_cchar_array(arr: &Vec<CString>) -> Vec<*const c_char> {
//     arr.iter()
//         .map(|s| s.as_ptr())
//         .chain(std::iter::once(std::ptr::null()))
//         .collect()
// }

// fn str_from_ptr<'a>(ptr: *const c_char) -> Option<&'a str> {
//     if ptr == std::ptr::null() {
//         return None;
//     };
//     unsafe { CStr::from_ptr(ptr) }.to_str().ok()
// }

// fn run_impl<C>(
//     callback: *const callback::Callback,
//     check_id: *const c_char,
//     init_config: *const c_char,
//     instance_config: *const c_char,
// ) -> Result<()>
// where
//     C: Check<Snk = SharedLibrary>,
// {
//     let callback = unsafe { &*callback }; // FIXME check for nullptr
//     let check_id = str_from_ptr(check_id).ok_or(anyhow!("invalid check_id"))?;
//     let init_config = str_from_ptr(init_config).ok_or(anyhow!("invalid init_config"))?;
//     let instance_config =
//         str_from_ptr(instance_config).ok_or(anyhow!("invalid instance_config"))?;

//     run_check::<C>(callback, check_id, init_config, instance_config)
// }

// pub fn run<C>(
//     check_id: *const c_char,
//     init_config: *const c_char,
//     instance_config: *const c_char,
//     callback: *const callback::Callback,
//     error: *mut *const c_char,
// ) where
//     C: Check<Snk = SharedLibrary>,
// {
//     let res = run_impl::<C>(callback, check_id, init_config, instance_config);
//     if let Err(err) = res {
//         println!("Oopsie: {}", err.to_string());
//         unsafe {
//             *error = CString::from_str(err.to_string().as_str())
//                 .expect("allocation error")
//                 .into_raw()
//         }
//     }
// }

// //(char *, char *, char *, const aggregator_t *, const char **);
// #[unsafe(no_mangle)]
// pub extern "C" fn Run(
//     check_id: *const c_char,
//     init_config: *const c_char,
//     instance_config: *const c_char,
//     callback: *const callback::Callback,
//     error: *mut *const c_char,
// ) {
//     let res = run_impl(callback, check_id, init_config, instance_config);
//     if let Err(err) = res {
//         println!("Oopsie: {}", err.to_string());
//         unsafe {
//             *error = CString::from_str(err.to_string().as_str())
//                 .expect("allocation error")
//                 .into_raw()
//         }
//     }
// }

// #[unsafe(no_mangle)]
// pub extern "C" fn Version(_error: *mut *const c_char) -> *const c_char {
//     http_check::version::VERSION.as_ptr().cast()
// }

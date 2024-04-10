use std::time::SystemTime;

// TODO: Think about if we need/might need to have a time provider to handle mocked time.
//
// My initial thought is that mocked time would be useful for time-based component testing, such as the aggregate
// transform.. but right now, it's a little weird/heavy to thread through an environment provider to places that need
// time. As an example, the DogStatsD codec wants to be able to get the time so that it can mark it appropriately on
// decoded metrics... so we'd have to pass it through to the codec, which is doable, but we'd also have to box it to
// avoid the generics, and then we have indirection through the box for every call, where the call might already be as
// fast as tens of nanoseconds.... it's not great.
//
// Anyways, we can defer for now but I _think_ we may want it in the future as a clean way to handle real vs mocked
// time.

pub fn get_unix_timestamp() -> u64 {
    let since_unix_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    since_unix_epoch.as_secs()
}

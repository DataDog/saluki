(function() {
    var implementors = Object.fromEntries([["saluki_io",[["impl&lt;C, L, Req, Res, Error&gt; <a class=\"trait\" href=\"https://docs.rs/tower/0.5.2/tower/retry/policy/trait.Policy.html\" title=\"trait tower::retry::policy::Policy\">Policy</a>&lt;Req, Res, Error&gt; for <a class=\"struct\" href=\"saluki_io/net/util/retry/struct.RollingExponentialBackoffRetryPolicy.html\" title=\"struct saluki_io::net::util::retry::RollingExponentialBackoffRetryPolicy\">RollingExponentialBackoffRetryPolicy</a>&lt;C, L&gt;<div class=\"where\">where\n    C: <a class=\"trait\" href=\"saluki_io/net/util/retry/trait.RetryClassifier.html\" title=\"trait saluki_io::net::util::retry::RetryClassifier\">RetryClassifier</a>&lt;Res, Error&gt;,\n    L: RetryLifecycle&lt;Req, Res, Error&gt;,\n    Req: <a class=\"trait\" href=\"https://doc.rust-lang.org/nightly/core/clone/trait.Clone.html\" title=\"trait core::clone::Clone\">Clone</a>,</div>"],["impl&lt;Req, Res, Error&gt; <a class=\"trait\" href=\"https://docs.rs/tower/0.5.2/tower/retry/policy/trait.Policy.html\" title=\"trait tower::retry::policy::Policy\">Policy</a>&lt;Req, Res, Error&gt; for <a class=\"struct\" href=\"saluki_io/net/util/retry/struct.NoopRetryPolicy.html\" title=\"struct saluki_io::net::util::retry::NoopRetryPolicy\">NoopRetryPolicy</a>"]]]]);
    if (window.register_implementors) {
        window.register_implementors(implementors);
    } else {
        window.pending_implementors = implementors;
    }
})()
//{"start":57,"fragment_lengths":[1253]}
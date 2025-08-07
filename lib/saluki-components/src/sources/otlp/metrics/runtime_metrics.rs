//! Mappings for OTLP runtime metrics to Datadog-conventional metric names.

use std::collections::HashMap;
use std::sync::LazyLock;

// Defines the fields needed to map OTel runtime metrics to their equivalent Datadog runtime metrics.
pub(super) struct RuntimeMetricMapping {
    pub mapped_name: &'static str,
    // The attribute(s) this metric originates from. If empty, no attribute matching is performed.
    pub attributes: &'static [RuntimeMetricAttribute],
}

// Defines the structure for an attribute in regard to mapping runtime metrics.
// The presence of a RuntimeMetricAttribute means that a metric must be mapped from a data point
// with the given attribute(s).
pub(super) struct RuntimeMetricAttribute {
    // The attribute name.
    pub key: &'static str,
    // The attribute value, or multiple values if there is more than one value for the same mapping.
    pub values: &'static [&'static str],
}

// runtimeMetricPrefixLanguageMap defines the runtime metric prefixes and which languages they map to
#[allow(dead_code)]
pub(super) static RUNTIME_METRIC_PREFIX_LANGUAGE_MAP: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        m.insert("process.runtime.go", "go");
        m.insert("process.runtime.dotnet", "dotnet");
        m.insert("process.runtime.jvm", "jvm");
        m.insert("jvm", "jvm");
        m
    });

// Defines the mappings from OTel runtime metric names to their equivalent Datadog runtime metric names.
pub(super) static RUNTIME_METRICS_MAPPINGS: LazyLock<HashMap<&'static str, Vec<RuntimeMetricMapping>>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        m.extend(go_runtime_metrics_mappings());
        m.extend(dotnet_runtime_metrics_mappings());
        m.extend(java_runtime_metrics_mappings());
        m.extend(stable_java_runtime_metrics_mappings());
        m
    });

fn go_runtime_metrics_mappings() -> HashMap<&'static str, Vec<RuntimeMetricMapping>> {
    let mut m = HashMap::new();
    m.insert(
        "process.runtime.go.goroutines",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.num_goroutine",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.cgo.calls",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.num_cgo_call",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.lookups",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.lookups",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.mem.heap_alloc",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.heap_alloc",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.mem.heap_sys",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.heap_sys",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.mem.heap_idle",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.heap_idle",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.mem.heap_inuse",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.heap_inuse",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.mem.heap_released",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.heap_released",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.mem.heap_objects",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.heap_objects",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.gc.pause_total_ns",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.pause_total_ns",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.go.gc.count",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.go.mem_stats.num_gc",
            attributes: &[],
        }],
    );
    m
}

fn dotnet_runtime_metrics_mappings() -> HashMap<&'static str, Vec<RuntimeMetricMapping>> {
    let mut m = HashMap::new();
    m.insert(
        "process.runtime.dotnet.monitor.lock_contention.count",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.dotnet.threads.contention_count",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.dotnet.exceptions.count",
        vec![RuntimeMetricMapping {
            mapped_name: "runtime.dotnet.exceptions.count",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.dotnet.gc.heap.size",
        vec![
            RuntimeMetricMapping {
                mapped_name: "runtime.dotnet.gc.size.gen0",
                attributes: &[RuntimeMetricAttribute {
                    key: "generation",
                    values: &["gen0"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "runtime.dotnet.gc.size.gen1",
                attributes: &[RuntimeMetricAttribute {
                    key: "generation",
                    values: &["gen1"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "runtime.dotnet.gc.size.gen2",
                attributes: &[RuntimeMetricAttribute {
                    key: "generation",
                    values: &["gen2"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "runtime.dotnet.gc.size.loh",
                attributes: &[RuntimeMetricAttribute {
                    key: "generation",
                    values: &["loh"],
                }],
            },
        ],
    );
    m.insert(
        "process.runtime.dotnet.gc.collections.count",
        vec![
            RuntimeMetricMapping {
                mapped_name: "runtime.dotnet.gc.count.gen0",
                attributes: &[RuntimeMetricAttribute {
                    key: "generation",
                    values: &["gen0"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "runtime.dotnet.gc.count.gen1",
                attributes: &[RuntimeMetricAttribute {
                    key: "generation",
                    values: &["gen1"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "runtime.dotnet.gc.count.gen2",
                attributes: &[RuntimeMetricAttribute {
                    key: "generation",
                    values: &["gen2"],
                }],
            },
        ],
    );
    m
}

fn stable_java_runtime_metrics_mappings() -> HashMap<&'static str, Vec<RuntimeMetricMapping>> {
    let mut m = HashMap::new();
    m.insert(
        "jvm.thread.count",
        vec![RuntimeMetricMapping {
            mapped_name: "jvm.thread_count",
            attributes: &[],
        }],
    );
    m.insert(
        "jvm.class.count",
        vec![RuntimeMetricMapping {
            mapped_name: "jvm.loaded_classes",
            attributes: &[],
        }],
    );
    m.insert(
        "jvm.system.cpu.utilization",
        vec![RuntimeMetricMapping {
            mapped_name: "jvm.cpu_load.system",
            attributes: &[],
        }],
    );
    m.insert(
        "jvm.cpu.recent_utilization",
        vec![RuntimeMetricMapping {
            mapped_name: "jvm.cpu_load.process",
            attributes: &[],
        }],
    );
    m.insert(
        "jvm.memory.used",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.heap_memory",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.memory.type",
                    values: &["heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.non_heap_memory",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.memory.type",
                    values: &["non_heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.gc.old_gen_size",
                attributes: &[
                    RuntimeMetricAttribute {
                        key: "jvm.memory.pool.name",
                        values: &["G1 Old Gen", "Tenured Gen", "PS Old Gen"],
                    },
                    RuntimeMetricAttribute {
                        key: "jvm.memory.type",
                        values: &["heap"],
                    },
                ],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.gc.eden_size",
                attributes: &[
                    RuntimeMetricAttribute {
                        key: "jvm.memory.pool.name",
                        values: &["G1 Eden Space", "Eden Space", "Par Eden Space", "PS Eden Space"],
                    },
                    RuntimeMetricAttribute {
                        key: "jvm.memory.type",
                        values: &["heap"],
                    },
                ],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.gc.survivor_size",
                attributes: &[
                    RuntimeMetricAttribute {
                        key: "jvm.memory.pool.name",
                        values: &[
                            "G1 Survivor Space",
                            "Survivor Space",
                            "Par Survivor Space",
                            "PS Survivor Space",
                        ],
                    },
                    RuntimeMetricAttribute {
                        key: "jvm.memory.type",
                        values: &["heap"],
                    },
                ],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.gc.metaspace_size",
                attributes: &[
                    RuntimeMetricAttribute {
                        key: "jvm.memory.pool.name",
                        values: &["Metaspace"],
                    },
                    RuntimeMetricAttribute {
                        key: "jvm.memory.type",
                        values: &["non_heap"],
                    },
                ],
            },
        ],
    );
    m.insert(
        "jvm.memory.committed",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.heap_memory_committed",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.memory.type",
                    values: &["heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.non_heap_memory_committed",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.memory.type",
                    values: &["non_heap"],
                }],
            },
        ],
    );
    m.insert(
        "jvm.memory.init",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.heap_memory_init",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.memory.type",
                    values: &["heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.non_heap_memory_init",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.memory.type",
                    values: &["non_heap"],
                }],
            },
        ],
    );
    m.insert(
        "jvm.memory.limit",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.heap_memory_max",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.memory.type",
                    values: &["heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.non_heap_memory_max",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.memory.type",
                    values: &["non_heap"],
                }],
            },
        ],
    );
    m.insert(
        "jvm.buffer.memory.usage",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.direct.used",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.buffer.pool.name",
                    values: &["direct"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.mapped.used",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.buffer.pool.name",
                    values: &["mapped"],
                }],
            },
        ],
    );
    m.insert(
        "jvm.buffer.count",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.direct.count",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.buffer.pool.name",
                    values: &["direct"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.mapped.count",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.buffer.pool.name",
                    values: &["mapped"],
                }],
            },
        ],
    );
    m.insert(
        "jvm.buffer.memory.limit",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.direct.limit",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.buffer.pool.name",
                    values: &["direct"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.mapped.limit",
                attributes: &[RuntimeMetricAttribute {
                    key: "jvm.buffer.pool.name",
                    values: &["mapped"],
                }],
            },
        ],
    );
    m
}

fn java_runtime_metrics_mappings() -> HashMap<&'static str, Vec<RuntimeMetricMapping>> {
    let mut m = HashMap::new();
    m.insert(
        "process.runtime.jvm.threads.count",
        vec![RuntimeMetricMapping {
            mapped_name: "jvm.thread_count",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.jvm.classes.current_loaded",
        vec![RuntimeMetricMapping {
            mapped_name: "jvm.loaded_classes",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.jvm.system.cpu.utilization",
        vec![RuntimeMetricMapping {
            mapped_name: "jvm.cpu_load.system",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.jvm.cpu.utilization",
        vec![RuntimeMetricMapping {
            mapped_name: "jvm.cpu_load.process",
            attributes: &[],
        }],
    );
    m.insert(
        "process.runtime.jvm.memory.usage",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.heap_memory",
                attributes: &[RuntimeMetricAttribute {
                    key: "type",
                    values: &["heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.non_heap_memory",
                attributes: &[RuntimeMetricAttribute {
                    key: "type",
                    values: &["non_heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.gc.old_gen_size",
                attributes: &[
                    RuntimeMetricAttribute {
                        key: "pool",
                        values: &["G1 Old Gen", "Tenured Gen", "PS Old Gen"],
                    },
                    RuntimeMetricAttribute {
                        key: "type",
                        values: &["heap"],
                    },
                ],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.gc.eden_size",
                attributes: &[
                    RuntimeMetricAttribute {
                        key: "pool",
                        values: &["G1 Eden Space", "Eden Space", "Par Eden Space", "PS Eden Space"],
                    },
                    RuntimeMetricAttribute {
                        key: "type",
                        values: &["heap"],
                    },
                ],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.gc.survivor_size",
                attributes: &[
                    RuntimeMetricAttribute {
                        key: "pool",
                        values: &[
                            "G1 Survivor Space",
                            "Survivor Space",
                            "Par Survivor Space",
                            "PS Survivor Space",
                        ],
                    },
                    RuntimeMetricAttribute {
                        key: "type",
                        values: &["heap"],
                    },
                ],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.gc.metaspace_size",
                attributes: &[
                    RuntimeMetricAttribute {
                        key: "pool",
                        values: &["Metaspace"],
                    },
                    RuntimeMetricAttribute {
                        key: "type",
                        values: &["non_heap"],
                    },
                ],
            },
        ],
    );
    m.insert(
        "process.runtime.jvm.memory.committed",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.heap_memory_committed",
                attributes: &[RuntimeMetricAttribute {
                    key: "type",
                    values: &["heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.non_heap_memory_committed",
                attributes: &[RuntimeMetricAttribute {
                    key: "type",
                    values: &["non_heap"],
                }],
            },
        ],
    );
    m.insert(
        "process.runtime.jvm.memory.init",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.heap_memory_init",
                attributes: &[RuntimeMetricAttribute {
                    key: "type",
                    values: &["heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.non_heap_memory_init",
                attributes: &[RuntimeMetricAttribute {
                    key: "type",
                    values: &["non_heap"],
                }],
            },
        ],
    );
    m.insert(
        "process.runtime.jvm.memory.limit",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.heap_memory_max",
                attributes: &[RuntimeMetricAttribute {
                    key: "type",
                    values: &["heap"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.non_heap_memory_max",
                attributes: &[RuntimeMetricAttribute {
                    key: "type",
                    values: &["non_heap"],
                }],
            },
        ],
    );
    m.insert(
        "process.runtime.jvm.buffer.usage",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.direct.used",
                attributes: &[RuntimeMetricAttribute {
                    key: "pool",
                    values: &["direct"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.mapped.used",
                attributes: &[RuntimeMetricAttribute {
                    key: "pool",
                    values: &["mapped"],
                }],
            },
        ],
    );
    m.insert(
        "process.runtime.jvm.buffer.count",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.direct.count",
                attributes: &[RuntimeMetricAttribute {
                    key: "pool",
                    values: &["direct"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.mapped.count",
                attributes: &[RuntimeMetricAttribute {
                    key: "pool",
                    values: &["mapped"],
                }],
            },
        ],
    );
    m.insert(
        "process.runtime.jvm.buffer.limit",
        vec![
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.direct.limit",
                attributes: &[RuntimeMetricAttribute {
                    key: "pool",
                    values: &["direct"],
                }],
            },
            RuntimeMetricMapping {
                mapped_name: "jvm.buffer_pool.mapped.limit",
                attributes: &[RuntimeMetricAttribute {
                    key: "pool",
                    values: &["mapped"],
                }],
            },
        ],
    );
    m
}

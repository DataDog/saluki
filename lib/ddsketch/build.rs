use std::marker::PhantomData;

const I32_MIN_AS_F64: f64 = i32::MIN as f64;
const I32_MAX_AS_F64: f64 = i32::MAX as f64;

struct RawSketchParameters<K, V> {
    bin_key_type: PhantomData<K>,
    bin_value_type: PhantomData<V>,
    bin_limit: usize,
    min_value: f64,
    rel_accuracy: f64,
    gamma_v: f64,
    gamma_ln: f64,
    norm_min: f64,
    norm_bias: i32,
}

impl<K, V> RawSketchParameters<K, V> {
    fn from_limits(bin_limit: usize, rel_accuracy: f64, min_value: f64) -> Self {
        assert!(
            rel_accuracy > 0.0 && rel_accuracy < 1.0,
            "eps must be between 0.0 and 1.0"
        );
        assert!(min_value > 0.0, "min value must be greater than 0.0");
        assert!(bin_limit > 0, "bin limit must be greater than 0");

        // The gamma parameter is derived from the relative accuracy: gamma = 1 + 2 * rel_accuracy.
        // This relationship comes from the DDSketch paper's logarithmic index mapping.
        let two_rel_accuracy = rel_accuracy * 2.0;
        let gamma_v = 1.0 + two_rel_accuracy;
        let gamma_ln = two_rel_accuracy.ln_1p();

        let raw_norm_eff_min = (min_value.ln() / gamma_ln).floor();
        assert!(
            (I32_MIN_AS_F64..=I32_MAX_AS_F64).contains(&raw_norm_eff_min),
            "norm_eff_min must fit in an i32"
        );

        let norm_eff_min = raw_norm_eff_min as i32;
        let norm_bias = -norm_eff_min + 1;
        let norm_min = gamma_v.powf(f64::from(1 - norm_bias));

        assert!(norm_min <= min_value, "norm min should not exceed min_value");

        Self {
            bin_key_type: PhantomData,
            bin_value_type: PhantomData,
            bin_limit,
            min_value,
            rel_accuracy,
            gamma_v,
            gamma_ln,
            norm_min,
            norm_bias,
        }
    }

    fn bin_key_type(&self) -> &'static str {
        std::any::type_name::<K>()
    }

    fn bin_value_type(&self) -> &'static str {
        std::any::type_name::<V>()
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Calculate the sketch configuration parameters for the various scenarios we have (Agent, APM statistics, etc) and
    // generate the scenario-specific configuration types that can then be referenced when creating `DDSketch`.
    let agent_sketch_params = RawSketchParameters::<i16, u16>::from_limits(4096, 1.0 / 128.0, 1.0e-9);
    let generated_agent_sketch_params = generate_sketch_parameters_impl(
        "AgentSketchParameters",
        "Sketch parameters for Agent-specific sketches.",
        &agent_sketch_params,
    );

    // Concatenate the generated sketch parameters code and write it out.
    let mut contents = Vec::new();
    contents.extend(generated_agent_sketch_params.as_bytes());

    let config_file = std::env::var("OUT_DIR").unwrap() + "/config.rs";
    std::fs::write(config_file, contents).expect("failed to write config file");
}

fn generate_sketch_parameters_impl<K, V>(
    params_type_name: &str, params_type_desc: &str, params: &RawSketchParameters<K, V>,
) -> String {
    format!(
        r#"
        /// {}
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct {};

        impl crate::SketchParameters for {} {{
            type BinKey = {};
            type BinCount = {};

            const BIN_LIMIT: usize = {};
            const MINIMUM_VALUE: f64 = {};
            const RELATIVE_ACCURACY: f64 = {};
            const GAMMA_V: f64 = {};
            const GAMMA_LN: f64 = {};
            const NORM_MIN: f64 = {};
            const NORM_BIAS: i32 = {};
        }}

		"#,
        params_type_desc,
        params_type_name,
        params_type_name,
        params.bin_key_type(),
        params.bin_value_type(),
        params.bin_limit,
        params.min_value,
        params.rel_accuracy,
        params.gamma_v,
        params.gamma_ln,
        params.norm_min,
        params.norm_bias
    )
}

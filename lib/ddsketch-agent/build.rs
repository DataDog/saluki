// Define our Agent-specific sketch configuration parameters and constants related to DDSketch.
const AGENT_DEFAULT_BIN_LIMIT: u16 = 4096;
const AGENT_DEFAULT_EPS: f64 = 1.0 / 128.0;
const AGENT_DEFAULT_MIN_VALUE: f64 = 1.0e-9;

struct ConfigurationParams {
    bin_limit: u16,
    gamma_v: f64,
    gamma_ln: f64,
    norm_min: f64,
    norm_bias: i32,
}

fn calculate_configuration_params(bin_limit: u16, mut eps: f64, min_value: f64) -> ConfigurationParams {
    assert!(eps > 0.0 && eps < 1.0, "eps must be between 0.0 and 1.0");
    assert!(min_value > 0.0, "min value must be greater than 0.0");
    assert!(bin_limit > 0, "bin limit must be greater than 0");

    eps *= 2.0;
    let gamma_v = 1.0 + eps;
    let gamma_ln = eps.ln_1p();

    // SAFETY: We expect `log_gamma` to return a value between -2^16 and 2^16, so it will always fit in an i32.
    let norm_eff_min = (min_value.ln() / gamma_ln).floor() as i32;
    let norm_bias = -norm_eff_min + 1;

    let norm_min = gamma_v.powf(f64::from(1 - norm_bias));

    assert!(norm_min <= min_value, "norm min should not exceed min_value");

    ConfigurationParams {
        bin_limit,
        gamma_v,
        gamma_ln,
        norm_min,
        norm_bias,
    }
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    // Calculate the sketch configuration parameters for the Agent-specific defaults, and write them out as constants to
    // the given file, which gets imported in lib.rs.
    let config = calculate_configuration_params(AGENT_DEFAULT_BIN_LIMIT, AGENT_DEFAULT_EPS, AGENT_DEFAULT_MIN_VALUE);

    let config_file = std::env::var("OUT_DIR").unwrap() + "/config.rs";
    std::fs::write(
        config_file,
        format!(
            r#"
    pub const AGENT_DEFAULT_BIN_LIMIT: u16 = {};
    pub const AGENT_DEFAULT_EPS: f64 = {};
    pub const AGENT_DEFAULT_MIN_VALUE: f64 = {};
    pub const DDSKETCH_CONF_BIN_LIMIT: u16 = {};
    pub const DDSKETCH_CONF_GAMMA_V: f64 = {};
    pub const DDSKETCH_CONF_GAMMA_LN: f64 = {};
    pub const DDSKETCH_CONF_NORM_MIN: f64 = {};
    pub const DDSKETCH_CONF_NORM_BIAS: i32 = {};
			"#,
            AGENT_DEFAULT_BIN_LIMIT,
            AGENT_DEFAULT_EPS,
            AGENT_DEFAULT_MIN_VALUE,
            config.bin_limit,
            config.gamma_v,
            config.gamma_ln,
            config.norm_min,
            config.norm_bias,
        ),
    )
    .expect("failed to write config file");
}

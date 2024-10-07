use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
pub struct Config {
    // https://github.com/opencontainers/runtime-spec/blob/main/config.md
    // fully filled in config.json ready to pass to crun
    pub oci_runtime_config: String,
}

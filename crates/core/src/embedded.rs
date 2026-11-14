//! Bundled assets embedded into the binary, so proxy-fire works from any
//! directory with zero external files. On-disk copies always win over these.

pub const CONFIG: &str = include_str!("../../../configs/config.example.yaml");
pub const CATALOG: &str = include_str!("../../../configs/catalog.json");
pub const FAMILIES: &str = include_str!("../../../configs/families.yaml");
pub const CAPABILITIES: &str = include_str!("../../../configs/capabilities.yaml");
pub const REFRESH_PROMPT: &str = include_str!("../../../configs/refresh-prompt.txt");

pub fn for_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "config.example.yaml" => CONFIG,
        "catalog.json" => CATALOG,
        "families.yaml" => FAMILIES,
        "capabilities.yaml" => CAPABILITIES,
        "refresh-prompt.txt" => REFRESH_PROMPT,
        _ => return None,
    })
}

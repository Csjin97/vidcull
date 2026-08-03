use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BestCopyMode {
    #[default]
    Archival,
    SpaceSaving,
    MaxQuality,
    MinSize,
    Compatible,
    MaxResolution,
}

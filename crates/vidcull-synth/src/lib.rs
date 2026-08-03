mod corpus;
mod plan;
mod regression;
mod render;
mod rng;
mod transform;

pub use corpus::{ClipVariant, plan_clip_corpus, variant_outputs};
pub use plan::{RenderPlan, SidecarSrt, plan};
pub use regression::plan_regression_corpus;
pub use render::{render, render_recipe, render_source, render_testsrc};
pub use rng::SplitMix64;
pub use transform::{Clip, Container, Encode, Filter, Recipe};

pub use vidcull_parser::fallback::FfmpegBinaries;

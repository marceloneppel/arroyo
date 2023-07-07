use serde::{Deserialize, Serialize};
use typify::import_types;

pub mod blackhole;
pub mod impulse;
pub mod kafka;
pub mod nexmark;
pub mod parquet;
pub mod sse;
pub mod websocket;

import_types!(schema = "../connector-schemas/common.json",);

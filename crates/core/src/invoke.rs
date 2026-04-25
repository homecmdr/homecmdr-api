use serde::{Deserialize, Serialize};

use crate::model::AttributeValue;

// An adapter-specific function call that doesn't map to a standard device command.
// `target` names the adapter and operation separated by a colon
// (e.g. `"my_adapter:reload_config"`).  `payload` carries optional input data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeRequest {
    pub target: String,
    #[serde(default = "default_payload")]
    pub payload: AttributeValue,
}

// The return value of an InvokeRequest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeResponse {
    pub value: AttributeValue,
}

fn default_payload() -> AttributeValue {
    AttributeValue::Null
}

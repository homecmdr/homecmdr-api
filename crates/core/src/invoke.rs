use serde::{Deserialize, Serialize};

use crate::model::AttributeValue;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeRequest {
    pub target: String,
    #[serde(default = "default_payload")]
    pub payload: AttributeValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvokeResponse {
    pub value: AttributeValue,
}

fn default_payload() -> AttributeValue {
    AttributeValue::Null
}

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::capability::{action_requires_value, capability_definition};
use crate::model::AttributeValue;
use crate::validation::validate_capability_attribute_value;

// A command to send to a device: which capability to act on, what action to
// perform, and an optional value (e.g. brightness 75 for a "set" action).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceCommand {
    pub capability: String,
    pub action: String,
    #[serde(default)]
    pub value: Option<AttributeValue>,
    /// Optional hardware transition duration in seconds.
    ///
    /// Adapters that support smooth transitions (e.g. Zigbee2MQTT) will pass this
    /// to the device firmware.  Adapters that do not support it silently ignore it.
    /// When set, the command returns immediately after dispatch — the caller is
    /// responsible for waiting (e.g. `ctx:sleep`) while the hardware fades.
    #[serde(default)]
    pub transition_secs: Option<f64>,
}

impl DeviceCommand {
    /// Check that the capability exists, the action is allowed for it, and the
    /// value (if any) matches the expected schema.  Returns an error with a
    /// human-readable message if anything is wrong.
    pub fn validate(&self) -> Result<()> {
        let capability = capability_definition(&self.capability)
            .with_context(|| format!("unknown capability '{}'", self.capability))?;

        if !capability.actions.contains(&self.action.as_str()) {
            bail!(
                "action '{}' is not supported for capability '{}'",
                self.action,
                self.capability
            );
        }

        if action_requires_value(&self.action) {
            let value = self.value.as_ref().with_context(|| {
                format!(
                    "command action '{}' for capability '{}' requires a value",
                    self.action, self.capability
                )
            })?;

            validate_capability_attribute_value(capability.schema, value).map_err(|message| {
                anyhow::anyhow!(
                    "invalid command value for capability '{}': {message}",
                    self.capability
                )
            })?;
        } else if self.value.is_some() {
            bail!(
                "command action '{}' for capability '{}' does not accept a value",
                self.action,
                self.capability
            );
        }

        Ok(())
    }
}

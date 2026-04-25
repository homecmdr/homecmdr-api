use crate::model::{AttributeValue, Device};
use crate::store::{AutomationExecutionHistoryEntry, CommandAuditEntry, SceneExecutionHistoryEntry};

/// Describes which devices, capabilities, and adapters should have their
/// history recorded. Empty lists mean "allow all".
#[derive(Debug, Clone, Default)]
pub struct HistorySelection {
    pub device_ids: Vec<String>,
    pub capabilities: Vec<String>,
    pub adapter_names: Vec<String>,
}

// ── Top-level record-or-skip predicates ───────────────────────────────────────

/// Return true if this device's full state snapshot should be persisted to history.
pub fn should_record_device(sel: &HistorySelection, device: &Device) -> bool {
    selection_allows_device(sel, &device.id.0, &device.metadata.source)
}

/// Return true if a single attribute change on this device should be logged.
pub fn should_record_attribute(
    sel: &HistorySelection,
    device: &Device,
    attribute: &str,
) -> bool {
    selection_allows_device(sel, &device.id.0, &device.metadata.source)
        && selection_allows_capability(sel, attribute)
}

/// Return true if this command entry should be written to the audit log.
pub fn should_record_command_audit(sel: &HistorySelection, entry: &CommandAuditEntry) -> bool {
    selection_allows_device(
        sel,
        &entry.device_id.0,
        device_adapter_name(&entry.device_id.0),
    ) && selection_allows_capability(sel, &entry.command.capability)
}

/// Return true if this scene run should be stored in history.
pub fn should_record_scene_execution(
    sel: &HistorySelection,
    entry: &SceneExecutionHistoryEntry,
) -> bool {
    if sel.adapter_names.is_empty() && sel.capabilities.is_empty() {
        return true;
    }

    entry
        .results
        .iter()
        .any(|result| selection_allows_device(sel, &result.target, device_adapter_name(&result.target)))
}

/// Return true if this automation run should be stored in history.
pub fn should_record_automation_execution(
    sel: &HistorySelection,
    entry: &AutomationExecutionHistoryEntry,
) -> bool {
    if !selection_allows_trigger_payload(sel, &entry.trigger_payload) {
        return false;
    }

    if sel.capabilities.is_empty() && sel.adapter_names.is_empty() {
        return true;
    }

    entry.results.iter().any(|result| {
        selection_allows_device(sel, &result.target, device_adapter_name(&result.target))
    }) || entry.results.is_empty()
}

// ── Primitive allow-checks ────────────────────────────────────────────────────

/// Return true if the selection filter permits this device ID and adapter name.
/// An empty list in the selection means "allow all".
pub fn selection_allows_device(
    sel: &HistorySelection,
    device_id: &str,
    adapter_name: &str,
) -> bool {
    let device_match = sel.device_ids.is_empty()
        || sel.device_ids.iter().any(|c| c == device_id);
    let adapter_match = sel.adapter_names.is_empty()
        || sel.adapter_names.iter().any(|c| c == adapter_name);

    device_match && adapter_match
}

/// Return true if the selection filter permits this capability key.
pub fn selection_allows_capability(sel: &HistorySelection, capability: &str) -> bool {
    sel.capabilities.is_empty()
        || sel.capabilities.iter().any(|c| c == capability)
}

/// Return true if the trigger payload's embedded device/attribute passes the
/// selection filter.  Automations without a device-based trigger always pass
/// when no device or adapter filter is configured.
pub fn selection_allows_trigger_payload(
    sel: &HistorySelection,
    payload: &AttributeValue,
) -> bool {
    let AttributeValue::Object(fields) = payload else {
        return sel.device_ids.is_empty() && sel.adapter_names.is_empty();
    };

    let device_id = fields.get("device_id").and_then(attribute_text);
    let attribute = fields.get("attribute").and_then(attribute_text);

    let device_match = match device_id {
        Some(id) => selection_allows_device(sel, id, device_adapter_name(id)),
        None => sel.device_ids.is_empty() && sel.adapter_names.is_empty(),
    };
    let capability_match = match attribute {
        Some(attr) => selection_allows_capability(sel, attr),
        None => sel.capabilities.is_empty(),
    };

    device_match && capability_match
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn attribute_text(value: &AttributeValue) -> Option<&str> {
    match value {
        AttributeValue::Text(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Extracts the adapter prefix from a device ID of the form `"adapter:rest"`.
pub fn device_adapter_name(device_id: &str) -> &str {
    device_id
        .split_once(':')
        .map(|(adapter, _)| adapter)
        .unwrap_or(device_id)
}

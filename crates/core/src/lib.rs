// Core library for HomeCmdr.
//
// This crate is the shared foundation that every other crate in the workspace
// depends on.  It contains:
//   - domain types (devices, rooms, groups, persons, zones)
//   - the capability catalogue (what sensors and actuators can do)
//   - the in-memory device registry and event bus
//   - the Runtime that owns and drives all adapters
//   - persistence traits (DeviceStore, PersonStore, ApiKeyStore)
//   - command/invoke plumbing and validation helpers
//   - shared HTTP client for external requests (weather APIs, etc.)

pub mod adapter;
pub mod bus;
pub mod capability;
pub mod command;
pub mod config;
pub mod event;
pub mod history_filter;
pub mod http;
pub mod invoke;
pub mod model;
pub mod person_registry;
pub mod registry;
pub mod runtime;
pub mod store;
pub mod validation;

#[cfg(test)]
mod tests;

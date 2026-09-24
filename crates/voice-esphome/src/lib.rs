//! ESPHome native API client for voice satellites (Home Assistant Voice PE, ESPHome voice devices,
//! Linux Voice Assistant): the same connection Home Assistant makes, so the engine can be the
//! device's voice assistant. Only one client may subscribe to a device's voice assistant; disable
//! the device's Assist satellite entity in Home Assistant to leave it to this one.

pub mod client;
pub mod frame;
pub mod proto;

pub use client::{Device, Incoming};

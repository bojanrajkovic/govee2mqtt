use crate::ble::NotifyHumidifierNightlightParams;
use crate::commands::serve::{FAN_COMMAND_SETTLE_TIME, FAN_POLL_INTERVAL, POLL_INTERVAL};
use crate::lan_api::{DeviceColor, DeviceStatus as LanDeviceStatus, LanDevice};
use crate::platform_api::{
    DeviceCapability, DeviceCapabilityState, DeviceParameters, DeviceType, HttpDeviceInfo,
    HttpDeviceState,
};
use crate::service::quirks::{resolve_quirk, Quirk, BULB};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::borrow::Cow;
use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Default, Clone, Debug)]
pub struct Device {
    pub sku: String,
    pub id: String,

    /// Probed LAN device information, found either via discovery
    /// or explicit probing by IP address
    pub lan_device: Option<LanDevice>,
    pub last_lan_device_update: Option<DateTime<Utc>>,

    pub lan_device_status: Option<LanDeviceStatus>,
    pub last_lan_device_status_update: Option<DateTime<Utc>>,

    pub http_device_info: Option<HttpDeviceInfo>,
    pub last_http_device_update: Option<DateTime<Utc>>,

    pub http_device_state: Option<HttpDeviceState>,
    pub last_http_device_state_update: Option<DateTime<Utc>>,

    pub undoc_device_info: Option<UndocDeviceInfo>,
    pub last_undoc_device_info_update: Option<DateTime<Utc>>,

    pub iot_device_status: Option<LanDeviceStatus>,
    pub last_iot_device_status_update: Option<DateTime<Utc>>,

    pub nightlight_state: Option<NotifyHumidifierNightlightParams>,
    pub target_humidity_percent: Option<u8>,
    pub humidifier_work_mode: Option<u8>,
    pub humidifier_param_by_mode: HashMap<u8, u8>,

    /// The last known fanSpeedMode value, from a command or a platform poll.
    pub fan_speed_mode: Option<JsonValue>,
    /// The last known fanToggle state, from a command or a platform poll.
    pub fan_toggle_on: Option<bool>,
    /// When the last fan poll started, whether or not it succeeded.
    pub last_fan_poll: Option<DateTime<Utc>>,
    /// When the platform API last accepted a fan command.
    last_fan_command: Option<DateTime<Utc>>,

    pub last_polled: Option<DateTime<Utc>>,

    active_scene: Option<ActiveSceneInfo>,
}

impl std::fmt::Display for Device {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "{} ({} {})", self.name(), self.id, self.sku)
    }
}

/// Govee doesn't report the active scene or music mode,
/// so we maintain our own idea of it, clearing it when
/// the color of the light is changed
#[derive(Clone, Debug)]
struct ActiveSceneInfo {
    pub name: String,
    pub color: crate::lan_api::DeviceColor,
    pub kelvin: u32,
}

/// Represents the device state; synthesized from the various
/// sources of facts that we have in the Device
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceState {
    /// Whether the device is powered on
    pub on: bool,
    /// Whether the light function of the device is powered on
    pub light_on: Option<bool>,

    /// Whether the device is connected to the Govee cloud
    pub online: Option<bool>,

    /// The color temperature in kelvin
    pub kelvin: u32,

    /// The color
    pub color: crate::lan_api::DeviceColor,

    /// The brightness in percent (0-100)
    pub brightness: u8,

    /// The active effect mode, if known
    pub scene: Option<String>,

    /// Where the information came from
    pub source: &'static str,
    pub updated: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct UndocDeviceInfo {
    pub room_name: Option<String>,
    pub entry: crate::undoc_api::DeviceEntry,
}

impl Device {
    /// Create a new device given just its sku and id.
    /// No other facts are known or reflected by it at this time;
    /// they will need to be added by the caller.
    pub fn new<S: Into<String>, I: Into<String>>(sku: S, id: I) -> Self {
        Self {
            sku: sku.into(),
            id: id.into(),
            ..Self::default()
        }
    }

    /// Returns the device name; either the name defined in the Govee App,
    /// or, if we don't have the information for some reason, then we compute
    /// a name from the SKU and the last couple of bytes from the device id,
    /// similar to the device name that would show up in a BLE scan, or
    /// the default name for the device if not otherwise configured in the
    /// Govee App.
    pub fn name(&self) -> String {
        if let Some(name) = self.govee_name() {
            return name.to_string();
        }
        self.computed_name()
    }

    /// Returns the name defined for the device in the Govee App
    pub fn govee_name(&self) -> Option<&str> {
        if let Some(info) = &self.http_device_info {
            return Some(&info.device_name);
        }
        None
    }

    pub fn room_name(&self) -> Option<&str> {
        if let Some(info) = &self.undoc_device_info {
            return info.room_name.as_deref();
        }
        None
    }

    /// compute a name from the SKU and the last couple of bytes from the
    /// device id, similar to the device name that would show up in a BLE
    /// scan, or the default name for the device if not otherwise configured
    /// in the Govee App.
    pub fn computed_name(&self) -> String {
        // The id is usually "XX:XX:XX:XX:XX:XX:XX:XX" but some devices
        // report it without colons, and in lowercase.  Normalize it.
        let mut id = String::new();
        for c in self.id.chars() {
            if c == ':' {
                continue;
            }
            id.push(c.to_ascii_uppercase());
        }

        format!("{}_{}", self.sku, &id[id.len().saturating_sub(4)..])
    }

    pub fn preferred_poll_interval(&self) -> chrono::Duration {
        match self.device_type() {
            // If the kettle is on, read its temperature more frequently
            DeviceType::Kettle => {
                if self.device_state().map(|s| s.on).unwrap_or(false) {
                    chrono::Duration::seconds(60)
                } else {
                    *POLL_INTERVAL
                }
            }
            _ => *POLL_INTERVAL,
        }
    }

    pub fn ip_addr(&self) -> Option<IpAddr> {
        self.lan_device.as_ref().map(|device| device.ip)
    }

    pub fn set_last_polled(&mut self) {
        self.last_polled.replace(Utc::now());
    }

    pub fn set_nightlight_state(&mut self, params: NotifyHumidifierNightlightParams) {
        self.nightlight_state.replace(params);
    }

    pub fn set_target_humidity(&mut self, percent: u8) {
        self.target_humidity_percent.replace(percent);
    }

    pub fn set_humidifier_work_mode_and_param(&mut self, mode: u8, param: u8) {
        self.humidifier_work_mode.replace(mode);
        self.humidifier_param_by_mode.insert(mode, param);
    }

    /// Update the LAN device information
    pub fn set_lan_device(&mut self, device: LanDevice) {
        self.lan_device.replace(device);
        self.last_lan_device_update.replace(Utc::now());
    }

    /// Update the LAN device status information
    pub fn set_lan_device_status(&mut self, status: LanDeviceStatus) -> bool {
        let changed = self
            .lan_device_status
            .as_ref()
            .map(|prior| *prior != status)
            .unwrap_or(true);
        self.lan_device_status.replace(status);
        self.last_lan_device_status_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
        changed
    }

    pub fn set_iot_device_status(&mut self, status: LanDeviceStatus) {
        self.iot_device_status.replace(status);
        self.last_iot_device_status_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
    }

    pub fn set_http_device_info(&mut self, info: HttpDeviceInfo) {
        self.http_device_info.replace(info);
        self.last_http_device_update.replace(Utc::now());
    }

    pub fn set_http_device_state(&mut self, state: HttpDeviceState) {
        self.http_device_state.replace(state);
        self.last_http_device_state_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
    }

    pub fn set_undoc_device_info(
        &mut self,
        entry: crate::undoc_api::DeviceEntry,
        room_name: Option<&str>,
    ) {
        self.undoc_device_info.replace(UndocDeviceInfo {
            entry,
            room_name: room_name.map(|s| s.to_string()),
        });
        self.last_undoc_device_info_update.replace(Utc::now());
        self.clear_scene_if_color_changed();
    }

    pub fn compute_iot_device_state(&self) -> Option<DeviceState> {
        let updated = self.last_iot_device_status_update?;
        let status = self.iot_device_status.as_ref()?;

        Some(DeviceState {
            on: status.on,
            light_on: if self.device_type() == DeviceType::Light {
                Some(status.on)
            } else {
                self.nightlight_state.as_ref().map(|s| s.on)
            },
            online: None,
            brightness: status.brightness,
            color: status.color,
            kelvin: status.color_temperature_kelvin,
            scene: self.active_scene.as_ref().map(|info| info.name.to_string()),
            source: "AWS IoT API",
            updated,
        })
    }

    pub fn compute_lan_device_state(&self) -> Option<DeviceState> {
        let updated = self.last_lan_device_status_update?;
        let status = self.lan_device_status.as_ref()?;

        Some(DeviceState {
            on: status.on,
            light_on: Some(status.on), // assumption: LAN API == light
            online: None,
            brightness: status.brightness,
            color: status.color,
            kelvin: status.color_temperature_kelvin,
            scene: self.active_scene.as_ref().map(|info| info.name.to_string()),
            source: "LAN API",
            updated,
        })
    }

    pub fn compute_http_device_state(&self) -> Option<DeviceState> {
        let updated = self.last_http_device_state_update?;
        let state = self.http_device_state.as_ref()?;

        let mut online = None;
        let mut on = false;
        let mut light_on = None;
        let mut brightness = 0;
        let mut color = DeviceColor::default();
        let mut kelvin = 0;

        #[derive(serde::Deserialize)]
        struct IntegerValueState {
            value: u32,
        }
        #[derive(serde::Deserialize)]
        struct BoolValueState {
            value: bool,
        }

        let light_instance = self.get_light_power_toggle_instance_name();

        for cap in &state.capabilities {
            if let Ok(value) = serde_json::from_value::<IntegerValueState>(cap.state.clone()) {
                if light_instance
                    .map(|inst| inst == cap.instance.as_str())
                    .unwrap_or(false)
                {
                    light_on.replace(value.value != 0);
                }

                match cap.instance.as_str() {
                    "powerSwitch" => {
                        on = value.value != 0;
                    }
                    "colorRgb" => {
                        color = DeviceColor {
                            r: ((value.value >> 16) & 0xff) as u8,
                            g: ((value.value >> 8) & 0xff) as u8,
                            b: (value.value & 0xff) as u8,
                        };
                    }
                    "brightness" => {
                        brightness = value.value as u8;
                    }
                    "colorTemperatureK" => {
                        kelvin = value.value;
                    }
                    _ => {}
                }
            } else if cap.instance == "online" {
                if let Ok(value) = serde_json::from_value::<BoolValueState>(cap.state.clone()) {
                    online.replace(value.value);
                }
            }
        }

        Some(DeviceState {
            on,
            light_on,
            online,
            brightness,
            color,
            kelvin,
            scene: self.active_scene.as_ref().map(|info| info.name.to_string()),
            source: "PLATFORM API",
            updated,
        })
    }

    /// Returns the most recently received state information
    pub fn device_state(&self) -> Option<DeviceState> {
        let mut candidates = vec![];

        if let Some(state) = self.compute_lan_device_state() {
            candidates.push(state);
        }
        if let Some(state) = self.compute_http_device_state() {
            candidates.push(state);
        }
        if let Some(state) = self.compute_iot_device_state() {
            candidates.push(state);
        }

        candidates.sort_by(|a, b| a.updated.cmp(&b.updated));

        candidates.pop()
    }

    /// Records the active scene name
    pub fn set_active_scene(&mut self, scene: Option<&str>) {
        match scene {
            None => {
                self.active_scene.take();
            }
            Some(scene) => {
                let (color, kelvin) = self
                    .device_state()
                    .map(|s| (s.color, s.kelvin))
                    .unwrap_or_default();
                self.active_scene.replace(ActiveSceneInfo {
                    name: scene.to_string(),
                    color,
                    kelvin,
                });
            }
        }
    }

    pub fn clear_scene_if_color_changed(&mut self) {
        if let Some(info) = &self.active_scene {
            let current = self
                .device_state()
                .map(|s| (s.color, s.kelvin))
                .unwrap_or_default();
            let scene_state = (info.color, info.kelvin);
            if current != scene_state {
                log::info!(
                    "Clearing reported scene because current {current:?} != {scene_state:?}"
                );
                self.active_scene.take();
            }
        }
    }

    pub fn device_type(&self) -> DeviceType {
        if let Some(info) = &self.http_device_info {
            info.device_type.clone()
        } else if let Some(q) = resolve_quirk(&self.sku) {
            q.device_type.clone()
        } else {
            DeviceType::Light
        }
    }

    /// Indicate whether we require the platform API data in order
    /// to correctly report the device
    pub fn needs_platform_poll(&self) -> bool {
        if !self.iot_api_supported() {
            return true;
        }

        let device_type = self.device_type();
        match (device_type, self.sku.as_str()) {
            (_, "H7160") => false,
            (DeviceType::Humidifier, _) => true,
            (DeviceType::Light, _) => false,
            (DeviceType::Kettle, _) => true,
            _ => true,
        }
    }

    pub fn pollable_via_lan(&self) -> bool {
        self.lan_device.is_some()
    }

    pub fn pollable_via_iot(&self) -> bool {
        if !self.iot_api_supported() {
            return false;
        }
        let device_type = self.device_type();
        matches!(
            (device_type, self.sku.as_str()),
            (_, "H7160") | (DeviceType::Light, _)
        )
    }

    pub fn avoid_platform_api(&self) -> bool {
        if let Some(quirk) = self.resolve_quirk() {
            if quirk.avoid_platform_api {
                return true;
            }
            if self.lan_device.is_some()
                && !self
                    .http_device_info
                    .as_ref()
                    .map(|info| info.supports_rgb())
                    .unwrap_or(false)
            {
                // Conflicting information:
                // Platform API says that this device isn't
                // a light, but the LAN API support suggests
                // that it is a light!
                // Therefore we will not trust the Platform API
                return true;
            }
        }
        false
    }

    pub fn resolve_quirk(&self) -> Option<Quirk> {
        match resolve_quirk(&self.sku) {
            Some(q) => Some(q.clone()),
            None => {
                // It's an unknown device, but since it showed up via LAN disco,
                // we can assume that it is a light
                if self.lan_device.is_some() {
                    Some(Quirk::light(Cow::Owned(self.sku.to_string()), BULB).with_lan_api())
                } else {
                    None
                }
            }
        }
    }

    pub fn get_capability_by_instance(&self, instance: &str) -> Option<&DeviceCapability> {
        self.http_device_info
            .as_ref()
            .and_then(|info| info.capability_by_instance(instance))
    }

    /// True for devices whose light uses LAN while their fan state is only
    /// available from the platform API.
    pub fn tracks_fan_state(&self) -> bool {
        self.sku == "H1310"
    }

    pub fn fan_poll_due(&self, now: DateTime<Utc>) -> bool {
        self.tracks_fan_state()
            && !self
                .last_fan_poll
                .is_some_and(|last| now - last < *FAN_POLL_INTERVAL)
    }

    /// Update fan state from a platform poll, returning true if it changed.
    ///
    /// This doesn't touch `http_device_state`, so a fan poll can't replace
    /// fresher LAN light state. Empty values keep the last known state, and
    /// a poll that started before a command, or just after one, is ignored
    /// because it can hold the old state. `started` is when the request began,
    /// so a slow response can't get past this check.
    pub fn record_fan_poll(&mut self, state: &HttpDeviceState, started: DateTime<Utc>) -> bool {
        if !self.tracks_fan_state()
            || self
                .last_fan_command
                .is_some_and(|at| started - at < *FAN_COMMAND_SETTLE_TIME)
        {
            return false;
        }

        let numeric_value = |instance| {
            state
                .capability_by_instance(instance)
                .and_then(|cap| cap.state.pointer("/value"))
                .filter(|value| value.is_i64())
        };
        let speed = numeric_value("fanSpeedMode")
            .cloned()
            .or_else(|| self.fan_speed_mode.clone());
        let toggle = numeric_value("fanToggle")
            .map(|value| value.as_i64() != Some(0))
            .or(self.fan_toggle_on);

        let changed = speed != self.fan_speed_mode || toggle != self.fan_toggle_on;
        self.fan_speed_mode = speed;
        self.fan_toggle_on = toggle;
        changed
    }

    /// Setting a speed also turns the fan on.
    pub fn record_fan_speed_command(&mut self, value: JsonValue, now: DateTime<Utc>) {
        self.fan_speed_mode = Some(value);
        self.record_fan_toggle_command(true, now);
    }

    pub fn record_fan_toggle_command(&mut self, on: bool, now: DateTime<Utc>) {
        self.fan_toggle_on = Some(on);
        self.last_fan_command = Some(now);
    }

    /// The option name of the last known fan speed, if metadata lists it.
    pub fn fan_speed_label(&self) -> Option<String> {
        let value = self.fan_speed_mode.as_ref()?;
        match &self.get_capability_by_instance("fanSpeedMode")?.parameters {
            Some(DeviceParameters::Enum { options }) => options
                .iter()
                .find(|o| &o.value == value)
                .map(|o| o.name.clone()),
            _ => None,
        }
    }

    pub fn get_state_capability_by_instance(
        &self,
        instance: &str,
    ) -> Option<&DeviceCapabilityState> {
        self.http_device_state
            .as_ref()
            .and_then(|info| info.capability_by_instance(instance))
    }

    pub fn get_light_power_toggle_instance_name(&self) -> Option<&'static str> {
        match self.device_type() {
            DeviceType::Light => Some("powerSwitch"),
            _ => {
                // If the device's primary function is not a light,
                // then we need to avoid powering on its other function
                // here.  If it has a nightlight capability, that is
                // probably what we are controlling.
                // We may need to expand this to other power toggles
                // in the future.
                if self
                    .get_capability_by_instance("nightlightToggle")
                    .is_some()
                {
                    Some("nightlightToggle")
                } else {
                    None
                }
            }
        }
    }

    pub fn get_color_temperature_range(&self) -> Option<(u32, u32)> {
        if let Some(quirk) = self.resolve_quirk() {
            return quirk.color_temp_range;
        }

        if self.lan_device.is_some() {
            // LAN API support suggests that it is a light
            return Some((2000, 9000));
        }

        self.http_device_info
            .as_ref()
            .and_then(|info| info.get_color_temperature_range())
    }

    pub fn supports_brightness(&self) -> bool {
        if let Some(quirk) = self.resolve_quirk() {
            return quirk.supports_brightness;
        }

        if self.lan_device.is_some() {
            // LAN API support suggests that it is a light
            return true;
        }

        self.http_device_info
            .as_ref()
            .map(|info| info.supports_brightness())
            .unwrap_or(false)
    }

    pub fn iot_api_supported(&self) -> bool {
        if let Some(quirk) = self.resolve_quirk() {
            return quirk.iot_api_supported;
        }

        false
    }

    pub fn supports_rgb(&self) -> bool {
        if let Some(quirk) = self.resolve_quirk() {
            return quirk.supports_rgb;
        }

        if self.lan_device.is_some() {
            // LAN API support suggests that it is a light
            return true;
        }

        self.http_device_info
            .as_ref()
            .map(|info| info.supports_rgb())
            .unwrap_or(false)
    }

    pub fn is_ble_only_device(&self) -> Option<bool> {
        if let Some(quirk) = self.resolve_quirk() {
            return Some(quirk.ble_only);
        }

        if self.http_device_info.is_some() {
            // truly BLE-only devices are not returned via the Platform API,
            // unless we have a quirk to say otherwise
            return Some(false);
        }

        self.undoc_device_info
            .as_ref()
            .map(|info| info.entry.device_ext.device_settings.wifi_name.is_none())
    }

    pub fn is_controllable(&self) -> bool {
        !matches!(self.is_ble_only_device(), Some(true))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::platform_api::{from_json, DeviceCapabilityKind};
    use serde_json::json;

    fn h1310_fan() -> Device {
        let mut device = Device::new("H1310", "AA:BB:CC:DD:EE:FF:00:01");
        device.http_device_info =
            Some(from_json(include_str!("../../test-data/h1310_fan_speed.json")).unwrap());
        device
    }

    fn h1310_poll(speed: JsonValue, toggle: JsonValue) -> HttpDeviceState {
        let capability = |kind, instance: &str, value| DeviceCapabilityState {
            kind,
            instance: instance.to_string(),
            state: json!({ "value": value }),
        };
        HttpDeviceState {
            sku: "H1310".to_string(),
            device: "AA:BB:CC:DD:EE:FF:00:01".to_string(),
            capabilities: vec![
                capability(DeviceCapabilityKind::Mode, "fanSpeedMode", speed),
                capability(DeviceCapabilityKind::Toggle, "fanToggle", toggle),
            ],
        }
    }

    #[test]
    fn fan_poll_sets_speed_and_toggle() {
        let mut device = h1310_fan();
        assert_eq!(device.fan_speed_label(), None);
        assert_eq!(device.fan_toggle_on, None);

        let changed = device.record_fan_poll(&h1310_poll(json!(1), json!(0)), Utc::now());

        assert!(changed);
        assert_eq!(device.fan_speed_label().as_deref(), Some("Speed 1"));
        assert_eq!(device.fan_toggle_on, Some(false));
    }

    #[test]
    fn fan_poll_reports_no_change_for_same_values() {
        let mut device = h1310_fan();
        device.record_fan_poll(&h1310_poll(json!(1), json!(0)), Utc::now());
        assert!(!device.record_fan_poll(&h1310_poll(json!(1), json!(0)), Utc::now()));
    }

    #[test]
    fn fan_poll_leaves_light_state_alone() {
        let mut device = h1310_fan();
        device.record_fan_poll(&h1310_poll(json!(1), json!(0)), Utc::now());
        assert!(device.http_device_state.is_none());
        assert!(device.last_http_device_state_update.is_none());
    }

    #[test]
    fn empty_fan_poll_keeps_last_known_state() {
        let mut device = h1310_fan();
        device.record_fan_poll(&h1310_poll(json!(3), json!(1)), Utc::now());

        let changed = device.record_fan_poll(&h1310_poll(json!(""), json!("")), Utc::now());

        assert!(!changed);
        assert_eq!(device.fan_speed_label().as_deref(), Some("Speed 3"));
        assert_eq!(device.fan_toggle_on, Some(true));
    }

    #[test]
    fn speed_command_marks_fan_on() {
        let mut device = h1310_fan();
        let now = Utc::now();
        device.record_fan_poll(&h1310_poll(json!(1), json!(0)), now);

        device.record_fan_speed_command(json!(2), now);

        assert_eq!(device.fan_speed_label().as_deref(), Some("Speed 2"));
        assert_eq!(device.fan_toggle_on, Some(true));
    }

    #[test]
    fn toggle_command_leaves_speed_alone() {
        let mut device = h1310_fan();
        let now = Utc::now();
        device.record_fan_poll(&h1310_poll(json!(2), json!(1)), now);

        device.record_fan_toggle_command(false, now);

        assert_eq!(device.fan_toggle_on, Some(false));
        assert_eq!(device.fan_speed_label().as_deref(), Some("Speed 2"));
    }

    #[test]
    fn poll_soon_after_command_does_not_restore_old_state() {
        let mut device = h1310_fan();
        let now = Utc::now();
        device.record_fan_poll(&h1310_poll(json!(1), json!(0)), now);
        device.record_fan_speed_command(json!(2), now);

        let stale = now + chrono::Duration::seconds(4);
        assert!(!device.record_fan_poll(&h1310_poll(json!(1), json!(0)), stale));
        assert_eq!(device.fan_speed_label().as_deref(), Some("Speed 2"));
        assert_eq!(device.fan_toggle_on, Some(true));

        // After the window, the device is the authority again.
        let later = now + *FAN_COMMAND_SETTLE_TIME + chrono::Duration::seconds(1);
        assert!(device.record_fan_poll(&h1310_poll(json!(1), json!(0)), later));
        assert_eq!(device.fan_speed_label().as_deref(), Some("Speed 1"));
        assert_eq!(device.fan_toggle_on, Some(false));
    }

    #[test]
    fn slow_poll_started_before_command_does_not_restore_old_state() {
        let mut device = h1310_fan();
        let now = Utc::now();
        device.record_fan_poll(&h1310_poll(json!(1), json!(0)), now);
        device.record_fan_speed_command(json!(2), now);

        let started_before_command = now - chrono::Duration::seconds(1);
        assert!(!device.record_fan_poll(&h1310_poll(json!(1), json!(0)), started_before_command));
        assert_eq!(device.fan_speed_label().as_deref(), Some("Speed 2"));
        assert_eq!(device.fan_toggle_on, Some(true));
    }

    #[test]
    fn fan_poll_due_only_for_h1310_at_the_interval() {
        let now = Utc::now();
        let mut device = h1310_fan();
        assert!(device.fan_poll_due(now));

        device.last_fan_poll = Some(now);
        assert!(!device.fan_poll_due(now + chrono::Duration::seconds(59)));
        assert!(device.fan_poll_due(now + *FAN_POLL_INTERVAL));

        let mut other = Device::new("H1370", "aa:bb");
        other.http_device_info = device.http_device_info.clone();
        assert!(!other.fan_poll_due(now));
    }

    #[test]
    fn other_skus_ignore_fan_state() {
        let mut device = Device::new("H1370", "aa:bb");
        assert!(!device.record_fan_poll(&h1310_poll(json!(1), json!(1)), Utc::now()));
        assert_eq!(device.fan_speed_mode, None);
        assert_eq!(device.fan_toggle_on, None);
    }

    #[test]
    fn name_compute() {
        let device = Device::new("H6000", "AA:BB:CC:DD:EE:FF:42:2A");
        assert_eq!(device.name(), "H6000_422A");

        let device = Device::new("H6127", "cef142b0b354995f");
        assert_eq!(device.name(), "H6127_995F");

        let device = Device::new("H6127", "ce");
        assert_eq!(device.name(), "H6127_CE");
    }
}

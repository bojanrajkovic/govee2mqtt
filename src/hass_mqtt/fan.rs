use crate::hass_mqtt::base::{Device, EntityConfig, Origin};
use crate::hass_mqtt::instance::{publish_entity_config, EntityInstance};
use crate::platform_api::{DeviceCapability, DeviceParameters};
use crate::service::coordinator::Coordinator;
use crate::service::device::Device as ServiceDevice;
use crate::service::hass::{
    availability_topic, switch_instance_state_topic, topic_safe_id, HassClient, IdParameter,
};
use crate::service::state::StateHandle;
use anyhow::{anyhow, Context};
use chrono::Utc;
use mosquitto_rs::router::{Params, Payload, State};
use serde::Serialize;

/// <https://www.home-assistant.io/integrations/fan.mqtt/>
///
/// The fan replaces the fanToggle and reverseAirflowToggle switches. Its on/off
/// topics are the ones the fanToggle switch used, so the switch command
/// handler still turns the fan on and off.
///
/// Hass draws a slider rather than buttons for six speeds plus off, so each
/// speed is also a preset. The speed range is 1-100 so that hass passes
/// percentages through unchanged, and the bridge maps them to speeds.
#[derive(Serialize, Clone, Debug)]
pub struct FanConfig {
    #[serde(flatten)]
    pub base: EntityConfig,
    pub command_topic: String,
    pub state_topic: String,
    pub percentage_command_topic: String,
    pub percentage_state_topic: String,
    pub speed_range_min: i64,
    pub speed_range_max: i64,
    pub preset_mode_command_topic: String,
    pub preset_mode_state_topic: String,
    pub preset_modes: Vec<String>,
    /// Only present when the device has a reverseAirflowToggle capability.
    #[serde(flatten)]
    pub direction: Option<FanDirectionTopics>,
}

#[derive(Serialize, Clone, Debug)]
pub struct FanDirectionTopics {
    pub direction_command_topic: String,
    pub direction_state_topic: String,
}

impl FanConfig {
    fn for_device(device: &ServiceDevice, speeds: &[FanSpeed], has_direction: bool) -> Self {
        let id = topic_safe_id(device);

        Self {
            base: EntityConfig {
                availability_topic: availability_topic(),
                name: Some("Fan".to_string()),
                device_class: None,
                origin: Origin::default(),
                device: Device::for_device(device),
                unique_id: format!("gv2mqtt-{id}-fan"),
                entity_category: None,
                icon: Some("mdi:fan".to_string()),
            },
            command_topic: format!("gv2mqtt/switch/{id}/command/fanToggle"),
            state_topic: switch_instance_state_topic(device, "fanToggle"),
            percentage_command_topic: format!("gv2mqtt/{id}/set-fan-percentage"),
            percentage_state_topic: format!("gv2mqtt/{id}/notify-fan-percentage"),
            speed_range_min: 1,
            speed_range_max: 100,
            preset_mode_command_topic: format!("gv2mqtt/{id}/set-fan-preset"),
            preset_mode_state_topic: format!("gv2mqtt/{id}/notify-fan-preset"),
            preset_modes: speeds.iter().map(|s| s.label.clone()).collect(),
            direction: has_direction.then(|| FanDirectionTopics {
                direction_command_topic: format!("gv2mqtt/{id}/set-fan-direction"),
                direction_state_topic: format!("gv2mqtt/{id}/notify-fan-direction"),
            }),
        }
    }
}

/// The H1310 blows down (hass's "forward") while reverseAirflowToggle is on.
fn fan_direction(reverse_airflow_on: bool) -> &'static str {
    if reverse_airflow_on {
        "forward"
    } else {
        "reverse"
    }
}

fn reverse_airflow_for_direction(direction: &str) -> Option<bool> {
    match direction {
        "forward" => Some(true),
        "reverse" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FanSpeed {
    pub value: i64,
    pub label: String,
}

/// The numeric speeds in the capability metadata, in ascending order.
fn fan_speeds(cap: &DeviceCapability) -> Vec<FanSpeed> {
    let mut speeds: Vec<FanSpeed> = match &cap.parameters {
        Some(DeviceParameters::Enum { options }) => options
            .iter()
            .filter_map(|o| {
                Some(FanSpeed {
                    value: o.value.as_i64()?,
                    label: o.name.clone(),
                })
            })
            .collect(),
        _ => vec![],
    };
    speeds.sort_by_key(|s| s.value);
    speeds
}

#[derive(Debug, PartialEq)]
pub enum FanPercentageCommand {
    TurnOff,
    SetSpeed(i64),
}

/// Rounds to the nearest speed rather than up, so that each percentage from
/// `speed_to_percentage` maps back to its own speed: 17% is speed 1, not 2.
/// Returns None for a percentage outside 0-100.
fn percentage_to_speed(percentage: i64, max_speed: i64) -> Option<FanPercentageCommand> {
    match percentage {
        0 => Some(FanPercentageCommand::TurnOff),
        1..=100 => Some(FanPercentageCommand::SetSpeed(
            ((percentage * max_speed + 50) / 100).clamp(1, max_speed),
        )),
        _ => None,
    }
}

fn speed_to_percentage(speed: i64, max_speed: i64) -> i64 {
    (speed * 100 + max_speed / 2) / max_speed
}

fn max_speed(speeds: &[FanSpeed]) -> Option<i64> {
    speeds.last().map(|s| s.value)
}

pub struct FanEntity {
    fan: FanConfig,
    speeds: Vec<FanSpeed>,
    device_id: String,
    topic_id: String,
    state: StateHandle,
}

impl FanEntity {
    /// Returns None without numeric speeds or a fanToggle capability, because
    /// the fan's on/off control relies on that capability.
    pub fn new(
        device: &ServiceDevice,
        state: &StateHandle,
        cap: &DeviceCapability,
    ) -> Option<Self> {
        device.get_capability_by_instance("fanToggle")?;
        let speeds = fan_speeds(cap);
        max_speed(&speeds)?;
        let has_direction = device
            .get_capability_by_instance("reverseAirflowToggle")
            .is_some();

        Some(Self {
            fan: FanConfig::for_device(device, &speeds, has_direction),
            speeds,
            device_id: device.id.to_string(),
            topic_id: topic_safe_id(device),
            state: state.clone(),
        })
    }

    /// The toggle capabilities this fan controls, which get no switch entity.
    pub fn replaced_switch_instances(&self) -> &'static [&'static str] {
        if self.fan.direction.is_some() {
            &["fanToggle", "reverseAirflowToggle"]
        } else {
            &["fanToggle"]
        }
    }
}

#[async_trait::async_trait]
impl EntityInstance for FanEntity {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        // This entity replaced the Fan Speed select and the switches it
        // controls. An empty config makes hass delete each one if it still has
        // it from this session.
        let disco = state.get_hass_disco_prefix().await;
        let id = &self.topic_id;
        let replaced = std::iter::once(("select", "fanSpeedMode")).chain(
            self.replaced_switch_instances()
                .iter()
                .map(|i| ("switch", *i)),
        );
        for (integration, instance) in replaced {
            client
                .publish(
                    format!("{disco}/{integration}/gv2mqtt-{id}-{instance}/config"),
                    "",
                )
                .await?;
        }

        publish_entity_config("fan", state, client, &self.fan.base, &self.fan).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let device = self
            .state
            .device_by_id(&self.device_id)
            .await
            .expect("device to exist");

        if let Some(on) = device.fan_toggle_on {
            client
                .publish(&self.fan.state_topic, if on { "ON" } else { "OFF" })
                .await?;
        }

        if let (Some(topics), Some(reverse_airflow_on)) =
            (&self.fan.direction, device.fan_reverse_airflow_on)
        {
            client
                .publish(
                    &topics.direction_state_topic,
                    fan_direction(reverse_airflow_on),
                )
                .await?;
        }

        let Some(speed) = device.fan_speed_mode.as_ref().and_then(|v| v.as_i64()) else {
            return Ok(());
        };

        // Hass stores these separately, and neither message clears the other.
        let is_known_speed = self.speeds.iter().any(|s| s.value == speed);
        if let (true, Some(max_speed)) = (is_known_speed, max_speed(&self.speeds)) {
            client
                .publish(
                    &self.fan.percentage_state_topic,
                    speed_to_percentage(speed, max_speed).to_string(),
                )
                .await?;
        } else if device.fan_toggle_on != Some(false) {
            log::warn!("{device} reports unknown fan speed {speed}");
        }

        if let Some(preset) = fan_preset_state(&self.speeds, speed, device.fan_toggle_on) {
            client
                .publish(&self.fan.preset_mode_state_topic, preset)
                .await?;
        }

        Ok(())
    }
}

/// The preset to publish. While the fan is off this is hass's default preset
/// reset payload, because hass sends nothing when the selected preset is
/// tapped again, so a highlighted preset couldn't turn the fan back on.
fn fan_preset_state(speeds: &[FanSpeed], speed: i64, fan_on: Option<bool>) -> Option<&str> {
    if fan_on == Some(false) {
        return Some("None");
    }
    speeds
        .iter()
        .find(|s| s.value == speed)
        .map(|s| s.label.as_str())
}

async fn set_fan_speed(
    state: &StateHandle,
    device: &Coordinator,
    cap: &DeviceCapability,
    speed: i64,
) -> anyhow::Result<()> {
    state
        .device_control(device, cap, speed)
        .await
        .context("set_fan_speed: state.device_control")?;

    state
        .device_mut(&device.sku, &device.id)
        .await
        .record_fan_speed_command(speed.into(), Utc::now());
    state.notify_of_state_change(&device.id).await
}

pub async fn mqtt_set_fan_percentage(
    Payload(percentage): Payload<i64>,
    Params(IdParameter { id }): Params<IdParameter>,
    State(state): State<StateHandle>,
) -> anyhow::Result<()> {
    let device = state.resolve_device_for_control(&id).await?;

    let cap = device
        .get_capability_by_instance("fanSpeedMode")
        .ok_or_else(|| anyhow!("{id} has no fanSpeedMode"))?;
    let max_speed = max_speed(&fan_speeds(cap)).ok_or_else(|| anyhow!("{id} has no fan speeds"))?;

    match percentage_to_speed(percentage, max_speed)
        .ok_or_else(|| anyhow!("invalid fan percentage {percentage} for {id}"))?
    {
        FanPercentageCommand::TurnOff => state.device_set_fan_toggle(&device, false).await,
        FanPercentageCommand::SetSpeed(speed) => set_fan_speed(&state, &device, cap, speed).await,
    }
}

pub async fn mqtt_set_fan_preset(
    Payload(preset): Payload<String>,
    Params(IdParameter { id }): Params<IdParameter>,
    State(state): State<StateHandle>,
) -> anyhow::Result<()> {
    let device = state.resolve_device_for_control(&id).await?;

    let cap = device
        .get_capability_by_instance("fanSpeedMode")
        .ok_or_else(|| anyhow!("{id} has no fanSpeedMode"))?;
    let speed = fan_speeds(cap)
        .into_iter()
        .find(|s| s.label == preset)
        .ok_or_else(|| anyhow!("fan preset {preset} not found for {id}"))?
        .value;

    set_fan_speed(&state, &device, cap, speed).await
}

pub async fn mqtt_set_fan_direction(
    Payload(direction): Payload<String>,
    Params(IdParameter { id }): Params<IdParameter>,
    State(state): State<StateHandle>,
) -> anyhow::Result<()> {
    let device = state.resolve_device_for_control(&id).await?;
    let on = reverse_airflow_for_direction(&direction)
        .ok_or_else(|| anyhow!("invalid fan direction {direction} for {id}"))?;

    state.device_set_fan_reverse_airflow(&device, on).await
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::hass_mqtt::switch::SwitchConfig;
    use crate::platform_api::{from_json, HttpDeviceInfo};
    use crate::service::state::State as ServiceState;
    use serde_json::json;
    use std::sync::Arc;

    fn h1310() -> (ServiceDevice, DeviceCapability) {
        let info: HttpDeviceInfo =
            from_json(include_str!("../../test-data/h1310_fan_speed.json")).unwrap();
        let cap = info.capability_by_instance("fanSpeedMode").unwrap().clone();
        let mut device = ServiceDevice::new("H1310", &info.device);
        device.http_device_info = Some(info);
        (device, cap)
    }

    fn fan_entity(device: &ServiceDevice, cap: &DeviceCapability) -> Option<FanEntity> {
        FanEntity::new(device, &Arc::new(ServiceState::new()), cap)
    }

    #[test]
    fn discovery_config() {
        let (device, cap) = h1310();
        let fan = fan_entity(&device, &cap).unwrap();
        let config = serde_json::to_value(&fan.fan).unwrap();

        let id = "AABBCCDDEEFF0001";
        for (key, expected) in [
            ("name", json!("Fan")),
            ("icon", json!("mdi:fan")),
            ("unique_id", json!(format!("gv2mqtt-{id}-fan"))),
            (
                "percentage_command_topic",
                json!(format!("gv2mqtt/{id}/set-fan-percentage")),
            ),
            (
                "percentage_state_topic",
                json!(format!("gv2mqtt/{id}/notify-fan-percentage")),
            ),
            ("speed_range_min", json!(1)),
            ("speed_range_max", json!(100)),
            (
                "preset_mode_command_topic",
                json!(format!("gv2mqtt/{id}/set-fan-preset")),
            ),
            (
                "preset_mode_state_topic",
                json!(format!("gv2mqtt/{id}/notify-fan-preset")),
            ),
            (
                "preset_modes",
                json!(["Speed 1", "Speed 2", "Speed 3", "Speed 4", "Speed 5", "Speed 6"]),
            ),
        ] {
            assert_eq!(config[key], expected, "{key}");
        }
    }

    #[test]
    fn direction_topics_follow_the_reverse_airflow_capability() {
        let (mut device, cap) = h1310();
        let id = "AABBCCDDEEFF0001";

        let config = serde_json::to_value(&fan_entity(&device, &cap).unwrap().fan).unwrap();
        assert_eq!(
            config["direction_command_topic"],
            json!(format!("gv2mqtt/{id}/set-fan-direction"))
        );
        assert_eq!(
            config["direction_state_topic"],
            json!(format!("gv2mqtt/{id}/notify-fan-direction"))
        );

        let info = device.http_device_info.as_mut().unwrap();
        info.capabilities
            .retain(|c| c.instance != "reverseAirflowToggle");
        let config = serde_json::to_value(&fan_entity(&device, &cap).unwrap().fan).unwrap();
        assert!(config.get("direction_command_topic").is_none());
        assert!(config.get("direction_state_topic").is_none());
    }

    #[test]
    fn fan_replaces_only_the_switches_it_controls() {
        let (mut device, cap) = h1310();
        let fan = fan_entity(&device, &cap).unwrap();
        assert_eq!(
            fan.replaced_switch_instances(),
            ["fanToggle", "reverseAirflowToggle"]
        );

        let info = device.http_device_info.as_mut().unwrap();
        info.capabilities
            .retain(|c| c.instance != "reverseAirflowToggle");
        let fan = fan_entity(&device, &cap).unwrap();
        assert_eq!(fan.replaced_switch_instances(), ["fanToggle"]);
    }

    #[test]
    fn directions_map_to_reverse_airflow() {
        for (reverse_airflow_on, direction) in [(true, "forward"), (false, "reverse")] {
            assert_eq!(fan_direction(reverse_airflow_on), direction);
            assert_eq!(
                reverse_airflow_for_direction(direction),
                Some(reverse_airflow_on)
            );
        }
        assert_eq!(reverse_airflow_for_direction("sideways"), None);
    }

    #[tokio::test]
    async fn on_off_topics_match_the_fan_toggle_switch() {
        let (device, cap) = h1310();
        let toggle = device.get_capability_by_instance("fanToggle").unwrap();
        let switch = SwitchConfig::for_device(&device, toggle).await.unwrap();

        let fan = fan_entity(&device, &cap).unwrap();

        assert_eq!(fan.fan.command_topic, switch.command_topic);
        assert_eq!(fan.fan.state_topic, switch.state_topic);
    }

    #[test]
    fn no_entity_without_fan_toggle() {
        let (mut device, cap) = h1310();
        let info = device.http_device_info.as_mut().unwrap();
        info.capabilities.retain(|c| c.instance != "fanToggle");
        assert!(fan_entity(&device, &cap).is_none());
    }

    #[test]
    fn no_entity_without_numeric_speeds() {
        let (device, mut cap) = h1310();
        cap.parameters = Some(DeviceParameters::Enum { options: vec![] });
        assert!(fan_entity(&device, &cap).is_none());
    }

    #[test]
    fn preset_state_clears_while_the_fan_is_off() {
        let (_, cap) = h1310();
        let speeds = fan_speeds(&cap);

        assert_eq!(fan_preset_state(&speeds, 2, Some(true)), Some("Speed 2"));
        assert_eq!(fan_preset_state(&speeds, 2, None), Some("Speed 2"));
        assert_eq!(fan_preset_state(&speeds, 2, Some(false)), Some("None"));
        assert_eq!(fan_preset_state(&speeds, 0, Some(false)), Some("None"));
        assert_eq!(fan_preset_state(&speeds, 0, Some(true)), None);
    }

    #[test]
    fn percentage_commands_map_to_speeds() {
        use FanPercentageCommand::*;
        for (percentage, expected) in [
            (0, Some(TurnOff)),
            (1, Some(SetSpeed(1))),
            (17, Some(SetSpeed(1))),
            (33, Some(SetSpeed(2))),
            (50, Some(SetSpeed(3))),
            (67, Some(SetSpeed(4))),
            (83, Some(SetSpeed(5))),
            (99, Some(SetSpeed(6))),
            (100, Some(SetSpeed(6))),
            (-1, None),
            (101, None),
        ] {
            assert_eq!(percentage_to_speed(percentage, 6), expected, "{percentage}");
        }
    }

    #[test]
    fn published_percentages_round_trip_to_their_speed() {
        for (speed, percentage) in [(1, 17), (2, 33), (3, 50), (4, 67), (5, 83), (6, 100)] {
            assert_eq!(speed_to_percentage(speed, 6), percentage);
            assert_eq!(
                percentage_to_speed(percentage, 6),
                Some(FanPercentageCommand::SetSpeed(speed))
            );
        }
    }

    #[test]
    fn every_percentage_maps_to_a_valid_speed() {
        for percentage in 1..=100 {
            let Some(FanPercentageCommand::SetSpeed(speed)) = percentage_to_speed(percentage, 6)
            else {
                panic!("{percentage} should set a speed");
            };
            assert!((1..=6).contains(&speed), "{percentage} -> {speed}");
        }
    }
}

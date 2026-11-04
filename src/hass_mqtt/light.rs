use crate::hass_mqtt::base::{Device, EntityConfig, Origin};
use crate::hass_mqtt::instance::{
    publish_entity_config, unpublish_replaced_entities, EntityInstance,
};
use crate::lan_api::DeviceColor;
use crate::platform_api::DeviceType;
use crate::service::device::{Device as ServiceDevice, SplitHalf};
use crate::service::hass::{
    availability_topic, kelvin_to_mired, light_segment_state_topic, light_state_topic,
    mired_to_kelvin, topic_safe_id, HassClient, HassLightCommand,
};
use crate::service::state::StateHandle;
use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value as JsonValue};

/// <https://www.home-assistant.io/integrations/light.mqtt/#json-schema>
#[derive(Serialize, Clone, Debug)]
pub struct LightConfig {
    #[serde(flatten)]
    pub base: EntityConfig,
    pub schema: String,

    pub command_topic: String,
    /// The docs say that this is optional, but hass errors out if
    /// it is not passed
    pub state_topic: String,
    pub optimistic: bool,
    pub supported_color_modes: Vec<String>,
    /// Flag that defines if the light supports brightness.
    pub brightness: bool,
    /// Defines the maximum brightness value (i.e., 100%) of the MQTT device.
    pub brightness_scale: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,

    /// Flag that defines if the light supports effects.
    pub effect: bool,
    /// The list of effects the light supports.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub effect_list: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_mireds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_mireds: Option<u32>,

    pub payload_available: String,

    /// Hass only reads this when it first creates the entity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_by_default: Option<bool>,
}

impl LightConfig {
    pub async fn publish(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        publish_entity_config("light", state, client, &self.base, self).await
    }
}

/// What a light entity controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightTarget {
    /// The whole device.
    Device,
    /// One segment, numbered from 0.
    Segment(u32),
    /// One of the two lights on a device with split lights.
    Half(SplitHalf),
}

/// The switches that the split lights replace. Each light turns itself on and
/// off, and the device turns its power on and off to match.
pub const SPLIT_LIGHT_REPLACED_SWITCHES: [&str; 3] =
    ["powerSwitch", "mainLightToggle", "backgroundLightToggle"];

#[derive(Clone)]
pub struct DeviceLight {
    light: LightConfig,
    target: LightTarget,
    topic_id: String,
    device_id: String,
    state: StateHandle,
}

#[async_trait]
impl EntityInstance for DeviceLight {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        if self.target == LightTarget::Half(SplitHalf::Main) {
            let replaced = SPLIT_LIGHT_REPLACED_SWITCHES.map(|i| ("switch", i));
            unpublish_replaced_entities(state, client, &self.topic_id, &replaced).await?;
        }

        self.light.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        if self.light.optimistic {
            return Ok(());
        }

        let device = self
            .state
            .device_by_id(&self.device_id)
            .await
            .expect("device to exist");

        if let LightTarget::Half(half) = self.target {
            // Only on/off is known. Brightness and color come from the echo of
            // each command, and hass keeps them when a message leaves them out.
            let state = device
                .split_light_on(half)
                .map(|on| if on { "ON" } else { "OFF" });
            return client
                .publish_obj(&self.light.state_topic, &json!({ "state": state }))
                .await;
        }

        match device.device_state() {
            Some(device_state) => {
                log::trace!("LightConfig::notify_state: state is {device_state:?}");

                let is_on = device_state.light_on.unwrap_or(false);

                let light_state = if is_on {
                    if device_state.kelvin == 0 {
                        json!({
                            "state": "ON",
                            "color_mode": "rgb",
                            "color": {
                                "r": device_state.color.r,
                                "g": device_state.color.g,
                                "b": device_state.color.b,
                            },
                            "brightness": device_state.brightness,
                            "effect": device_state.scene,
                        })
                    } else {
                        json!({
                            "state": "ON",
                            "color_mode": "color_temp",
                            "brightness": device_state.brightness,
                            "color_temp": kelvin_to_mired(device_state.kelvin),
                            "effect": device_state.scene,
                        })
                    }
                } else {
                    json!({"state":"OFF"})
                };

                client
                    .publish_obj(&self.light.state_topic, &light_state)
                    .await
            }
            None => {
                // TODO: mark as unavailable or something? Don't
                // want to prevent attempting to control it though,
                // as that could cause it to wake up.
                client
                    .publish_obj(&self.light.state_topic, &json!({"state":"OFF"}))
                    .await
            }
        }
    }
}

impl DeviceLight {
    pub async fn for_device(
        device: &ServiceDevice,
        state: &StateHandle,
        target: LightTarget,
    ) -> anyhow::Result<Self> {
        let quirk = device.resolve_quirk();
        let device_type = device.device_type();
        let id = topic_safe_id(device);

        let command_topic = match target {
            LightTarget::Device => format!("gv2mqtt/light/{id}/command"),
            LightTarget::Segment(seg) => format!("gv2mqtt/light/{id}/command/{seg}"),
            LightTarget::Half(half) => {
                format!("gv2mqtt/light/{id}/command/{}", half.topic_name())
            }
        };

        let icon = match target {
            LightTarget::Device if device_type == DeviceType::Light => {
                quirk.as_ref().map(|q| q.icon.to_string())
            }
            _ => None,
        };

        let state_topic = match target {
            LightTarget::Device => light_state_topic(device),
            LightTarget::Segment(seg) => light_segment_state_topic(device, seg),
            LightTarget::Half(half) => split_light_state_topic(device, half),
        };
        let availability_topic = availability_topic();
        // The main light keeps the id of the device light it replaced, so
        // hass keeps its history.
        let unique_id = match target {
            LightTarget::Device | LightTarget::Half(SplitHalf::Main) => format!("gv2mqtt-{id}"),
            LightTarget::Segment(seg) => format!("gv2mqtt-{id}-{seg}"),
            LightTarget::Half(half) => format!("gv2mqtt-{id}-{}", half.topic_name()),
        };

        // Scenes and color temperature change the whole device, so only the
        // main light offers them.
        let has_device_controls = matches!(
            target,
            LightTarget::Device | LightTarget::Half(SplitHalf::Main)
        );

        let effect_list = if has_device_controls {
            match state.device_list_scenes(device).await {
                Ok(scenes) => scenes,
                Err(err) => {
                    log::error!("Unable to list scenes for {device}: {err:#}");
                    vec![]
                }
            }
        } else {
            vec![]
        };

        let mut supported_color_modes = vec![];

        if target != LightTarget::Device || device.supports_rgb() {
            supported_color_modes.push("rgb".to_string());
        }

        let (min_mireds, max_mireds) = match device.get_color_temperature_range() {
            Some((min, max)) if has_device_controls => {
                supported_color_modes.push("color_temp".to_string());
                // Note that min and max are swapped by the translation
                // from kelvin to mired
                (Some(kelvin_to_mired(max)), Some(kelvin_to_mired(min)))
            }
            _ => (None, None),
        };

        let brightness = target != LightTarget::Device
            || quirk
                .as_ref()
                .map(|q| q.supports_brightness)
                .unwrap_or(false)
            || device
                .http_device_info
                .as_ref()
                .map(|info| info.supports_brightness())
                .unwrap_or(false);

        let name = match target {
            LightTarget::Segment(n) => Some(format!("Segment {:03}", n + 1)),
            LightTarget::Half(SplitHalf::Main) => Some("Main Light".to_string()),
            LightTarget::Half(SplitHalf::Background) => Some("Background Light".to_string()),
            LightTarget::Device if device_type == DeviceType::Humidifier => {
                Some("Night Light".to_string())
            }
            LightTarget::Device => None,
        };

        // The split lights cover what the segments do.
        let enabled_by_default = match target {
            LightTarget::Segment(_) if device.has_split_lights() => Some(false),
            _ => None,
        };

        Ok(Self {
            light: LightConfig {
                base: EntityConfig {
                    availability_topic,
                    name,
                    device_class: None,
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id,
                    entity_category: None,
                    icon: None,
                },
                schema: "json".to_string(),
                command_topic,
                state_topic,
                supported_color_modes,
                brightness,
                brightness_scale: 100,
                effect: target != LightTarget::Half(SplitHalf::Background),
                effect_list,
                payload_available: "online".to_string(),
                max_mireds,
                min_mireds,
                optimistic: matches!(target, LightTarget::Segment(_)),
                icon,
                enabled_by_default,
            },
            target,
            topic_id: id,
            device_id: device.id.to_string(),
            state: state.clone(),
        })
    }
}

fn split_light_state_topic(device: &ServiceDevice, half: SplitHalf) -> String {
    light_segment_state_topic(device, half.topic_name())
}

/// One Platform API call made for a split light command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitLightAction {
    Brightness(u8),
    Color(DeviceColor),
    ColorTemperature { kelvin: u32 },
    Scene(String),
    Toggle(bool),
}

impl SplitLightAction {
    /// True for actions that change both halves.
    fn affects_other_half(&self) -> bool {
        matches!(self, Self::ColorTemperature { .. } | Self::Scene(_))
    }
}

/// Decides which calls a hass command for one half needs, in order.
///
/// Brightness and color go before the toggle, because the device applies them
/// to a dark half and it then comes on at the new values. Every ON ends with
/// the toggle, because the known state can be up to a poll interval old.
/// Color temperature and scenes are only sent from the main light. Like the
/// device light, a scene skips any color in the same command.
pub fn split_light_plan(half: SplitHalf, command: &HassLightCommand) -> Vec<SplitLightAction> {
    if command.state == "OFF" {
        return vec![SplitLightAction::Toggle(false)];
    }

    let mut actions = vec![];
    if let Some(brightness) = command.brightness {
        actions.push(SplitLightAction::Brightness(brightness));
    }
    match (&command.effect, half) {
        (Some(effect), SplitHalf::Main) => actions.push(SplitLightAction::Scene(effect.clone())),
        _ => {
            if let Some(color) = command.color {
                actions.push(SplitLightAction::Color(color));
            }
            if let (Some(mired), SplitHalf::Main) = (command.color_temp, half) {
                actions.push(SplitLightAction::ColorTemperature {
                    kelvin: mired_to_kelvin(mired),
                });
            }
        }
    }
    actions.push(SplitLightAction::Toggle(true));
    actions
}

/// The state to publish after an ON command succeeds. Polls only report
/// on/off, so this is the only source of brightness and color.
fn split_light_echo(actions: &[SplitLightAction]) -> JsonValue {
    let mut echo = json!({ "state": "ON" });
    for action in actions {
        match action {
            SplitLightAction::Brightness(brightness) => echo["brightness"] = json!(brightness),
            SplitLightAction::Color(color) => {
                echo["color_mode"] = json!("rgb");
                echo["color"] = json!(color);
            }
            SplitLightAction::ColorTemperature { kelvin } => {
                echo["color_mode"] = json!("color_temp");
                echo["color_temp"] = json!(kelvin_to_mired(*kelvin));
            }
            SplitLightAction::Scene(scene) => echo["effect"] = json!(scene),
            SplitLightAction::Toggle(_) => {}
        }
    }
    echo
}

/// Runs a hass command for one half over the Platform API.
pub async fn split_light_command(
    state: &StateHandle,
    device: &ServiceDevice,
    half: SplitHalf,
    command: &HassLightCommand,
) -> anyhow::Result<()> {
    let (Some(client), Some(info)) = (state.get_platform_client().await, &device.http_device_info)
    else {
        anyhow::bail!(
            "set {half:?} light for {id}: Platform API is not available",
            id = device.id
        );
    };

    let actions = split_light_plan(half, command);
    log::info!(
        "{id} {half:?} light: {count} Platform API calls",
        id = device.id,
        count = actions.len()
    );

    // Before the toggle, so its state update carries this, and a poll that
    // started earlier can't restore the other half's old state.
    if actions.iter().any(SplitLightAction::affects_other_half) {
        state
            .device_mut(&device.sku, &device.id)
            .await
            .forget_split_light_state(half.other(), Utc::now());
    }

    for action in &actions {
        match action {
            SplitLightAction::Brightness(brightness) => {
                client
                    .set_segment_brightness(info, half.segments(), *brightness)
                    .await?;
            }
            SplitLightAction::Color(color) => {
                client
                    .set_segment_rgb(info, half.segments(), color.r, color.g, color.b)
                    .await?;
            }
            SplitLightAction::ColorTemperature { kelvin } => {
                client.set_color_temperature(info, *kelvin).await?;
            }
            SplitLightAction::Scene(scene) => state.device_set_scene(device, scene).await?,
            SplitLightAction::Toggle(on) => {
                state
                    .set_platform_toggle(device, half.toggle_instance(), *on, |device, on, now| {
                        device.record_split_light_command(half, on, now)
                    })
                    .await?;
            }
        }
    }

    if command.state != "OFF" {
        if let Some(hass) = state.get_hass_client().await {
            hass.publish_obj(
                split_light_state_topic(device, half),
                &split_light_echo(&actions),
            )
            .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use SplitLightAction::*;

    fn command(json: JsonValue) -> HassLightCommand {
        serde_json::from_value(json).unwrap()
    }

    const RED: DeviceColor = DeviceColor { r: 255, g: 0, b: 0 };

    #[test]
    fn off_only_sends_the_toggle() {
        let plan = split_light_plan(
            SplitHalf::Background,
            &command(json!({"state": "OFF", "brightness": 10})),
        );
        assert_eq!(plan, [Toggle(false)]);
    }

    #[test]
    fn on_always_ends_with_the_toggle() {
        assert_eq!(
            split_light_plan(SplitHalf::Main, &command(json!({"state": "ON"}))),
            [Toggle(true)]
        );
        assert_eq!(
            split_light_plan(
                SplitHalf::Background,
                &command(json!({"state": "ON", "brightness": 60, "color": RED}))
            ),
            [Brightness(60), Color(RED), Toggle(true)]
        );
    }

    #[test]
    fn only_the_main_light_sends_device_wide_controls() {
        let ct = command(json!({"state": "ON", "color_temp": 370}));
        assert_eq!(
            split_light_plan(SplitHalf::Main, &ct),
            [ColorTemperature { kelvin: 2702 }, Toggle(true)]
        );
        assert_eq!(split_light_plan(SplitHalf::Background, &ct), [Toggle(true)]);

        let scene = command(json!({"state": "ON", "effect": "Sunrise", "color": RED}));
        assert_eq!(
            split_light_plan(SplitHalf::Main, &scene),
            [Scene("Sunrise".to_string()), Toggle(true)]
        );
        assert_eq!(
            split_light_plan(SplitHalf::Background, &scene),
            [Color(RED), Toggle(true)]
        );
    }

    fn h1310(sku: &str) -> ServiceDevice {
        let mut device = ServiceDevice::new(sku, "AA:BB:CC:DD:EE:FF:00:01");
        device.http_device_info = Some(
            crate::platform_api::from_json(include_str!("../../test-data/h1310.json")).unwrap(),
        );
        device
    }

    // Main and Device configs list scenes, which needs the network, so only
    // these two are built here.
    #[tokio::test]
    async fn background_light_has_no_device_wide_controls() {
        let state = std::sync::Arc::new(crate::service::state::State::new());
        let light = DeviceLight::for_device(
            &h1310("H1310"),
            &state,
            LightTarget::Half(SplitHalf::Background),
        )
        .await
        .unwrap()
        .light;

        assert_eq!(light.base.unique_id, "gv2mqtt-AABBCCDDEEFF0001-background");
        assert_eq!(
            light.command_topic,
            "gv2mqtt/light/AABBCCDDEEFF0001/command/background"
        );
        assert_eq!(light.supported_color_modes, ["rgb"]);
        assert!(light.brightness);
        assert!(!light.effect);
        assert!(!light.optimistic);
        assert_eq!(light.min_mireds, None);
    }

    #[tokio::test]
    async fn segments_start_disabled_only_with_split_lights() {
        let state = std::sync::Arc::new(crate::service::state::State::new());
        let segment = |sku| {
            let state = state.clone();
            async move {
                DeviceLight::for_device(&h1310(sku), &state, LightTarget::Segment(3))
                    .await
                    .unwrap()
                    .light
                    .enabled_by_default
            }
        };
        assert_eq!(segment("H1310").await, Some(false));
        assert_eq!(segment("H6199").await, None);
    }

    #[test]
    fn echo_reports_what_was_sent() {
        assert_eq!(
            split_light_echo(&[Brightness(60), Color(RED), Toggle(true)]),
            json!({
                "state": "ON",
                "brightness": 60,
                "color_mode": "rgb",
                "color": {"r": 255, "g": 0, "b": 0},
            })
        );
        assert_eq!(
            split_light_echo(&[ColorTemperature { kelvin: 2700 }]),
            json!({"state": "ON", "color_mode": "color_temp", "color_temp": 370})
        );
    }
}

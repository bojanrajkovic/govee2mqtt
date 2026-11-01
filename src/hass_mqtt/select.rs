use crate::hass_mqtt::base::{Device, EntityConfig, Origin};
use crate::hass_mqtt::instance::{publish_entity_config, EntityInstance};
use crate::hass_mqtt::work_mode::ParsedWorkMode;
use crate::platform_api::{DeviceCapability, DeviceParameters};
use crate::service::device::Device as ServiceDevice;
use crate::service::hass::{availability_topic, topic_safe_id, HassClient, IdParameter};
use crate::service::state::StateHandle;
use anyhow::{anyhow, Context};
use chrono::Utc;
use mosquitto_rs::router::{Params, Payload, State};
use serde::Serialize;
use serde_json::json;

#[derive(Serialize, Clone, Debug)]
pub struct SelectConfig {
    #[serde(flatten)]
    pub base: EntityConfig,

    pub command_topic: String,
    pub options: Vec<String>,
    pub state_topic: String,
}

impl SelectConfig {
    pub async fn publish(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        publish_entity_config("select", state, client, &self.base, self).await
    }
}

pub struct WorkModeSelect {
    select: SelectConfig,
    device_id: String,
    state: StateHandle,
}

impl WorkModeSelect {
    pub fn new(device: &ServiceDevice, work_modes: &ParsedWorkMode, state: &StateHandle) -> Self {
        let command_topic = format!("gv2mqtt/{id}/set-work-mode", id = topic_safe_id(device),);
        let state_topic = format!("gv2mqtt/{id}/notify-work-mode", id = topic_safe_id(device));
        let availability_topic = availability_topic();
        let unique_id = format!("gv2mqtt-{id}-workMode", id = topic_safe_id(device),);

        Self {
            select: SelectConfig {
                base: EntityConfig {
                    availability_topic,
                    name: Some("Mode".to_string()),
                    device_class: None,
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id,
                    entity_category: None,
                    icon: None,
                },
                command_topic,
                state_topic,
                options: work_modes.get_mode_names(),
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        }
    }
}

#[async_trait::async_trait]
impl EntityInstance for WorkModeSelect {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.select.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let device = self
            .state
            .device_by_id(&self.device_id)
            .await
            .expect("device to exist");

        if let Some(mode_value) = device.humidifier_work_mode {
            if let Ok(work_mode) = ParsedWorkMode::with_device(&device) {
                let mode_value_json = json!(mode_value);
                if let Some(mode) = work_mode.mode_for_value(&mode_value_json) {
                    client
                        .publish(&self.select.state_topic, mode.name.to_string())
                        .await?;
                }
            }
        } else {
            let work_modes = ParsedWorkMode::with_device(&device)?;

            if let Some(cap) = device.get_state_capability_by_instance("workMode") {
                if let Some(mode_num) = cap.state.pointer("/value/workMode") {
                    if let Some(mode) = work_modes.mode_for_value(mode_num) {
                        return client
                            .publish(&self.select.state_topic, mode.name.to_string())
                            .await;
                    }
                }
            }
        }
        Ok(())
    }
}

pub struct SceneModeSelect {
    select: SelectConfig,
    device_id: String,
    state: StateHandle,
}

impl SceneModeSelect {
    pub async fn new(device: &ServiceDevice, state: &StateHandle) -> anyhow::Result<Option<Self>> {
        let scenes = state.device_list_scenes(device).await?;
        if scenes.is_empty() {
            return Ok(None);
        }

        let command_topic = format!("gv2mqtt/{id}/set-mode-scene", id = topic_safe_id(device));
        let state_topic = format!("gv2mqtt/{id}/notify-mode-scene", id = topic_safe_id(device));
        let availability_topic = availability_topic();
        let unique_id = format!("gv2mqtt-{id}-mode-scene", id = topic_safe_id(device));

        Ok(Some(Self {
            select: SelectConfig {
                base: EntityConfig {
                    availability_topic,
                    name: Some("Mode/Scene".to_string()),
                    device_class: None,
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id,
                    entity_category: None,
                    icon: None,
                },
                command_topic,
                state_topic,
                options: scenes,
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        }))
    }
}

#[async_trait::async_trait]
impl EntityInstance for SceneModeSelect {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.select.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let device = self
            .state
            .device_by_id(&self.device_id)
            .await
            .expect("device to exist");

        if let Some(device_state) = device.device_state() {
            client
                .publish(
                    &self.select.state_topic,
                    device_state.scene.as_deref().unwrap_or(""),
                )
                .await?;
        }

        Ok(())
    }
}

pub async fn mqtt_set_mode_scene(
    Payload(scene): Payload<String>,
    Params(IdParameter { id }): Params<IdParameter>,
    State(state): State<StateHandle>,
) -> anyhow::Result<()> {
    let device = state.resolve_device_for_control(&id).await?;

    state
        .device_set_scene(&device, &scene)
        .await
        .context("mqtt_set_mode_scene: state.device_set_scene")?;

    Ok(())
}

pub struct FanSpeedSelect {
    select: SelectConfig,
    device_id: String,
    state: StateHandle,
}

impl FanSpeedSelect {
    /// Returns None when the capability metadata offers no speeds, because
    /// hass rejects a select that has no options.
    pub fn new(
        device: &ServiceDevice,
        state: &StateHandle,
        cap: &DeviceCapability,
    ) -> Option<Self> {
        let Some(DeviceParameters::Enum { options }) = &cap.parameters else {
            return None;
        };
        if options.is_empty() {
            return None;
        }

        let id = topic_safe_id(device);

        Some(Self {
            select: SelectConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some("Fan Speed".to_string()),
                    device_class: None,
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id: format!("gv2mqtt-{id}-fanSpeedMode"),
                    entity_category: None,
                    icon: None,
                },
                command_topic: format!("gv2mqtt/{id}/set-fan-speed"),
                state_topic: format!("gv2mqtt/{id}/notify-fan-speed"),
                options: options.iter().map(|o| o.name.clone()).collect(),
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        })
    }
}

#[async_trait::async_trait]
impl EntityInstance for FanSpeedSelect {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.select.publish(state, client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let device = self
            .state
            .device_by_id(&self.device_id)
            .await
            .expect("device to exist");

        if let Some(label) = device.fan_speed_label() {
            client.publish(&self.select.state_topic, label).await?;
        }

        Ok(())
    }
}

pub async fn mqtt_set_fan_speed(
    Payload(label): Payload<String>,
    Params(IdParameter { id }): Params<IdParameter>,
    State(state): State<StateHandle>,
) -> anyhow::Result<()> {
    let device = state.resolve_device_for_control(&id).await?;

    let cap = device
        .get_capability_by_instance("fanSpeedMode")
        .ok_or_else(|| anyhow!("{id} has no fanSpeedMode"))?;
    let value = match &cap.parameters {
        Some(DeviceParameters::Enum { options }) => options
            .iter()
            .find(|o| o.name == label)
            .map(|o| o.value.clone()),
        _ => None,
    }
    .ok_or_else(|| anyhow!("fan speed {label} not found for {id}"))?;

    state
        .device_control(&device, cap, value.clone())
        .await
        .context("mqtt_set_fan_speed: state.device_control")?;

    state
        .device_mut(&device.sku, &device.id)
        .await
        .record_fan_speed_command(value, Utc::now());
    state.notify_of_state_change(&device.id).await?;

    Ok(())
}

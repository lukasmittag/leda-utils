use crate::CliArgs;
use anyhow::{anyhow, Result};
use lazy_static::lazy_static;
use rumqttc::{self, Client, Event::Incoming, MqttOptions, Packet::Publish, QoS};
use serde_json::{self, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

static SERVICE_ID: &str = "kanto_auto_deployer";
static TERMINATE_KEY_JSON: &str = "terminate";
static RECONNECT_TIMEOUT: u64 = 2;

lazy_static! {
    static ref LOCK_PATH: PathBuf = {
        match std::option_env!("KAD_LOCK_PATH") {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from("/var/lib/kanto-auto-deployer/KAD.enabled"),
        }
    };
}

fn kad_enabled(lock: &PathBuf) -> bool {
    lock.exists() && lock.is_file()
}

fn disable_kad(lock_path: &PathBuf) -> Result<()> {
    let mut disabled_lock_path = lock_path.clone();

    if !lock_path.is_file() {
        return Err(anyhow!(
            "{:?} is not a regular file or does not exist",
            lock_path
        ));
    }
    if !disabled_lock_path.set_extension("disabled") {
        return Err(anyhow!(
            "Could not change extension of {:?} to *.disabled",
            lock_path
        ));
    }
    fs::rename(lock_path, disabled_lock_path)?;

    Ok(())
}

fn handle_mqtt_payload(
    payload: &[u8],
    lock_path: &PathBuf,
    thread_terminate_flag: &AtomicBool,
) -> Result<()> {
    // We don't care about non-json messages
    let terminate_flag_mqtt = serde_json::from_slice::<HashMap<String, Value>>(payload)?
        .get(TERMINATE_KEY_JSON)
        .ok_or_else(|| {
            anyhow!("MQTT message is valid json, but does not contain key {TERMINATE_KEY_JSON}")
        })?
        .as_bool()
        .ok_or_else(|| anyhow!("Expected boolean type for value for key {TERMINATE_KEY_JSON}"))?;

    // Only if an actual termination request is received, update the atomic flag
    if terminate_flag_mqtt {
        if let Err(e) = disable_kad(&lock_path) {
            log::error!("Could not set KAD lock to disabled: {e}")
        }
        thread_terminate_flag.store(true, Ordering::Relaxed);
    }

    Ok(())
}

fn try_mqtt_reconnect(timeout: &mut Duration, client: &mut Client, topic: &str, delta: Duration) {
    log::error!(
        "MQTT connection lost, trying to re-subscribe in {} s",
        timeout.as_secs()
    );
    if let Err(e) = client.try_subscribe(topic, QoS::ExactlyOnce) {
        log::debug!("Failed to resubscribe: {e}");
        *timeout += delta;
        std::thread::sleep(*timeout);
    } else {
        // Success, reset timeout
        *timeout = delta;
    }
}

pub fn mqtt_main(cli_config: Arc<CliArgs>, thread_terminate_flag: &AtomicBool) -> Result<()> {
    log::debug!(
        "Trying to start MQTT connection with options {:?}",
        &cli_config.mqtt
    );

    if !kad_enabled(&LOCK_PATH) {
        log::error!(
            "The lock at {:?} does not exist, but KAD was started with the MQTT client \
        option. MQTT listener will not be started, but KAD will still run in daemon mode. \
        If running as a system service this might mean KAD has previously seen the desired \
        state MQTT message and has auto-disabled itself to avoid conflicts with CUA and might \
        lead to unexpected behavior.",
            *LOCK_PATH
        );
        return Ok(());
    }
    log::info!("MQTT for daemon mode enabled. Will auto-disable whenever VUM takes over.");

    let mut mqttoptions = MqttOptions::new(SERVICE_ID, &cli_config.mqtt.ip, cli_config.mqtt.port);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    let delta = Duration::from_secs(RECONNECT_TIMEOUT);
    let mut timeout = delta;

    let (mut client, mut connection) = Client::new(mqttoptions.clone(), 10);
    client.subscribe(&cli_config.mqtt.topic, QoS::ExactlyOnce)?;

    for notification in connection.iter() {
        // We only care about incoming messages
        if let Ok(msg) = notification {
            if let Incoming(Publish(pub_msg)) = msg {
                let _r = handle_mqtt_payload(&pub_msg.payload, &LOCK_PATH, thread_terminate_flag);
                if let Err(e) = _r {
                    log::debug!("MQTT message parsing error: {e}")
                }
            }
        } else {
            try_mqtt_reconnect(&mut timeout, &mut client, &cli_config.mqtt.topic, delta);
        }
    }

    Ok(())
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Module to handle Philips Hue bridges
//!
//! This module implements various aspects Philips Hue bridges (short: hubs).
//! It handles pairing and light enumeration. Detected lights are
//! reported to the adapter's main loop via IPC.
//!
//! The module spawns a management thread for every hub.

use serde_json;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use super::hub_api::HubApi;
use super::{ HueAction, PhilipsHueAdapter, structs };
use traits::Controller;
use uuid::Uuid;

pub struct Hub<C> {
    pub adapter: PhilipsHueAdapter<C>,
    pub id: String,
    pub ip: String,
    pub api: Arc<HubApi>,
}

impl<C: Controller> Hub<C> {
    pub fn new(adapter: PhilipsHueAdapter<C>, id: &str, ip: &str) -> Self {
        // Get API token from config store, default to a random UUID.
        // The API token is used like a password when pairing with a
        // Philips Hue bridge. Once paired, it is sent with every
        // API request for authentication purposes, so it is crucial
        // that it is not predictable.
        let token = adapter.controller.get_config().get_or_set_default(
            "philips_hue",
            &format!("token_{}", id),
            &Uuid::new_v4().to_simple_string());
        Hub {
            adapter: adapter,
            id: id.to_owned(),
            ip: ip.to_owned(),
            api: Arc::new(HubApi::new(id, ip, &token)),
        }
    }
    pub fn start(&self) {
        info!("Starting Hue Hub Service for {}", self.id);
        let adapter = self.adapter.clone();
        let id = self.id.clone();
        let api = self.api.clone();

        thread::spawn(move || {

            // The main Hub management loop
            loop {
                if !api.is_available() {
                    // Re-check availability every minute.
                    thread::sleep(Duration::from_millis(60*1000));
                    continue;
                }

                // If the Hub is not paired, try pairing.
                if !api.is_paired() {
                    debug!("Push pairing button on Philips Hue Bridge ID {}", id);

                    // Try pairing for 120 seconds.
                    for _ in 0..120 {
                        adapter.controller.adapter_notification(
                            json_value!({ adapter: "philips_hue",
                                message: "NeedsPairing", hub: id }));
                        if api.try_pairing() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(1000));
                    }

                    if api.is_paired() {
                        info!("Paired with Philips Hue Bridge ID {}", id);
                        adapter.controller.adapter_notification(
                            json_value!({ adapter: "philips_hue", message: "PairingSuccess",
                                hub: id }));
                    } else {
                        warn!("Pairing timeout with Philips Hue Bridge ID {}", id);
                        adapter.controller.adapter_notification(
                            json_value!({ adapter: "philips_hue", message: "PairingTimeout",
                                hub: id }));
                        // Giving up for this Hub.
                        // Re-try pairing every hour.
                        thread::sleep(Duration::from_millis(60*60*1000));
                        continue;
                    }
                }

                // We have a paired Hub, instantiate the lights services.
                // Extract and log some info
                let setting = api.get_settings();
                let hs = structs::Settings::new(&setting).unwrap(); // TODO: no unwrap
                info!(
                    "Connected to Philips Hue bridge model {}, ID {}, software version {}, IP address {}",
                    hs.config.modelid, hs.config.bridgeid, hs.config.swversion,
                    hs.config.ipaddress);

                let light_ids = api.get_lights();
                for light_id in light_ids {
                    debug!("Found light {} on hub {}", light_id, id);
                    adapter.send(HueAction::AddLight(id.to_owned(), light_id.to_owned()));
                }

                loop { // Forever
                    // TODO: add hub monitoring (polling) here
                    thread::sleep(Duration::from_millis(60*1000));
                }
            }
        });
    }
    pub fn update_ip(&mut self, new_ip: &str) {
        debug!("Updating IP for {} to {}", self.id, new_ip);
        self.ip = new_ip.to_owned();
    }
    pub fn stop(&self) {
        debug!("Stopping Hue Hub Service for {}", self.id)
    }
}

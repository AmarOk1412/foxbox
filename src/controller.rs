/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

extern crate serde_json;
extern crate collections;
extern crate mio;

use adapters::AdapterManager;
use config_store::ConfigStore;
use core::marker::Reflect;
use http_server::HttpServer;
use iron::{Request, Response, IronResult};
use iron::headers::{ ContentType, AccessControlAllowOrigin };
use iron::status::Status;
use self::collections::vec::IntoIter;
use service::{ Service, ServiceAdapter, ServiceProperties };
use std::collections::hash_map::HashMap;
use std::io;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::{ Arc, Mutex, RwLock };
use ws_server::WsServer;
use ws;

#[derive(Clone)]
pub struct FoxBox {
    pub verbose: bool,
    hostname: String,
    http_port: u16,
    ws_port: u16,
    services: Arc<Mutex<HashMap<String, Box<Service>>>>,
    websockets: Arc<Mutex<HashMap<ws::util::Token, ws::Sender>>>,
    pub config: Arc<RwLock<ConfigStore>>,
}

const DEFAULT_HOSTNAME: &'static str = "::"; // ipv6 default.
const DEFAULT_DOMAIN: &'static str = ".local";

pub trait Controller : Send + Sync + Clone + Reflect + 'static {
    fn run(&mut self);
    fn dispatch_service_request(&self, id: String, request: &mut Request) -> IronResult<Response>;
    fn adapter_started(&self, adapter: String);
    fn adapter_notification(&self, notification: serde_json::value::Value);
    fn add_service(&self, service: Box<Service>);
    fn remove_service(&self, id: String);
    fn get_service_properties(&self, id: String) -> Option<ServiceProperties>;
    fn services_count(&self) -> usize;
    fn services_as_json(&self) -> Result<String, serde_json::error::Error>;
    fn get_http_root_for_service(&self, service_id: String) -> String;
    fn get_ws_root_for_service(&self, service_id: String) -> String;
    fn get_config(&self, namespace: &str, property: &str) -> Option<String>;
    fn get_config_or_set_default(&mut self, namespace: &str, property: &str, default: &str) -> String;
    fn set_config(&mut self, namespace: &str, property: &str, value: &str);
    fn http_as_addrs(&self) -> Result<IntoIter<SocketAddr>, io::Error>;

    fn add_websocket(&mut self, socket: ws::Sender);
    fn remove_websocket(&mut self, socket: ws::Sender);
    fn broadcast_to_websockets(&self, data: serde_json::value::Value);
}

impl FoxBox {

    pub fn new(verbose: bool,
               hostname: Option<String>,
               http_port: u16,
               ws_port: u16) -> Self {

        FoxBox {
            services: Arc::new(Mutex::new(HashMap::new())),
            websockets: Arc::new(Mutex::new(HashMap::new())),
            verbose: verbose,
            hostname: hostname.map_or(DEFAULT_HOSTNAME.to_owned(), |name| {
                format!("{}{}", name, DEFAULT_DOMAIN)
            }),
            http_port: http_port,
            ws_port: ws_port,
            config: Arc::new(RwLock::new(ConfigStore::new("foxbox.conf"))),
        }
    }
}

impl Controller for FoxBox {

    fn run(&mut self) {
        debug!("Starting controller");

        let mut event_loop = mio::EventLoop::new().unwrap();

        HttpServer::new(self.clone()).start();
        WsServer::start(self.clone(), self.hostname.to_owned(), self.ws_port);
        AdapterManager::new(self.clone()).start();
        event_loop.run(&mut FoxBoxEventLoop { controller: self.clone() }).unwrap();
    }

    fn dispatch_service_request(&self, id: String, request: &mut Request) -> IronResult<Response> {
        let services = self.services.lock().unwrap();
        match services.get(&id) {
            None => {
                let mut response = Response::with(format!("No Such Service: {}", id));
                response.status = Some(Status::BadRequest);
                response.headers.set(AccessControlAllowOrigin::Any);
                response.headers.set(ContentType::plaintext());
                Ok(response)
            }
            Some(service) => {
                service.process_request(request)
            }
        }
    }

    fn adapter_started(&self, adapter: String) {
        self.broadcast_to_websockets(json_value!({ type: "core/adapter/start", name: adapter }));
    }

    fn adapter_notification(&self, notification: serde_json::value::Value) {
        self.broadcast_to_websockets(json_value!({ type: "core/adapter/notification", message: notification }));
    }

    fn add_service(&self, service: Box<Service>) {
        let mut services = self.services.lock().unwrap();
        let service_id = service.get_properties().id;
        services.insert(service_id.clone(), service);
        self.broadcast_to_websockets(json_value!({ type: "core/service/start", id: service_id }));
    }

    fn remove_service(&self, id: String) {
        let mut services = self.services.lock().unwrap();
        services.remove(&id);
        self.broadcast_to_websockets(json_value!({ type: "core/service/stop", id: id }));
    }

    fn services_count(&self) -> usize {
        let services = self.services.lock().unwrap();
        services.len()
    }

    fn get_service_properties(&self, id: String) -> Option<ServiceProperties> {
        let services = self.services.lock().unwrap();
        services.get(&id).map(|v| v.get_properties().clone() )
    }

    fn services_as_json(&self) -> Result<String, serde_json::error::Error> {
        let services = self.services.lock().unwrap();
        let mut array: Vec<&Box<Service>> = vec!();
        for service in services.values() {
            array.push(service);
        }
        serde_json::to_string(&array)
    }

    fn get_http_root_for_service(&self, service_id: String) -> String {
        format!("http://{}:{}/services/{}/", self.hostname, self.http_port, service_id)
    }

    fn get_ws_root_for_service(&self, service_id: String) -> String {
        format!("ws://{}:{}/services/{}/", self.hostname, self.ws_port, service_id)
    }

    fn get_config(&self, namespace: &str, property: &str) -> Option<String> {
        self.config.read().unwrap().get(namespace, property)
            .map(|value| { value.to_owned() })
    }

    fn get_config_or_set_default(&mut self, namespace: &str, property: &str, default: &str) -> String {
        self.get_config(namespace, property).unwrap_or_else(|| {
            self.set_config(namespace, property, default);
            default.to_owned()
        })
    }

    fn set_config(&mut self, namespace: &str, property: &str, value: &str) {
        self.config.write().unwrap().set(namespace, property, value);
    }

    fn http_as_addrs(&self) -> Result<IntoIter<SocketAddr>, io::Error> {
        (self.hostname.as_str(), self.http_port).to_socket_addrs()
    }

    fn add_websocket(&mut self, socket: ws::Sender) {
        self.websockets.lock().unwrap().insert(socket.token(), socket);
    }

    fn remove_websocket(&mut self, socket: ws::Sender) {
        self.websockets.lock().unwrap().remove(&socket.token());
    }

    fn broadcast_to_websockets(&self, data: serde_json::value::Value) {
        let serialized = serde_json::to_string(&data).unwrap_or("{}".to_owned());
        debug!("broadcast_to_websockets {}", serialized.clone());
        for socket in self.websockets.lock().unwrap().values() {
            match socket.send(serialized.clone()) {
                Ok(_) => (),
                Err(err) => error!("Error sending to socket: {}", err)
            }
        }
    }
}

struct FoxBoxEventLoop {
    controller: FoxBox
}

impl mio::Handler for FoxBoxEventLoop {
    type Timeout = ();
    type Message = ();
}


#[cfg(test)]
describe! controller {

    before_each {
        use stubs::service::ServiceStub;

        let service = ServiceStub;
        let controller = FoxBox::new(false, Some("foxbox".to_owned()), 1234, 5678);
    }

    describe! add_service {
        it "should increase number of services" {
            controller.add_service(Box::new(service));
            assert_eq!(controller.services_count(), 1);
        }

        it "should make service available" {
            controller.add_service(Box::new(service));

            match controller.get_service_properties("1".to_owned()) {
                Some(props) => {
                    assert_eq!(props.id, "1");
                }
                None => assert!(false, "No service with id 1")
            }
        }

        it "should create http root" {
            controller.add_service(Box::new(service));
            assert_eq!(controller.get_http_root_for_service("1".to_string()),
                       "http://foxbox.local:1234/services/1/");
        }

        it "should create ws root" {
            controller.add_service(Box::new(service));
            assert_eq!(controller.get_ws_root_for_service("1".to_string()),
                       "ws://foxbox.local:5678/services/1/");
        }

        it "should return a json" {
            controller.add_service(Box::new(service));

            match controller.services_as_json() {
                Ok(txt) => assert_eq!(txt, "[{\"id\":\"1\",\"name\":\"dummy service\",\"description\":\"really nothing to see\",\"http_url\":\"2\",\"ws_url\":\"3\"}]"),
                Err(err) => assert!(false, err)
            }
        }
    }


    it "should delete a service" {
        controller.add_service(Box::new(service));
        let id = "1".to_owned();
        controller.remove_service(id);
        assert_eq!(controller.services_count(), 0);
    }
}

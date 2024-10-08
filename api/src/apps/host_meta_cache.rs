/*-
 * ========================LICENSE_START=================================
 * PREvant REST API
 * %%
 * Copyright (C) 2018 - 2021 aixigo AG
 * %%
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
 * THE SOFTWARE.
 * =========================LICENSE_END==================================
 */

use crate::apps::{Apps, AppsError};
use crate::infrastructure::HttpForwarder;
use crate::models::service::{Service, ServiceBuilder, ServiceStatus};
use crate::models::{AppName, RequestInfo, WebHostMeta};
use chrono::{DateTime, Utc};
use evmap::{ReadHandleFactory, WriteHandle};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use http::header::{HOST, USER_AGENT};
use multimap::MultiMap;
use std::collections::{HashMap, HashSet};
use std::convert::From;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use yansi::Paint;

pub struct HostMetaCache {
    reader_factory: ReadHandleFactory<Key, Arc<Value>>,
}
pub struct HostMetaCrawler {
    writer: WriteHandle<Key, Arc<Value>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    app_name: AppName,
    service_id: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Value {
    timestamp: DateTime<Utc>,
    web_host_meta: WebHostMeta,
}

pub fn new() -> (HostMetaCache, HostMetaCrawler) {
    let (reader, writer) = evmap::new();

    (
        HostMetaCache {
            reader_factory: reader.factory(),
        },
        HostMetaCrawler { writer },
    )
}

impl HostMetaCache {
    pub fn update_meta_data(
        &self,
        services: MultiMap<AppName, Service>,
        request_info: &RequestInfo,
    ) -> MultiMap<AppName, Service> {
        let mut assigned_apps = MultiMap::new();

        let reader = self.reader_factory.handle();

        for (app_name, service) in services.iter_all() {
            for service in service.iter().cloned() {
                let key = Key {
                    app_name: app_name.clone(),
                    service_id: service.id().to_string(),
                };

                let mut b =
                    ServiceBuilder::from(service).base_url(request_info.get_base_url().clone());
                if let Some(value) = reader.get_one(&key) {
                    b = b.web_host_meta(
                        value
                            .web_host_meta
                            .with_base_url(request_info.get_base_url()),
                    );
                }

                assigned_apps.insert(key.app_name, b.build().unwrap());
            }
        }

        assigned_apps
    }
}

impl HostMetaCrawler {
    pub fn spawn(mut self, apps: Arc<Apps>) {
        let timestamp_prevant_startup = Utc::now();

        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(5)).await;
                if let Err(err) = self.crawl(apps.clone(), timestamp_prevant_startup).await {
                    error!("Cannot load apps: {}", err);
                }
            }
        });
    }

    async fn crawl(
        &mut self,
        all_apps: Arc<Apps>,
        since_timestamp: DateTime<Utc>,
    ) -> Result<(), AppsError> {
        debug!("Resolving list of apps for web host meta cache.");
        let apps = all_apps.get_apps().await?;

        self.clear_stale_web_host_meta(&apps);

        let services_without_host_meta = apps
            .iter_all()
            .flat_map(|(app_name, services)| {
                services
                    .iter()
                    // avoid cloning when https://github.com/havarnov/multimap/issues/24 has been implemented
                    .map(move |service| {
                        let key = Key {
                            app_name: app_name.clone(),
                            service_id: service.id().to_string(),
                        };
                        (key, service.clone())
                    })
            })
            .filter(|(key, _service)| !self.writer.contains_key(key))
            .collect::<Vec<(Key, Service)>>();

        if services_without_host_meta.is_empty() {
            return Ok(());
        }

        debug!(
            "Resolving web host meta data for {:?}.",
            services_without_host_meta
                .iter()
                .map(|(k, service)| format!("({}, {})", k.app_name, service.service_name()))
                .fold(String::new(), |a, b| a + &b + ", ")
        );
        let now = Utc::now();
        let duration_prevant_startup = Utc::now().signed_duration_since(since_timestamp);
        let resolved_host_meta_infos = Self::resolve_host_meta(
            all_apps,
            services_without_host_meta,
            duration_prevant_startup,
        )
        .await;
        for (key, _service, web_host_meta) in resolved_host_meta_infos {
            if !web_host_meta.is_valid() {
                continue;
            }

            self.writer.insert(
                key,
                Arc::new(Value {
                    timestamp: now,
                    web_host_meta,
                }),
            );
        }

        self.writer.refresh();
        Ok(())
    }

    fn clear_stale_web_host_meta(&mut self, apps: &MultiMap<AppName, Service>) {
        let copy: HashMap<Key, Vec<_>> = self
            .writer
            .map_into(|k, vs| (k.clone(), vs.iter().cloned().collect()));

        let keys_to_clear = copy
            .into_iter()
            .flat_map(|(key, values)| values.into_iter().map(move |v| (key.clone(), v)))
            .filter(|(key, value)| {
                let service = match apps.get_vec(&key.app_name) {
                    Some(services) => services.iter().find(|s| s.id() == &key.service_id),
                    None => {
                        return true;
                    }
                };

                match service {
                    Some(service) => {
                        *service.status() == ServiceStatus::Paused
                            || *service.started_at() > value.timestamp
                    }
                    None => true,
                }
            })
            .map(|(key, _)| key)
            .collect::<HashSet<Key>>();

        if keys_to_clear.is_empty() {
            return;
        }

        debug!("Clearing stale apps: {:?}", keys_to_clear);

        for key in keys_to_clear {
            self.writer.empty(key);
        }
        self.writer.refresh();
    }

    async fn resolve_host_meta(
        apps: Arc<Apps>,
        services_without_host_meta: Vec<(Key, Service)>,
        duration_prevant_startup: chrono::Duration,
    ) -> Vec<(Key, Service, WebHostMeta)> {
        let number_of_services = services_without_host_meta.len();
        if number_of_services == 0 {
            return Vec::with_capacity(0);
        }

        let infrastructure = apps.infrastructure();

        let mut futures = services_without_host_meta
            .into_iter()
            .map(|(key, service)| async {
                let http_forwarder = match infrastructure.http_forwarder().await {
                    Ok(portforwarder) => portforwarder,
                    Err(err) => {
                        error!(
                            "Cannot forward TCP connection for {}, {}: {err}",
                            key.app_name,
                            service.service_name()
                        );
                        return (key, service, WebHostMeta::empty());
                    }
                };
                Self::resolve_web_host_meta(http_forwarder, key, service, duration_prevant_startup)
                    .await
            })
            .collect::<FuturesUnordered<_>>();

        let mut resolved_host_meta_infos = Vec::with_capacity(number_of_services);
        while let Some(resolved_host_meta) = futures.next().await {
            resolved_host_meta_infos.push(resolved_host_meta);
        }

        resolved_host_meta_infos
    }

    async fn resolve_web_host_meta(
        http_forwarder: Box<dyn HttpForwarder + Send>,
        key: Key,
        service: Service,
        duration_prevant_startup: chrono::Duration,
    ) -> (Key, Service, WebHostMeta) {
        let response = http_forwarder
            .request_web_host_meta(
                &key.app_name,
                service.service_name(),
                http::Request::builder()
                    // TODO: include real service traefic route, see #169
                    .header(
                        USER_AGENT.as_str(),
                        format!("PREvant/{}", clap::crate_version!()),
                    )
                    .method("GET")
                    .uri("/.well-known/host-meta.json")
                    .header(HOST, "127.0.0.1")
                    .header("Connection", "Close")
                    .header("Forwarded", "host=www.prevant.example.com;proto=http")
                    .header(
                        "X-Forwarded-Prefix",
                        format!("/{}/{}", service.app_name(), service.service_name()),
                    )
                    .header("Accept", "application/json")
                    .body(http_body_util::Empty::<bytes::Bytes>::new())
                    .unwrap(),
            )
            .await;

        let meta = match response {
            Ok(Some(meta)) => {
                debug!(
                    "Got host meta for service {} of {}",
                    Paint::magenta(service.service_name()),
                    Paint::magenta(service.app_name()),
                );
                meta
            }
            Ok(None) => {
                debug!(
                    "Cannot parse host meta for service {} of {}",
                    Paint::magenta(service.service_name()),
                    Paint::magenta(service.app_name()),
                );
                WebHostMeta::empty()
            }
            Err(err) => {
                debug!(
                    "Cannot acquire host meta for service {} of {}: {}",
                    Paint::magenta(service.service_name()),
                    Paint::magenta(service.app_name()),
                    err
                );

                let duration = Utc::now().signed_duration_since(*service.started_at());
                if duration >= chrono::Duration::minutes(5)
                    && duration_prevant_startup >= chrono::Duration::minutes(1)
                {
                    info!(
                        "Service {} is running for {}, therefore, it will be assumed that host-meta.json is not available.",
                        Paint::magenta(service.service_name()), duration
                    );
                    WebHostMeta::empty()
                } else {
                    WebHostMeta::invalid()
                }
            }
        };
        (key, service, meta)
    }
    #[cfg(test)]
    pub fn fake_empty_host_meta_info(&mut self, app_name: AppName, service_id: String) {
        let web_host_meta = WebHostMeta::empty();
        let value = Arc::new(Value {
            timestamp: chrono::Utc::now(),
            web_host_meta,
        });

        self.writer.insert(
            Key {
                app_name,
                service_id,
            },
            value,
        );

        self.writer.refresh();
        self.writer.flush();
    }
}

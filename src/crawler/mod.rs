pub mod crawl_target;

use core::fmt;
use std::collections::HashSet;

use futures::FutureExt;
use reqwest::{header, Client, Url};
use scraper::{Html, Selector};

use crawl_target::CrawlTarget;
use tokio::{sync::mpsc, task::JoinSet};
use url::Host;

use crate::util::web::*;

pub struct Vdovitsa {
    crawl_targets: HashSet<CrawlTarget>,
    client: Client,
}

impl Vdovitsa {
    /// Create a Vdovitsa crawler with initial targets.
    pub fn new(initial_targets: HashSet<CrawlTarget>) -> Result<Vdovitsa, CrawlerError> {
        // Configure the web client
        let client_config = Client::builder().user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ));

        if let Ok(client) = client_config.build() {
            Ok(Vdovitsa {
                crawl_targets: initial_targets,
                client,
            })
        } else {
            Err(CrawlerError::with_message(
                "Failed to initialse web client.",
            ))
        }
    }

    pub async fn crawl(&mut self) {
        let (tx, mut new_targets) = mpsc::channel::<CrawlTarget>(32);
        let weak_tx = tx.downgrade();

        let mut crawl_target_tasks: JoinSet<()> = JoinSet::new();

        // Start crawling the initial targets
        for target in &self.crawl_targets {
            crawl_target_tasks.spawn(Self::crawl_target(
                self.client.clone(),
                target.clone(),
                weak_tx.upgrade().unwrap(),
            ));
        }

        drop(tx);

        // Process new potential targets
        while let Some(new_potential_target) = new_targets.recv().await {
            while let Some(Some(_)) = crawl_target_tasks.join_next().now_or_never() {} // Remove finished tasks from crawl_target_tasks
            if crawl_target_tasks.is_empty() {
                new_targets.close();
            }

            if !self.crawl_targets.contains(&new_potential_target) {
                self.crawl_targets.insert(new_potential_target.clone());

                crawl_target_tasks.spawn(Self::crawl_target(
                    self.client.clone(),
                    new_potential_target,
                    weak_tx.upgrade().unwrap(),
                ));
            }
        }

        println!("Crawling done");
    }

    async fn crawl_target(
        client: Client,
        crawl_target: CrawlTarget,
        new_targets: mpsc::Sender<CrawlTarget>,
    ) {
        let crawl_target_host = crawl_target.host().to_owned();
        println!("Crawling target... {}", crawl_target_host);

        let mut crawled_urls: HashSet<String> = HashSet::new();
        crawled_urls.insert(format!("{}", crawl_target.host()).clone());

        let (tx, mut new_links) = mpsc::channel(32);
        let mut crawl_url_tasks: JoinSet<()> = JoinSet::new();
        crawl_url_tasks.spawn(Self::crawl_url(
            client.clone(),
            Url::parse(&format!("https://{}", crawl_target.host())).unwrap(),
            tx.clone(),
        ));

        while let Some(new_potential_link) = new_links.recv().await {
            while let Some(Some(_)) = crawl_url_tasks.join_next().now_or_never() {} // Remove finished tasks from crawl_url_tasks
            if crawl_url_tasks.is_empty() {
                new_links.close();
            }
            for link in new_potential_link {
                if let Ok(parsed_url) = Url::parse(&link) {
                    // Only HTTP and HTTPS are supported
                    if parsed_url.scheme().eq("https") || parsed_url.scheme().eq("http") {
                        match parsed_url.host() {
                            Some(parsed_url_host) => {
                                match Self::compare_hosts(
                                    &parsed_url_host.to_owned(),
                                    crawl_target.host(),
                                ) {
                                    HostRelation::Same => {
                                        // The link belongs to the current target
                                        let normalized_url: String = parsed_url
                                            .to_string()
                                            .trim_end_matches('/')
                                            .split_once("://")
                                            .unwrap()
                                            .1
                                            .to_string();
                                        if !crawled_urls.contains(&normalized_url) {
                                            crawled_urls.insert(normalized_url);
                                            crawl_url_tasks.spawn(Self::crawl_url(
                                                client.clone(),
                                                parsed_url,
                                                tx.clone(),
                                            ));
                                        }
                                    }
                                    HostRelation::Related => {
                                        // The link points to a new potential target
                                        new_targets
                                            .send(CrawlTarget::new(parsed_url_host.clone()))
                                            .await
                                            .unwrap();
                                    }

                                    HostRelation::Unrelated => (), // Skip links to unrelated hosts
                                }
                            }
                            None => (),
                        }
                    } else {
                        continue;
                    }
                } else {
                    // The link may be relative
                    let relative_path = link.trim_end_matches('/').to_string();
                    if relative_path.starts_with('/') {
                        // The URL is indeed a relative path
                        let constructed_link = format!(
                            "https://{}{}",
                            crawl_target.host().to_string(),
                            relative_path
                        );
                        if !crawled_urls.contains(&constructed_link) {
                            crawl_url_tasks.spawn(Self::crawl_url(
                                client.clone(),
                                Url::parse(&constructed_link).unwrap(),
                                tx.clone(),
                            ));
                            crawled_urls.insert(constructed_link);
                        }
                    }
                }
            }
        }

        println!("Finished crawling target: {}", crawl_target_host);
    }

    async fn crawl_url(client: Client, url: Url, new_links: mpsc::Sender<HashSet<String>>) {
        // Check if the URL returns an HTML page
        let Ok(response_headers) = get_url_response_headers(&client, url.clone()).await else { return; };
        let Some(content_type) = response_headers.get(header::CONTENT_TYPE) else { return; };
        let Ok(content_type) = content_type.to_str() else { return; };
        if !content_type.starts_with("text/html") {
            return;
        }

        // Send get request
        let mut new_links_to_crawl: HashSet<String> = HashSet::new();

        if let Ok(response) = get_url(&client, url).await {
            if let Ok(response_text) = response.text().await {
                // Check content for links
                let document = Html::parse_document(&response_text);
                let selector = Selector::parse("a").unwrap();

                // Parse links from the webpage
                for element in document.select(&selector) {
                    // Try to get the href attribute
                    if let Some(href) = element.value().attr("href") {
                        new_links_to_crawl.insert(href.to_owned());
                    }
                }
            }
        }

        // Send the new links to the parent crawl_target
        if !new_links_to_crawl.is_empty() {
            new_links.send(new_links_to_crawl).await.unwrap();
        }
    }

    /// Returns whether two hosts are related.
    fn compare_hosts(host1: &Host<String>, host2: &Host<String>) -> HostRelation {
        match (host1, host2) {
            (Host::Domain(domain1), Host::Domain(domain2)) => {
                if domain1.eq(domain2) {
                    HostRelation::Same
                } else {
                    let host1_parts: Vec<&str> = domain1.split('.').rev().take(2).collect();
                    let host2_parts: Vec<&str> = domain2.split('.').rev().take(2).collect();

                    if host1_parts.eq(&host2_parts) {
                        HostRelation::Related
                    } else {
                        HostRelation::Unrelated
                    }
                }
            }

            // Both hosts are IPv4 addresses
            (Host::Ipv4(ip1), Host::Ipv4(ip2)) => {
                if ip1.eq(ip2) {
                    HostRelation::Same
                } else {
                    HostRelation::Unrelated
                }
            }

            // Both hosts are IPv6 address
            (Host::Ipv6(ip1), Host::Ipv6(ip2)) => {
                if ip1.eq(ip2) {
                    HostRelation::Same
                } else {
                    HostRelation::Unrelated
                }
            }

            // TODO: implement domain name resolution for the cases where one host is a domain and the other is an IP
            _ => HostRelation::Unrelated,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HostRelation {
    Same,      // The hosts are the same host
    Related,   // The hosts are related
    Unrelated, // The hosts are unrelated
}

#[derive(Debug)]
pub struct CrawlerError {
    message: String,
}

impl CrawlerError {
    fn new() -> CrawlerError {
        CrawlerError {
            message: String::from("Crawl Target Error"),
        }
    }

    fn with_message(message: &str) -> CrawlerError {
        CrawlerError {
            message: String::from(message),
        }
    }
}

impl std::error::Error for CrawlerError {}

impl fmt::Display for CrawlerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}
